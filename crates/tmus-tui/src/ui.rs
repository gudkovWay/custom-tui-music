//! TUI. Терминальные глифы — только ASCII и Unicode-блоки: Nerd Font /
//! FontAwesome не используются, потому что именно на них сломался
//! готовый клиент youtui, чей README требует особых шрифтов.

use std::io::stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use tmus_core::model::{LoopMode, PlaybackStatus, Playlist, SearchResult, Track};
use tmus_core::protocol::{Cmd, Event, Payload, PlayerState};
use tmus_core::Paths;

use crate::client::Client;
use crate::fmt_time;

/// Панели левой колонки. Tab переключает по кругу.
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Library,
    Queue,
    Search,
}

struct App {
    client: Client,
    state: PlayerState,
    playlists: Vec<Playlist>,
    /// Треки выбранного плейлиста — правая колонка.
    playlist_tracks: Vec<Track>,
    search_results: Vec<SearchResult>,
    search_input: String,
    search_mode: bool,
    panel: Panel,
    library_sel: ListState,
    queue_sel: ListState,
    search_sel: ListState,
    /// Что показать в строке состояния при отсутствии живых данных:
    /// «демон не отвечает» вместо падения при обрыве.
    notice: Option<String>,
}

pub async fn run(paths: &Paths) -> Result<()> {
    let mut client = Client::connect_or_spawn(paths).await?;
    let mut events = Client::subscribe(paths).await?;

    let mut state = PlayerState::default();
    let mut playlists = Vec::new();
    // Стартовый снимок: подписка сообщает только об изменениях, без
    // первого `state` интерфейс был бы пуст до первого действия.
    if let Ok(Payload::State(s)) = client.call(Cmd::State).await {
        state = s;
    }
    if let Ok(Payload::Playlists(ps)) = client.call(Cmd::Library { provider: None }).await {
        playlists = ps;
    }

    let mut app = App {
        client,
        state,
        playlists,
        playlist_tracks: Vec::new(),
        search_results: Vec::new(),
        search_input: String::new(),
        search_mode: false,
        panel: Panel::Library,
        library_sel: ListState::default(),
        queue_sel: ListState::default(),
        search_sel: ListState::default(),
        notice: None,
    };

    let mut terminal = enter_terminal()?;
    let result = event_loop(&mut app, &mut events, &mut terminal).await;
    restore_terminal()?;
    result
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
    loop {
        // Демон может перезапуститься между командами: обрыв здесь не
        // фатален, следующий удачный вызов продолжит работу.
        if let Err(e) = terminal.draw(|f| draw(f, app)) {
            return Err(e.into());
        }

        // Сначала выгребаем то, что уже копилось из подписки.
        loop {
            match events.try_recv() {
                Ok(event) => apply_event(app, event).await,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    app.notice = Some("поток событий потерян; выход".to_owned());
                    return Ok(());
                }
            }
        }

        if event::poll(Duration::from_millis(100))? {
            if let TermEvent::Key(key) = event::read()? {
                // Зажатие клавиши в некоторых терминалах даёт повторные
                // Press + Release; реагируем только на Press.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if handle_key(app, key.code, key.modifiers).await? {
                    return Ok(());
                }
            }
        }
    }
}

async fn apply_event(app: &mut App, event: Event) {
    match event {
        Event::StateChanged { state } => app.state = state,
        Event::Position { position, duration } => {
            app.state.position = Some(position);
            if duration.is_some() {
                app.state.duration = duration;
            }
        }
        // Очередь изменилась не нами — перечитываем, чтобы список был
        // честным. Ошибка обрыва не страшна: при следующем событии
        // попробуем снова.
        Event::QueueChanged { .. } => {
            if let Ok(Payload::Queue(q)) = app.client.call(Cmd::Queue).await {
                app.state.queue_len = q.tracks.len();
                app.state.queue_index = q.index;
            }
        }
        Event::TrackChanged { .. } | Event::CacheProgress { .. } | Event::AuthChanged { .. } => {}
    }
}

/// `true` — выход из TUI. Ошибки команд не роняют интерфейс: показываем
/// их в строке состояния и продолжаем.
async fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<bool> {
    if app.search_mode {
        return handle_search_key(app, code).await.map(|_| false);
    }
    match code {
        KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(true),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        KeyCode::Tab => {
            app.panel = match app.panel {
                Panel::Library => Panel::Queue,
                Panel::Queue => Panel::Search,
                Panel::Search => Panel::Library,
            };
        }
        KeyCode::Char('j') | KeyCode::Down => {
            move_sel(app, 1);
            load_selected_playlist(app).await;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            move_sel(app, -1);
            load_selected_playlist(app).await;
        }
        KeyCode::Enter => play_selected(app).await,
        KeyCode::Char(' ') => call_quiet(app, Cmd::Toggle).await,
        KeyCode::Char('n') => call_quiet(app, Cmd::Next).await,
        KeyCode::Char('p') => call_quiet(app, Cmd::Prev).await,
        KeyCode::Char('+') | KeyCode::Char('=') => {
            call_quiet(app, Cmd::SetVolume { volume: (app.state.volume + 5.0).clamp(0.0, 100.0) }).await;
        }
        KeyCode::Char('-') => {
            call_quiet(app, Cmd::SetVolume { volume: (app.state.volume - 5.0).clamp(0.0, 100.0) }).await;
        }
        KeyCode::Char('s') => call_quiet(app, Cmd::SetShuffle { shuffle: !app.state.shuffle }).await,
        KeyCode::Char('r') => {
            let next = match app.state.loop_mode {
                LoopMode::None => LoopMode::Track,
                LoopMode::Track => LoopMode::Queue,
                LoopMode::Queue => LoopMode::None,
            };
            call_quiet(app, Cmd::SetLoop { mode: next }).await;
        }
        KeyCode::Char('/') => {
            app.search_mode = true;
            app.search_input.clear();
        }
        KeyCode::Char('f') => {
            if let Some(track) = app.state.track.clone() {
                call_quiet(app, Cmd::CachePin { tracks: vec![track.id] }).await;
            }
        }
        _ => {}
    }
    Ok(false)
}

async fn handle_search_key(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Esc => app.search_mode = false,
        KeyCode::Enter => {
            app.search_mode = false;
            let query = app.search_input.clone();
            if let Ok(Payload::Results(results)) = app
                .client
                .call(Cmd::Search { query, kind: tmus_core::model::SearchKind::Tracks, provider: None })
                .await
            {
                app.search_results = results;
                app.panel = Panel::Search;
                app.search_sel.select(Some(0));
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

/// Загрузить треки подсвеченного плейлиста в правую колонку. На каждый
/// шаг курсора — один запрос; треки не кэшируются, каталог может
/// меняться под ногами.
async fn load_selected_playlist(app: &mut App) {
    let Some(idx) = app.library_sel.selected() else { return };
    let Some(playlist) = app.playlists.get(idx) else { return };
    let id = playlist.id.clone();
    if let Ok(Payload::Tracks(tracks)) = app.client.call(Cmd::LibraryTracks { playlist: id }).await {
        app.playlist_tracks = tracks;
    }
}

fn move_sel(app: &mut App, delta: i64) {
    let (len, sel) = match app.panel {
        Panel::Library => (app.playlists.len(), &mut app.library_sel),
        Panel::Queue => (app.state.queue_len, &mut app.queue_sel),
        Panel::Search => (app.search_results.len(), &mut app.search_sel),
    };
    if len == 0 {
        return;
    }
    let current = sel.selected().unwrap_or(0) as i64;
    let next = (current + delta).clamp(0, len as i64 - 1);
    sel.select(Some(next as usize));
}

async fn play_selected(app: &mut App) {
    match app.panel {
        Panel::Library => {
            let Some(idx) = app.library_sel.selected() else { return };
            let Some(playlist) = app.playlists.get(idx) else { return };
            call_quiet(app, Cmd::PlayPlaylist { playlist: playlist.id.clone(), start: Some(0) }).await;
        }
        Panel::Queue => {
            if let Some(idx) = app.queue_sel.selected() {
                call_quiet(app, Cmd::QueueGoto { index: idx }).await;
            }
        }
        Panel::Search => {
            let Some(idx) = app.search_sel.selected() else { return };
            if let Some(SearchResult::Track(track)) = app.search_results.get(idx) {
                call_quiet(app, Cmd::PlayTrack { track: track.id.clone() }).await;
            }
        }
    }
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
    let [main, status] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .areas(f.area());

    let [left, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .areas(main);

    // Левая колонка: текущая панель. В режиме поиска заголовок
    // показывает ввод.
    let (title, items, sel_panel) = match app.panel {
        Panel::Library => (
            "Библиотека [Tab]".to_owned(),
            app.playlists
                .iter()
                .map(|p| {
                    let count = p.track_count.map(|c| format!(" ({c})")).unwrap_or_default();
                    ListItem::new(Line::from(format!("{}{}", p.title, count)))
                })
                .collect(),
            &mut app.library_sel,
        ),
        Panel::Queue => (
            "Очередь [Tab]".to_owned(),
            queue_items(app),
            &mut app.queue_sel,
        ),
        Panel::Search => (
            format!("Результаты: {}", app.search_input),
            search_items(app),
            &mut app.search_sel,
        ),
    };

    let left_list = List::new(items)
        .block(Block::new().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(left_list, left, sel_panel);

    // Правая колонка: треки выбранного плейлиста (или очередь, если
    // плейлист не выбран). Источник трека виден в списке: очередь
    // смешанная, без колонки провайдера она нечитаема.
    let right_items: Vec<ListItem> = if !app.playlist_tracks.is_empty() {
        app.playlist_tracks.iter().map(track_line).collect()
    } else {
        queue_items(app)
    };
    let right_list = List::new(right_items)
        .block(Block::new().borders(Borders::ALL).title("Треки [Tab]"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(right_list, right, &mut app.queue_sel.clone());

    draw_status(f, app, status);
}

/// Строка очереди с провайдером: `ytmusic:abc` из `TrackId::Display`.
fn track_line(track: &Track) -> ListItem<'static> {
    ListItem::new(Line::from(format!(
        "[{}] {} — {}",
        track.id.provider,
        track.artist_line(),
        track.title
    )))
}

fn queue_items(app: &App) -> Vec<ListItem<'static>> {
    // Полная очередь у клиента не хранится (приходит только длина),
    // показываем текущий трек и подсказку.
    match &app.state.track {
        Some(track) => vec![track_line(track)],
        None => vec![ListItem::new("очередь пуста")],
    }
}

fn search_items(app: &App) -> Vec<ListItem<'static>> {
    app.search_results
        .iter()
        .map(|r| match r {
            SearchResult::Track(t) => track_line(t),
            SearchResult::Playlist(p) => ListItem::new(format!("{}: {}", p.id, p.title)),
            SearchResult::Artist { provider, id, name } => {
                ListItem::new(format!("[{provider}:{id}] {name}"))
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

    if let Some(track) = &app.state.track {
        let pos = app.state.position.map(|d| d.as_secs());
        let dur = app.state.duration.map(|d| d.as_secs());
        let line = trim_fit(
            &format!(
                "{} {} {} {} {}  vol:{} {}{} [{}] f=кэш q=выход",
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

#[cfg(test)]
mod tests {
    use super::*;

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

/// Обрезка по символам с многоточием, если строка не влезла.
fn trim_fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_owned();
    }
    let mut cut: String = s.chars().take(width.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}
