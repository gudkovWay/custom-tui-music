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

use tmus_core::cache::Cache;
use tmus_core::config::Config;
use tmus_core::model::{
    eq_presets, PlaybackStatus, PlaylistId, Track, TrackId, EQ_GAIN_LIMIT_DB,
};
use tmus_core::paths::Paths;
use tmus_core::protocol::{Ack, CatalogSource, Cmd, Event, Payload, QueueView};
use tmus_player::Player;
use tmus_provider::Registry;
use tokio::sync::broadcast;

use crate::catalog::{resolve_catalog_source, HomeCache};
use crate::filler;

/// Сколько событий держится в шине для отстающего подписчика.
///
/// `Position` идёт раз в секунду, и подписчик, уснувший на минуту,
/// должен потерять старые позиции, а не заблокировать шину. Потеря
/// части событий безопасна: `StateChanged` восстанавливает полную
/// картину.
const EVENT_BUFFER: usize = 256;

pub struct App {
    // доступ модулей filler/watcher после сплита
    pub(crate) player: Player,
    // доступ каталожных модулей после сплита
    pub(crate) registry: Registry,
    /// `rusqlite::Connection` не `Sync` (внутри `RefCell`), поэтому
    /// без замка `Arc<App>` нельзя отдать в `tokio::spawn` — а его
    /// ждут все четыре подсистемы. Замок берётся на один вызов и
    /// НИКОГДА не удерживается через `await`: все методы `Cache`
    /// синхронные и короткие.
    cache: Arc<std::sync::Mutex<Cache>>,
    config: Config,
    paths: Paths,
    events: broadcast::Sender<Event>,
    pub(crate) catalog_source: std::sync::Mutex<CatalogSource>,
    /// Кэш домашней ленты. tokio-замок: держится через await —
    /// параллельные `Cmd::Home` сливаются в один сетевой заход
    /// (single-flight).
    pub(crate) home_cache: tokio::sync::Mutex<Option<HomeCache>>,
    /// Сигнал «пора гаситься». Нужен, потому что `Cmd::Shutdown`
    /// приходит из задачи control-socket, а гасить обязан `main`: только
    /// он снимает файл сокета и убивает mpv. Вызов `std::process::exit`
    /// из подсистемы обходил бы обе очистки — замерено, оставались
    /// осиротевшие mpv по ~100 МБ и мёртвый файл сокета.
    shutdown: tokio::sync::Notify,
    /// Момент последней попытки перечитать cookies, по провайдеру.
    /// Без кулдауна ливень auth-ошибок превращается в ливень чтений
    /// профиля браузера.
    pub(crate) auth_retry: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// Слабая ссылка на себя для фоновых задач: команды приходят по
    /// `&self`, а фоновая работа (грядущий `Cmd::CacheWarm`) должна
    /// пережить ответ клиенту, но и не держать `Arc` вечно.
    pub(crate) self_arc: std::sync::OnceLock<std::sync::Weak<App>>,
    /// Общий семафор резолвов на процесс. Один и тот же `Arc` внедряется
    /// и в плеер, и в филлер: параллельные yt-dlp не имеют смысла
    /// (замер перф-раунда: 0.9 CPU-с и 335 МБ на процесс), а филлер без
    /// общего гейта обгонял плеер и запускал второй yt-dlp параллельно.
    resolve_gate: Arc<tokio::sync::Semaphore>,
}

impl App {
    pub fn new(
        player: Player,
        registry: Registry,
        cache: Arc<std::sync::Mutex<Cache>>,
        config: Config,
        paths: Paths,
        resolve_gate: Arc<tokio::sync::Semaphore>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let saved = tmus_core::catalog_source::load(&paths);
        let connected: Vec<String> =
            registry.iter().map(|provider| provider.id().as_str().to_owned()).collect();
        let catalog_source = resolve_catalog_source(saved.as_ref(), connected);
        let arc = Arc::new(Self {
            player,
            registry,
            cache,
            config,
            paths,
            events,
            catalog_source: std::sync::Mutex::new(catalog_source),
            home_cache: tokio::sync::Mutex::new(None),
            shutdown: tokio::sync::Notify::new(),
            auth_retry: std::sync::Mutex::new(std::collections::HashMap::new()),
            self_arc: std::sync::OnceLock::new(),
            resolve_gate,
        });
        // Поле нельзя заполнить внутри конструируемого `Self`: нужен
        // готовый `Arc`. Слабая ссылка не продлевает жизнь, а фоновые
        // задачи через `self_arc()` просто не стартуют, если демон
        // уже гасится.
        let _ = arc.self_arc.set(Arc::downgrade(&arc));
        arc
    }

    /// Arc на себя для фоновых задач из handle(): команды приходят по
    /// `&self`, а фоновая работа должна пережить ответ клиенту.
    pub(crate) fn self_arc(&self) -> Option<Arc<Self>> {
        self.self_arc.get().and_then(std::sync::Weak::upgrade)
    }

    /// Общий семафор резолвов на процесс (см. поле).
    pub(crate) fn resolve_gate(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.resolve_gate)
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
            Cmd::PlayContext { tracks, start } => {
                self.play_context(tracks, start).await?;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::Toggle => {
                // Медиа-клавиша play/pause обязана что-то делать в любом
                // статусе: на Stopped в пустом mpv пауза — тихий нооп.
                let state = self.player.state().await;
                match state.status {
                    PlaybackStatus::Playing => self.player.mpv().pause(true).await?,
                    PlaybackStatus::Stopped => match state.track {
                        // Трек из `state` берётся из очереди по индексу,
                        // значит он там гарантированно есть.
                        Some(track) => self.play_known(&track.id).await?,
                        None => return Ok(Payload::Ack(Ack::default())),
                    },
                    PlaybackStatus::Paused => self.player.mpv().pause(false).await?,
                }
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
            Cmd::Equalizer { enabled, preset, bands } => {
                // Частичная команда сливается с текущим состоянием:
                // опущенные поля остаются как были (см. serde-контракт
                // `Cmd::Equalizer` в ядре).
                let mut eq = self.player.equalizer().await;
                if let Some(v) = enabled {
                    eq.enabled = v;
                }
                if let Some(name) = preset {
                    match eq_presets().iter().find(|(n, _)| *n == name) {
                        // Пресет подставляет и имя, и табличные полосы:
                        // держать полосы без имени значило бы терять
                        // происхождение настройки при следующем рестарте.
                        Some((matched, table)) => {
                            eq.preset = (*matched).to_owned();
                            eq.bands = *table;
                        }
                        None => {
                            // Не перечисляем пресеты — их двадцать, а
                            // клиенты знают их из eq_presets(). Намёк на
                            // формат дешевле и полезнее простыни имён.
                            return Err(anyhow::anyhow!(
                                "неизвестный пресет эквалайзера {name:?}; точное имя из списка пресетов (см. eq_presets)"
                            ));
                        }
                    }
                }
                if let Some(raw) = bands {
                    let out_of_range = |g: f64| {
                        !(-EQ_GAIN_LIMIT_DB..=EQ_GAIN_LIMIT_DB).contains(&g)
                    };
                    if raw.len() != 10 || raw.iter().copied().any(out_of_range) {
                        return Err(anyhow::anyhow!(
                            "bands: ожидается ровно 10 значений в диапазоне -{EQ_GAIN_LIMIT_DB}..={EQ_GAIN_LIMIT_DB} дБ"
                        ));
                    }
                    let gains: [f64; 10] = raw.try_into().expect("длина проверена выше");
                    // Точное совпадение с табличным пресетом сохраняет
                    // его имя — пользователь выбрал пресет, а не набрал
                    // свой; всё остальное честно зовётся Custom.
                    eq.preset = match eq_presets().iter().find(|(_, table)| *table == gains) {
                        Some((matched, _)) => (*matched).to_owned(),
                        None => "Custom".to_owned(),
                    };
                    eq.bands = gains;
                }
                self.player.set_equalizer(eq).await?;
                // emit_state публикует StateChanged — персист подхватит
                // громкость/эквалайзер через своё правило dirty.
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
                        // Позицию только что подтвердил `goto`, повторно
                        // искать и двигать очередь не нужно.
                        self.play_known(&track.id).await?;
                        Ok(Payload::Ack(Ack::default()))
                    }
                    None => anyhow::bail!("в очереди нет позиции {index}"),
                }
            }

            Cmd::State => Ok(Payload::State(self.player.state().await)),
            Cmd::Providers => Ok(Payload::Providers(self.providers())),
            Cmd::RefreshAuth { provider } => {
                for target in self.targets(provider.as_deref())? {
                    let id = target.id().as_str().to_owned();
                    // refresh ходит в сеть; клиент ждёт ответ, поэтому таймаут.
                    match tokio::time::timeout(std::time::Duration::from_secs(10), target.account().refresh()).await {
                        Ok(Ok(auth)) => self.emit(Event::AuthChanged { provider: id, auth }),
                        Ok(Err(err)) => tracing::debug!(provider = %id, %err, "перечитывание сессии не удалось"),
                        Err(_) => tracing::debug!(provider = %id, "перечитывание сессии не уложилось в таймаут"),
                    }
                }
                Ok(Payload::Ack(Ack::default()))
            }
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
            Cmd::LibraryTracksPage { playlist } => {
                let (tracks, next) = self.playlist_tracks_page(&playlist).await?;
                release_memory(); // тот же мегабайтный JSON продолжений, что и у LibraryTracks
                Ok(Payload::TracksPage { tracks, next })
            }
            Cmd::Liked { provider } => {
                let out = self.liked(provider.as_deref()).await?;
                release_memory();
                Ok(Payload::Tracks(out))
            }
            Cmd::Home { provider } => {
                let out = self.home(provider.as_deref()).await?;
                release_memory(); // FEmusic_home — мегабайтный JSON, та же причина, что у каталожных arm'ов выше
                Ok(Payload::Home(out))
            }

            // Оценки: список — из локального кэша, установка — через
            // оптимистичную запись и сетевой вызов провайдера
            // (см. `rate_track` в catalog.rs).
            Cmd::Rate { track, rating } => {
                self.rate_track(&track, rating).await?;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::Ratings => Ok(Payload::Ratings(self.with_cache(|c| c.ratings())?)),

            // Плейлистные мутации идут напрямую в провайдер (кэш
            // рейтингов их не касается), после успеха демон рассылает
            // `PlaylistsChanged`, а список клиенты перечитывают сами —
            // см. `playlist_*` в catalog.rs.
            Cmd::PlaylistCreate { title } => {
                let playlist = self.playlist_create(&title).await?;
                Ok(Payload::PlaylistCreated { playlist: playlist.id })
            }
            Cmd::PlaylistAdd { playlist, track } => {
                self.playlist_add(&playlist, &track).await?;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::PlaylistRemove { playlist, track } => {
                self.playlist_remove(&playlist, &track).await?;
                Ok(Payload::Ack(Ack::default()))
            }
            Cmd::PlaylistDelete { playlist } => {
                self.playlist_delete(&playlist).await?;
                Ok(Payload::Ack(Ack::default()))
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
            Cmd::CacheWarm { tracks } => {
                // Прогрев сотен треков не должен держать соединение
                // клиента: спавним фоновую задачу и сразу отвечаем Ack.
                // Прогресс клиент видит потоком CacheProgress.
                let Some(app) = self.self_arc() else {
                    anyhow::bail!("демон гасится — прогрев не стартовал");
                };
                tokio::spawn(filler::warm(app, tracks));
                Ok(Payload::Ack(Ack::default()))
            }

            Cmd::Shutdown => {
                // Транспорт увидит `Ack` и закроется сам; гасит демон
                // `main`, чтобы не рвать соединение до ответа.
                Ok(Payload::Ack(Ack::default()))
            }
        }
    }

    // ────────────────────────────────────────────────── внутреннее

    /// Однотрековый контекст: очередь заменяется единственным треком.
    ///
    /// Раньше незнакомый трек дописывался в конец очереди, и трек из
    /// поиска жил позицией ~1001 в очереди старого плейлиста — next/prev
    /// продолжали играть старый контекст. Утверждённое решение: любой
    /// поштучный запуск — это новый контекст из одного трека.
    async fn play_track(&self, id: &TrackId) -> anyhow::Result<()> {
        // `Player::state()` берёт текущий трек из очереди по индексу, и
        // без добавления в очередь `tmus play <id>` играл бы «в никуда»:
        // музыка идёт, а `status`, MPRIS и Discord показывают пустоту.
        // Замерено на стенде.
        let track = self.hydrate(std::slice::from_ref(id))?.remove(0);
        let len = self
            .player
            .with_queue(|q| {
                q.clear();
                q.append(track);
                q.goto(0);
                q.len()
            })
            .await;
        self.emit(Event::QueueChanged { len, index: Some(0) });
        self.player.resolve_and_play(id).await?;
        // События о смене трека не шлём отсюда: плеер уже дал сигнал
        // вахтёру, а тот — единственный, кто сравнивает состояние с
        // прошлым. Дубль здесь давал на один скип два `TrackChanged` и
        // два `StateChanged` (замерено в потоке `tmus events`), то есть
        // двойную перерисовку бара, причём первый кадр — без
        // длительности, которую mpv сообщает позже.
        Ok(())
    }

    /// Запуск трека, который гарантированно уже в очереди: только
    /// навигация и резолв, очередь не трогаем.
    async fn play_known(&self, id: &TrackId) -> anyhow::Result<()> {
        let index = match self.player.with_queue(|q| q.find_index(id)).await {
            Some(index) => index,
            None => anyhow::bail!("трека {id} нет в очереди"),
        };
        self.player.with_queue(|q| q.goto(index)).await;
        self.player.resolve_and_play(id).await?;
        Ok(())
    }

    /// Контекст из готового списка треков — обобщение `play_playlist`
    /// без чтения плейлиста: список приносит сам клиент.
    async fn play_context(&self, ids: Vec<TrackId>, start: usize) -> anyhow::Result<()> {
        if ids.is_empty() {
            anyhow::bail!("пустой контекст воспроизведения");
        }
        let start = start.min(ids.len() - 1);
        let current = ids[start].clone();
        let tracks = self.hydrate(&ids)?;
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
        self.play_known(&current).await
    }

    async fn play_playlist(&self, id: &PlaylistId, start: usize) -> anyhow::Result<()> {
        let mut tracks = self.playlist_tracks(id).await?;
        if tracks.is_empty() {
            anyhow::bail!("плейлист {id} пуст");
        }
        // Панель шлёт позицию из своего снапшота, а тот мог видеть
        // больше страниц, чем лежит в свежем срезе кэша: TTL протух
        // или демон перезапущен, и полное чтение взяло только батч
        // первых страниц. Догружаем до запрошенной позиции, а не
        // клампим молча в хвост среза — иначе играет «какой-то трек»,
        // а не выбранный человеком.
        while start >= tracks.len() {
            let (page, next) = self.playlist_tracks_page(id).await?;
            if page.is_empty() {
                break;
            }
            tracks.extend(page);
            if next.is_none() {
                break;
            }
        }
        if start >= tracks.len() {
            anyhow::bail!(
                "позиция {start} вне плейлиста {id}: загружено {} треков",
                tracks.len()
            );
        }
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
        // Трек уже поставлен в очередь выше: `play_track` здесь нельзя —
        // он теперь заменяет очередь однотрековым контекстом.
        self.play_known(&first).await
    }

    /// Шаг по очереди. Отдельно от `Cmd::Next`, потому что то же нужно
    /// по концу трека — и логика перехода обязана быть одна.
    ///
    /// Скип серие-поглощающий: `Player::skip` двигает очередь сразу и
    /// запускает трек после короткого покоя, поэтому `Next`/`Prev`
    /// отвечают `Ack` мгновенно, а не после резолва (~4 с на
    /// незакэшированном треке — закрывая давний пункт TODO). Ошибки
    /// запуска видны событием `StateChanged` и журналом демона.
    pub(crate) async fn step(&self, forward: bool) -> anyhow::Result<()> {
        match self.player.skip(forward).await {
            Some(_) => Ok(()),
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
pub(crate) fn release_memory() {
    // SAFETY: malloc_trim не принимает указателей и не имеет
    // предусловий; аргумент — сколько байт оставить в запасе на вершине
    // кучи, 0 значит «вернуть всё, что можно».
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tmus_core::model::{AuthStatus, Playlist, PlaylistId, ProviderId, Rating};
    use tmus_provider::{Catalog, Provider, ProviderError, Resolver};

    /// Фейковый провайдер: резолв отдаёт локальный WAV (mpv играет его
    /// без сети), rate ведёт себя по флагу — так проверяются и успех, и
    /// сетевой сбой с откатом.
    struct FakeProvider {
        id: ProviderId,
        audio: PathBuf,
        fail_rate: AtomicBool,
        /// Сбой плейлистных операций: отдельный флаг, чтобы не
        /// пересекаться с проверками rate.
        fail_playlist: AtomicBool,
        /// Журнал плейлистных вызовов: тесты проверяют не только ответ,
        /// но и что вызов дошёл до провайдера с правильными аргументами.
        playlist_calls: std::sync::Mutex<Vec<String>>,
        /// Постраничные данные состава: страница по индексу курсора.
        pages: std::sync::Mutex<Vec<Vec<Track>>>,
    }

    impl FakeProvider {
        fn set_pages(&self, pages: Vec<Vec<Track>>) {
            *self.pages.lock().expect("pages") = pages;
        }
        fn record(&self, call: &str) {
            self.playlist_calls.lock().expect("calls").push(call.to_owned());
        }
        fn calls(&self) -> Vec<String> {
            self.playlist_calls.lock().expect("calls").clone()
        }
        fn playlist_error(&self) -> ProviderError {
            ProviderError::Format {
                provider: self.id,
                reason: "тестовый сбой сети".to_owned(),
            }
        }
    }

    #[async_trait::async_trait]
    impl tmus_provider::Account for FakeProvider {
        fn provider(&self) -> ProviderId {
            self.id
        }
        fn display_name(&self) -> &str {
            "Фейк"
        }
        fn auth(&self) -> AuthStatus {
            AuthStatus::Ready
        }
        async fn refresh(&self) -> Result<AuthStatus, ProviderError> {
            Ok(AuthStatus::Ready)
        }
    }

    #[async_trait::async_trait]
    impl Catalog for FakeProvider {
        fn provider(&self) -> ProviderId {
            self.id
        }
        async fn search(
            &self,
            _query: &str,
            _kind: tmus_core::model::SearchKind,
        ) -> Result<Vec<tmus_core::model::SearchResult>, ProviderError> {
            Ok(Vec::new())
        }
        async fn playlists(&self) -> Result<Vec<tmus_core::model::Playlist>, ProviderError> {
            Ok(Vec::new())
        }
        async fn playlist_tracks(
            &self,
            _playlist: &tmus_core::model::PlaylistId,
        ) -> Result<Vec<Track>, ProviderError> {
            Ok(Vec::new())
        }
        /// Курсор — индекс страницы строкой: токен "3" открывает
        /// `pages[3]`, None — `pages[0]`. `next` указывает на следующую
        /// страницу или отсутствует на последней.
        async fn playlist_tracks_page(
            &self,
            _playlist: &tmus_core::model::PlaylistId,
            cursor: Option<&str>,
        ) -> Result<tmus_provider::TrackPage, ProviderError> {
            let idx: usize = match cursor {
                None => 0,
                Some(token) => token.parse().map_err(|_| ProviderError::Format {
                    provider: self.id,
                    reason: format!("плохой курсор {token}"),
                })?,
            };
            let pages = self.pages.lock().expect("pages");
            let tracks = pages.get(idx).cloned().unwrap_or_default();
            let next = (idx + 1 < pages.len()).then(|| (idx + 1).to_string());
            Ok(tmus_provider::TrackPage { tracks, next })
        }
        async fn liked(&self) -> Result<Vec<Track>, ProviderError> {
            Ok(Vec::new())
        }
        async fn rate(&self, _id: &TrackId, _rating: Rating) -> Result<(), ProviderError> {
            if self.fail_rate.load(Ordering::SeqCst) {
                Err(ProviderError::Format {
                    provider: self.id,
                    reason: "тестовый сбой сети".to_owned(),
                })
            } else {
                Ok(())
            }
        }
        async fn playlist_create(&self, title: &str) -> Result<Playlist, ProviderError> {
            if self.fail_playlist.load(Ordering::SeqCst) {
                return Err(self.playlist_error());
            }
            self.record(&format!("create:{title}"));
            Ok(Playlist {
                id: PlaylistId::new(self.id, "PLnew"),
                title: title.to_owned(),
                subtitle: None,
                art_url: None,
                track_count: Some(0),
            })
        }
        async fn playlist_add(
            &self,
            playlist: &PlaylistId,
            track: &TrackId,
        ) -> Result<(), ProviderError> {
            if self.fail_playlist.load(Ordering::SeqCst) {
                return Err(self.playlist_error());
            }
            self.record(&format!("add:{playlist}:{track}"));
            Ok(())
        }
        async fn playlist_remove(
            &self,
            playlist: &PlaylistId,
            track: &TrackId,
        ) -> Result<(), ProviderError> {
            if self.fail_playlist.load(Ordering::SeqCst) {
                return Err(self.playlist_error());
            }
            self.record(&format!("remove:{playlist}:{track}"));
            Ok(())
        }
        async fn playlist_delete(&self, playlist: &PlaylistId) -> Result<(), ProviderError> {
            if self.fail_playlist.load(Ordering::SeqCst) {
                return Err(self.playlist_error());
            }
            self.record(&format!("delete:{playlist}"));
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Resolver for FakeProvider {
        fn provider(&self) -> ProviderId {
            self.id
        }
        async fn resolve(
            &self,
            _track: &TrackId,
        ) -> Result<tmus_core::model::StreamSource, ProviderError> {
            Ok(tmus_core::model::StreamSource::Local(self.audio.clone()))
        }
    }

    impl Provider for FakeProvider {
        fn account(&self) -> &dyn tmus_provider::Account {
            self
        }
        fn catalog(&self) -> &dyn Catalog {
            self
        }
        fn resolver(&self) -> &dyn Resolver {
            self
        }
    }

    /// Тишина в WAV-контейнере: настоящий playable-файл, чтобы mpv
    /// честно перешёл в Playing, но без слышимого звука.
    fn write_silence_wav(path: PathBuf) -> PathBuf {
        const RATE: u32 = 8000;
        const SECS: u32 = 1;
        let samples = (RATE * SECS) as usize;
        let data_len = (samples * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1_u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&RATE.to_le_bytes());
        bytes.extend_from_slice(&(RATE * 2).to_le_bytes()); // байт/с: mono 16-bit
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize(44 + data_len as usize, 0);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, bytes).expect("write wav");
        path
    }

    fn sample_track(id: &TrackId) -> Track {
        Track {
            id: id.clone(),
            title: id.id.clone(),
            artists: vec!["Артист".to_owned()],
            album: None,
            duration: None,
            art_url: None,
            page_url: None,
        }
    }

    /// Полный `App` с живым mpv (headless, сокет во временном каталоге)
    /// и одним фейковым провайдером.
    async fn app(fail_rate: bool) -> (Arc<App>, tempfile::TempDir) {
        let (app, _provider, dir) = app_full(fail_rate, false).await;
        (app, dir)
    }

    /// Вариант стенда с доступом к типизированному фейку: тесты читают
    /// журнал плейлистных вызовов, не даункастя `dyn Provider`.
    async fn app_full(
        fail_rate: bool,
        fail_playlist: bool,
    ) -> (Arc<App>, Arc<FakeProvider>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::under(dir.path());
        // mpv создаёт IPC-сокет сам, но каталог run/ должен уже быть.
        paths.ensure_dirs().expect("dirs");
        let audio = write_silence_wav(dir.path().join("audio/silence.wav"));
        let mut registry = Registry::new();
        let provider = Arc::new(FakeProvider {
            id: ProviderId::YTMUSIC,
            audio,
            fail_rate: AtomicBool::new(fail_rate),
            fail_playlist: AtomicBool::new(fail_playlist),
            playlist_calls: std::sync::Mutex::new(Vec::new()),
            pages: std::sync::Mutex::new(Vec::new()),
        });
        registry.insert(Arc::clone(&provider) as Arc<dyn Provider>);
        let gate = Arc::new(tokio::sync::Semaphore::new(2));
        let player = Player::new(
            registry.clone(),
            std::path::PathBuf::from("mpv"),
            &paths,
            0.0,
            gate.clone(),
        )
        .await
        .expect("mpv запустился");
        let cache = Arc::new(std::sync::Mutex::new(
            Cache::open(&paths, u64::MAX).expect("cache"),
        ));
        let app = App::new(player, registry, cache, Config::default(), paths, gate);
        (app, provider, dir)
    }

    /// Успешная оценка: пишется в кэш, вещается событие и видна в
    /// `Cmd::Ratings` — весь путь панели.
    #[tokio::test]
    async fn liked_rating_is_cached_broadcast_and_listed() {
        let (app, _dir) = app(false).await;
        let track = TrackId::new(ProviderId::YTMUSIC, "vid-1");
        let mut events = app.subscribe();

        app.handle(Cmd::Rate { track: track.clone(), rating: Rating::Liked })
            .await
            .expect("rate");

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("событие пришло")
            .expect("шина жива");
        assert!(
            matches!(&event, Event::RatingChanged { track: t, rating: Rating::Liked } if t == &track),
            "неожиданное событие: {event:?}"
        );
        assert_eq!(
            app.with_cache(|c| c.get_rating(&track)).expect("get"),
            Some(Rating::Liked)
        );

        match app.handle(Cmd::Ratings).await.expect("ratings") {
            Payload::Ratings(list) => assert_eq!(list, vec![(track, Rating::Liked)]),
            other => panic!("неожиданный ответ: {other:?}"),
        }
    }

    /// Дизлайк играющего трека скипает его немедленно.
    #[tokio::test]
    async fn disliked_playing_track_is_skipped() {
        let (app, _dir) = app(false).await;
        let first = TrackId::new(ProviderId::YTMUSIC, "vid-1");
        let second = TrackId::new(ProviderId::YTMUSIC, "vid-2");
        app.player
            .with_queue(|q| {
                q.append(sample_track(&first));
                q.append(sample_track(&second));
            })
            .await;
        app.play_known(&first).await.expect("play");
        assert_eq!(
            app.player.state().await.track.map(|t| t.id),
            Some(first.clone()),
            "первый трек должен играть"
        );

        app.handle(Cmd::Rate {
            track: first.clone(),
            rating: Rating::Disliked,
        })
        .await
        .expect("rate");

        // Скип асинхронный (короткий покой перед запуском следующего):
        // ждём смены играющего трека, а не мгновенного состояния.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if app.player.state().await.track.as_ref().map(|t| &t.id) == Some(&second) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "дизлайк играющего трека не скипнул его"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Ошибка провайдера откатывает оптимистичную запись: и прошлое
    /// значение возвращается, и событие не разлетается.
    #[tokio::test]
    async fn provider_error_rolls_back_local_rating() {
        let (app, _dir) = app(true).await;
        let track = TrackId::new(ProviderId::YTMUSIC, "vid-1");
        let fresh = TrackId::new(ProviderId::YTMUSIC, "vid-2");
        app.with_cache(|c| c.set_rating(&track, Rating::Liked))
            .expect("seed");
        let mut events = app.subscribe();

        let result = app
            .handle(Cmd::Rate { track: track.clone(), rating: Rating::Disliked })
            .await;
        assert!(result.is_err(), "ошибка провайдера обязана дойти наружу");
        assert_eq!(
            app.with_cache(|c| c.get_rating(&track)).expect("get"),
            Some(Rating::Liked),
            "прошлая оценка вернулась после отката"
        );

        // Откат из состояния «оценки не было» не оставляет строки.
        let result = app
            .handle(Cmd::Rate { track: fresh.clone(), rating: Rating::Liked })
            .await;
        assert!(result.is_err());
        assert_eq!(app.with_cache(|c| c.get_rating(&fresh)).expect("get"), None);

        assert!(
            events.try_recv().is_err(),
            "при ошибке сети RatingChanged вещаться не должен"
        );
    }

    /// Успешное создание: провайдер получил заголовок, клиент — id
    /// нового плейлиста, шина — сигнал перечитать список.
    #[tokio::test]
    async fn playlist_create_returns_id_and_broadcasts() {
        let (app, provider, _dir) = app_full(false, false).await;
        let mut events = app.subscribe();

        let payload = app
            .handle(Cmd::PlaylistCreate { title: "Chill".into() })
            .await
            .expect("create");
        match payload {
            Payload::PlaylistCreated { playlist } => {
                assert_eq!(playlist, PlaylistId::new(ProviderId::YTMUSIC, "PLnew"));
            }
            other => panic!("неожиданный ответ: {other:?}"),
        }
        assert_eq!(provider.calls(), vec!["create:Chill".to_owned()]);

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("событие пришло")
            .expect("шина жива");
        assert!(
            matches!(event, Event::PlaylistsChanged),
            "неожиданное событие: {event:?}"
        );
    }

    /// Add/remove/delete проходят в провайдер с правильными аргументами
    /// и каждый успех сопровождается сигналом.
    #[tokio::test]
    async fn playlist_mutations_reach_provider_and_broadcast() {
        let (app, provider, _dir) = app_full(false, false).await;
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "PL1");
        let track = TrackId::new(ProviderId::YTMUSIC, "vid-1");
        let mut events = app.subscribe();

        app.handle(Cmd::PlaylistAdd { playlist: playlist.clone(), track: track.clone() })
            .await
            .expect("add");
        app.handle(Cmd::PlaylistRemove { playlist: playlist.clone(), track: track.clone() })
            .await
            .expect("remove");
        app.handle(Cmd::PlaylistDelete { playlist: playlist.clone() })
            .await
            .expect("delete");

        assert_eq!(
            provider.calls(),
            vec![
                format!("add:{playlist}:{track}"),
                format!("remove:{playlist}:{track}"),
                format!("delete:{playlist}"),
            ]
        );
        // Ровно три сигнала — по одному на каждую мутацию, без лишних.
        for _ in 0..3 {
            let event = events.try_recv().expect("событие в шине");
            assert!(matches!(event, Event::PlaylistsChanged));
        }
        assert!(events.try_recv().is_err(), "лишних событий быть не должно");
    }

    /// Ошибка провайдера доходит наружу как обычная ошибка команды,
    /// вызова-ответа `PlaylistCreated` нет, и сигнал не разлетается.
    #[tokio::test]
    async fn playlist_provider_error_yields_err_without_event() {
        let (app, provider, _dir) = app_full(false, true).await;
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "PL1");
        let track = TrackId::new(ProviderId::YTMUSIC, "vid-1");
        let mut events = app.subscribe();

        assert!(
            app.handle(Cmd::PlaylistCreate { title: "Chill".into() })
                .await
                .is_err(),
            "ошибка создания обязана дойти наружу"
        );
        assert!(
            app.handle(Cmd::PlaylistAdd { playlist: playlist.clone(), track: track.clone() })
                .await
                .is_err()
        );
        assert!(app.handle(Cmd::PlaylistDelete { playlist: playlist.clone() }).await.is_err());

        assert!(
            provider.calls().is_empty(),
            "при сбое журнал вызовов обязан остаться пустым"
        );
        assert!(
            events.try_recv().is_err(),
            "при ошибке провайдера PlaylistsChanged вещаться не должен"
        );
    }

    /// Поштучный запуск — новый контекст из одного трека: очередь
    /// старого плейлиста обязана уйти, а не дописаться хвостом.
    #[tokio::test]
    async fn play_track_replaces_queue_with_single_context() {
        let (app, _dir) = app(false).await;
        // Два трека старого контекста — как очередь после плейлиста.
        app.player
            .with_queue(|q| {
                q.append(sample_track(&TrackId::new(ProviderId::YTMUSIC, "old-1")));
                q.append(sample_track(&TrackId::new(ProviderId::YTMUSIC, "old-2")));
            })
            .await;
        let outsider = TrackId::new(ProviderId::YTMUSIC, "vid-out");

        app.handle(Cmd::PlayTrack { track: outsider.clone() }).await.expect("play");

        let state = app.player.state().await;
        assert_eq!(state.queue_len, 1, "очередь обязана замениться одним треком");
        assert_eq!(state.queue_index, Some(0));
        assert_eq!(state.track.map(|t| t.id), Some(outsider));
    }

    /// PlayContext: готовый список становится очередью, старт — с
    /// указанного индекса.
    #[tokio::test]
    async fn play_context_builds_queue_and_starts_at_index() {
        let (app, _dir) = app(false).await;
        let ids: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|s| TrackId::new(ProviderId::YTMUSIC, *s))
            .collect();

        app.handle(Cmd::PlayContext { tracks: ids.clone(), start: 1 })
            .await
            .expect("context");

        let state = app.player.state().await;
        assert_eq!(state.queue_len, 3);
        assert_eq!(state.queue_index, Some(1));
        assert_eq!(state.track.map(|t| t.id), Some(ids[1].clone()));
    }

    /// QueueGoto после PlayContext двигает курсор, не трогая очередь.
    #[tokio::test]
    async fn queue_goto_after_play_context_moves_cursor_only() {
        let (app, _dir) = app(false).await;
        let ids: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|s| TrackId::new(ProviderId::YTMUSIC, *s))
            .collect();
        app.handle(Cmd::PlayContext { tracks: ids, start: 0 }).await.expect("context");

        app.handle(Cmd::QueueGoto { index: 2 }).await.expect("goto");

        let state = app.player.state().await;
        assert_eq!(state.queue_len, 3, "goto не должен менять размер очереди");
        assert_eq!(state.queue_index, Some(2));
        assert_eq!(state.track.map(|t| t.id), Some(TrackId::new(ProviderId::YTMUSIC, "c")));
    }

    /// Пустой контекст — ошибка команды, очередь не трогается.
    #[tokio::test]
    async fn play_context_with_empty_list_is_err() {
        let (app, _dir) = app(false).await;
        assert!(
            app.handle(Cmd::PlayContext { tracks: Vec::new(), start: 0 })
                .await
                .is_err()
        );
    }

    /// Страница из `n` треков с последовательными id "v0".."v{n-1}".
    fn page_of(base: usize, n: usize) -> Vec<Track> {
        (base..base + n)
            .map(|i| sample_track(&TrackId::new(ProviderId::YTMUSIC, format!("v{i}"))))
            .collect()
    }

    /// Полное чтение после протухания кладёт в кэш ВСЕ страницы, а курсор
    /// остаётся на токене после последней взятой страницы: батч-потолок
    /// не съедает хвост плейлиста — он остаётся доступен догрузке.
    #[tokio::test]
    async fn stale_playlist_tracks_reads_all_pages_and_keeps_cursor() {
        let (app, provider, _dir) = app_full(false, false).await;
        // 11 страниц по треку: PLAYLIST_BATCH_PAGES=10 заберёт первые
        // десять, курсор обязан запомниться на "10".
        provider.set_pages((0..11).map(|i| page_of(i, 1)).collect());
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "LM");

        let out = app.playlist_tracks(&playlist).await.expect("full read");
        assert_eq!(out.len(), 10, "батч обязан взять первые десять страниц");
        assert_eq!(
            app.with_cache(|c| c.playlist_tracks(&playlist)).expect("cached").len(),
            10
        );
        assert_eq!(
            app.with_cache(|c| c.playlist_continuation(playlist.provider.as_str(), &playlist))
                .expect("cursor"),
            Some("10".to_owned()),
            "токен после последней взятой страницы обязан сохраниться"
        );

        // Догрузка добирает хвост: позиции продолжаются, затем
        // исчерпание даёт пустую страницу с next=None.
        let (first, next) = app
            .playlist_tracks_page(&playlist)
            .await
            .expect("page 10");
        assert_eq!(first.len(), 1);
        assert_eq!(next, None);
        let cached = app
            .with_cache(|c| c.playlist_tracks(&playlist))
            .expect("cached");
        assert_eq!(cached.len(), 11);
        assert_eq!(cached[10].id.id, "v10", "догруженная страница встала в конец");
        assert_eq!(
            app.with_cache(|c| c.playlist_continuation(playlist.provider.as_str(), &playlist))
                .expect("cursor"),
            None,
            "после исчерпания курсор обязан быть стёрт"
        );
    }

    /// Выбор позиции глубже свежего среза обязан догрузиться до неё, а
    /// не играть хвост батча: панель шлёт `--start` из снапшота, который
    /// пережил TTL кэша. Раньше `start.min(len-1)` молча клампил в
    /// последний трек батча — человек выбирал трек на 11-й странице, а
    /// играл последний из первых десяти.
    #[tokio::test]
    async fn play_playlist_fetches_pages_until_requested_start() {
        let (app, provider, _dir) = app_full(false, false).await;
        provider.set_pages((0..11).map(|i| page_of(i, 1)).collect());
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "LM");

        // Срез протух: полное чтение берёт батч 10 страниц, курсор "10".
        let slice = app.playlist_tracks(&playlist).await.expect("full read");
        assert_eq!(slice.len(), 10, "свежий срез — только батч");

        app.handle(Cmd::PlayPlaylist { playlist: playlist.clone(), start: Some(10) })
            .await
            .expect("play at 10");

        let state = app.player.state().await;
        assert_eq!(
            state.track.map(|t| t.id),
            Some(TrackId::new(ProviderId::YTMUSIC, "v10")),
            "играет выбранный трек одиннадцатой страницы, а не хвост батча"
        );
        assert_eq!(state.queue_len, 11, "догруженный хвост обязан попасть в очередь");
        assert_eq!(state.queue_index, Some(10));
    }

    /// Позиция за пределами реального состава — честная ошибка, а не
    /// тихий сдвиг на последний трек.
    #[tokio::test]
    async fn play_playlist_rejects_start_beyond_listing() {
        let (app, provider, _dir) = app_full(false, false).await;
        provider.set_pages((0..3).map(|i| page_of(i, 1)).collect());
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "LM");

        let err = app
            .handle(Cmd::PlayPlaylist { playlist: playlist.clone(), start: Some(50) })
            .await
            .expect_err("позиция вне состава — отказ");
        assert!(err.to_string().contains("вне плейлиста"), "неожиданная ошибка: {err}");
    }

    /// Полное чтение с исчерпанием фиксирует «дочитано»: курсор None.
    #[tokio::test]
    async fn full_read_to_exhaustion_clears_cursor() {
        let (app, provider, _dir) = app_full(false, false).await;
        provider.set_pages(vec![page_of(0, 2), page_of(2, 1)]);
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "PL1");

        let out = app.playlist_tracks(&playlist).await.expect("full read");
        assert_eq!(out.len(), 3);
        assert_eq!(
            app.with_cache(|c| c.playlist_continuation(playlist.provider.as_str(), &playlist))
                .expect("cursor"),
            None,
            "исчерпанный плейлист обязан быть помечен дочитанным"
        );
    }

    /// LibraryTracksPage без курсора — пустая страница (панель
    /// прекратит догрузку).
    #[tokio::test]
    async fn library_tracks_page_without_cursor_is_empty() {
        let (app, _provider, _dir) = app_full(false, false).await;
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "PL1");
        match app.handle(Cmd::LibraryTracksPage { playlist }).await.expect("page") {
            Payload::TracksPage { tracks, next } => {
                assert!(tracks.is_empty());
                assert_eq!(next, None);
            }
            other => panic!("неожиданный ответ: {other:?}"),
        }
    }

    /// Полный путь догрузки через IPC: страницы аппендятся с
    /// продолжающимися позициями, курсор движется, исчерпание даёт
    /// пустую страницу.
    #[tokio::test]
    async fn library_tracks_page_appends_and_advances_cursor() {
        let (app, provider, _dir) = app_full(false, false).await;
        // 12 страниц: полное чтение берёт десять, дальше — три догрузки.
        provider.set_pages((0..12).map(|i| page_of(i, 1)).collect());
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "LM");

        app.handle(Cmd::LibraryTracks { playlist: playlist.clone() })
            .await
            .expect("full read");

        let payload = app
            .handle(Cmd::LibraryTracksPage { playlist: playlist.clone() })
            .await
            .expect("page");
        match payload {
            Payload::TracksPage { tracks, next } => {
                assert_eq!(tracks.len(), 1);
                assert_eq!(tracks[0].id.id, "v10");
                assert_eq!(next, Some("11".to_owned()));
            }
            other => panic!("неожиданный ответ: {other:?}"),
        }
        let payload = app
            .handle(Cmd::LibraryTracksPage { playlist: playlist.clone() })
            .await
            .expect("page");
        match payload {
            Payload::TracksPage { tracks, next } => {
                assert_eq!(tracks[0].id.id, "v11");
                assert_eq!(next, None);
            }
            other => panic!("неожиданный ответ: {other:?}"),
        }

        let cached = app
            .with_cache(|c| c.playlist_tracks(&playlist))
            .expect("cached");
        assert_eq!(cached.len(), 12, "догруженные страницы обязаны попасть в кэш");
        let ids: Vec<_> = cached.iter().map(|t| t.id.id.clone()).collect();
        let expected: Vec<_> = (0..12).map(|i| format!("v{i}")).collect();
        assert_eq!(ids, expected, "позиции страниц обязаны продолжиться по порядку");

        // Повторный вызов после исчерпания — пусто, без похода в сеть.
        let payload = app
            .handle(Cmd::LibraryTracksPage { playlist })
            .await
            .expect("page");
        match payload {
            Payload::TracksPage { tracks, next } => {
                assert!(tracks.is_empty());
                assert_eq!(next, None);
            }
            other => panic!("неожиданный ответ: {other:?}"),
        }
    }

    /// forget_playlist_sync затирает курсор вместе со строкой sync:
    /// после него догрузка обязана считаться «дочитанной» (пусто, None).
    #[tokio::test]
    async fn forget_playlist_sync_clears_cursor() {
        let (app, provider, _dir) = app_full(false, false).await;
        provider.set_pages((0..11).map(|i| page_of(i, 1)).collect());
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "LM");
        app.playlist_tracks(&playlist).await.expect("full read");
        assert!(
            app.with_cache(|c| c.playlist_continuation(playlist.provider.as_str(), &playlist))
                .expect("cursor")
                .is_some(),
            "перед forget курсор обязан существовать"
        );

        app.with_cache(|c| c.forget_playlist_sync(&playlist))
            .expect("forget");

        assert_eq!(
            app.with_cache(|c| c.playlist_continuation(playlist.provider.as_str(), &playlist))
                .expect("cursor"),
            None,
            "курсор обязан сброситься вместе со строкой sync"
        );
        let (tracks, next) = app
            .playlist_tracks_page(&playlist)
            .await
            .expect("page");
        assert!(tracks.is_empty());
        assert_eq!(next, None);
    }
}
