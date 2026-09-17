//! Ядро демона: одна точка обработки команд и одна шина событий.
//!
//! Правило, которое здесь закреплено кодом: **у плеера нет других
//! потребителей, кроме [`App`]**. Control-socket, MPRIS, иконка в трее и
//! Discord RPC — все зовут [`App::handle`] и все слушают
//! [`App::subscribe`]. Причина не в красоте: четыре подсистемы,
//! дёргающие плеер напрямую, разошлись бы в том, что считают текущим
//! состоянием, и расхождение увидел бы человек — панель noctalia
//! показывала бы одно, Discord другое.
//!
//! Второе правило: здесь нет ни одного упоминания конкретного сервиса.
//! Провайдеры приходят через [`Registry`], и добавление провайдера не
//! требует правок ни в одной подсистеме.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tmus_core::cache::Cache;
use tmus_core::config::Config;
use tmus_core::model::{
    PlaybackStatus, Playlist, PlaylistId, SearchKind, SearchResult, Track, TrackId,
};
use tmus_core::paths::Paths;
use tmus_core::protocol::{Ack, Cmd, Event, Payload, ProviderView, QueueView};
use tmus_player::Player;
use tmus_provider::Registry;
use tokio::sync::broadcast;

/// Сколько событий держится в шине для отстающего подписчика.
///
/// `Position` идёт раз в секунду, и подписчик, уснувший на минуту,
/// должен потерять старые позиции, а не заблокировать шину. Потеря
/// части событий безопасна: `StateChanged` восстанавливает полную
/// картину.
const EVENT_BUFFER: usize = 256;

pub struct App {
    player: Player,
    registry: Registry,
    /// `rusqlite::Connection` не `Sync` (внутри `RefCell`), поэтому
    /// без замка `Arc<App>` нельзя отдать в `tokio::spawn` — а его
    /// ждут все четыре подсистемы. Замок берётся на один вызов и
    /// НИКОГДА не удерживается через `await`: все методы `Cache`
    /// синхронные и короткие.
    cache: Arc<std::sync::Mutex<Cache>>,
    config: Config,
    paths: Paths,
    events: broadcast::Sender<Event>,
    /// Сигнал «пора гаситься». Нужен, потому что `Cmd::Shutdown`
    /// приходит из задачи control-socket, а гасить обязан `main`: только
    /// он снимает файл сокета и убивает mpv. Вызов `std::process::exit`
    /// из подсистемы обходил бы обе очистки — замерено, оставались
    /// осиротевшие mpv по ~100 МБ и мёртвый файл сокета.
    shutdown: tokio::sync::Notify,
}

impl App {
    pub fn new(
        player: Player,
        registry: Registry,
        cache: Arc<std::sync::Mutex<Cache>>,
        config: Config,
        paths: Paths,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Arc::new(Self {
            player,
            registry,
            cache,
            config,
            paths,
            events,
            shutdown: tokio::sync::Notify::new(),
        })
    }

    pub fn player(&self) -> &Player {
        &self.player
    }

    /// Доступ к кэшу под замком. Замыкание, а не `&Cache`: так guard
    /// нельзя случайно пронести через `await` и получить взаимоблокировку.
    pub fn with_cache<R>(&self, f: impl FnOnce(&Cache) -> R) -> R {
        let cache = self.cache.lock().expect("замок кэша отравлен паникой");
        f(&cache)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Попросить демон погаситься. Возврата к работе нет.
    pub fn request_shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    /// Дождаться просьбы погаситься.
    pub async fn wait_shutdown(&self) {
        self.shutdown.notified().await;
    }

    /// Подписаться на поток событий. Отстающий подписчик теряет старые
    /// события, а не тормозит остальных.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Разослать событие. Отсутствие подписчиков — не ошибка: демон
    /// штатно работает без единого клиента.
    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Разослать полное состояние. Зовётся после всякого изменения,
    /// которое подписчик не может вывести из частных событий.
    pub async fn emit_state(&self) {
        let state = self.player.state().await;
        self.emit(Event::StateChanged { state });
    }

    /// Единственная точка обработки команд.
    pub async fn handle(&self, cmd: Cmd) -> anyhow::Result<Payload> {
        match cmd {
            // `Subscribe` обрабатывает транспорт: это не команда плееру,
            // а смена режима соединения.
            Cmd::Subscribe => Ok(Payload::Ack(Ack::default())),

            Cmd::PlayTrack { track } => {
                self.play_track(&track).await?;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::PlayPlaylist { playlist, start } => {
                self.play_playlist(&playlist, start.unwrap_or(0)).await?;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::Toggle => {
                let paused = matches!(self.player.state().await.status, PlaybackStatus::Playing);
                self.player.mpv().pause(paused).await?;
                self.emit_state().await;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::Play => {
                self.player.mpv().pause(false).await?;
                self.emit_state().await;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::Pause => {
                self.player.mpv().pause(true).await?;
                self.emit_state().await;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::Stop => {
                self.player.mpv().stop().await?;
                self.emit_state().await;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::Next => {
                self.step(true).await?;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::Prev => {
                self.step(false).await?;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::Seek { position } => {
                self.player.mpv().seek_absolute(position).await?;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::SeekBy { delta } => {
                self.player.mpv().seek_relative(delta).await?;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::SetVolume { volume } => {
                self.player.set_volume(volume.clamp(0.0, 100.0)).await?;
                self.emit_state().await;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::SetLoop { mode } => {
                self.player.with_queue(|q| q.set_loop_mode(mode)).await;
                self.emit_state().await;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::SetShuffle { shuffle } => {
                self.player.with_queue(|q| q.set_shuffle(shuffle)).await;
                self.emit_state().await;
                Ok(Payload::Ack(Ack::default()))
            }

            Cmd::Queue => {
                let view = self
                    .player
                    .with_queue(|q| QueueView {
                        tracks: (0..q.len())
                            .filter_map(|i| q.track_at(i).cloned())
                            .collect(),
                        index: q.current_index(),
                    })
                    .await;
                Ok(Payload::Queue(view))
            }
            Cmd::QueueAppend { tracks } => {
                let resolved = self.hydrate(&tracks)?;
                let len = self
                    .player
                    .with_queue(|q| {
                        for track in resolved {
                            q.append(track);
                        }
                        q.len()
                    })
                    .await;
                let index = self.player.with_queue(|q| q.current_index()).await;
                self.emit(Event::QueueChanged { len, index });
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::QueueClear => {
                self.player.with_queue(|q| q.clear()).await;
                self.emit(Event::QueueChanged { len: 0, index: None });
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::QueueGoto { index } => {
                let track = self
                    .player
                    .with_queue(|q| {
                        if q.goto(index) {
                            q.current().cloned()
                        } else {
                            None
                        }
                    })
                    .await;
                match track {
                    Some(track) => {
                        self.play_track(&track.id).await?;
                        Ok(Payload::Ack(Ack::default()))
                    }
                    None => anyhow::bail!("в очереди нет позиции {index}"),
                }
            }

            Cmd::State => Ok(Payload::State(self.player.state().await)),
            Cmd::Providers => Ok(Payload::Providers(self.providers())),
            Cmd::Search { query, kind, provider } => {
                Ok(Payload::Results(self.search(&query, kind, provider.as_deref()).await?))
            }
            Cmd::Library { provider } => {
                Ok(Payload::Playlists(self.library(provider.as_deref()).await?))
            }
            Cmd::LibraryTracks { playlist } => {
                Ok(Payload::Tracks(self.playlist_tracks(&playlist).await?))
            }
            Cmd::Liked { provider } => Ok(Payload::Tracks(self.liked(provider.as_deref()).await?)),

            Cmd::CacheStats => Ok(Payload::Cache(self.with_cache(|c| c.stats())?)),
            Cmd::CachePin { tracks } => {
                self.with_cache(|c| c.set_pinned(&tracks, true))?;
                Ok(Payload::Cache(self.with_cache(|c| c.stats())?))
            }
            Cmd::CacheUnpin { tracks } => {
                self.with_cache(|c| c.set_pinned(&tracks, false))?;
                Ok(Payload::Cache(self.with_cache(|c| c.stats())?))
            }
            Cmd::CacheGc => {
                self.with_cache(|c| c.gc())?;
                Ok(Payload::Cache(self.with_cache(|c| c.stats())?))
            }

            Cmd::Shutdown => {
                // Транспорт увидит `Ack` и закроется сам; гасит демон
                // `main`, чтобы не рвать соединение до ответа.
                Ok(Payload::Ack(Ack::default()))
            }
        }
    }

    // ────────────────────────────────────────────────── внутреннее

    async fn play_track(&self, id: &TrackId) -> anyhow::Result<()> {
        // Трек обязан оказаться в очереди, даже если его включили
        // поштучно. `Player::state()` берёт текущий трек из очереди по
        // индексу, и без этого `tmus play <id>` играл бы «в никуда»:
        // музыка идёт, а `status`, MPRIS и Discord показывают пустоту.
        // Замерено на стенде.
        let known = self.player.with_queue(|q| q.find_index(id)).await;
        match known {
            Some(index) => {
                self.player.with_queue(|q| q.goto(index)).await;
            }
            None => {
                let track = self.hydrate(std::slice::from_ref(id))?.remove(0);
                let index = self
                    .player
                    .with_queue(|q| {
                        q.append(track);
                        q.len() - 1
                    })
                    .await;
                self.player.with_queue(|q| q.goto(index)).await;
            }
        }

        self.player.resolve_and_play(id).await?;
        let state = self.player.state().await;
        self.emit(Event::TrackChanged {
            track: state.track.clone(),
            queue_index: state.queue_index,
        });
        self.emit(Event::StateChanged { state });
        Ok(())
    }

    async fn play_playlist(&self, id: &PlaylistId, start: usize) -> anyhow::Result<()> {
        let tracks = self.playlist_tracks(id).await?;
        if tracks.is_empty() {
            anyhow::bail!("плейлист {id} пуст");
        }
        let start = start.min(tracks.len() - 1);
        let first = tracks[start].id.clone();
        let len = self
            .player
            .with_queue(|q| {
                q.clear();
                for track in tracks {
                    q.append(track);
                }
                q.goto(start);
                q.len()
            })
            .await;
        self.emit(Event::QueueChanged { len, index: Some(start) });
        self.play_track(&first).await
    }

    /// Шаг по очереди. Отдельно от `Cmd::Next`, потому что то же нужно
    /// по концу трека — и логика перехода обязана быть одна.
    async fn step(&self, forward: bool) -> anyhow::Result<()> {
        let next = self
            .player
            .with_queue(|q| {
                if forward { q.next().cloned() } else { q.prev().cloned() }
            })
            .await;
        match next {
            Some(track) => self.play_track(&track.id).await,
            None => {
                self.player.mpv().stop().await?;
                self.emit_state().await;
                Ok(())
            }
        }
    }

    /// Достать полные `Track` по идентификаторам: из кэша метаданных, а
    /// при промахе — минимальной заглушкой с самим id.
    ///
    /// Заглушка честнее отказа: трек по id в очередь добавить можно и
    /// нужно (его метаданные приедут при воспроизведении), а падать на
    /// «нет в кэше» значило бы требовать от клиента заранее прогреть
    /// базу.
    fn hydrate(&self, ids: &[TrackId]) -> anyhow::Result<Vec<Track>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let track = self.with_cache(|c| c.track(id))?.unwrap_or_else(|| Track {
                id: id.clone(),
                title: id.id.clone(),
                artists: Vec::new(),
                album: None,
                duration: None,
                art_url: None,
                page_url: None,
            });
            out.push(track);
        }
        Ok(out)
    }

    fn providers(&self) -> Vec<ProviderView> {
        self.registry
            .iter()
            .map(|provider| {
                let account = provider.account();
                ProviderView {
                    id: provider.id().as_str().to_owned(),
                    name: account.display_name().to_owned(),
                    auth: account.auth(),
                }
            })
            .collect()
    }

    /// Провайдеры под запрос: назван один — только он, не назван — все.
    fn targets(&self, provider: Option<&str>) -> anyhow::Result<Vec<&Arc<dyn tmus_provider::Provider>>> {
        match provider {
            Some(name) => {
                let one = self
                    .registry
                    .get_by_str(name)
                    .ok_or_else(|| anyhow::anyhow!("провайдер {name} не подключён"))?;
                Ok(vec![one])
            }
            None => Ok(self.registry.iter().collect()),
        }
    }

    async fn search(
        &self,
        query: &str,
        kind: SearchKind,
        provider: Option<&str>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let mut out = Vec::new();
        for target in self.targets(provider)? {
            // Один упавший провайдер не должен обнулять поиск по
            // остальным: агрегирующий поиск тем и полезен.
            match target.catalog().search(query, kind).await {
                Ok(found) => out.extend(found),
                Err(err) => {
                    tracing::warn!(provider = %target.id(), %err, "поиск не удался");
                    self.report_auth(target, &err);
                }
            }
        }
        self.remember(&out)?;
        Ok(out)
    }

    async fn library(&self, provider: Option<&str>) -> anyhow::Result<Vec<Playlist>> {
        let mut out = Vec::new();
        for target in self.targets(provider)? {
            match target.catalog().playlists().await {
                Ok(found) => out.extend(found),
                Err(err) => {
                    tracing::warn!(provider = %target.id(), %err, "библиотека не прочиталась");
                    self.report_auth(target, &err);
                }
            }
        }
        if out.is_empty() {
            // Сеть могла отвалиться целиком — тогда показываем то, что
            // уже знаем. Это половина смысла офлайн-кэша.
            out = self.with_cache(|c| c.playlists(provider))?;
        } else {
            self.with_cache(|c| c.put_playlists(&out))?;
        }
        Ok(out)
    }

    async fn playlist_tracks(&self, id: &PlaylistId) -> anyhow::Result<Vec<Track>> {
        let provider = self
            .registry
            .get(id.provider)
            .ok_or_else(|| anyhow::anyhow!("провайдер {} не подключён", id.provider))?;
        match provider.catalog().playlist_tracks(id).await {
            Ok(tracks) => {
                self.with_cache(|c| c.put_playlist_tracks(id, &tracks))?;
                Ok(tracks)
            }
            Err(err) => {
                tracing::warn!(playlist = %id, %err, "плейлист не прочитался, беру из кэша");
                self.report_auth(provider, &err);
                Ok(self.with_cache(|c| c.playlist_tracks(id))?)
            }
        }
    }

    async fn liked(&self, provider: Option<&str>) -> anyhow::Result<Vec<Track>> {
        let mut out = Vec::new();
        for target in self.targets(provider)? {
            match target.catalog().liked().await {
                Ok(found) => out.extend(found),
                Err(err) => {
                    tracing::warn!(provider = %target.id(), %err, "лайки не прочитались");
                    self.report_auth(target, &err);
                }
            }
        }
        if !out.is_empty() {
            self.with_cache(|c| c.put_tracks(&out))?;
        }
        Ok(out)
    }

    fn remember(&self, results: &[SearchResult]) -> anyhow::Result<()> {
        let tracks: Vec<Track> = results
            .iter()
            .filter_map(|r| match r {
                SearchResult::Track(track) => Some(track.clone()),
                _ => None,
            })
            .collect();
        if !tracks.is_empty() {
            self.with_cache(|c| c.put_tracks(&tracks))?;
        }
        Ok(())
    }

    /// Отвалившаяся авторизация обязана дойти до бара событием, а не
    /// всплыть позже невнятным «bot check» при попытке что-то включить.
    fn report_auth(&self, provider: &Arc<dyn tmus_provider::Provider>, err: &tmus_provider::ProviderError) {
        if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
            self.emit(Event::AuthChanged {
                provider: provider.id().as_str().to_owned(),
                auth: provider.account().auth(),
            });
        }
    }
}

/// Вахтер состояния: рассылает позицию и замечает смену трека.
///
/// Почему опросом, а не подпиской на события mpv: `Player` забирает
/// приёмник событий себе в конструкторе и сам ведёт переходы по очереди
/// — второго читателя у `mpsc` быть не может. Диффа раз в секунду хватает:
/// mpv присылает `time-pos` десятки раз в секунду, и пересылать каждое
/// значило бы будить всех подписчиков зря, а смену трека бар и Discord
/// переживут с задержкой до секунды.
///
/// Здесь же единственное место, где состояние сравнивается с прошлым:
/// подписчик получает `TrackChanged` ровно один раз на трек, а не на
/// каждый тик.
pub async fn run_state_watcher(app: Arc<App>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut last_track: Option<TrackId> = None;
    let mut last_status = PlaybackStatus::Stopped;
    let mut last_queue = (0usize, None);

    loop {
        ticker.tick().await;
        let state = app.player.state().await;

        let track = state.track.as_ref().map(|t| t.id.clone());
        if track != last_track {
            last_track = track;
            app.emit(Event::TrackChanged {
                track: state.track.clone(),
                queue_index: state.queue_index,
            });
            app.emit(Event::StateChanged { state: state.clone() });
        } else if state.status != last_status {
            app.emit(Event::StateChanged { state: state.clone() });
        }
        last_status = state.status;

        let queue = (state.queue_len, state.queue_index);
        if queue != last_queue {
            last_queue = queue;
            app.emit(Event::QueueChanged { len: queue.0, index: queue.1 });
        }

        if !matches!(state.status, PlaybackStatus::Playing) {
            continue;
        }
        if let Some(position) = state.position {
            app.emit(Event::Position { position, duration: state.duration });
        }
    }
}

/// Фоновая докачка в офлайн-кэш.
///
/// Хозяин заказал офлайн явно, и «скачать по требованию» его не закрывает:
/// нужен трек, который уже слушали. Поэтому играемый трек кладётся на
/// диск, а следующий в очереди докачивается заранее.
pub async fn run_cache_filler(app: Arc<App>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;

        let wanted = {
            let state = app.player.state().await;
            let current = state.track.map(|t| t.id);
            let next = app.player.with_queue(|q| q.peek_next().map(|t| t.id.clone())).await;
            [current, next]
        };

        for id in wanted.into_iter().flatten() {
            match app.with_cache(|c| c.lookup_audio(&id)) {
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(%err, "кэш не опрашивается");
                    continue;
                }
            }
            if let Err(err) = fetch_into_cache(&app, &id).await {
                tracing::warn!(track = %id, %err, "докачка не удалась");
            }
        }

        if let Err(err) = app.with_cache(|c| c.gc()) {
            tracing::warn!(%err, "вытеснение кэша не удалось");
        }
    }
}

/// Скачать трек в офлайн-кэш через тот же резолв, что и воспроизведение.
///
/// URL не кэшируется никогда: у googlevideo он живёт около шести часов
/// (`expire=`). Кэшируется файл.
async fn fetch_into_cache(app: &Arc<App>, id: &TrackId) -> anyhow::Result<()> {
    let resolver = app.registry.resolver_for(id)?;
    let source = resolver.resolve(id).await?;
    let tmus_core::model::StreamSource::Remote { url, user_agent, expires_at } = &source else {
        // Уже локальный — значит кэш опередил нас, докачивать нечего.
        return Ok(());
    };
    if source.is_expired(SystemTime::now()) {
        anyhow::bail!("резолв истёк до начала загрузки");
    }

    let ext = "webm";
    let target = app.with_cache(|c| c.audio_path(id, ext));
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Пишем в соседний файл и переименовываем: оборванная загрузка не
    // должна попасть в кэш как готовый трек — она игралась бы обрезанной.
    let partial = target.with_extension(format!("{ext}.part"));
    let mut request = reqwest_get(url, user_agent.as_deref()).await?;
    let mut file = tokio::fs::File::create(&partial).await?;
    let mut written: u64 = 0;
    let total = request.content_length();
    while let Some(chunk) = request.chunk().await? {
        use tokio::io::AsyncWriteExt as _;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        app.emit(Event::CacheProgress { track: id.clone(), bytes: written, total });
    }
    use tokio::io::AsyncWriteExt as _;
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&partial, &target).await?;

    app.with_cache(|c| c.register_audio(id, &target, ext))?;
    let _ = expires_at;
    Ok(())
}

/// Загрузка с ровно одним заголовком.
///
/// Передаём только `User-Agent`, как и в mpv: остальные заголовки
/// yt-dlp содержат запятые, и mpv их разрезает, отчего googlevideo
/// отвечает `400`. Здесь разрезать нечему, но набор заголовков держим
/// одинаковым — иначе кэш и воспроизведение расходились бы в том, что
/// именно сервер считает валидным запросом.
async fn reqwest_get(url: &str, user_agent: Option<&str>) -> anyhow::Result<reqwest::Response> {
    let client = reqwest::Client::builder().build()?;
    let mut request = client.get(url);
    if let Some(ua) = user_agent {
        request = request.header(reqwest::header::USER_AGENT, ua);
    }
    let response = request.send().await?.error_for_status()?;
    Ok(response)
}


