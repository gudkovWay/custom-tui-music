//! Плеер: mpv-IPC + смешанная очередь + реестр провайдеров.
//!
//! Здесь не должно быть ни строчки про конкретный сервис: резолв трека
//! идёт через `Registry`, наружу от провайдера уходят только
//! `StreamSource` и `Track`.

use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::Mutex;
use tmus_core::model::{EqState, EQ_FREQUENCIES_HZ, EQ_GAIN_LIMIT_DB, PlaybackStatus, TrackId};
use tmus_core::protocol::PlayerState;
use tmus_provider::{ProviderError, Registry};

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
    /// Состояние эквалайзера — зеркало последнего применённого `af`:
    /// mpv не наблюдается на фильтрах, поэтому держим свою копию и для
    /// `state()`, и для слияния частичных команд Equalizer.
    equalizer: Mutex<EqState>,
    status: Mutex<PlaybackStatus>,
    /// Причина последнего сбоя воспроизведения, отдаётся в
    /// `PlayerState.last_error`. Ставится при `PlaybackFailed`, чистится
    /// при успешном `mpv.load` — нечистая причина липла бы к чужим
    /// трекам и выглядела как сбой там, где всё играет.
    last_error: Mutex<Option<String>>,
    /// Трек, КОТОРЫЙ ЗАПРОШЕН, но ещё не загружен в mpv. `current`
    /// обновляется только после `mpv.load`, и без этого поля бар
    /// показывал бы старый трек все секунды резолва — человек нажал
    /// Space, а виджет делает вид, что ничего не было. `state()`
    /// отдаёт pending приоритетнее current; позиция/длительность на
    /// время переключения скрываются (они от старого файла).
    pending: Mutex<Option<TrackId>>,
    /// Зарезолвленный следующий трек. Держится до момента его
    /// проигрывания и никогда не переживает смены текущего трека.
    preloaded: Mutex<Option<(TrackId, tmus_core::model::StreamSource)>>,
    /// Счётчик команд воспроизведения: каждая новая `resolve_and_play`
    /// забирает номер, и команда с неактуальным номером отменяется.
    /// Без него 8 параллельных `tmus next` = 8 одновременных yt-dlp,
    /// 676 МБ RSS и 52.8 CPU-с за 25 с (~2.1 ядра).
    ///
    /// watch, а не атомик с Notify: отмена ждёт ИЗМЕНЕНИЯ номера, а у
    /// Notify событие без памяти — команда, сдвинувшая номер до
    /// регистрации официанта, не разбудила бы его, и устаревший yt-dlp
    /// молча доживал бы свои ~4 с, держа семафор резолвов. Пара
    /// sender/receiver: у `Sender` нет чтения текущего значения.
    play_gen: tokio::sync::watch::Sender<u64>,
    play_gen_rx: tokio::sync::watch::Receiver<u64>,
    /// Сериализатор запусков yt-dlp: один резолв — 0.9 CPU-с и 335 МБ,
    /// поэтому параллелить их бессмысленно, ждущие проверяют gen и выходят.
    /// Семафор внедряется снаружи и общий на процесс: филлер демона
    /// зовёт тот же резолв, и без общего гейта он обгонял плеер.
    resolve_gate: Arc<tokio::sync::Semaphore>,
    /// Хендл живой предзагрузки с её целью: прерывать надо только чужую.
    /// Своя (тот же трек) доживёт и положит результат в слот — убив её,
    /// settle скипа запускал бы второй yt-dlp на тот же трек.
    preload_task: tokio::sync::Mutex<Option<(TrackId, tokio::task::JoinHandle<()>)>>,
    /// Отложенный запуск после серии скипов: очередь двигается сразу,
    /// а yt-dlp/loadfile — только после SETTLE покоя (см. `skip`).
    ///
    /// Цель хранится рядом с хендлом по той же причине, что и у
    /// [`Self::preload_task`]: `resolve_and_play` обязан гасить только
    /// ЧУЖОЙ отложенный запуск. Без цели он гасил свой собственный —
    /// settle-задача сама зовёт `resolve_and_play`, тот брал её же хендл
    /// и делал `abort`: задача умирала на первом же ожидании, не дойдя ни
    /// до резолва, ни до mpv. Внешне это выглядело как «нажал `n`/
    /// медиа-клавишу — очередь сдвинулась, а трек не грузится» (найдено
    /// живой проверкой на стенде 21.09.2026).
    skip_settle: tokio::sync::Mutex<Option<(TrackId, tokio::task::JoinHandle<()>)>>,
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
    /// Сколько треков подряд не сыграли (`PlaybackFailed`). Защита от
    /// выжженной очереди: при общем сбое (волна 403 от 20.09) авто-переход
    /// без потолка промотал бы тысячу треков в тишину за минуты. Сбрасывается,
    /// когда трек реально заиграл (`Position` > 0 — отказ приходит уже ПОСЛЕ
    /// `loadfile`, поэтому сам факт загрузки успехом не считается) или
    /// дослушан до конца (`EndOfFile`).
    failure_streak: std::sync::atomic::AtomicU32,
}

/// Потолок авто-переходов при неудачах подряд: три перехода допустимы
/// (единичный битый файл и пара соседних — не повод вставать), четвёртая
/// неудача подряд останавливает плеер с видимой причиной. Без потолка
/// общий сбой промотал бы всю очередь в тишину.
const MAX_AUTO_SKIPS: u32 = 3;

/// Решение после отказа воспроизведения: промотать дальше или встать.
enum FailureAction {
    Advance,
    Stop,
}

fn failure_action(streak: u32) -> FailureAction {
    if streak <= MAX_AUTO_SKIPS {
        FailureAction::Advance
    } else {
        FailureAction::Stop
    }
}

/// Собрать lavfi-граф эквалайзера для mpv-фильтра `af`.
///
/// Выключенный эквалайзер — пустой `af` (см. [`Mpv::set_audio_filter`]);
/// включённый — один фильтр `lavfi` с цепочкой из десяти `equalizer`
/// (биквад-секций): нулевые полосы НЕ выбрасываются, потому что
/// пропуск менял бы индексацию в едином графе и усложнял чтение, а
/// биквад с нулевым усилением стоит копейки.
///
/// Отдельная чистая функция — единственная нетривиальная часть
/// эквалайзера, тестируемая без mpv-процесса.
pub fn eq_af_graph(eq: &EqState) -> Vec<String> {
    if !eq.enabled {
        return Vec::new();
    }
    let chain = EQ_FREQUENCIES_HZ
        .iter()
        .zip(eq.bands.iter())
        .map(|(hz, gain)| format!("equalizer=f={hz}:t=q:w=1:g={gain:.1}"))
        .collect::<Vec<_>>()
        .join(",");
    vec![format!("lavfi=[{chain}]")]
}

/// Пауза покоя после последнего скипа перед запуском трека. Автоповтор
/// клавиатуры шлёт нажатия каждые ~33–40 мс: за это время серия должна
/// успеть перевзвести отложенный запуск. Одиночный скип платит те же
/// 120 мс — незаметно на фоне сетевого резолва; естественный конец
/// трека (`on_track_end`) задержки не платит вовсе.
const SKIP_SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

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
    /// `"mpv"` делало бы эту настройку ложью. `resolve_gate` — общий
    /// на процесс семафор резолвов (владелец — демон).
    pub async fn new(
        registry: Registry,
        mpv_binary: std::path::PathBuf,
        paths: &tmus_core::paths::Paths,
        volume: f64,
        resolve_gate: Arc<tokio::sync::Semaphore>,
    ) -> Result<Self, PlayerError> {
        let (mpv, events) = Mpv::spawn(mpv_binary, paths, volume).await?;
        mpv.observe().await?;
        let (play_gen, play_gen_rx) = tokio::sync::watch::channel(0u64);
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
                // Эквалайзер стартует выключенным: начальное значение
                // накладывает демон после `Player::new` (конфиг), затем
                // персист (сохранённое состояние).
                equalizer: Mutex::new(EqState::default()),
                status: Mutex::new(PlaybackStatus::Stopped),
                last_error: Mutex::new(None),
                pending: Mutex::new(None),
                preloaded: Mutex::new(None),
                play_gen,
                play_gen_rx,
                resolve_gate,
                preload_task: tokio::sync::Mutex::new(None),
                skip_settle: tokio::sync::Mutex::new(None),
                changed: tokio::sync::Notify::new(),
                advancing: std::sync::atomic::AtomicBool::new(false),
                failure_streak: std::sync::atomic::AtomicU32::new(0),
            }),
        };
        tokio::spawn(Player::clone(&player).run(events));
        Ok(player)
    }

    /// Зарезолвить трек и играть его. Существующий `StreamSource`
    /// (из предзагрузки) переиспользуется, если он ещё не истёк.
    pub async fn resolve_and_play(&self, track_id: &TrackId) -> Result<(), PlayerError> {
        let my_gen = self.next_generation();
        // Новая команда гасит отложенный запуск серии скипов: явный
        // выбор важнее накопленных нажатий. Свой (тот же трек) не гасим:
        // этот вызов пришёл ИЗ него — `abort` собственного хендла убивал
        // settle-задачу до резолва, и скип не играл (см. `skip_settle`).
        if let Some((settling, handle)) = self.inner.skip_settle.lock().await.take() {
            if &settling != track_id {
                handle.abort();
            }
        }
        // Чужая предзагрузка отменяется (её трек уже не следующий), своя
        // (этот же трек) доживает и положит результат в слот — иначе
        // запуск убивал бы собственный резолв и стартовал второй.
        if let Some((preloading, handle)) = self.inner.preload_task.lock().await.take() {
            if &preloading != track_id {
                handle.abort();
            }
        }
        // Трек объявляется ДО резолва: бар, TUI и MPRIS обязаны
        // переключить название немедленно, а не после секунд резолва —
        // иначе человек не видит, что нажатие вообще принято.
        *self.inner.pending.lock().await = Some(track_id.clone());
        self.inner.changed.notify_one();

        let source = match self.take_preloaded(track_id).await {
            // Годная предзагрузка — играем сразу, без сети.
            Some(source) if !source.is_expired(SystemTime::now()) => Some(source),
            _ => match self.resolve_fresh(track_id, my_gen).await {
                Ok(source) => source,
                // Резолв-время «трек нельзя воспроизвести» (DRM и т.п.)
                // приравнивается к провалу воспроизведения на стороне
                // mpv: видимая причина в `last_error`, затем тот же
                // счётчик (`failure_streak`) и тот же потолок
                // (`MAX_AUTO_SKIPS`) — без своего пути промотки, чтобы
                // серия не могла уйти в бесконечность. Сетевые и
                // инструментальные сбои остаются на прежнем пути
                // «остановиться и показать ошибку»: они чинятся
                // повтором, а трек — нет. Промотка синхронная, а не в
                // отдельной задаче: к этому месту семафор резолва уже
                // отпущен (`resolve_fresh` вернулся), так что вложенный
                // резолв следующего трека не встанет навсегда в очередь
                // к самому себе; глубина ограничена потолком скипов.
                Err(e @ PlayerError::Provider(ProviderError::Unplayable {
                    ref reason,
                    ..
                })) => {
                    tracing::warn!(track = %track_id, reason = %reason, "трек нельзя воспроизвести");
                    *self.inner.last_error.lock().await = Some(reason.clone());
                    // Окно перехода закрываем до промотки: висящий
                    // `pending` отказавшего трека иначе пережил бы
                    // конец очереди и залип бы в баре.
                    self.transition_failed(track_id).await;
                    let streak = self
                        .inner
                        .failure_streak
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        + 1;
                    match failure_action(streak) {
                        FailureAction::Advance => {
                            tracing::warn!(track = %track_id, streak, "трек нельзя воспроизвести — перехожу к следующему");
                            // Исход решает цепочка промотки (вплоть до
                            // `Stop` или конца очереди): наверх отдаём
                            // успех, чтобы внешняя обработка сбоя не
                            // затёрла состояние уже играющего трека.
                            self.on_track_end().await;
                            return Ok(());
                        }
                        FailureAction::Stop => {
                            tracing::warn!("подряд нельзя воспроизвести {} треков — останавливаюсь", streak);
                            return Err(e);
                        }
                    }
                }
                Err(e) => return Err(e),
            },
        };
        let Some(source) = source else {
            // Вытеснены более новой командой: её трек и будет играть.
            return Ok(());
        };

        // Последняя проверка перед mpv.load: устаревшая команда не должна
        // перебить свежий трек. Status/position/duration тоже выставляем
        // только здесь — раньше они портили бы состояние живой команды.
        if self.generation() != my_gen {
            return Ok(());
        }
        *self.inner.status.lock().await = PlaybackStatus::Playing;
        *self.inner.paused.lock().await = false;
        *self.inner.position.lock().await = None;
        *self.inner.duration.lock().await = None;
        self.inner.mpv.load(&source).await?;
        // Загрузка прошла — прошлый сбой больше не характеризует
        // состояние плеера: иначе причина 403 от мёртвой ссылки висела
        // бы в `tmus status` до конца сессии.
        *self.inner.last_error.lock().await = None;
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
        // Запрос исполнен: показываемый трек снова считается текущим.
        // Гвардия обязательна — между резолвом и загрузкой могла прийти
        // более новая команда и положить в pending свой трек.
        {
            let mut pending = self.inner.pending.lock().await;
            if pending.as_ref() == Some(track_id) {
                *pending = None;
            }
        }
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
            let preload_target = next_id.clone();
            let base_gen = self.generation();
            let handle = tokio::spawn(async move {
                // Задержка, затем двойной gen-чек: серия скипов отменяет
                // задачу до запуска yt-dlp; досидевшие до permit — те, кто
                // ещё актуален на момент взятия семафора.
                tokio::time::sleep(PRELOAD_DELAY).await;
                if this.generation() != base_gen {
                    return;
                }
                let permit = this
                    .inner
                    .resolve_gate
                    .acquire()
                    .await
                    .expect("семафор резолва живёт столько же, сколько плеер");
                if this.generation() != base_gen {
                    return;
                }
                let result = this.resolve(&preload_target).await;
                drop(permit);
                // Результат кладём БЕЗ проверки gen: слот адресован по id
                // трека, и вытеснившая команда заберёт его сама — второй
                // проверкой слота в `resolve_fresh`, уже после семафора.
                // Прежняя проверка выбрасывала готовый источник ровно
                // тогда, когда он нужнее всего: в момент скипа gen уже
                // сдвинут, а слот «следующего» — это и есть цель скипа.
                match result {
                    Ok(source) => *this.inner.preloaded.lock().await = Some((preload_target, source)),
                    Err(e) => tracing::debug!(track = %preload_target, error = %e, "предзагрузка следующего трека не удалась"),
                }
            });
            *self.inner.preload_task.lock().await = Some((next_id, handle));
        }
        Ok(())
    }

    /// Текущий номер поколения команд воспроизведения.
    fn generation(&self) -> u64 {
        *self.inner.play_gen_rx.borrow()
    }

    /// Взять следующий номер поколения: каждая команда воспроизведения
    /// начинается с этого, устаревшие по номеру молча уходят.
    ///
    /// Инкремент и захват номера — одна операция под замком канала:
    /// две concurrent-команды обязаны получить РАЗНЫЕ номера, иначе
    /// старшая из них проходила бы проверки свежести чужим номером.
    fn next_generation(&self) -> u64 {
        let mut mine = 0;
        self.inner.play_gen.send_modify(|g| {
            *g += 1;
            mine = *g;
        });
        mine
    }

    /// Подождать, пока поколение `my_gen` не станет устаревшим.
    ///
    /// Гонки нет по построению: `changed()` сверяет замеченную отметку
    /// с текущим значением канала, а не полагается на момент
    /// регистрации официанта.
    async fn superseded(&self, my_gen: u64) {
        let mut rx = self.inner.play_gen.subscribe();
        if *rx.borrow() != my_gen {
            return;
        }
        let _ = rx.changed().await;
    }

    /// Забрать слот предзагрузки, если он про этот трек.
    ///
    /// Слот расходуется при первом использовании: take, а не клон —
    /// иначе источник зависал бы до смены трека.
    async fn take_preloaded(
        &self,
        track_id: &TrackId,
    ) -> Option<tmus_core::model::StreamSource> {
        let mut preloaded = self.inner.preloaded.lock().await;
        match preloaded.as_ref() {
            Some((id, _)) if id == track_id => preloaded.take().map(|(_, s)| s),
            _ => None,
        }
    }

    /// Свежий источник: очередь на семафор резолвов, затем yt-dlp.
    ///
    /// `Ok(None)` — команду вытеснила более новая: вызывающий тихо
    /// уходит, не трогая mpv и состояние.
    ///
    /// Оба ожидания отменяются вытеснением, а не дожидаются конца:
    /// дроп будущего резолва убивает yt-dlp (`kill_on_drop`), и семафор
    /// освобождается сразу. До этой правки брошенный резолв доживал
    /// свои ~4 с, держа семафор, — скип вставал в очередь позади него,
    /// и два быстрых нажатия звучали как «сначала грузится первый,
    /// потом второй»: цепочка ~8 с вместо ~4 с от последнего нажатия.
    async fn resolve_fresh(
        &self,
        track_id: &TrackId,
        my_gen: u64,
    ) -> Result<Option<tmus_core::model::StreamSource>, PlayerError> {
        let _permit = tokio::select! {
            biased;
            _ = self.superseded(my_gen) => return Ok(None),
            permit = self.inner.resolve_gate.acquire() => {
                permit.expect("семафор резолва живёт столько же, сколько плеер")
            }
        };
        if self.generation() != my_gen {
            // Пока ждали очереди, человек ушёл дальше — yt-dlp вообще
            // не запускаем, это и есть устранение шторма процессов.
            return Ok(None);
        }
        // Предзагрузка могла закончиться, пока мы ждали семафор: её
        // результат лежит в слоте и адресован этому же треку — берём
        // без второго yt-dlp.
        if let Some(source) = self.take_preloaded(track_id).await {
            if !source.is_expired(SystemTime::now()) {
                return Ok(Some(source));
            }
        }
        // У googlevideo-ссылок `expire=` около 6 часов: трек, добавленный
        // в очередь давно, иначе отдал бы 403 при проигрывании.
        // Один permit: yt-dlp — 0.9 CPU-с и 335 МБ, параллелить некому.
        let source = match tokio::select! {
            biased;
            _ = self.superseded(my_gen) => return Ok(None),
            source = self.resolve(track_id) => source,
        } {
            Ok(source) => source,
            Err(e) => {
                self.transition_failed(track_id).await;
                return Err(e);
            }
        };
        Ok(Some(source))
    }

    /// Свести окно перехода при неудачном резолве.
    ///
    /// `advancing` взводит конец трека, а снимает — успешная загрузка.
    /// Если победившая команда (ручной скип, вытеснивший естественный
    /// переход) резолвится с ошибкой, флаг остался бы взведённым:
    /// mpv-idle дальше игнорируется, и бар залипает в «играет» над
    /// тишиной. Здесь флаг гасится, объявленный трек снимается (если
    /// это всё ещё наш), а по-настоящему пустой mpv честно отмечается
    /// остановкой.
    async fn transition_failed(&self, track_id: &TrackId) {
        self.inner
            .advancing
            .store(false, std::sync::atomic::Ordering::SeqCst);
        {
            let mut pending = self.inner.pending.lock().await;
            if pending.as_ref() == Some(track_id) {
                *pending = None;
            }
        }
        if self.inner.mpv.idle_active().await.unwrap_or(true) {
            *self.inner.status.lock().await = PlaybackStatus::Stopped;
        }
        self.inner.changed.notify_one();
    }

    /// Текущее состояние для control-протокола.
    ///
    /// Показываемый трек: заявленный (`pending`) важнее играемого
    /// (`current`) — секунды резолва бар обязан показывать ВЫБРАННЫЙ
    /// трек, а не прежний. Позиция и длительность на время
    /// переключения скрываются: они относятся к старому файлу, и
    /// «новое название со старым таймкодом» читалось бы как баг.
    pub async fn state(&self) -> PlayerState {
        let queue = self.inner.queue.lock().await;
        let current = self.inner.current.lock().await;
        let pending = self.inner.pending.lock().await.clone();
        let shown = pending.as_ref().or(current.as_ref().map(|c| &c.track_id));
        let index = shown.and_then(|id| queue.find_index(id));
        let track = index.and_then(|i| queue.track_at(i)).cloned();
        let offline = current
            .as_ref()
            .map(|c| c.source.is_local())
            .unwrap_or(false);
        let switching = pending.is_some() && pending != current.as_ref().map(|c| c.track_id.clone());
        let (position, duration) = if switching {
            (None, None)
        } else {
            (*self.inner.position.lock().await, *self.inner.duration.lock().await)
        };
        PlayerState {
            status: *self.inner.status.lock().await,
            track,
            position,
            duration,
            volume: *self.inner.volume.lock().await,
            equalizer: self.inner.equalizer.lock().await.clone(),
            loop_mode: queue.loop_mode(),
            shuffle: queue.shuffle(),
            queue_index: index,
            queue_len: queue.len(),
            offline,
            last_error: self.inner.last_error.lock().await.clone(),
        }
    }

    /// Шаг по очереди с поглощением серии: очередь двигается сразу,
    /// запуск трека — после SETTLE покоя.
    ///
    /// Автоповтор зажатой медиа-клавиши шлёт скипы каждые ~33–40 мс, и
    /// без settle каждый скип по закэшированному треку успевал полностью
    /// загрузиться и заиграть: серия нажатий прокручивалась аудио, а
    /// скип по незакэшированному — ставить хвост команды в очередь к
    /// семафору. Отложенный запуск перевзводится каждым скипом серии;
    /// явная команда (`resolve_and_play`) гасит его — выбор человека
    /// важнее накопленных нажатий.
    ///
    /// `None` — очередь кончилась: терминальное состояние, вызывающий
    /// останавливает mpv без задержки.
    pub async fn skip(&self, forward: bool) -> Option<TrackId> {
        let id = {
            let mut queue = self.inner.queue.lock().await;
            if forward { queue.next().cloned() } else { queue.prev().cloned() }
        }
        .map(|t| t.id)?;

        if let Some((_, handle)) = self.inner.skip_settle.lock().await.take() {
            handle.abort();
        }
        let this = self.clone();
        let target = id.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(SKIP_SETTLE).await;
            if let Err(e) = this.resolve_and_play(&target).await {
                tracing::warn!(track = %target, error = %e, "скип не удался");
            }
        });
        *self.inner.skip_settle.lock().await = Some((id.clone(), handle));
        Some(id)
    }

    /// Громкость: обновить свою копию и mpv.
    pub async fn set_volume(&self, volume: f64) -> Result<(), PlayerError> {
        self.inner.mpv.set_volume(volume).await?;
        *self.inner.volume.lock().await = volume;
        Ok(())
    }

    /// Применить эквалайзер: полосы клампятся к ±`EQ_GAIN_LIMIT_DB`
    /// (клиппинг дальше ограничения не наша забота), копия хранится для
    /// `state()` и слияния частичных команд, граф уходит в mpv одним
    /// свойством `af`. Порядок «сначала mpv, потом копия» сознательный:
    /// при ошибке IPC копия остаётся честной — применён не она.
    pub async fn set_equalizer(&self, eq: EqState) -> Result<(), PlayerError> {
        let mut eq = eq;
        for gain in &mut eq.bands {
            *gain = gain.clamp(-EQ_GAIN_LIMIT_DB, EQ_GAIN_LIMIT_DB);
        }
        self.inner.mpv.set_audio_filter(&eq_af_graph(&eq)).await?;
        *self.inner.equalizer.lock().await = eq;
        Ok(())
    }

    /// Текущее состояние эквалайзера (клон из мьютекса).
    pub async fn equalizer(&self) -> EqState {
        self.inner.equalizer.lock().await.clone()
    }

    /// Текущая громкость (клон из мьютекса) — персисту и вахтёру.
    pub async fn volume(&self) -> f64 {
        *self.inner.volume.lock().await
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
                    // Трек реально заиграл: отказ приходит после `loadfile`,
                    // поэтому загрузка сама по себе счётчиком не считается —
                    // сброс только по факту слышимого звука.
                    if position > std::time::Duration::ZERO {
                        self.inner
                            .failure_streak
                            .store(0, std::sync::atomic::Ordering::SeqCst);
                    }
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
                // Дослушан до конца — успех, потолок неудач начинается заново.
                MpvEvent::EndOfFile => {
                    self.inner
                        .failure_streak
                        .store(0, std::sync::atomic::Ordering::SeqCst);
                    self.on_track_end().await
                },
                MpvEvent::PlaybackFailed { reason } => {
                    // Причина привязывается к текущему треку из `current`:
                    // это тот файл, который mpv отказался играть. Не
                    // выдумываем «играет другой трек» — после отказа mpv
                    // ничего не играет, `current` ещё держит упавший.
                    let current = self.inner.current.lock().await;
                    let track = current
                        .as_ref()
                        .map(|c| c.track_id.to_string())
                        .unwrap_or_else(|| "?".to_owned());
                    drop(current);
                    tracing::warn!(track = %track, reason = %reason, "воспроизведение прервалось на стороне mpv");
                    *self.inner.last_error.lock().await = Some(reason);
                    *self.inner.status.lock().await = PlaybackStatus::Stopped;
                    self.inner.changed.notify_one();
                    // Авто-переход: единичный битый файл не должен рушить
                    // прослушивание, но и молча жечь очередь нельзя —
                    // поэтому потолок (`MAX_AUTO_SKIPS`), а не бесконечная
                    // промотка.
                    let streak = self
                        .inner
                        .failure_streak
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        + 1;
                    match failure_action(streak) {
                        FailureAction::Advance => {
                            tracing::warn!(track = %track, streak, "трек не сыграл — перехожу к следующему");
                            self.on_track_end().await;
                        }
                        FailureAction::Stop => {
                            tracing::warn!("подряд не сыграли {} треков — останавливаюсь", streak);
                        }
                    }
                }
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
                MpvEvent::Restarted => {
                    // Супервизор существует ради продолжения игры: после
                    // рестарта mpv пуст, поэтому текущий трек запускаем
                    // заново — как это делает on_track_end для следующего.
                    let current = self
                        .inner
                        .current
                        .lock()
                        .await
                        .as_ref()
                        .map(|c| c.track_id.clone());
                    match current {
                        Some(id) => {
                            self.inner
                                .advancing
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                            if let Err(e) = self.resolve_and_play(&id).await {
                                tracing::warn!(track = %id, error = %e, "перезапуск трека после рестарта mpv не удался");
                                self.inner
                                    .advancing
                                    .store(false, std::sync::atomic::Ordering::SeqCst);
                                *self.inner.status.lock().await = PlaybackStatus::Stopped;
                                self.inner.changed.notify_one();
                            }
                        }
                        None => {
                            // Играть было нечего — прежнее поведение.
                            *self.inner.status.lock().await = PlaybackStatus::Stopped;
                            *self.inner.position.lock().await = None;
                            self.inner
                                .advancing
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                            self.inner.changed.notify_one();
                        }
                    }
                }
                MpvEvent::GaveUp { .. } => {
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
            queue.next_natural().map(|t| t.id.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Границы потолка авто-переходов: три неудачи подряд ещё переходят к
    /// следующему треку (единичный битый файл и пара соседних), четвёртая —
    /// остановка. Это ровно то, что обещает README.
    #[test]
    fn failure_action_advances_within_limit() {
        assert!(matches!(failure_action(0), FailureAction::Advance));
        assert!(matches!(failure_action(1), FailureAction::Advance));
        assert!(matches!(failure_action(2), FailureAction::Advance));
        assert!(matches!(failure_action(3), FailureAction::Advance));
    }

    #[test]
    fn failure_action_stops_after_limit() {
        assert!(matches!(failure_action(4), FailureAction::Stop));
        assert!(matches!(failure_action(5), FailureAction::Stop));
    }

    /// Выключенный эквалайзер обязан давать пустой `af`: фильтр с
    /// нулевыми усилениями продолжал бы тратить CPU без смысла.
    #[test]
    fn disabled_eq_gives_empty_filter() {
        let eq = EqState { enabled: false, ..EqState::default() };
        assert!(eq_af_graph(&eq).is_empty());
    }

    /// Включённый эквалайзер — ровно один элемент `af` (единый lavfi
    /// граф) с десятью equalizer-фильтрами; нулевые полосы не
    /// выбрасываются, индексация полос остаётся стабильной.
    #[test]
    fn enabled_eq_gives_one_filter_with_ten_bands() {
        let eq = EqState { enabled: true, ..EqState::default() };
        let graph = eq_af_graph(&eq);
        assert_eq!(graph.len(), 1);
        assert_eq!(graph[0].matches("equalizer=").count(), 10, "граф: {:?}", graph[0]);
        assert!(graph[0].starts_with("lavfi=["), "граф: {:?}", graph[0]);
    }

    /// Усиление форматируется с одной десятичной: устойчивая строка
    /// графа важна для логов и для сравнения в тестах персиста.
    #[test]
    fn gains_formatted_with_one_decimal() {
        let eq = EqState { enabled: true, bands: [3.0, -1.24, 0.0, 15.0, 0.0, 0.0, 0.0, 0.0, 0.0, -15.0], ..EqState::default() };
        let graph = eq_af_graph(&eq);
        assert!(graph[0].contains("f=32:t=q:w=1:g=3.0"), "граф: {:?}", graph[0]);
        assert!(graph[0].contains("f=64:t=q:w=1:g=-1.2"), "граф: {:?}", graph[0]);
        assert!(graph[0].contains("f=16000:t=q:w=1:g=-15.0"), "граф: {:?}", graph[0]);
    }
}

