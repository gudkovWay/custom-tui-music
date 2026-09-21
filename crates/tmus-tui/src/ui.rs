//! TUI. Терминальные глифы — только ASCII и Unicode-блоки: Nerd Font /
//! FontAwesome не используются, потому что именно на них сломался
//! готовый клиент youtui, чей README требует особых шрифтов.

use std::collections::HashMap;
use std::io::stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use tmus_core::model::{AuthStatus, LoopMode, PlaybackStatus, Playlist, Rating, SearchResult, Track, TrackId};
use tmus_core::protocol::{CatalogSource, Cmd, Event, Payload, PlayerState};
use tmus_core::Paths;

use crate::client::Client;
use crate::fmt_time;

/// Бейдж провайдера: глиф и цвет из ProviderView. Живёт в App как карта
/// id -> бейдж, заполняется из Cmd::Providers и обновляется на
/// AuthChanged — сессии могут оживать и протухать на ходу.
struct Badge {
    glyph: String,
    color: Color,
}

/// Парсинг "#rrggbb": по 2 hex-цифры на канал. None при любом
/// несовпадении — кривой цвет от провайдера не должен ронять кадр.
fn hex_color(s: &str) -> Option<Color> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Color::Rgb(channel(0)?, channel(2)?, channel(4)?))
}

/// Цветной глиф провайдера перед названием: None — бейджа нет, строка
/// остаётся в прежнем текстовом виде "[{provider}] ...".
fn badge_span<'a>(badges: &HashMap<String, Badge>, provider: &str) -> Option<Span<'a>> {
    badges.get(provider).map(|b| {
        Span::styled(format!("{} ", b.glyph), Style::default().fg(b.color))
    })
}

/// Подпись источника каталога для заголовков панелей: "all" или имя
/// провайдера (fallback — id, если Cmd::Providers ещё не отвечал).
fn source_label(app: &App) -> String {
    match &app.source.provider {
        None => "all".to_owned(),
        Some(id) => app.provider_names.get(id).cloned().unwrap_or_else(|| id.clone()),
    }
}

/// Панели левой колонки. Tab переключает по кругу.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Panel {
    Library,
    Queue,
    Search,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Focus {
    Panel,
    Tracks,
}

struct Nav {
    panel: Panel,
    focus: Focus,
    open_playlist: Option<usize>,
    library_sel: ListState,
    queue_sel: ListState,
    search_sel: ListState,
    playlist_sel: ListState,
}

impl Nav {
    fn new() -> Self {
        Self {
            panel: Panel::Library,
            focus: Focus::Panel,
            open_playlist: None,
            library_sel: ListState::default(),
            queue_sel: ListState::default(),
            search_sel: ListState::default(),
            playlist_sel: ListState::default(),
        }
    }

    fn enter_playlist(&mut self, playlists_len: usize) -> bool {
        if self.focus != Focus::Panel || self.panel != Panel::Library {
            return false;
        }
        let Some(idx) = self.library_sel.selected() else {
            return false;
        };
        if idx >= playlists_len {
            return false;
        }
        self.focus = Focus::Tracks;
        self.open_playlist = Some(idx);
        self.playlist_sel.select(Some(0));
        true
    }

    fn leave_playlist(&mut self) {
        self.focus = Focus::Panel;
        self.open_playlist = None;
    }

    fn tab(&mut self) {
        self.focus = Focus::Panel;
        self.open_playlist = None;
        self.panel = match self.panel {
            Panel::Library => Panel::Queue,
            Panel::Queue => Panel::Search,
            Panel::Search => Panel::Library,
        };
    }

    fn move_active(&mut self, delta: i64, panel_len: usize, queue_len: usize, search_len: usize, tracks_len: usize) {
        let (len, sel) = match (self.focus, self.panel) {
            (Focus::Tracks, _) => (tracks_len, &mut self.playlist_sel),
            (Focus::Panel, Panel::Library) => (panel_len, &mut self.library_sel),
            (Focus::Panel, Panel::Queue) => (queue_len, &mut self.queue_sel),
            (Focus::Panel, Panel::Search) => (search_len, &mut self.search_sel),
        };
        if len == 0 {
            return;
        }
        let current = sel.selected().unwrap_or(0) as i64;
        let next = (current + delta).clamp(0, len as i64 - 1);
        sel.select(Some(next as usize));
    }

    fn track_play_sel(&self) -> Option<(usize, usize)> {
        let idx = self.open_playlist?;
        let start = self.playlist_sel.selected()?;
        Some((idx, start))
    }
}

/// Пикер плейлистов (`a` на треке): список «+ New playlist…» плюс
/// библиотечные плейлисты. `input` — режим ввода имени нового
/// плейлиста; в нём j/k и выбор пунктов не работают до Enter/Esc.
struct PlaylistPicker {
    sel: usize,
    input: Option<String>,
}

impl PlaylistPicker {
    fn new() -> Self {
        Self { sel: 0, input: None }
    }

    /// Пункт 0 — «создать новый», остальные — плейлисты библиотеки.
    fn move_cursor(&mut self, delta: i64, playlists_len: usize) {
        let len = playlists_len + 1;
        if len == 0 {
            return;
        }
        let next = (self.sel as i64 + delta).clamp(0, len as i64 - 1);
        self.sel = next as usize;
    }

    fn is_new_selected(&self) -> bool {
        self.sel == 0
    }

    fn begin_input(&mut self) {
        self.input = Some(String::new());
    }

    /// Готовое имя, если введено что-то кроме пробелов; иначе режим
    /// ввода остаётся открытым — пустое имя создавать нельзя.
    fn confirmed_title(&self) -> Option<String> {
        let trimmed = self.input.as_deref()?.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }
}

struct App {
    client: Client,
    /// Для фоновых команд: play-команды не должны держать цикл ввода.
    paths: Paths,
    state: PlayerState,
    source: CatalogSource,
    playlists: Vec<Playlist>,
    /// Треки выбранного плейлиста — правая колонка.
    playlist_tracks: Vec<Track>,
    search_results: Vec<SearchResult>,
    search_input: String,
    search_mode: bool,
    nav: Nav,
    /// Кэш треков плейлистов по id: (треки, момент загрузки). Без него
    /// каждое движение j/k по колонке библиотеки уходило в InnerTube
    /// (~1–2 с на запрос), и зажатая клавиша сериально гоняла сеть.
    /// TTL 600 с: каталог меняется редко, инвалидация по событиям ради
    /// этого не стоит.
    playlist_cache: std::collections::HashMap<tmus_core::model::PlaylistId, (Vec<Track>, std::time::Instant)>,
    /// Отложенная загрузка: (плейлист, момент последнего шага курсора).
    /// Ставится при промахе кэша, исполняется в event_loop, когда курсор
    /// стоит 400 мс, — дебаунс сетевых запросов.
    pending_load: Option<(tmus_core::model::PlaylistId, std::time::Instant)>,
    /// Что показать в строке состояния при отсутствии живых данных:
    /// «демон не отвечает» вместо падения при обрыве.
    notice: Option<String>,
    /// Локальные оценки: маркеры и фильтрация списков не должны
    /// запрашивать демон на каждый кадр.
    ratings: HashMap<TrackId, Rating>,
    /// ctrl+d: false — дизлайкнутые скрыты из списков, true — видны.
    show_disliked: bool,
    /// Индексы видимых треков плейлиста после фильтрации дизлайков:
    /// курсор списка ходит по отфильтрованному множеству, поэтому
    /// воспроизведение и оценка обязаны маппить индекс через него.
    /// Пересчитывается в `draw`.
    track_view: Vec<usize>,
    /// То же для результатов поиска (плейлисты и артисты не фильтруются).
    search_view: Vec<usize>,
    /// Открытый пикер плейлистов (`a`); None — закрыт.
    picker: Option<PlaylistPicker>,
    /// Бейджи провайдеров: id -> глиф+цвет. Обновляются из Cmd::Providers.
    badges: HashMap<String, Badge>,
    /// id провайдера -> человекочитаемое имя, для заголовков панелей.
    provider_names: HashMap<String, String>,
    /// id -> последний AuthStatus: ctrl+r собирает из него notice.
    auths: HashMap<String, AuthStatus>,
}

pub async fn run(paths: &Paths) -> Result<()> {
    let mut client = Client::connect_or_spawn(paths).await?;
    let mut events = Client::subscribe(paths).await?;

    let mut state = PlayerState::default();
    let mut playlists = Vec::new();
    let mut source = CatalogSource::default();
    // Стартовый снимок: подписка сообщает только об изменениях, без
    // первого `state` интерфейс был бы пуст до первого действия.
    if let Ok(Payload::State(s)) = client.call(Cmd::State).await {
        state = s;
    }
    if let Ok(Payload::Catalog(s)) = client.call(Cmd::GetCatalogSource).await {
        source = s;
    }
    if let Ok(Payload::Playlists(ps)) =
        client.call(Cmd::Library { provider: source.provider.clone() }).await
    {
        playlists = ps;
    }
    // Стартовый снимок оценок: далее карта живёт на событиях
    // RatingChanged, полные списки больше не запрашиваются.
    let mut ratings = HashMap::new();
    if let Ok(Payload::Ratings(rs)) = client.call(Cmd::Ratings).await {
        ratings = rs.into_iter().collect();
    }
    let mut app = App {
        client,
        paths: paths.clone(),
        state,
        source,
        playlists,
        playlist_tracks: Vec::new(),
        playlist_cache: std::collections::HashMap::new(),
        pending_load: None,
        search_results: Vec::new(),
        search_input: String::new(),
        search_mode: false,
        nav: Nav::new(),
        notice: None,
        ratings,
        show_disliked: false,
        track_view: Vec::new(),
        search_view: Vec::new(),
        picker: None,
        badges: HashMap::new(),
        provider_names: HashMap::new(),
        auths: HashMap::new(),
    };
    // Стартовый снимок бейджей: сразу после снимка оценок, до первого
    // кадра — иначе в очереди/поиске мелькнет текстовый "[provider]".
    refresh_provider_badges(&mut app).await;

    let mut terminal = enter_terminal()?;
    let result = event_loop(&mut app, &mut events, &mut terminal).await;
    restore_terminal()?;
    result
}

/// Перезапросить Cmd::Providers и обновить бейджи/имена. Вызывается на
/// старте, на AuthChanged и после ctrl+r: сессии меняются вне цикла
/// событий, тянуть их инкрементально нечем — карта провайдеров мала.
async fn refresh_provider_badges(app: &mut App) {
    if let Ok(Payload::Providers(list)) = app.client.call(Cmd::Providers).await {
        app.badges = list
            .iter()
            .map(|p| {
                (
                    p.id.clone(),
                    Badge { glyph: p.glyph.clone(), color: hex_color(&p.color).unwrap_or(Color::Reset) },
                )
            })
            .collect();
        app.provider_names =
            list.iter().map(|p| (p.id.clone(), p.name.clone())).collect();
        app.auths = list.iter().map(|p| (p.id.clone(), p.auth.clone())).collect();
    }
}

fn enter_terminal() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    // Хук паники обязан вернуть терминал: без него паника оставляет
    // человека с raw mode и alternate screen в обычном шелле.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));
    terminal::enable_raw_mode()?;
    execute!(stdout(), terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    Ok(Terminal::new(backend)?)
}

fn restore_terminal() -> Result<()> {
    terminal::disable_raw_mode()?;
    execute!(stdout(), terminal::LeaveAlternateScreen)?;
    Ok(())
}

/// Цикл событий: состояние приходит из потока подписки, терминальный
/// ввод опрашивается с таймаутом. `crossterm::event::EventStream`
/// требует нестабильной фичи `event-stream`, которой в зависимостях
/// нет, поэтому опрос — доступный способ совместить оба источника.
async fn event_loop(
    app: &mut App,
    events: &mut mpsc::Receiver<Event>,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<()> {
    // Рисуем только по изменению: poll(100ms) тикает 10 раз в секунду,
    // и безусловный draw давал ~10 полных кадров в секунду впустую.
    let mut dirty = true;
    loop {
        if dirty {
            // Демон может перезапуститься между командами: обрыв здесь не
            // фатален, следующий удачный вызов продолжит работу.
            if let Err(e) = terminal.draw(|f| draw(f, app)) {
                return Err(e.into());
            }
            dirty = false;
        }

        // Сначала выгребаем то, что уже копилось из подписки.
        loop {
            match events.try_recv() {
                Ok(event) => {
                    if apply_event(app, event).await {
                        dirty = true;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    app.notice = Some("поток событий потерян; выход".to_owned());
                    return Ok(());
                }
            }
        }

        // Дебаунс загрузки библиотеки: сначала ждём, не двинется ли
        // курсор дальше, — только дозревший запрос уходит в сеть.
        if flush_pending_load(app).await {
            dirty = true;
        }

        if event::poll(Duration::from_millis(100))? {
            // Читаем ровно один раз: второй `event::read()` в ветке else
            // блокировал бы цикл до следующего события терминала.
            match event::read()? {
                TermEvent::Key(key) => {
                    // Зажатие клавиши в некоторых терминалах даёт повторные
                    // Press + Release; реагируем только на Press.
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    dirty = true;
                    if handle_key(app, key.code, key.modifiers).await? {
                        return Ok(());
                    }
                }
                // После ресайза буфер терминала битый; без перерисовки
                // интерфейс остался бы искажённым до следующего события.
                TermEvent::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
}

async fn apply_event(app: &mut App, event: Event) -> bool {
    // Возврат — «видимый стейт изменился, кадр надо перерисовать»:
    // цикл рисует только по изменению, поэтому игнорируемые события
    // обязаны отличаться от меняющих стейт.
    match event {
        Event::StateChanged { state } => {
            app.state = state;
            true
        }
        Event::Position { position, duration } => {
            app.state.position = Some(position);
            if duration.is_some() {
                app.state.duration = duration;
            }
            true
        }
        // 339 КиБ JSON на очереди из 1000 треков ради двух чисел, которые
        // уже лежат в самом событии, — полный Cmd::Queue не запрашиваем.
        Event::QueueChanged { len, index } => {
            app.state.queue_len = len;
            app.state.queue_index = index;
            true
        }
        Event::RatingChanged { track, rating } => {
            // `None` удаляет запись: «нет оценки» и «none» — одно
            // состояние, мёртвые ключи в карте не нужны.
            if rating == Rating::None {
                app.ratings.remove(&track);
            } else {
                app.ratings.insert(track, rating);
            }
            true
        }
        Event::PlaylistsChanged => {
            // Событие не несёт дельту, а библиотека плейлистов невелика:
            // перечитываем список целиком, как это делает CLI-путь
            // library/list.
            if let Ok(Payload::Playlists(ps)) =
                app.client.call(Cmd::Library { provider: app.source.provider.clone() }).await
            {
                app.playlists = ps;
            }
            true
        }
        Event::TrackChanged { .. } | Event::CacheProgress { .. } => false,
        Event::AuthChanged { .. } => {
            // Сессия ожила или протухла — бейджи и имена могли измениться;
            // true: перерисовать (кадр по изменению, false бы оставил
            // старые глифы до следующего события).
            refresh_provider_badges(app).await;
            true
        }
    }
}

/// `true` — выход из TUI. Ошибки команд не роняют интерфейс: показываем
/// их в строке состояния и продолжаем.
async fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<bool> {
    if app.search_mode {
        return handle_search_key(app, code).await.map(|_| false);
    }
    if app.picker.is_some() {
        return handle_picker_key(app, code).await.map(|_| false);
    }
    match code {
        KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(true),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        KeyCode::Tab => app.nav.tab(),
        KeyCode::Char('l') | KeyCode::Right => {
            if app.nav.enter_playlist(app.playlists.len()) {
                load_selected_playlist(app).await;
            }
        }
        KeyCode::Char('h') | KeyCode::Left => app.nav.leave_playlist(),
        KeyCode::Char('j') | KeyCode::Down => {
            let (p, q, s, t) = (
                app.playlists.len(),
                app.state.queue_len,
                app.search_results.len(),
                app.track_view.len(),
            );
            app.nav.move_active(1, p, q, s, t);
            if app.nav.focus == Focus::Panel && app.nav.panel == Panel::Library {
                load_selected_playlist(app).await;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let (p, q, s, t) = (
                app.playlists.len(),
                app.state.queue_len,
                app.search_results.len(),
                app.track_view.len(),
            );
            app.nav.move_active(-1, p, q, s, t);
            if app.nav.focus == Focus::Panel && app.nav.panel == Panel::Library {
                load_selected_playlist(app).await;
            }
        }
        KeyCode::Enter => play_selected(app).await,
        KeyCode::Char(' ') => {
            if app.nav.focus == Focus::Tracks {
                if let Some(cmd) = track_cmd(app) {
                    call_quiet(app, cmd).await;
                }
            } else {
                call_quiet(app, Cmd::Toggle).await;
            }
        }
        KeyCode::Char('n') => call_quiet(app, Cmd::Next).await,
        KeyCode::Char('p') => call_quiet(app, Cmd::Prev).await,
        KeyCode::Char('+') | KeyCode::Char('=') => {
            call_quiet(app, Cmd::SetVolume { volume: (app.state.volume + 5.0).clamp(0.0, 100.0) }).await;
        }
        KeyCode::Char('-') => {
            call_quiet(app, Cmd::SetVolume { volume: (app.state.volume - 5.0).clamp(0.0, 100.0) }).await;
        }
        KeyCode::Char('s') => call_quiet(app, Cmd::SetShuffle { shuffle: !app.state.shuffle }).await,
        // ctrl+r: перечитать сессии всех провайдеров и обновить бейджи;
        // notice — только если что-то не в порядке. Раньше голого `r` —
        // иначе `r` с CONTROL попадает в цикл повтора.
        KeyCode::Char('r') if modifiers.contains(KeyModifiers::CONTROL) => {
            call_quiet(app, Cmd::RefreshAuth { provider: None }).await;
            refresh_provider_badges(app).await;
            app.notice = auth_notices(app);
        }
        KeyCode::Char('r') => {
            let next = match app.state.loop_mode {
                LoopMode::None => LoopMode::Track,
                LoopMode::Track => LoopMode::Queue,
                LoopMode::Queue => LoopMode::None,
            };
            call_quiet(app, Cmd::SetLoop { mode: next }).await;
        }
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.show_disliked = !app.show_disliked;
        }
        // c: источник каталога None (все) -> провайдеры по алфавиту ->
        // снова None. Библиотека перечитывается сразу, поиск — той же
        // логикой, что Enter в строке поиска, если результаты есть.
        KeyCode::Char('c') => {
            let mut keys: Vec<String> = app.badges.keys().cloned().collect();
            keys.sort();
            let next = match &app.source.provider {
                None => keys.first().cloned(),
                Some(cur) => {
                    let idx = keys.iter().position(|k| k == cur).map(|i| i + 1).unwrap_or(0);
                    keys.get(idx).cloned()
                }
            };
            app.source.provider = next;
            call_quiet(app, Cmd::SetCatalogSource { source: app.source.clone() }).await;
            if let Ok(Payload::Playlists(ps)) =
                app.client.call(Cmd::Library { provider: app.source.provider.clone() }).await
            {
                app.playlists = ps;
            }
            if !app.search_results.is_empty() && !app.search_input.is_empty() {
                if let Ok(Payload::Results(results)) = app
                    .client
                    .call(Cmd::Search {
                        query: app.search_input.clone(),
                        kind: tmus_core::model::SearchKind::Tracks,
                        provider: app.source.provider.clone(),
                    })
                    .await
                {
                    app.search_results = results;
                    app.nav.search_sel.select(Some(0));
                }
            }
        }
        KeyCode::Char('f') => rate_selected(app, Rating::Liked).await,
        KeyCode::Char('d') => rate_selected(app, Rating::Disliked).await,
        // `a` — добавить трек под курсором (плейлист, очередь/now-playing
        // или поиск — те же поверхности, что у f/d) в плейлист.
        KeyCode::Char('a') => {
            if selected_track(app).is_some() {
                app.picker = Some(PlaylistPicker::new());
            }
        }
        // `D` (shift): обычное `d` занято дизлайком, поэтому разрушительное
        // действие сознательно посажено на shift — убрать трек из плейлиста
        // или удалить плейлист целиком.
        KeyCode::Char('D') => playlist_remove_or_delete(app).await,
        KeyCode::Char('/') => {
            app.nav.leave_playlist();
            app.search_mode = true;
            app.search_input.clear();
        }
        _ => {}
    }
    Ok(false)
}

/// Подсказки по нерабочим сессиям для строки состояния после ctrl+r:
/// "<имя>: <hint>"; None, когда у всех всё в порядке.
fn auth_notices(app: &App) -> Option<String> {
    let parts: Vec<String> = app
        .auths
        .iter()
        .filter(|(_, auth)| !auth.is_usable())
        .map(|(id, auth)| {
            let hint = match auth {
                AuthStatus::Missing { hint } | AuthStatus::Expired { hint } => hint.clone(),
                _ => String::new(),
            };
            let name = app.provider_names.get(id).cloned().unwrap_or_else(|| id.clone());
            if hint.is_empty() {
                name
            } else {
                format!("{name}: {hint}")
            }
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("; "))
}

async fn handle_search_key(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Esc => app.search_mode = false,
        KeyCode::Enter => {
            app.search_mode = false;
            let query = app.search_input.clone();
            if let Ok(Payload::Results(results)) = app
                .client
                .call(Cmd::Search {
                    query,
                    kind: tmus_core::model::SearchKind::Tracks,
                    provider: app.source.provider.clone(),
                })
                .await
            {
                app.search_results = results;
                app.nav.panel = Panel::Search;
                app.nav.search_sel.select(Some(0));
            }
        }
        KeyCode::Backspace => {
            app.search_input.pop();
        }
        KeyCode::Char(c) => app.search_input.push(c),
        _ => {}
    }
    Ok(())
}

/// Клавиши открытого пикера плейлистов: пока он открыт, остальные
/// бинды не срабатывают. j/k — навигация, Enter — выбор, Esc — отмена;
/// в режиме ввода имени символы идут в строку, Enter — создать.
async fn handle_picker_key(app: &mut App, code: KeyCode) -> Result<()> {
    if app.picker.as_ref().is_some_and(|p| p.input.is_some()) {
        match code {
            KeyCode::Esc => {
                if let Some(picker) = app.picker.as_mut() {
                    picker.input = None;
                }
            }
            KeyCode::Enter => create_and_add(app).await,
            KeyCode::Backspace => {
                if let Some(input) = app.picker.as_mut().and_then(|p| p.input.as_mut()) {
                    input.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(input) = app.picker.as_mut().and_then(|p| p.input.as_mut()) {
                    input.push(c);
                }
            }
            _ => {}
        }
        return Ok(());
    }

    match code {
        KeyCode::Esc => app.picker = None,
        KeyCode::Enter => {
            let Some(picker) = app.picker.as_ref() else { return Ok(()) };
            if picker.is_new_selected() {
                app.picker.as_mut().expect("проверено выше").begin_input();
                return Ok(());
            }
            let sel = picker.sel;
            // Трек и плейлист читаем до закрытия пикера: selected_track
            // ходит по app, а не по пикеру, так что порядок не важен, но
            // borrow-чеккер требует разнести мутацию и чтение.
            let track = selected_track(app);
            let playlist = app.playlists.get(sel - 1).map(|p| p.id.clone());
            app.picker = None;
            if let (Some(track), Some(playlist)) = (track, playlist) {
                fire(app, Cmd::PlaylistAdd { playlist, track });
            }
        }
        _ => {
            let len = app.playlists.len();
            let delta = match code {
                KeyCode::Char('j') | KeyCode::Down => 1,
                KeyCode::Char('k') | KeyCode::Up => -1,
                _ => return Ok(()),
            };
            if let Some(picker) = app.picker.as_mut() {
                picker.move_cursor(delta, len);
            }
        }
    }
    Ok(())
}

/// Создать плейлист из введённого имени и сразу добавить в него трек
/// под курсором: пользователь просил «добавить», создание — лишь
/// средство. Ответ `PlaylistCreated` приходит синхронно, поэтому
/// добавление уходит сразу после него.
async fn create_and_add(app: &mut App) {
    let title = match app.picker.as_ref().and_then(|p| p.confirmed_title()) {
        Some(title) => title,
        None => return,
    };
    // Плейлист создаём у того клиента, откуда трек: в мультисессионном
    // режиме «первый подключённый» был бы сюрпризом. Пикер открыт, только
    // когда есть выбранный трек, а пока он открыт, курсор списков не ходит
    // (дж/к перехватывает пикер), так что цель та же, что при открытии.
    let provider = selected_track(app).map(|t| t.provider.as_str().to_owned());
    match app.client.call(Cmd::PlaylistCreate { title, provider }).await {
        Ok(Payload::PlaylistCreated { playlist }) => {
            app.picker = None;
            app.notice = None;
            if let Some(track) = selected_track(app) {
                fire(app, Cmd::PlaylistAdd { playlist, track });
            }
        }
        Ok(_) => app.notice = Some("неожиданный ответ на PlaylistCreate".to_owned()),
        Err(e) => app.notice = Some(e.to_string()),
    }
}

/// `D`: в списке треков плейлиста — убрать трек под курсором из
/// плейлиста; в списке плейлистов библиотеки — удалить плейлист.
async fn playlist_remove_or_delete(app: &mut App) {
    if app.nav.focus == Focus::Tracks {
        let Some(idx) = app.nav.open_playlist else { return };
        let Some(playlist) = app.playlists.get(idx) else { return };
        let id = playlist.id.clone();
        let Some(track) = selected_track(app) else { return };
        // Кэш и открытый список правим локально: PlaylistsChanged треки
        // не несёт, иначе убранный трек висел бы до протухания TTL.
        app.playlist_tracks.retain(|t| t.id != track);
        if let Some((tracks, _)) = app.playlist_cache.get_mut(&id) {
            tracks.retain(|t| t.id != track);
        }
        call_quiet(app, Cmd::PlaylistRemove { playlist: id, track }).await;
    } else if app.nav.focus == Focus::Panel && app.nav.panel == Panel::Library {
        let Some(idx) = app.nav.library_sel.selected() else { return };
        let Some(playlist) = app.playlists.get(idx) else { return };
        let id = playlist.id.clone();
        app.playlist_cache.remove(&id);
        call_quiet(app, Cmd::PlaylistDelete { playlist: id }).await;
    }
}

/// TTL кэша треков плейлиста. Каталог провайдера меняется редко;
/// протокол не сообщает об изменениях, поэтому живём устареванием.
const PLAYLIST_CACHE_TTL: Duration = Duration::from_secs(600);

/// Пауза дебаунса: загрузка стартует, только когда курсор стоит так
/// долго. Замерено на InnerTube: ответ занимает 1–2 с, так что 400 мс
/// надёжно покрывают темп ручного и зажатого листания.
const LOAD_DEBOUNCE: Duration = Duration::from_millis(400);

/// Подготовить загрузку треков подсвеченного плейлиста. Свежий кэш
/// применяется мгновенно; при промахе сеть НЕ дёргаем — ставим
/// `pending_load`, его исполнит event_loop после паузы. Инвариант
/// дебаунса: каждое движение курсора перезаписывает метку времени, при
/// непрерывном листании возраст не дорастает до `LOAD_DEBOUNCE` и
/// запросы не уходят вовсе; один запрос — когда курсор замер.
async fn load_selected_playlist(app: &mut App) {
    let Some(idx) = app.nav.library_sel.selected() else { return };
    let Some(playlist) = app.playlists.get(idx) else { return };
    let id = playlist.id.clone();
    if let Some((tracks, loaded_at)) = app.playlist_cache.get(&id) {
        if loaded_at.elapsed() < PLAYLIST_CACHE_TTL {
            app.playlist_tracks = tracks.clone();
            app.notice = None;
            return;
        }
    }
    app.playlist_tracks.clear();
    app.notice = Some("загрузка…".to_owned());
    app.pending_load = Some((id, std::time::Instant::now()));
}

/// Исполнить дозревший `pending_load`: один сетевой запрос вместо
/// запроса на каждый шаг курсора. Ошибки — в строку состояния.
async fn flush_pending_load(app: &mut App) -> bool {
    let due = match app.pending_load.as_ref() {
        Some((_, started)) if started.elapsed() >= LOAD_DEBOUNCE => {
            app.pending_load.take().map(|(id, _)| id)
        }
        _ => None,
    };
    let Some(id) = due else { return false };
    match app.client.call(Cmd::LibraryTracks { playlist: id.clone() }).await {
        Ok(Payload::Tracks(tracks)) => {
            app.playlist_cache.insert(id, (tracks.clone(), std::time::Instant::now()));
            app.playlist_tracks = tracks;
            app.notice = None;
        }
        Ok(_) => app.notice = Some("неожиданный ответ на LibraryTracks".to_owned()),
        Err(e) => app.notice = Some(e.to_string()),
    }
    true
}

fn track_cmd(app: &App) -> Option<Cmd> {
    let (idx, start) = app.nav.track_play_sel()?;
    let playlist = app.playlists.get(idx)?;
    // Курсор ходит по отфильтрованному списку: маппим в индекс полного.
    let start = *app.track_view.get(start)?;
    Some(Cmd::PlayPlaylist { playlist: playlist.id.clone(), start: Some(start) })
}

/// Маркер рейтинга перед названием трека в списках: нет оценки —
/// ничего, лайк — сердце, дизлайк — крест.
fn rating_marker(rating: Option<&Rating>) -> &'static str {
    match rating {
        Some(Rating::Liked) => "♥ ",
        Some(Rating::Disliked) => "× ",
        _ => "",
    }
}

/// Видимость трека в списках: дизлайкнутые по умолчанию спрятаны,
/// ctrl+d возвращает их. Чистая функция ради тестов.
fn track_visible(rating: Option<&Rating>, show_disliked: bool) -> bool {
    show_disliked || rating != Some(&Rating::Disliked)
}

/// Индексы видимых треков после фильтрации: курсор и рендер ходят по
/// ним, полный вектор остаётся источником данных.
fn visible_track_indices(
    tracks: &[Track],
    ratings: &HashMap<TrackId, Rating>,
    show_disliked: bool,
) -> Vec<usize> {
    tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| track_visible(ratings.get(&t.id), show_disliked))
        .map(|(i, _)| i)
        .collect()
}

/// То же для выдачи поиска: фильтруются только треки-результаты,
/// плейлисты и артисты остаются всегда.
fn visible_search_indices(
    results: &[SearchResult],
    ratings: &HashMap<TrackId, Rating>,
    show_disliked: bool,
) -> Vec<usize> {
    results
        .iter()
        .enumerate()
        .filter(|(_, r)| match r {
            SearchResult::Track(t) => track_visible(ratings.get(&t.id), show_disliked),
            _ => true,
        })
        .map(|(i, _)| i)
        .collect()
}

/// Повторное f/d на уже проставленной оценке снимает её.
fn rate_toggle(current: Option<Rating>, want: Rating) -> Rating {
    if current == Some(want) {
        Rating::None
    } else {
        want
    }
}

/// Трек-цель f/d/a/пикера: сначала играющий (`state.track`, зеркало
/// фика панели — оценка и «добавить в плейлист» относятся к тому, что
/// звучит, а не к тому, где курсор); когда ничего не играет — трек под
/// курсором активного списка, тот же, которого касается
/// Space/Return: плейлист (через фильтр), очередь (единственный
/// видимый трек — текущий) или результат поиска. Курсор вне трека
/// (плейлисты библиотеки, плейлист/артист в поиске) — ничего.
fn selected_track(app: &App) -> Option<TrackId> {
    if let Some(t) = app.state.track.as_ref() {
        return Some(t.id.clone());
    }
    if app.nav.focus == Focus::Tracks {
        let idx = app.nav.playlist_sel.selected()?;
        let idx = *app.track_view.get(idx)?;
        return app.playlist_tracks.get(idx).map(|t| t.id.clone());
    }
    match app.nav.panel {
        Panel::Queue => {
            // Играющий уже проверен выше; в тишине в очереди цели нет.
            None
        }
        Panel::Search => {
            let idx = app.nav.search_sel.selected()?;
            match app.search_results.get(idx) {
                Some(SearchResult::Track(t)) => Some(t.id.clone()),
                _ => None,
            }
        }
        Panel::Library => None,
    }
}

/// Оценить трек под курсором: повторное f/d снимает оценку. Ответ
/// демона не нужен — придёт событие RatingChanged.
async fn rate_selected(app: &mut App, want: Rating) {
    let Some(track) = selected_track(app) else { return };
    let rating = rate_toggle(app.ratings.get(&track).copied(), want);
    call_quiet(app, Cmd::Rate { track, rating }).await;
}

/// Сколько треков активного списка спрятано фильтром дизлайков: для
/// счётчика в строке состояния.
fn hidden_count(app: &App) -> usize {
    if app.show_disliked {
        return 0;
    }
    let disliked = |t: &Track| app.ratings.get(&t.id) == Some(&Rating::Disliked);
    if app.nav.focus == Focus::Tracks {
        app.playlist_tracks.iter().filter(|t| disliked(t)).count()
    } else if app.nav.panel == Panel::Search {
        app.search_results
            .iter()
            .filter(|r| matches!(r, SearchResult::Track(t) if disliked(t)))
            .count()
    } else {
        0
    }
}

async fn play_selected(app: &mut App) {
    if app.nav.focus == Focus::Tracks {
        if let Some(cmd) = track_cmd(app) {
            fire(app, cmd);
        }
        return;
    }
    match app.nav.panel {
        Panel::Library => {
            let Some(idx) = app.nav.library_sel.selected() else { return };
            let Some(playlist) = app.playlists.get(idx) else { return };
            fire(app, Cmd::PlayPlaylist { playlist: playlist.id.clone(), start: Some(0) });
        }
        Panel::Queue => {
            if let Some(idx) = app.nav.queue_sel.selected() {
                fire(app, Cmd::QueueGoto { index: idx });
            }
        }
        Panel::Search => {
            let Some(sel) = app.nav.search_sel.selected() else { return };
            let Some(&sel) = app.search_view.get(sel) else { return };
            let Some(SearchResult::Track(track)) = app.search_results.get(sel) else { return };
            let selected = track.id.clone();
            // Контекстный запуск: Enter играет весь видимый список
            // результатов (тот же search_view, по которому ходит курсор
            // и рендер), стартуя с выбранного, — плейлист целиком
            // заменяется, а не дописывается (решение хозяина).
            let tracks: Vec<TrackId> = app
                .search_view
                .iter()
                .filter_map(|&i| app.search_results.get(i))
                .filter_map(|r| match r {
                    SearchResult::Track(t) => Some(t.id.clone()),
                    _ => None,
                })
                .collect();
            let start = tracks.iter().position(|id| id == &selected).unwrap_or(0);
            fire(app, Cmd::PlayContext { tracks, start });
        }
    }
}

/// Запустить play-команду, не держа цикл ввода.
///
/// Демон отвечает на `PlayTrack`/`PlayPlaylist`/`QueueGoto` только после
/// резолва и загрузки (до ~4–6 с на незакэшированном треке), и
/// инлайновое ожидание замораживало весь TUI: ни ввода, ни кадров, ни
/// событий — при том, что событие `TrackChanged` теперь приходит сразу
/// (`pending`-трек в демоне). Отдельное одноразовое соединение:
/// основное нужно для сериализации команд в одном цикле, а тут
/// параллельность уместна. Ошибка уходит в журнал — состояние всё
/// равно приедет событием.
fn fire(app: &App, cmd: Cmd) {
    let paths = app.paths.clone();
    tokio::spawn(async move {
        if let Ok(mut client) = Client::connect(&paths).await {
            // Ошибку не показываем и не логируем: TUI живёт в alternate
            // screen, stderr испортил бы кадр, а tracing в зависимостях
            // нет. Неудавшийся запуск виден событием StateChanged и
            // журналом демона.
            let _ = client.call(cmd).await;
        }
    });
}

/// Выполнить команду, не роняя TUI при обрыве: сообщение об ошибке
/// попадает в строку состояния, попытки продолжаются.
async fn call_quiet(app: &mut App, cmd: Cmd) {
    if let Err(e) = app.client.call(cmd).await {
        app.notice = Some(e.to_string());
    } else {
        app.notice = None;
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    // Пересчёт фильтров в начале кадра: и рендер, и обработчики клавиш
    // (через track_cmd/play_selected/rate_selected) ходят по ним.
    app.track_view = visible_track_indices(&app.playlist_tracks, &app.ratings, app.show_disliked);
    app.search_view = visible_search_indices(&app.search_results, &app.ratings, app.show_disliked);

    let [main, status] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .areas(f.area());

    let [left, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .areas(main);

    // Левая колонка: текущая панель. В режиме поиска заголовок
    // показывает ввод. Библиотека и поиск показывают источник каталога:
    // клавиша `c` меняет его, без подписи непонятно, откуда данные.
    let (title, items, sel_panel) = match app.nav.panel {
        Panel::Library => (
            format!("Библиотека [Tab] [c] [l] · src: {}", source_label(app)),
            app.playlists
                .iter()
                .map(|p| {
                    let count = p.track_count.map(|c| format!(" ({c})")).unwrap_or_default();
                    let text = format!("{}{}", p.title, count);
                    match badge_span(&app.badges, p.id.provider.as_str()) {
                        Some(b) => ListItem::new(Line::from(vec![b, Span::raw(text)])),
                        None => ListItem::new(text),
                    }
                })
                .collect(),
            &mut app.nav.library_sel,
        ),
        Panel::Queue => (
            "Очередь [Tab]".to_owned(),
            queue_items(app),
            &mut app.nav.queue_sel,
        ),
        Panel::Search => (
            format!("Результаты: {} · src: {}", app.search_input, source_label(app)),
            search_items(app),
            &mut app.nav.search_sel,
        ),
    };

    let left_list = List::new(items)
        .block(Block::new().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(left_list, left, sel_panel);

    // Правая колонка: в режиме плейлиста — его треки с собственным
    // курсором, иначе очередь. Источник трека виден в списке: очередь
    // смешанная, без колонки провайдера она нечитаема.
    if app.nav.focus == Focus::Tracks {
        let title = match app.nav.open_playlist.and_then(|i| app.playlists.get(i)) {
            Some(p) => format!("Плейлист: {} [h D]", p.title),
            None => "Плейлист [h]".to_owned(),
        };
        let current = app.state.track.as_ref().map(|t| t.id.clone());
        let items: Vec<ListItem> = app
            .track_view
            .iter()
            .filter_map(|&i| app.playlist_tracks.get(i))
            .map(|t| track_line(t, current.as_ref(), app.ratings.get(&t.id), &app.badges))
            .collect();
        let right_list = List::new(items)
            .block(Block::new().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(right_list, right, &mut app.nav.playlist_sel);
    } else {
        let right_list = List::new(queue_items(app))
            .block(Block::new().borders(Borders::ALL).title("Очередь [Tab]"));
        f.render_widget(right_list, right);
    }

    // Пикер плейлистов — модальный оверлей поверх основной области:
    // рисуется последним, чтобы накрыть обе колонки.
    if app.picker.is_some() {
        draw_picker(f, app, main);
    }

    draw_status(f, app, status);
}

/// Модальный пикер плейлистов: минимальные Clear+List по центру
/// основной области — полноценных модалок в TUI нет. В режиме ввода
/// имени список скрыт, ввод отражается в заголовке.
fn draw_picker(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let Some(picker) = &app.picker else { return };
    let rows = app.playlists.len() as u16 + 3;
    let popup = centered_rect(60, rows, area);
    let (title, items) = match &picker.input {
        Some(input) => (format!("Новый плейлист: {input}_"), Vec::new()),
        None => {
            let mut items: Vec<ListItem> = Vec::with_capacity(app.playlists.len() + 1);
            for i in 0..=app.playlists.len() {
                let text = match i {
                    0 => "+ New playlist…".to_owned(),
                    _ => app.playlists[i - 1].title.clone(),
                };
                let item = if i == picker.sel {
                    ListItem::new(text).style(Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    ListItem::new(text)
                };
                items.push(item);
            }
            ("Добавить в плейлист [j/k Enter Esc]".to_owned(), items)
        }
    };
    // Clear затирает под собой списки колонок, иначе сквозь «модалку»
    // читается нижний текст.
    f.render_widget(Clear, popup);
    let list = List::new(items).block(Block::new().borders(Borders::ALL).title(title));
    f.render_widget(list, popup);
}

/// Прямоугольник `percent_x`% ширины и `height` строк по центру `area`.
fn centered_rect(percent_x: u16, height: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let height = height.min(area.height);
    let width = area.width.saturating_mul(percent_x) / 100;
    ratatui::layout::Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Строка трека в списках; `current` — играющий сейчас `TrackId`,
/// `rating` — локальная оценка трека. Играющий трек получает тёплый
/// фон и метку `▶`: в плейлисте на сотни строк взгляд ищет «что же
/// играет» чаще, чем позицию курсора. Бейдж глифа заменяет текстовый
/// "[{provider}]": очередь и поиск смешанные, цветной глиф читается
/// взглядом, а не чтением.
fn track_line(
    track: &Track,
    current: Option<&TrackId>,
    rating: Option<&Rating>,
    badges: &HashMap<String, Badge>,
) -> ListItem<'static> {
    let playing = current == Some(&track.id);
    let marker = rating_marker(rating);
    let spans: Vec<Span> = match badge_span(badges, track.id.provider.as_str()) {
        Some(badge) => {
            let head = if playing {
                Span::raw(format!("{marker}▶ "))
            } else {
                Span::raw(marker.to_owned())
            };
            vec![head, badge, Span::raw(format!("{} — {}", track.artist_line(), track.title))]
        }
        None => {
            let text = if playing {
                format!("{marker}▶ [{}] {} — {}", track.id.provider, track.artist_line(), track.title)
            } else {
                format!("{marker}[{}] {} — {}", track.id.provider, track.artist_line(), track.title)
            };
            vec![Span::raw(text)]
        }
    };
    let item = ListItem::new(Line::from(spans));
    if playing {
        item.style(
            // ANSI 0 (normal.black) и ANSI 11 (bright.yellow) темятся терминалом:
            // noctalia генерит тему alacritty (normal.black = тёплый подъём над
            // фоном), поэтому строка следует теме без хардкода. Курсор остаётся
            // REVERSED — играющая обязана отличаться от курсорной.
            Style::new()
                .bg(Color::Black)
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        item
    }
}

fn queue_items(app: &App) -> Vec<ListItem<'static>> {
    // Полная очередь у клиента не хранится (приходит только длина),
    // показываем текущий трек и подсказку. Играющий не прячется фильтром
    // дизлайков: скрыть единственную строку очереди — спрятать саму
    // очередь, а d на нём всё равно доступен.
    match &app.state.track {
        Some(track) => {
            vec![track_line(track, Some(&track.id), app.ratings.get(&track.id), &app.badges)]
        }
        None => vec![ListItem::new("очередь пуста")],
    }
}

fn search_items(app: &App) -> Vec<ListItem<'static>> {
    let current = app.state.track.as_ref().map(|t| t.id.clone());
    app.search_view
        .iter()
        .filter_map(|&i| app.search_results.get(i))
        .map(|r| match r {
            SearchResult::Track(t) => track_line(t, current.as_ref(), app.ratings.get(&t.id), &app.badges),
            SearchResult::Playlist(p) => {
                let badge = badge_span(&app.badges, p.id.provider.as_str());
                let text = format!("{}: {}", p.id, p.title);
                match badge {
                    Some(b) => ListItem::new(Line::from(vec![b, Span::raw(text)])),
                    None => ListItem::new(text),
                }
            }
            SearchResult::Artist { provider, id, name } => {
                let badge = badge_span(&app.badges, provider.as_str());
                let text = format!("[{provider}:{id}] {name}");
                match badge {
                    Some(b) => ListItem::new(Line::from(vec![b, Span::raw(text)])),
                    None => ListItem::new(text),
                }
            }
        })
        .collect()
}

fn draw_status(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let width = area.width.saturating_sub(2) as usize;
    let mut spans: Vec<Span> = Vec::new();

    if let Some(notice) = &app.notice {
        spans.push(Span::styled(format!("! {notice} "), Style::new().red()));
    }

    // Счётчик спрятанных фильтром дизлайков активного списка.
    let hidden = hidden_count(app);
    if hidden > 0 {
        spans.push(Span::styled(format!("hidden:{hidden} "), Style::new().fg(Color::DarkGray)));
    }

    if let Some(track) = &app.state.track {
        let pos = app.state.position.map(|d| d.as_secs());
        let dur = app.state.duration.map(|d| d.as_secs());
        let line = trim_fit(
            &format!(
                "{} {} {} {} {}  vol:{} {}{} [{}] f/d rate a add D del ctrl+d hidden q=выход",
                status_icon(app.state.status),
                track.title,
                track.artist_line(),
                fmt_time(pos),
                fmt_time(dur),
                app.state.volume.round() as u64,
                if app.state.shuffle { "shuf " } else { "" },
                match app.state.loop_mode {
                    LoopMode::None => "",
                    LoopMode::Track => "rep1 ",
                    LoopMode::Queue => "repq ",
                },
                if app.state.offline { "offline" } else { "online" },
            ),
            width.saturating_sub(24),
        );
        spans.push(Span::raw(line));
        spans.push(Span::styled(progress_bar(pos, dur, 24), Style::new().cyan()));
    } else {
        spans.push(Span::raw("нет трека; / — поиск, Tab — панель, q — выход"));
    }

    // Обрезка по символам, не по байтам: названия бывают на кириллице и
    // CJK, обрезка по байтам разрубила бы utf8.
    f.render_widget(Paragraph::new(Line::from(spans)).block(Block::new().borders(Borders::ALL)), area);
}

/// ASCII-глифы:▶ и ⏸ входят в заявленный набор (Unicode-блоки/символы),
/// Nerd Font не нужен.
fn status_icon(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Playing => "\u{25b6}",  // ▶
        PlaybackStatus::Paused => "\u{23f8}",   // ⏸
        PlaybackStatus::Stopped => "\u{25a0}", // ■
    }
}

/// Полоса прогресса из блоков `█▓░`; ширина — в символах, на границах
/// пустая строка.
fn progress_bar(position: Option<u64>, duration: Option<u64>, width: usize) -> String {
    let (Some(position), Some(duration)) = (position, duration) else {
        return String::new();
    };
    if duration == 0 || width == 0 {
        return String::new();
    }
    let filled = ((position.min(duration) * width as u64) / duration) as usize;
    let mut bar = String::with_capacity(width);
    for i in 0..width {
        bar.push(if i < filled { '\u{2588}' } else { '\u{2591}' });
    }
    bar
}

/// Обрезка по символам с многоточием, если строка не влезла.
fn trim_fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_owned();
    }
    let mut cut: String = s.chars().take(width.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmus_core::model::{PlaylistId, ProviderId};

    fn cyrillic(s: &str, width: usize) -> String {
        trim_fit(s, width)
    }

    /// Обрезка по символам: не разрубает кириллицу и влезает в ширину.
    #[test]
    fn trim_fits_width_without_splitting_utf8() {
        let s = "Привет, мир, это длинное название трека на кириллице";
        let got = cyrillic(s, 10);
        assert_eq!(got.chars().count(), 10);
        // Побайтовая обрезка здесь даёт панику/мусор: проверяем, что
        // результат — валидные символы с начала строки + многоточие.
        assert_eq!(got, "Привет, м\u{2026}");
    }

    #[test]
    fn trim_keeps_short_strings_and_marks_cut() {
        assert_eq!(trim_fit("ok", 10), "ok");
        assert!(trim_fit("abcdef", 3).contains('\u{2026}'));
        assert_eq!(trim_fit("abcdef", 3).chars().count(), 3);
    }

    fn pl(id: &str) -> Playlist {
        Playlist {
            id: PlaylistId { provider: ProviderId::YTMUSIC, id: id.into() },
            title: id.into(),
            subtitle: None,
            art_url: None,
            track_count: None,
        }
    }

    fn nav_at(sel: Option<usize>) -> Nav {
        let mut nav = Nav::new();
        nav.library_sel.select(sel);
        nav
    }

    #[test]
    fn enter_opens_playlist_tracks_with_cursor_at_top() {
        let mut nav = nav_at(Some(1));
        assert!(nav.enter_playlist(3));
        assert_eq!(nav.focus, Focus::Tracks);
        assert_eq!(nav.open_playlist, Some(1));
        assert_eq!(nav.playlist_sel.selected(), Some(0));
        assert_eq!(nav.library_sel.selected(), Some(1));
        assert!(!nav_at(None).enter_playlist(3));
        assert!(!nav_at(Some(5)).enter_playlist(3));
    }

    #[test]
    fn back_restores_panel_focus_and_library_cursor() {
        let mut nav = nav_at(Some(2));
        assert!(nav.enter_playlist(3));
        nav.leave_playlist();
        assert_eq!(nav.focus, Focus::Panel);
        assert_eq!(nav.library_sel.selected(), Some(2));
        assert_eq!(nav.open_playlist, None);
        assert_eq!(nav.track_play_sel(), None);
    }

    #[test]
    fn tab_clears_open_playlist() {
        let mut nav = nav_at(Some(0));
        nav.enter_playlist(3);
        nav.tab();
        assert_eq!(nav.focus, Focus::Panel);
        assert_eq!(nav.open_playlist, None);
        assert_eq!(nav.track_play_sel(), None);
    }

    #[test]
    fn tab_rotates_panels_and_leaves_playlist_focus() {
        let mut nav = nav_at(Some(0));
        nav.enter_playlist(3);
        nav.tab();
        assert_eq!(nav.focus, Focus::Panel);
        assert_eq!(nav.panel, Panel::Queue);
        nav.tab();
        assert_eq!(nav.panel, Panel::Search);
        nav.tab();
        assert_eq!(nav.panel, Panel::Library);
    }

    #[test]
    fn movement_targets_active_list_only() {
        let mut nav = nav_at(Some(0));
        nav.move_active(1, 3, 0, 0, 0);
        assert_eq!(nav.library_sel.selected(), Some(1));

        nav.enter_playlist(3);
        nav.move_active(1, 3, 0, 0, 5);
        nav.move_active(1, 3, 0, 0, 5);
        assert_eq!(nav.playlist_sel.selected(), Some(2));
        assert_eq!(nav.library_sel.selected(), Some(1));

        nav.move_active(-1, 3, 0, 0, 5);
        assert_eq!(nav.playlist_sel.selected(), Some(1));
        nav.move_active(-10, 3, 0, 0, 5);
        assert_eq!(nav.playlist_sel.selected(), Some(0));
        nav.move_active(10, 3, 0, 0, 5);
        assert_eq!(nav.playlist_sel.selected(), Some(4));

        nav.leave_playlist();
        nav.move_active(-10, 3, 0, 0, 5);
        assert_eq!(nav.library_sel.selected(), Some(0));
        assert_eq!(nav.playlist_sel.selected(), Some(4));
    }

    #[test]
    fn empty_active_list_does_not_move() {
        let mut nav = Nav::new();
        nav.move_active(1, 0, 4, 2, 0);
        assert_eq!(nav.library_sel.selected(), None);
    }

    #[test]
    fn track_play_cmd_uses_selected_index_and_full_playlist() {
        let playlists = vec![pl("a"), pl("b")];
        let mut nav = nav_at(Some(0));
        assert!(nav.enter_playlist(2));
        nav.move_active(2, 2, 0, 0, 5);
        assert_eq!(nav.track_play_sel(), Some((0, 2)));
        let (idx, start) = nav.track_play_sel().unwrap();
        assert_eq!(playlists[idx].id.id, "a");
        assert_eq!(start, 2);
        nav.leave_playlist();
        assert_eq!(nav.track_play_sel(), None);
    }

    fn track(provider_id: &str) -> Track {
        let (provider, id) = provider_id.split_once(':').expect("provider:id");
        Track {
            id: TrackId::new(ProviderId::from_name(provider).expect("known"), id.to_owned()),
            title: id.into(),
            artists: vec![],
            album: None,
            duration: None,
            art_url: None,
            page_url: None,
        }
    }

    #[test]
    fn marker_reflects_rating_only_when_present() {
        assert_eq!(rating_marker(None), "");
        assert_eq!(rating_marker(Some(&Rating::None)), "");
        assert_eq!(rating_marker(Some(&Rating::Liked)), "♥ ");
        assert_eq!(rating_marker(Some(&Rating::Disliked)), "× ");
    }

    #[test]
    fn disliked_hidden_until_flag_set() {
        assert!(!track_visible(Some(&Rating::Disliked), false));
        assert!(track_visible(Some(&Rating::Disliked), true));
        // Лайк и отсутствие оценки видны всегда.
        assert!(track_visible(Some(&Rating::Liked), false));
        assert!(track_visible(None, false));
    }

    #[test]
    fn visible_indices_filter_only_disliked_tracks() {
        let tracks = vec![track("ytmusic:a"), track("ytmusic:b"), track("ytmusic:c")];
        let mut ratings = HashMap::new();
        ratings.insert(tracks[0].id.clone(), Rating::Liked);
        ratings.insert(tracks[1].id.clone(), Rating::Disliked);

        assert_eq!(visible_track_indices(&tracks, &ratings, false), vec![0, 2]);
        assert_eq!(visible_track_indices(&tracks, &ratings, true), vec![0, 1, 2]);
        assert_eq!(visible_track_indices(&tracks, &HashMap::new(), false), vec![0, 1, 2]);
    }

    #[test]
    fn search_indices_keep_playlists_and_artists() {
        let results = vec![
            SearchResult::Track(track("ytmusic:a")),
            SearchResult::Playlist(Playlist {
                id: PlaylistId { provider: ProviderId::YTMUSIC, id: "pl".into() },
                title: "pl".into(),
                subtitle: None,
                art_url: None,
                track_count: None,
            }),
            SearchResult::Artist { provider: ProviderId::YTMUSIC, id: "ar".into(), name: "ar".into() },
            SearchResult::Track(track("ytmusic:b")),
        ];
        let mut ratings = HashMap::new();
        ratings.insert(TrackId::new(ProviderId::YTMUSIC, "a"), Rating::Disliked);
        ratings.insert(TrackId::new(ProviderId::YTMUSIC, "b"), Rating::Disliked);

        // Дизлайкнутые треки спрятаны, плейлист и артист остались.
        assert_eq!(visible_search_indices(&results, &ratings, false), vec![1, 2]);
        assert_eq!(visible_search_indices(&results, &ratings, true).len(), 4);
    }

    #[test]
    fn repeat_rating_removes_it() {
        assert_eq!(rate_toggle(None, Rating::Liked), Rating::Liked);
        assert_eq!(rate_toggle(Some(Rating::Liked), Rating::Liked), Rating::None);
        assert_eq!(rate_toggle(Some(Rating::Liked), Rating::Disliked), Rating::Disliked);
        assert_eq!(rate_toggle(Some(Rating::Disliked), Rating::Disliked), Rating::None);
    }

    #[test]
    fn picker_cursor_moves_over_new_plus_playlists() {
        let mut picker = PlaylistPicker::new();
        assert!(picker.is_new_selected());

        // 0 — «новый», 1..=3 — плейлисты; границы зажимаются.
        picker.move_cursor(1, 3);
        picker.move_cursor(1, 3);
        picker.move_cursor(1, 3);
        picker.move_cursor(1, 3);
        assert_eq!(picker.sel, 3);
        picker.move_cursor(1, 3);
        assert_eq!(picker.sel, 3);
        picker.move_cursor(-10, 3);
        assert_eq!(picker.sel, 0);
        assert!(picker.is_new_selected());
    }

    #[test]
    fn picker_input_collects_and_trims_name() {
        let mut picker = PlaylistPicker::new();
        picker.begin_input();
        // Пустое имя не подтверждается — режим ввода остаётся открытым.
        assert_eq!(picker.confirmed_title(), None);
        for c in "  Chill mix ".chars() {
            picker.input.as_mut().expect("input mode").push(c);
        }
        assert_eq!(picker.confirmed_title().as_deref(), Some("Chill mix"));
        // Имя не «тратится» чтением: сбрасывает только Esc-ветка.
        assert_eq!(picker.confirmed_title().as_deref(), Some("Chill mix"));
    }

    #[test]
    fn hex_color_parses_rrggbb_and_rejects_bad_input() {
        assert_eq!(hex_color("#ff5500"), Some(Color::Rgb(0xff, 0x55, 0x00)));
        assert_eq!(hex_color("#000000"), Some(Color::Rgb(0, 0, 0)));
        // Грабли: без #, короткая/длинная, не hex — всё None.
        assert_eq!(hex_color("ff5500"), None);
        assert_eq!(hex_color("#ff550"), None);
        assert_eq!(hex_color("#ff55000"), None);
        assert_eq!(hex_color("#zz5500"), None);
    }

    #[test]
    fn progress_bar_is_symbol_based() {
        let bar = progress_bar(Some(0), Some(100), 10);
        assert_eq!(bar, "\u{2591}".repeat(10));
        let half = progress_bar(Some(50), Some(100), 10);
        assert_eq!(half.chars().count(), 10);
        assert_eq!(half.chars().filter(|c| *c == '\u{2588}').count(), 5);
        assert_eq!(progress_bar(Some(1), None, 10), "");
        assert_eq!(progress_bar(Some(1), Some(0), 10), "");
    }
}
