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
    /// Счётчик команд воспроизведения: каждая новая `resolve_and_play`
    /// забирает номер, и команда с неактуальным номером отменяется.
    /// Без него 8 параллельных `tmus next` = 8 одновременных yt-dlp,
    /// 676 МБ RSS и 52.8 CPU-с за 25 с (~2.1 ядра).
    play_gen: std::sync::atomic::AtomicU64,
    /// Сериализатор запусков yt-dlp: один резолв — 0.9 CPU-с и 335 МБ,
    /// поэтому параллелить их бессмысленно, ждущие проверяют gen и выходят.
    resolve_gate: tokio::sync::Semaphore,
    /// Хендл живой предзагрузки: при новой команде её надо прервать
    /// до того, как она запустит ещё один yt-dlp.
    preload_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Пинг «состояние изменилось по событию mpv». Демон обязан узнать
    /// о смене трека и о приехавшей длительности сразу, а не следующим
    /// тиком опроса: замерено, бар до секунды рисовал `--:--` и
    /// перерисовывался дважды на один скип.
    ///
    /// Будим через `notify_one`, а не `notify_waiters`: у второго сигнал
    /// пропадает, если вахтёр в этот момент разбирает предыдущий, а
    /// `notify_one` оставляет разрешение до следующего `notified()`.
    /// Потребитель ровно один — вахтёр состояния демона.
    changed: tokio::sync::Notify,
    /// Идёт переход на следующий трек. mpv между файлами уходит в
    /// `idle` (а при незакэшированном следующем — на все секунды
    /// резолва), и честный `Stopped` в этот момент виден человеку
    /// вспышкой «■ остановлено» в баре.
    advancing: std::sync::atomic::AtomicBool,
}

/// Задержка перед предзагрузкой следующего трека: серия быстрых скипов
/// должна успевать отменить задачу ДО того, как она потянет yt-dlp.
const PRELOAD_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

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
                play_gen: std::sync::atomic::AtomicU64::new(0),
                resolve_gate: tokio::sync::Semaphore::new(1),
                preload_task: tokio::sync::Mutex::new(None),
                changed: tokio::sync::Notify::new(),
                advancing: std::sync::atomic::AtomicBool::new(false),
            }),
        };
        tokio::spawn(Player::clone(&player).run(events));
        Ok(player)
    }

    /// Зарезолвить трек и играть его. Существующий `StreamSource`
    /// (из предзагрузки) переиспользуется, если он ещё не истёк.
    pub async fn resolve_and_play(&self, track_id: &TrackId) -> Result<(), PlayerError> {
        let my_gen = self
            .inner
            .play_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        // Новая команда отменяет живую предзагрузку: иначе она закончится
        // ещё одним yt-dlp для трека, который уже не будет следующим.
        if let Some(handle) = self.inner.preload_task.lock().await.take() {
            handle.abort();
        }

        let source = {
            let mut preloaded = self.inner.preloaded.lock().await;
            match preloaded.as_ref() {
                Some((id, _)) if id == track_id => {
                    // Слот расходуется при первом использовании: take, а не
                    // клон — иначе источник зависал бы до смены трека.
                    preloaded.take().map(|(_, s)| s)
                }
                _ => None,
            }
        };

        let source = match source {
            // Годная предзагрузка — играем сразу, без сети.
            Some(source) if !source.is_expired(SystemTime::now()) => source,
            // У googlevideo-ссылок `expire=` около 6 часов: трек, добавленный
            // в очередь давно, иначе отдал бы 403 при проигрывании.
            // Один permit: yt-dlp — 0.9 CPU-с и 335 МБ, параллелить некому.
            _ => {
                let permit = self
                    .inner
                    .resolve_gate
                    .acquire()
                    .await
                    .expect("семафор резолва живёт столько же, сколько плеер");
                if self.inner.play_gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                    // Пока ждали очереди, человек ушёл дальше — yt-dlp вообще
                    // не запускаем, это и есть устранение шторма процессов.
                    return Ok(());
                }
                let source = self.resolve(track_id).await?;
                drop(permit);
                source
            }
        };

        // Последняя проверка перед mpv.load: устаревшая команда не должна
        // перебить свежий трек. Status/position/duration тоже выставляем
        // только здесь — раньше они портили бы состояние живой команды.
        if self.inner.play_gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
            return Ok(());
        }
        *self.inner.status.lock().await = PlaybackStatus::Playing;
        *self.inner.paused.lock().await = false;
        *self.inner.position.lock().await = None;
        *self.inner.duration.lock().await = None;
        self.inner.mpv.load(&source).await?;
        // Паузу обязаны снять явно: `pause` у mpv глобальный и
        // `loadfile` его не сбрасывает. Без этого пауза (панель,
        // медиа-клавиша, `Space` в TUI) плюс любая смена трека давали
        // тишину при `status = Playing` — замерено: `tmus pause` +
        // `tmus next` → mpv `pause=true`, `time-pos=0.001`, а демон
        // отвечал `[Playing] 0:00`. Команда воспроизведения означает
        // играть, а не «загрузить и молчать».
        self.inner.mpv.pause(false).await?;

        let next = {
            let queue = self.inner.queue.lock().await;
            queue.peek_next().map(|t| t.id.clone())
        };
        *self.inner.current.lock().await = Some(Current {
            track_id: track_id.clone(),
            source,
        });
        // Трек поехал: переход закончен, а состояние изменилось —
        // будим вахтёра демона, чтобы бар узнал о смене трека сразу,
        // а не следующим тиком опроса.
        self.inner
            .advancing
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.inner.changed.notify_one();

        // Предзагрузка следующего: резолв заранее, чтобы смена трека не
        // ждала yt-dlp. Ошибку здесь не пробрасываем — она проявится
        // законным путём при переходе, а текущий трек не её виновник.
        if let Some(next_id) = next {
            let this = self.clone();
            let base_gen = self.inner.play_gen.load(std::sync::atomic::Ordering::SeqCst);
            let handle = tokio::spawn(async move {
                // Задержка, затем двойной gen-чек: серия скипов отменяет
                // задачу до запуска yt-dlp; досидевшие до permit — те, кто
                // ещё актуален на момент взятия семафора.
                tokio::time::sleep(PRELOAD_DELAY).await;
                if this.inner.play_gen.load(std::sync::atomic::Ordering::SeqCst) != base_gen {
                    return;
                }
                let permit = this
                    .inner
                    .resolve_gate
                    .acquire()
                    .await
                    .expect("семафор резолва живёт столько же, сколько плеер");
                if this.inner.play_gen.load(std::sync::atomic::Ordering::SeqCst) != base_gen {
                    return;
                }
                let result = this.resolve(&next_id).await;
                drop(permit);
                // Кладём результат только если команда всё ещё свежая:
                // иначе перезатрём предзагрузку уже сыгранного трека.
                if this.inner.play_gen.load(std::sync::atomic::Ordering::SeqCst) == base_gen {
                    match result {
                        Ok(source) => *this.inner.preloaded.lock().await = Some((next_id, source)),
                        Err(e) => tracing::debug!(track = %next_id, error = %e, "предзагрузка следующего трека не удалась"),
                    }
                }
            });
            *self.inner.preload_task.lock().await = Some(handle);
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

    /// Ждать изменения состояния, замеченного по событию mpv.
    ///
    /// Нужен вахтёру демона: приёмник событий mpv забирает `Player`, и
    /// второго читателя у `mpsc` быть не может, а узнавать о смене
    /// трека и о приехавшей длительности через секундный опрос — это
    /// замеренные до секунды `--:--` в баре и двойная перерисовка.
    pub async fn changed(&self) {
        self.inner.changed.notified().await;
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
                    // Длительность приезжает через десятки миллисекунд
                    // после загрузки — до этого бар рисует `--:--`.
                    self.inner.changed.notify_one();
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
                    self.inner.changed.notify_one();
                }
                MpvEvent::EndOfFile => self.on_track_end().await,
                MpvEvent::Idle => {
                    // `idle` после stop или неудачной загрузки — это
                    // остановка, а конец трека несёт `end-file`
                    // с `reason=eof`. Но между треками mpv проходит
                    // через idle тоже, и это событие разбирается позже,
                    // чем случилось: обработка конца трека ждёт резолв и
                    // загрузку, а стоящий за ней в очереди `idle`
                    // относится уже к прошлому файлу. Поэтому два
                    // условия: не идёт переход И mpv действительно пуст
                    // сейчас. Без них бар мигал «■ остановлено» на
                    // каждом переключении и залипал в «stopped» при
                    // играющем треке — замерено в потоке `tmus events`.
                    let advancing =
                        self.inner.advancing.load(std::sync::atomic::Ordering::SeqCst);
                    let idle_now = self.inner.mpv.idle_active().await.unwrap_or(true);
                    if !advancing && idle_now {
                        *self.inner.status.lock().await = PlaybackStatus::Stopped;
                        self.inner.changed.notify_one();
                    }
                }
                MpvEvent::Restarted | MpvEvent::GaveUp { .. } => {
                    // Процесс умер вместе с воспроизведением; человек
                    // обязан это увидеть, а не слушать тишину.
                    *self.inner.status.lock().await = PlaybackStatus::Stopped;
                    *self.inner.position.lock().await = None;
                    self.inner
                        .advancing
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    self.inner.changed.notify_one();
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
                // Окно перехода: mpv уже отпустил файл и ушёл в idle, а
                // следующий ещё резолвится. `Stopped` в этом окне —
                // вспышка «■» в баре на каждом переключении, поэтому
                // idle здесь игнорируется (снимает флаг сама загрузка).
                self.inner
                    .advancing
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                if let Err(e) = self.resolve_and_play(&id).await {
                    tracing::warn!(track = %id, error = %e, "переход к следующему треку не удался");
                    self.inner
                        .advancing
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    *self.inner.status.lock().await = PlaybackStatus::Stopped;
                    self.inner.changed.notify_one();
                }
            }
            None => {
                *self.inner.status.lock().await = PlaybackStatus::Stopped;
                *self.inner.current.lock().await = None;
                if let Err(e) = self.inner.mpv.stop().await {
                    tracing::debug!(error = %e, "stop после конца очереди не удался");
                }
                self.inner.changed.notify_one();
            }
        }
    }

    async fn resolve(&self, track_id: &TrackId) -> Result<tmus_core::model::StreamSource, PlayerError> {
        let resolver = self.inner.registry.resolver_for(track_id)?;
        Ok(resolver.resolve(track_id).await?)
    }
}
