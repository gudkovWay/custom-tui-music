//! MPRIS-сервер: интеграция с оболочкой (noctalia читает и управляет
//! воспроизведением именно отсюда).
//!
//! Два интерфейса на одном пути `/org/mpris/MediaPlayer2`, как требует
//! спецификация MPRIS2. Как и остальные подсистемы, здесь нет доступа к
//! плееру напрямую: команды идут через [`App::handle`], состояние — из
//! потока событий, с локальным кэшем для геттеров. Кэш нужен потому,
//! что D-Bus-клиенты опрашивают свойства синхронно и часто (noctalia
//! дергает `Position` раз в кадр), а ходить за каждым тиком в mpv —
//! значит будить плеер зря.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use std::sync::RwLock;
use tracing::warn;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, Value};
use zbus::{connection, Connection};

use tmus_core::model::{LoopMode, Track};
use tmus_core::paths::MPRIS_BUS_NAME;
use tmus_core::protocol::{Cmd, Event, PlayerState};

use crate::app::App;

/// Путь объектов обоих MPRIS-интерфейсов. Фиксирован спецификацией.
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";

/// Плейсхолдер для «трека нет» — константа спецификации TrackList.
const NO_TRACK_PATH: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

// --- чистые функции: без D-Bus и плеера, покрыты тестами ---

/// Легальные символы в компоненте D-Bus object path.
///
/// Спецификация допускает `[A-Za-z0-9_]` после `/`; всё прочее
/// (`-`, `.` в идентификаторах YouTube — обычное дело) заменяем на `_`.
/// Коллизии после замены не проблема: путь нужен только как уникальный
/// маркер текущего трека, а не адрес объекта, который кто-то вызывает.
fn sanitize_component(part: &str) -> String {
    part.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// `mpris:trackid` обязан быть валидным object path — произвольная
/// строка (наш `Display` вида `ytmusic:abc-DEF`) ломает часть клиентов,
/// включая noctalia.
fn track_object_path(track: Option<&Track>) -> ObjectPath<'static> {
    let raw = match track {
        None => NO_TRACK_PATH.to_owned(),
        Some(t) => format!(
            "/tmus/track/{}/{}",
            sanitize_component(t.id.provider.0),
            sanitize_component(&t.id.id),
        ),
    };
    // Санитайзер гарантирует легальность; паника здесь была бы багом
    // санитайзера, а не обработкой ошибок.
    ObjectPath::try_from(raw).expect("sanitized track path must be a valid object path")
}

/// `mpris:length` — микросекунды **i64**. Ошибка в единицах или знаке
/// (u128 → i64) — «семейная»: её не видит никто, кроме клиентов, у
/// которых прогрессбар уезжает в бесконечность. Клампим на всякий
/// случай: длительность свыше ~292 тыс. лет не бывает.
fn length_micros(d: Duration) -> i64 {
    u64::try_from(d.as_micros())
        .unwrap_or(i64::MAX as u64)
        .min(i64::MAX as u64) as i64
}

/// MPRIS хранит громкость в 0.0..1.0, `PlayerState.volume` — в 0..100.
/// Без перевода ползунок в панели всегда стоит в максимуме: клиент
/// читает 37 и трактует как 3700%.
fn volume_to_mpris(v: f64) -> f64 {
    (v / 100.0).clamp(0.0, 1.0)
}

fn volume_from_mpris(v: f64) -> f64 {
    (v * 100.0).clamp(0.0, 100.0)
}

/// Метаданные `Metadata` из трека. Ключи — ровно те, что читает
/// noctalia и ожидает спецификация.
///
/// `xesam:artist` — обязательно массив строк, даже когда артист один;
/// строка вместо массива ломает клиентов, ожидающих спецификацию. Когда
/// артистов нет, ключ отсутствует вовсе — пустой массив часть клиентов
/// трактует как «нет названия».
fn build_metadata(track: &Track) -> HashMap<String, Value<'static>> {
    let mut md = HashMap::new();
    md.insert("mpris:trackid".into(), Value::new(track_object_path(Some(track))));
    md.insert("xesam:title".into(), Value::from(track.title.clone()));
    if !track.artists.is_empty() {
        md.insert("xesam:artist".into(), Value::from(track.artists.clone()));
    }
    if let Some(album) = &track.album {
        md.insert("xesam:album".into(), Value::from(album.clone()));
    }
    if let Some(url) = &track.page_url {
        md.insert("xesam:url".into(), Value::from(url.clone()));
    }
    if let Some(d) = track.duration {
        md.insert("mpris:length".into(), Value::I64(length_micros(d)));
    }
    // Как есть, удалённый URL: noctalia тянет обложку своим HttpClient.
    // Локальный файл или скачивание на нашей стороне не нужны.
    if let Some(art) = &track.art_url {
        md.insert("mpris:artUrl".into(), Value::from(art.clone()));
    }
    md
}

// --- свойства из снимка состояния ---

/// Чистые вычисления отдельных свойств из снимка — один источник и для
/// геттеров, и для пакета `PropertiesChanged`, иначе сигнал и ответ на
/// запрос свойства рано или поздно разойдутся.
fn can_go_next_of(state: &PlayerState) -> bool {
    state.queue_index.is_some_and(|i| i + 1 < state.queue_len)
}

fn can_go_previous_of(state: &PlayerState) -> bool {
    state.queue_index.unwrap_or(0) > 0
}

/// Полный набор свойств для одного `PropertiesChanged` на интерфейсе
/// `org.mpris.MediaPlayer2.Player`. Замер `dbus-monitor` до коалесинга:
/// 40 сигналов (4 раунда × 10 свойств) на один `tmus next` — каждое
/// будило бар noctalia и заставляло перечитывать свойства. Теперь весь
/// набор уходит одним сообщением; клиент по-прежнему видит те же имена
/// и может фильтровать по ним, как делает noctalia.
fn state_properties(state: &PlayerState) -> HashMap<&'static str, Value<'static>> {
    let mut props: HashMap<&'static str, Value<'static>> = HashMap::new();
    props.insert("PlaybackStatus", Value::from(state.status.as_mpris().to_owned()));
    props.insert("LoopStatus", Value::from(state.loop_mode.as_mpris().to_owned()));
    props.insert("Shuffle", Value::from(state.shuffle));
    props.insert("Volume", Value::from(volume_to_mpris(state.volume)));
    props.insert(
        "Metadata",
        match state.track.as_ref() {
            Some(track) => Value::from(build_metadata(track)),
            None => Value::from(HashMap::<String, Value<'static>>::new()),
        },
    );
    props.insert("CanPlay", Value::from(state.track.is_some()));
    props.insert("CanPause", Value::from(state.track.is_some()));
    props.insert("CanSeek", Value::from(state.track.is_some()));
    props.insert("CanGoNext", Value::from(can_go_next_of(state)));
    props.insert("CanGoPrevious", Value::from(can_go_previous_of(state)));
    props
}

// --- разделяемый кэш состояния ---

/// Кэш снимка состояния для геттеров свойств.
///
/// D-Bus-геттеры вызываются синхронно из шины; обращаться из них к
/// плееру нельзя (и по правилу подсистем, и по цене: `Position`
/// опрашивается часто). Обновляется из потока событий.
#[derive(Default)]
struct StateCache {
    state: RwLock<PlayerState>,
}

impl StateCache {
    /// Прочитать нужное поле под замком, без копирования снимка.
    ///
    /// Оболочка опрашивает `Position` покадрово, и полный `clone()`
    /// снимка на каждый геттер — это трек, вектор артистов и все
    /// строки заново: замерено 0.40 % CPU демона в потоке zbus на
    /// покое. Замыкание синхронное, `await` внутрь не протащить.
    fn with<R>(&self, f: impl FnOnce(&PlayerState) -> R) -> R {
        f(&self.state.read().expect("кэш состояния отравлен"))
    }

    /// Полная копия — только там, где нужен весь снимок сразу
    /// (пакет `PropertiesChanged`, правка позиции в цикле событий).
    fn snapshot(&self) -> PlayerState {
        self.state.read().expect("кэш состояния отравлен").clone()
    }

    fn store(&self, state: PlayerState) {
        *self.state.write().expect("кэш состояния отравлен") = state;
    }
}

// --- интерфейсы ---

struct Root {
    app: Arc<App>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    /// Терминальный клиент поднять нельзя — окна у демона нет.
    fn raise(&self) {}

    async fn quit(&self) -> zbus::fdo::Result<()> {
        self.app
            .handle(Cmd::Shutdown)
            .await
            .map(|_| ())
            .map_err(mpris_error)
    }

    #[zbus(property)]
    fn identity(&self) -> &'static str {
        "tmus"
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> &'static str {
        "tmus"
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    // Пустые наборы: свои URI-схемы мы не резолвим, `OpenUri` отвергает.
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct PlayerIface {
    app: Arc<App>,
    cache: Arc<StateCache>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl PlayerIface {
    async fn play(&self) -> zbus::fdo::Result<()> {
        self.cmd(Cmd::Play).await
    }

    async fn pause(&self) -> zbus::fdo::Result<()> {
        self.cmd(Cmd::Pause).await
    }

    async fn play_pause(&self) -> zbus::fdo::Result<()> {
        self.cmd(Cmd::Toggle).await
    }

    async fn stop(&self) -> zbus::fdo::Result<()> {
        self.cmd(Cmd::Stop).await
    }

    async fn next(&self) -> zbus::fdo::Result<()> {
        self.cmd(Cmd::Next).await
    }

    async fn previous(&self) -> zbus::fdo::Result<()> {
        self.cmd(Cmd::Prev).await
    }

    /// `offset` в микросекундах по спецификации; у нас — секунды.
    async fn seek(&self, offset: i64) -> zbus::fdo::Result<()> {
        self.cmd(Cmd::SeekBy { delta: offset as f64 / 1_000_000.0 })
            .await
    }

    /// `track` — путь текущего трека: если клиент получил устаревший
    /// снимок, позицию применять нельзя (позиция другого трека).
    async fn set_position(&self, track: ObjectPath<'_>, position: i64) -> zbus::fdo::Result<()> {
        let matches_current =
            self.cache.with(|s| track == track_object_path(s.track.as_ref()));
        if matches_current {
            let position = u64::try_from(position.max(0)).unwrap_or(0);
            self.cmd(Cmd::Seek { position: Duration::from_micros(position) })
                .await?;
        }
        Ok(())
    }

    /// Схемы мы не объявляем в `SupportedUriSchemes`, поэтому отказ
    /// вместо тихого «сделано» — клиент честно покажет, что не вышло.
    async fn open_uri(&self, _uri: String) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "tmus не открывает произвольные URI; используйте поиск и очередь".into(),
        ))
    }

    #[zbus(property)]
    fn playback_status(&self) -> zbus::fdo::Result<String> {
        Ok(self.cache.with(|s| s.status.as_mpris().to_owned()))
    }

    #[zbus(property)]
    fn loop_status(&self) -> zbus::fdo::Result<String> {
        Ok(self.cache.with(|s| s.loop_mode.as_mpris().to_owned()))
    }

    /// Неизвестное значение игнорируем, а не ошибаемся: оболочки иногда
    /// присылают свои варианты, и ронять воспроизведение из-за этого
    /// недопустимо.
    #[zbus(property)]
    async fn set_loop_status(&self, value: String) -> zbus::fdo::Result<()> {
        if let Some(mode) = LoopMode::from_mpris(&value) {
            self.cmd(Cmd::SetLoop { mode }).await?;
        }
        Ok(())
    }

    #[zbus(property)]
    fn shuffle(&self) -> zbus::fdo::Result<bool> {
        Ok(self.cache.with(|s| s.shuffle))
    }

    #[zbus(property)]
    async fn set_shuffle(&self, value: bool) -> zbus::fdo::Result<()> {
        self.cmd(Cmd::SetShuffle { shuffle: value }).await
    }

    #[zbus(property)]
    fn volume(&self) -> zbus::fdo::Result<f64> {
        Ok(self.cache.with(|s| volume_to_mpris(s.volume)))
    }

    /// Пересчёт 0..100 ↔ 0.0..1.0 обязателен в обе стороны: без него
    /// noctalia показывает и выставляет громкость неверно.
    #[zbus(property)]
    async fn set_volume(&self, value: f64) -> zbus::fdo::Result<()> {
        let volume = volume_from_mpris(value);
        // Кэш обновляем сразу: иначе между set и приходом StateChanged
        // геттер вернул бы старое значение, и клиент решит, что set не
        // сработал.
        self.cache.state.write().expect("кэш состояния отравлен").volume = volume;
        self.cmd(Cmd::SetVolume { volume }).await
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    /// Свойство без уведомлений по спецификации: клиент опрашивает его
    /// сам. Раз в секунду будить всю шину Position-событием — зря.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> zbus::fdo::Result<i64> {
        // Самый горячий геттер: оболочка спрашивает позицию покадрово.
        Ok(self.cache.with(|s| s.position.map_or(0, length_micros)))
    }

    #[zbus(property)]
    fn can_play(&self) -> zbus::fdo::Result<bool> {
        Ok(self.cache.with(|s| s.track.is_some()))
    }

    #[zbus(property)]
    fn can_pause(&self) -> zbus::fdo::Result<bool> {
        Ok(self.cache.with(|s| s.track.is_some()))
    }

    #[zbus(property)]
    fn can_seek(&self) -> zbus::fdo::Result<bool> {
        Ok(self.cache.with(|s| s.track.is_some()))
    }

    #[zbus(property)]
    fn can_go_next(&self) -> zbus::fdo::Result<bool> {
        Ok(self.cache.with(can_go_next_of))
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> zbus::fdo::Result<bool> {
        Ok(self.cache.with(can_go_previous_of))
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn metadata(&self) -> zbus::fdo::Result<HashMap<String, Value<'static>>> {
        Ok(self.cache.with(|s| match s.track.as_ref() {
            Some(track) => build_metadata(track),
            None => HashMap::new(),
        }))
    }

    /// Единственный сигнал: при смене трека прогресс в панели
    /// продолжил бы бежать от старой позиции, потому что `Position`
    /// уведомлений не рассылает.
    #[zbus(signal)]
    async fn seeked(signal_emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
}

impl PlayerIface {
    /// Все действия идут через `App::handle` — правило одной точки
    /// обработки команд; прямого доступа к плееру отсюда нет.
    async fn cmd(&self, cmd: Cmd) -> zbus::fdo::Result<()> {
        self.app
            .handle(cmd)
            .await
            .map(|_| ())
            .map_err(mpris_error)
    }
}

/// Ошибки `App::handle` (anyhow) в ошибку D-Bus. `{:#}` оставляет цепочку
/// причин в тексте — иначе клиент увидит только верхний уровень.
fn mpris_error(err: anyhow::Error) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(format!("{err:#}"))
}

// --- цикл событий ---

/// Один `PropertiesChanged` с полным набором свойств вместо десяти
/// отдельных сигналов: замер `dbus-monitor` дал 40 сигналов (4 раунда ×
/// 10 свойств) на один `tmus next`, и каждое будило клиента (bar
/// noctalia) на перечитывание. Имена в карте — ровно имена геттеров
/// интерфейса, поэтому фильтрация по свойствам у клиентов продолжает
/// работать. `Position` в наборе нет: по спецификации он без
/// уведомлений, клиенты опрашивают его сами.
async fn notify_state_changed(player_ref: &zbus::object_server::InterfaceRef<PlayerIface>) -> zbus::Result<()> {
    let state = {
        let iface = player_ref.get().await;
        iface.cache.snapshot()
    };
    zbus::fdo::Properties::properties_changed(
        player_ref.signal_emitter(),
        zbus::names::InterfaceName::try_from("org.mpris.MediaPlayer2.Player")?,
        state_properties(&state),
        std::borrow::Cow::Borrowed(&[]),
    )
    .await
}

pub async fn run(app: Arc<App>) -> anyhow::Result<()> {
    let cache = Arc::new(StateCache::default());
    // Стартовый снимок: до первого события свойства уже должны быть
    // осмысленными, иначе клиент, подключившийся сразу, увидит нули.
    cache.store(app.player().state().await);

    let conn: Connection = connection::Builder::session()?
        .name(MPRIS_BUS_NAME)?
        .serve_at(MPRIS_PATH, Root { app: app.clone() })?
        .serve_at(MPRIS_PATH, PlayerIface { app: app.clone(), cache: cache.clone() })?
        .build()
        .await?;

    let player_ref = conn
        .object_server()
        .interface::<_, PlayerIface>(MPRIS_PATH)
        .await?;

    let mut rx = app.subscribe();
    loop {
        let event = match rx.recv().await {
            Ok(event) => event,
            // Отставание не повод умирать: потерянные события — это
            // старые тики позиции; снимок состояния восстанавливает
            // полную картину.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(skipped, "MPRIS отстал от шины событий; восстановление по снимку");
                cache.store(app.player().state().await);
                notify_state_changed(&player_ref).await?;
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        };

        // Коалесинг всплеска: панель noctalia запускает `tmus next` на
        // каждый клик, и очередь событий приходит пачкой. Всплеск
        // сворачиваем в два флага и последний снимок — иначе один next
        // даёт несколько раундов уведомлений (замер: 40 PropertiesChanged
        // на одно нажатие до коалесинга).
        let mut needs_notify = false;
        let mut track_changed = false;
        let mut last_state: Option<PlayerState> = None;
        let mut burst: Vec<Event> = vec![event];
        // Выгребаем всё, что уже лежит в приёмнике: это не ожидание
        // новых событий, а осушение уже накопленной пачки.
        while let Ok(event) = rx.try_recv() {
            burst.push(event);
        }
        for event in burst {
            match event {
                Event::TrackChanged { .. } => {
                    track_changed = true;
                    needs_notify = true;
                    // Частный снимок StateChanged мог прийти до смены
                    // трека; надёжнее пересобрать всё из плеера.
                    last_state = None;
                }
                Event::StateChanged { state } => {
                    needs_notify = true;
                    last_state = Some(state);
                }
                // Частое событие: только в кэш для геттера `Position`.
                // Сигналом не рассылаем — по спецификации `Position`
                // без уведомлений, клиенты опрашивают сами.
                Event::Position { position, duration } => {
                    let mut state = cache.snapshot();
                    state.position = Some(position);
                    if duration.is_some() {
                        state.duration = duration;
                    }
                    cache.store(state);
                }
                // Прогресс кэша и авторизация провайдеров в MPRIS не видны.
                Event::QueueChanged { .. }
                | Event::CacheProgress { .. }
                | Event::AuthChanged { .. } => {}
            }
        }

        if needs_notify {
            if track_changed || last_state.is_none() {
                // Трек сменился внутри плеера; снимок надёжнее частного
                // события: включает и позицию, и длину нового трека.
                cache.store(app.player().state().await);
            } else if let Some(state) = last_state {
                cache.store(state);
            }
            notify_state_changed(&player_ref).await?;
            if track_changed {
                // Seeked с новой позицией — прогрессбар не продолжает
                // бежать от старого трека.
                let position = cache.with(|s| s.position.map_or(0, length_micros));
                PlayerIface::seeked(player_ref.signal_emitter(), position).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{ObjectPath, Value};

    fn track(id: &str, artists: Vec<String>) -> Track {
        Track {
            id: tmus_core::model::TrackId {
                provider: tmus_core::model::ProviderId::from_name("ytmusic").unwrap(),
                id: id.into(),
            },
            title: "Test".into(),
            artists,
            album: None,
            duration: None,
            art_url: None,
            page_url: None,
        }
    }

    /// Регресс: `ytmusic:abc-DEF_123` — не валидный object path, и без
    /// санитайзера ломается часть клиентов.
    #[test]
    fn track_id_with_dashes_and_dots_becomes_valid_object_path() {
        let t = track("abc-DEF_12.3/x", vec!["A".into()]);
        let raw = track_object_path(Some(&t));
        let raw = raw.as_str();
        assert_eq!(raw, "/tmus/track/ytmusic/abc_DEF_12_3_x");
        // Валидность проверяет сам zvariant — то, что делает клиент.
        ObjectPath::try_from(raw).unwrap();
    }

    #[test]
    fn no_track_gives_notrack_path() {
        assert_eq!(
            track_object_path(None).as_str(),
            "/org/mpris/MediaPlayer2/TrackList/NoTrack"
        );
    }

    /// Регресс «семейной» ошибки: отдать секунды вместо микросекунд.
    #[test]
    fn length_is_micros() {
        assert_eq!(length_micros(Duration::from_secs_f64(3.5)), 3_500_000);
        assert_eq!(length_micros(Duration::from_millis(250)), 250_000);
    }

    /// Ползунок громкости: без перевода 0..100 ↔ 0.0..1.0 он всегда в
    /// максимуме; края не теряются, выход за границы клампится.
    #[test]
    fn volume_roundtrip_preserves_edges() {
        assert_eq!(volume_to_mpris(0.0), 0.0);
        assert_eq!(volume_to_mpris(100.0), 1.0);
        assert_eq!(volume_to_mpris(37.0), 0.37);
        assert_eq!(volume_from_mpris(0.0), 0.0);
        assert_eq!(volume_from_mpris(1.0), 100.0);
        assert_eq!(volume_from_mpris(0.37), 37.0);
        // Клиенты могут прислать за границами диапазона.
        assert_eq!(volume_from_mpris(1.5), 100.0);
        assert_eq!(volume_from_mpris(-0.5), 0.0);
        assert_eq!(volume_to_mpris(250.0), 1.0);
    }

    #[test]
    fn metadata_artist_is_array_and_absent_when_none() {
        let one = build_metadata(&track("t", vec!["Abba".into()]));
        assert!(matches!(
            one.get("xesam:artist"),
            Some(Value::Array(a)) if a.len() == 1
        ));
        assert_eq!(one.get("xesam:artist"), Some(&Value::from(vec!["Abba"])));

        let none = build_metadata(&track("t", vec![]));
        assert!(!none.contains_key("xesam:artist"));
        // Обязательные ключи на месте даже без артистов.
        assert!(none.contains_key("mpris:trackid"));
        assert!(none.contains_key("xesam:title"));
    }

    #[test]
    fn metadata_optional_fields_appear_only_when_present() {
        let mut t = track("t", vec![]);
        t.duration = Some(Duration::from_secs(200));
        let md = build_metadata(&t);
        assert_eq!(
            md.get("mpris:length"),
            Some(&Value::I64(200_000_000)),
            "длительность обязана быть в микросекундах"
        );
        assert!(!md.contains_key("xesam:album"));
        assert!(!md.contains_key("xesam:url"));
        assert!(!md.contains_key("mpris:artUrl"));
    }

    /// Сигнал несёт ровно контрактный набор из десяти свойств, и
    /// `PlaybackStatus` собран из того же источника, что и геттер, —
    /// иначе сигнал и ответ на запрос свойства расходятся.
    #[test]
    fn state_properties_has_full_contract_set() {
        let mut state = PlayerState {
            status: tmus_core::model::PlaybackStatus::Playing,
            track: Some(track("t", vec!["A".into()])),
            position: None,
            duration: None,
            volume: 42.0,
            loop_mode: LoopMode::Queue,
            shuffle: true,
            queue_index: Some(3),
            queue_len: 10,
            offline: false,
        };
        let props = state_properties(&state);
        let expected = [
            "PlaybackStatus",
            "LoopStatus",
            "Shuffle",
            "Volume",
            "Metadata",
            "CanPlay",
            "CanPause",
            "CanSeek",
            "CanGoNext",
            "CanGoPrevious",
        ];
        assert_eq!(props.len(), expected.len(), "ровно десять свойств контракта");
        for name in expected {
            assert!(props.contains_key(name), "нет свойства {name}");
        }
        assert_eq!(
            props.get("PlaybackStatus"),
            Some(&Value::from(state.status.as_mpris().to_owned()))
        );
        // Геттер `CanGoNext` считает по той же чистой функции.
        assert_eq!(props.get("CanGoNext"), Some(&Value::from(can_go_next_of(&state))));

        state.track = None;
        let props = state_properties(&state);
        assert!(matches!(props.get("Metadata"), Some(Value::Dict(_))));
        assert_eq!(props.get("CanPlay"), Some(&Value::from(false)));
    }

    /// Круговой перевод через MPRIS-имена, включая `Queue` ↔
    /// `"Playlist"` — noctalia пишет именно это имя.
    #[test]
    fn loop_mode_roundtrips_through_mpris_names() {
        for mode in [LoopMode::None, LoopMode::Track, LoopMode::Queue] {
            assert_eq!(LoopMode::from_mpris(mode.as_mpris()), Some(mode));
        }
        assert_eq!(LoopMode::Queue.as_mpris(), "Playlist");
        // Неизвестное значение игнорируется, а не интерпретируется.
        assert_eq!(LoopMode::from_mpris("One"), None);
    }

    /// `Array` в `xesam:artist` обязан быть массивом строк именно как
    /// D-Bus-тип: `Value::from(Vec<String>)` даёт `Array` из `Str`.
    #[test]
    fn artist_array_elements_are_strings() {
        let md = build_metadata(&track("t", vec!["A".into(), "B".into()]));
        match md.get("xesam:artist") {
            Some(Value::Array(a)) => {
                assert_eq!(a.len(), 2);
                assert_eq!(&a[0], &Value::from("A"));
                assert_eq!(&a[1], &Value::from("B"));
            }
            other => panic!("xesam:artist не массив: {other:?}"),
        }
    }
}
