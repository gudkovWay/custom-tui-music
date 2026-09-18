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
    PlaybackStatus, PlaylistId, Track, TrackId,
};
use tmus_core::paths::Paths;
use tmus_core::protocol::{Ack, CatalogSource, Cmd, Event, Payload, QueueView};
use tmus_player::Player;
use tmus_provider::Registry;
use tokio::sync::broadcast;

use crate::catalog::resolve_catalog_source;

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
    catalog_source: std::sync::Mutex<CatalogSource>,
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
        let arc = Arc::new(Self {
            player,
            registry,
            cache,
            config,
            paths,
            events,
            catalog_source: std::sync::Mutex::new(catalog_source),
            shutdown: tokio::sync::Notify::new(),
            auth_retry: std::sync::Mutex::new(std::collections::HashMap::new()),
        });

        arc
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
                // Медиа-клавиша play/pause обязана что-то делать в любом
                // статусе: на Stopped в пустом mpv пауза — тихий нооп.
                let state = self.player.state().await;
                match state.status {
                    PlaybackStatus::Playing => self.player.mpv().pause(true).await?,
                    PlaybackStatus::Stopped => match state.track {
                        Some(track) => self.play_track(&track.id).await?,
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
    ///
    /// Скип серие-поглощающий: `Player::skip` двигает очередь сразу и
    /// запускает трек после короткого покоя, поэтому `Next`/`Prev`
    /// отвечают `Ack` мгновенно, а не после резолва (~4 с на
    /// незакэшированном треке — закрывая давний пункт TODO). Ошибки
    /// запуска видны событием `StateChanged` и журналом демона.
    async fn step(&self, forward: bool) -> anyhow::Result<()> {
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
