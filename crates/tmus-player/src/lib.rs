//! Плеер: mpv-IPC + смешанная очередь + реестр провайдеров.
//!
//! Здесь не должно быть ни строчки про конкретный сервис: резолв трека
//! идёт через `Registry`, наружу от провайдера уходят только
//! `StreamSource` и `Track`.

use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::Mutex;
use tmus_core::model::{PlaybackStatus, TrackId};
use tmus_core::protocol::PlayerState;
use tmus_provider::Registry;

pub mod mpv;
pub mod queue;

pub use mpv::{Mpv, MpvError, MpvEvent};
pub use queue::Queue;

#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    #[error(transparent)]
    Provider(#[from] tmus_provider::ProviderError),
    #[error(transparent)]
    Mpv(#[from] MpvError),
}

struct Current {
    track_id: TrackId,
    source: tmus_core::model::StreamSource,
}

struct Inner {
    registry: Registry,
    queue: Mutex<Queue>,
    mpv: Mpv,
    current: Mutex<Option<Current>>,
    /// Индексы наблюдаются из событий mpv; кэшируются для `state()`.
    position: Mutex<Option<std::time::Duration>>,
    duration: Mutex<Option<std::time::Duration>>,
    paused: Mutex<bool>,
    /// Громкость зеркалируется из конфига и команд; mpv не наблюдается
    /// на ней, поэтому держим свою копию для `state()`.
    volume: Mutex<f64>,
    status: Mutex<PlaybackStatus>,
    /// Зарезолвленный следующий трек. Держится до момента его
    /// проигрывания и никогда не переживает смену текущего трека.
    preloaded: Mutex<Option<(TrackId, tmus_core::model::StreamSource)>>,
}

/// Клонируемый дескриптор плеера. `run` потребляет события mpv и ведёт
/// переходы по очереди.
#[derive(Clone)]
pub struct Player {
    inner: Arc<Inner>,
}

impl Player {
    /// Запустить mpv. `volume` — начальная громкость из конфига.
    /// `mpv_binary` приходит из конфига (`mpv = "..."`): жёсткое
    /// `"mpv"` делало бы эту настройку ложью.
    pub async fn new(
        registry: Registry,
        mpv_binary: std::path::PathBuf,
        paths: &tmus_core::paths::Paths,
        volume: f64,
    ) -> Result<Self, PlayerError> {
        let (mpv, events) = Mpv::spawn(mpv_binary, paths, volume).await?;
        mpv.observe().await?;
        let player = Self {
            inner: Arc::new(Inner {
                registry,
                queue: Mutex::new(Queue::new()),
                mpv,
                current: Mutex::new(None),
                position: Mutex::new(None),
                duration: Mutex::new(None),
                paused: Mutex::new(false),
                volume: Mutex::new(volume),
                status: Mutex::new(PlaybackStatus::Stopped),
                preloaded: Mutex::new(None),
            }),
        };
        tokio::spawn(Player::clone(&player).run(events));
        Ok(player)
    }

    /// Зарезолвить трек и играть его. Существующий `StreamSource`
    /// (из предзагрузки) переиспользуется, если он ещё не истёк.
    pub async fn resolve_and_play(&self, track_id: &TrackId) -> Result<(), PlayerError> {
        let mut source = self.take_source_for(track_id).await?;
        // У googlevideo-ссылок `expire=` около 6 часов: трек, добавленный
        // в очередь давно, иначе отдал бы 403 при проигрывании. Поэтому
        // перед запуском проверяем срок и при истечении резолвим заново.
        if source.is_expired(SystemTime::now()) {
            source = self.resolve(track_id).await?;
        }

        *self.inner.status.lock().await = PlaybackStatus::Playing;
        *self.inner.paused.lock().await = false;
        *self.inner.position.lock().await = None;
        *self.inner.duration.lock().await = None;
        self.inner.mpv.load(&source).await?;

        let next = {
            let queue = self.inner.queue.lock().await;
            queue.peek_next().map(|t| t.id.clone())
        };
        *self.inner.current.lock().await = Some(Current {
            track_id: track_id.clone(),
            source,
        });

        // Предзагрузка следующего: резолв заранее, чтобы смена трека не
        // ждала yt-dlp. Ошибку здесь не пробрасываем — она проявится
        // законным путём при переходе, а текущий трек не её виновник.
        if let Some(next_id) = next {
            let this = self.clone();
            tokio::spawn(async move {
                match this.resolve(&next_id).await {
                    Ok(source) => *this.inner.preloaded.lock().await = Some((next_id, source)),
                    Err(e) => tracing::debug!(track = %next_id, error = %e, "предзагрузка следующего трека не удалась"),
                }
            });
        }
        Ok(())
    }

    /// Текущее состояние для control-протокола.
    pub async fn state(&self) -> PlayerState {
        let queue = self.inner.queue.lock().await;
        let current = self.inner.current.lock().await;
        let index = current
            .as_ref()
            .and_then(|c| queue.find_index(&c.track_id));
        let track = index.and_then(|i| queue.track_at(i)).cloned();
        let offline = current
            .as_ref()
            .map(|c| c.source.is_local())
            .unwrap_or(false);
        PlayerState {
            status: *self.inner.status.lock().await,
            track,
            position: *self.inner.position.lock().await,
            duration: *self.inner.duration.lock().await,
            volume: *self.inner.volume.lock().await,
            loop_mode: queue.loop_mode(),
            shuffle: queue.shuffle(),
            queue_index: index,
            queue_len: queue.len(),
            offline,
        }
    }

    /// Громкость: обновить свою копию и mpv.
    pub async fn set_volume(&self, volume: f64) -> Result<(), PlayerError> {
        self.inner.mpv.set_volume(volume).await?;
        *self.inner.volume.lock().await = volume;
        Ok(())
    }

    /// Очередь — для команд демона (`queue_append`, `queue_goto`, …).
    pub async fn with_queue<R>(&self, f: impl FnOnce(&mut Queue) -> R) -> R {
        let mut queue = self.inner.queue.lock().await;
        f(&mut queue)
    }

    pub fn mpv(&self) -> &Mpv {
        &self.inner.mpv
    }

    /// Цикл обработки событий mpv: кэширует наблюдаемые свойства и ведёт
    /// переход по очереди на конец трека.
    async fn run(self, mut events: tokio::sync::mpsc::Receiver<MpvEvent>) {
        while let Some(event) = events.recv().await {
            match event {
                MpvEvent::Position { position } => {
                    *self.inner.position.lock().await = Some(position);
                }
                MpvEvent::Duration(duration) => {
                    *self.inner.duration.lock().await = Some(duration);
                }
                MpvEvent::Paused(paused) => {
                    *self.inner.paused.lock().await = paused;
                    let status = match paused {
                        true => PlaybackStatus::Paused,
                        false => {
                            // После паузы играем только если есть трек.
                            if self.inner.current.lock().await.is_some() {
                                PlaybackStatus::Playing
                            } else {
                                PlaybackStatus::Stopped
                            }
                        }
                    };
                    *self.inner.status.lock().await = status;
                }
                MpvEvent::EndOfFile => self.on_track_end().await,
                MpvEvent::Idle => {
                    // idle после stop или после неудачной загрузки — это
                    // остановка, а не конец трека (конец несёт eof-reached).
                    *self.inner.status.lock().await = PlaybackStatus::Stopped;
                }
                MpvEvent::Restarted | MpvEvent::GaveUp { .. } => {
                    // Процесс умер вместе с воспроизведением; человек
                    // обязан это увидеть, а не слушать тишину.
                    *self.inner.status.lock().await = PlaybackStatus::Stopped;
                    *self.inner.position.lock().await = None;
                }
            }
        }
    }

    async fn on_track_end(&self) {
        let next = {
            let mut queue = self.inner.queue.lock().await;
            queue.next().map(|t| t.id.clone())
        };
        *self.inner.position.lock().await = None;
        match next {
            Some(id) => {
                if let Err(e) = self.resolve_and_play(&id).await {
                    tracing::warn!(track = %id, error = %e, "переход к следующему треку не удался");
                    *self.inner.status.lock().await = PlaybackStatus::Stopped;
                }
            }
            None => {
                *self.inner.status.lock().await = PlaybackStatus::Stopped;
                *self.inner.current.lock().await = None;
                if let Err(e) = self.inner.mpv.stop().await {
                    tracing::debug!(error = %e, "stop после конца очереди не удался");
                }
            }
        }
    }

    /// Источник для трека: из предзагрузки, если она про этот трек,
    /// иначе — свежий резолв. Предзагрузка расходуется при первом
    /// использовании.
    async fn take_source_for(&self, track_id: &TrackId) -> Result<tmus_core::model::StreamSource, PlayerError> {
        let preloaded = self.inner.preloaded.lock().await;
        if let Some((id, source)) = preloaded.as_ref() {
            if id == track_id {
                return Ok(source.clone());
            }
        }
        drop(preloaded);
        self.resolve(track_id).await
    }

    async fn resolve(&self, track_id: &TrackId) -> Result<tmus_core::model::StreamSource, PlayerError> {
        let resolver = self.inner.registry.resolver_for(track_id)?;
        Ok(resolver.resolve(track_id).await?)
    }
}
