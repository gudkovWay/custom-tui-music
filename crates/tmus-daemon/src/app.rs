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
use std::time::{Duration, Instant, SystemTime};

use tmus_core::cache::Cache;
use tmus_core::config::Config;
use tmus_core::model::{
    PlaybackStatus, Playlist, PlaylistId, SearchKind, SearchResult, Track, TrackId,
};
use tmus_core::paths::Paths;
use tmus_core::protocol::{Ack, CatalogSource, Cmd, Event, Payload, ProviderView, QueueView};
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
    catalog_source: std::sync::Mutex<CatalogSource>,
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
        let saved = tmus_core::catalog_source::load(&paths);
        let connected: Vec<String> =
            registry.iter().map(|provider| provider.id().as_str().to_owned()).collect();
        let catalog_source = resolve_catalog_source(saved.as_ref(), connected);
        Arc::new(Self {
            player,
            registry,
            cache,
            config,
            paths,
            events,
            catalog_source: std::sync::Mutex::new(catalog_source),
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
            // Каталожные операции разбирают мегабайты JSON: библиотека
            // из 1000 треков приезжает десятком страниц продолжений.
            // После них арены glibc остаются раздутыми — замерено:
            // 9.9 МБ на старте → 187 МБ высокой метки, и на покое она
            // не опускается. Поэтому страницы возвращаются системе
            // явно, сразу после операции.
            Cmd::Search { query, kind, provider } => {
                let out = self.search(&query, kind, provider.as_deref()).await?;
                release_memory();
                Ok(Payload::Results(out))
            }
            Cmd::Library { provider } => {
                let out = self.library(provider.as_deref()).await?;
                release_memory();
                Ok(Payload::Playlists(out))
            }
            Cmd::LibraryTracks { playlist } => {
                let out = self.playlist_tracks(&playlist).await?;
                release_memory();
                Ok(Payload::Tracks(out))
            }
            Cmd::Liked { provider } => {
                let out = self.liked(provider.as_deref()).await?;
                release_memory();
                Ok(Payload::Tracks(out))
            }

            Cmd::GetCatalogSource => {
                Ok(Payload::Catalog(self.catalog_source.lock().expect("catalog source").clone()))
            }
            Cmd::SetCatalogSource { source } => {
                let mut guard = self.catalog_source.lock().expect("catalog source");
                if let Some(provider) = &source.provider {
                    if self.registry.get_by_str(provider).is_none() {
                        anyhow::bail!("провайдер {provider} не подключён");
                    }
                }
                tmus_core::catalog_source::save(&self.paths, &source)?;
                *guard = source;
                Ok(Payload::Catalog(guard.clone()))
            }

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
        // События о смене трека не шлём отсюда: плеер уже дал сигнал
        // вахтёру, а тот — единственный, кто сравнивает состояние с
        // прошлым. Дубль здесь давал на один скип два `TrackChanged` и
        // два `StateChanged` (замерено в потоке `tmus events`), то есть
        // двойную перерисовку бара, причём первый кадр — без
        // длительности, которую mpv сообщает позже.
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

pub fn resolve_catalog_source(
    saved: Option<&CatalogSource>,
    connected: Vec<String>,
) -> CatalogSource {
    match saved {
        Some(source)
            if source
                .provider
                .as_deref()
                .is_none_or(|provider| connected.iter().any(|known| known == provider)) =>
        {
            source.clone()
        }
        _ => CatalogSource { provider: connected.into_iter().next() },
    }
}

#[cfg(test)]
mod catalog_source_tests {
    use tmus_core::protocol::CatalogSource;

    use super::resolve_catalog_source;

    #[test]
    fn saved_provider_is_kept() {
        let saved = CatalogSource { provider: Some("soundcloud".into()) };
        assert_eq!(
            resolve_catalog_source(Some(&saved), vec!["ytmusic".into(), "soundcloud".into()]),
            saved
        );
    }

    #[test]
    fn invalid_saved_provider_falls_back_to_first_connected() {
        let saved = CatalogSource { provider: Some("spotify".into()) };
        assert_eq!(
            resolve_catalog_source(Some(&saved), vec!["ytmusic".into(), "soundcloud".into()]),
            CatalogSource { provider: Some("ytmusic".into()) }
        );
    }

    #[test]
    fn saved_all_is_kept() {
        let saved = CatalogSource { provider: None };
        assert_eq!(
            resolve_catalog_source(Some(&saved), vec!["ytmusic".into()]),
            CatalogSource { provider: None }
        );
    }

    #[test]
    fn no_saved_source_selects_first_connected_or_all() {
        assert_eq!(
            resolve_catalog_source(None, Vec::new()),
            CatalogSource { provider: None }
        );
        assert_eq!(
            resolve_catalog_source(None, vec!["soundcloud".into()]),
            CatalogSource { provider: Some("soundcloud".into()) }
        );
    }
}

#[cfg(test)]
mod progress_throttle_tests {
    use std::time::{Duration, Instant};

    use super::{should_emit, PROGRESS_INTERVAL};

    #[test]
    fn interval_boundary_and_final_frame() {
        let t0 = Instant::now();
        // Раньше интервала — молчим.
        assert!(!should_emit(t0, t0 + PROGRESS_INTERVAL - Duration::from_millis(1), false));
        // Ровно интервал — пора.
        assert!(should_emit(t0, t0 + PROGRESS_INTERVAL, false));
        // Завершающий кадр уходит всегда, даже раньше интервала.
        assert!(should_emit(t0, t0, true));
    }
}

/// Вернуть системе страницы, освобождённые аллокатором.
///
/// glibc держит освобождённую память в аренах и сам её не отдаёт, если
/// та фрагментирована. После разбора библиотеки это видно прямо:
/// замерено 9.9 МБ на старте демона, 14.4 МБ после чтения плейлистов,
/// **187 МБ высокой метки** после чтения 1000 лайков — и на покое она
/// не опускалась. Это не утечка: повторные вызовы упираются в то же
/// плато. Но 187 МБ в покое сводят на нет весь смысл замены Electron,
/// поэтому страницы возвращаются явно.
///
/// Функция специфична для glibc; на других аллокаторах она просто
/// ничего не сделает и вернёт 0.
fn release_memory() {
    // SAFETY: malloc_trim не принимает указателей и не имеет
    // предусловий; аргумент — сколько байт оставить в запасе на вершине
    // кучи, 0 значит «вернуть всё, что можно».
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Вахтер состояния: рассылает позицию и замечает смену трека.
///
/// Почему не подписка на события mpv: `Player` забирает приёмник себе в
/// конструкторе и сам ведёт переходы по очереди — второго читателя у
/// `mpsc` быть не может. Поэтому вахтёр просыпается по двум причинам:
/// раз в секунду (позиция) и по сигналу `Player::changed`, который
/// плеер даёт на смену трека, паузу, приехавшую длительность и падение
/// mpv. Чистый секундный опрос давал замеренный рассинк бара: до
/// секунды `--:--` вместо длительности и двойная перерисовка на скип.
///
/// Здесь же единственное место, где состояние сравнивается с прошлым:
/// подписчик получает `TrackChanged` ровно один раз на трек, а не на
/// каждый тик — поэтому команды сами событий смены трека не шлют.
pub async fn run_state_watcher(app: Arc<App>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut last_track: Option<TrackId> = None;
    let mut last_status = PlaybackStatus::Stopped;
    let mut last_duration: Option<Duration> = None;
    let mut last_queue = (0usize, None);

    loop {
        // Тик нужен позиции, сигнал — всему остальному: первый из двух
        // и будит цикл.
        let ticked = tokio::select! {
            _ = ticker.tick() => true,
            () = app.player.changed() => false,
        };
        let state = app.player.state().await;

        let track = state.track.as_ref().map(|t| t.id.clone());
        if track != last_track {
            last_track = track;
            app.emit(Event::TrackChanged {
                track: state.track.clone(),
                queue_index: state.queue_index,
            });
            app.emit(Event::StateChanged { state: state.clone() });
        } else if state.status != last_status || state.duration != last_duration {
            // Длительность приезжает от mpv позже загрузки, и без её
            // рассылки бар до следующей смены трека рисовал бы `--:--`.
            app.emit(Event::StateChanged { state: state.clone() });
        }
        last_status = state.status;
        last_duration = state.duration;

        let queue = (state.queue_len, state.queue_index);
        if queue != last_queue {
            last_queue = queue;
            app.emit(Event::QueueChanged { len: queue.0, index: queue.1 });
        }

        if !ticked || !matches!(state.status, PlaybackStatus::Playing) {
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

// Троттлинг прогресса: на chunk'ах по 8 КиБ событие шины уходило на каждый
// chunk — замерено 490 ev/s (183 события за 0.37 с на файле 3 МБ), а буфер
// шины всего 256, подписчики уходят в Lagged. Шкала быстрее 2 Гц всё равно
// никому не видна.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

// Проверка актуальности трека стоит обращений к state()/with_queue, поэтому
// не на каждый chunk, а не чаще раза в секунду.
const RELEVANCE_INTERVAL: Duration = Duration::from_secs(1);

/// Пора ли слать прогресс: либо прошло не меньше интервала, либо это
/// завершающий кадр — он обязан уйти всегда, иначе потребитель не увидит 100%.
fn should_emit(last: Instant, now: Instant, done: bool) -> bool {
    done || now.duration_since(last) >= PROGRESS_INTERVAL
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
    let mut last_emit = Instant::now() - PROGRESS_INTERVAL;
    let mut last_relevance = Instant::now();
    while let Some(chunk) = request.chunk().await? {
        use tokio::io::AsyncWriteExt as _;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;

        let now = Instant::now();
        if should_emit(last_emit, now, false) {
            last_emit = now;
            app.emit(Event::CacheProgress { track: id.clone(), bytes: written, total });
        }

        // Человек ушёл с трека — докачивать до конца бессмысленно: это лишние
        // трафик, диск и CPU на события. Проверяем актуальность не чаще раза
        // в секунду, чтобы не дёргать state()/with_queue на каждом chunk'е.
        if now.duration_since(last_relevance) >= RELEVANCE_INTERVAL {
            last_relevance = now;
            let still_wanted = {
                let state = app.player.state().await;
                let current = state.track.as_ref().map(|t| &t.id) == Some(id);
                let next = app
                    .player
                    .with_queue(|q| q.peek_next().map(|t| t.id.clone()))
                    .await
                    .as_ref() == Some(id);
                current || next
            };
            if !still_wanted {
                drop(file);
                if let Err(err) = tokio::fs::remove_file(&partial).await {
                    tracing::debug!(%err, "не удалось удалить .part отменённой докачки");
                }
                tracing::debug!("докачка отменена: трек больше не текущий и не следующий");
                return Ok(());
            }
        }
    }
    use tokio::io::AsyncWriteExt as _;
    app.emit(Event::CacheProgress { track: id.clone(), bytes: written, total });
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


