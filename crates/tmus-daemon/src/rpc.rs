//! Discord Rich Presence.
//!
//! Требование хозяина буквально: «RPC для всех библиотек». Поэтому
//! здесь нет ни одного упоминания провайдера — только абстрактный
//! [`Track`]. Что показать в профиле — решает то, что вернул
//! провайдер, а не имя сервиса.
//!
//! Крейт `discord-rich-presence` синхронный, а Discord может не
//! ответить вовсе. Весь его ввод-вывод вынесен в выделенный поток:
//! блокировать рантайм tokio нельзя, иначе вместе с Discord встал бы
//! и плеер.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use tmus_core::model::{PlaybackStatus, Track, TrackId};
use tmus_core::protocol::{Cmd, Event, Payload};
use tokio::sync::mpsc::UnboundedReceiver;

/// Имя переменной окружения с Application ID. Его надо завести руками
/// в Discord Developer Portal (discord.com/developers/applications):
/// вымышленный ID показывал бы в профиле человека чужое приложение.
const APP_ID_ENV: &str = "TMUS_DISCORD_APP_ID";

/// У Discord жёсткий rate limit на обновления presence: частые записи
/// он молча отбрасывает. Обновляемся не чаще раза в 15 с.
const MIN_UPDATE_INTERVAL: Duration = Duration::from_secs(15);

/// Начальная пауза перед переподключением. Растёт до максимума:
/// Discord может быть выключен сутками, и сыпать ошибками в журнал
/// на каждой попытке нельзя.
const RECONNECT_MIN: Duration = Duration::from_secs(5);
const RECONNECT_MAX: Duration = Duration::from_secs(60);

/// Снимок presence, готовый к отправке. Чистые данные — без клиента,
/// чтобы таймстампы и отсутствие пустых полей проверялись тестами.
struct PresenceData {
    details: String,
    state: Option<String>,
    large_image: Option<String>,
    large_text: Option<String>,
    /// Миллисекунды с эпохи Unix — формат Discord API (i64).
    start_ms: Option<i64>,
    end_ms: Option<i64>,
}

/// Presence для играющего трека: `start` сдвинут на текущую позицию,
/// `end` = `start` + длительность — Discord сам рисует прогресс.
fn presence_for(
    track: &Track,
    position: Duration,
    duration: Option<Duration>,
    now_ms: i64,
) -> PresenceData {
    let start = now_ms - position.as_millis() as i64;
    PresenceData {
        details: track.title.clone(),
        // Пустой `state` Discord показывает как пустую строку в
        // профиле, поэтому поля просто нет.
        state: {
            let artists = track.artist_line();
            (!artists.is_empty()).then_some(artists)
        },
        large_image: track.art_url.clone(),
        large_text: track.album.clone(),
        start_ms: Some(start),
        end_ms: duration.map(|d| start + d.as_millis() as i64),
    }
}

/// Гейт частоты обновлений. Второе обновление в пределах интервала
/// отбрасывается, кроме смены трека: человек переключил трек — это
/// стоит мгновенного обновления, даже если предыдущее было секунду
/// назад.
#[derive(Default)]
struct UpdateGate {
    last_track: Option<String>,
    last_at: Option<Instant>,
}

impl UpdateGate {
    fn allows(&mut self, track_id: Option<&TrackId>, now: Instant) -> bool {
        let key = track_id.map(|t| t.to_string());
        let track_changed = key != self.last_track;
        let enough_time = self
            .last_at
            .is_none_or(|at| now.duration_since(at) >= MIN_UPDATE_INTERVAL);
        if track_changed || enough_time {
            self.last_track = key;
            self.last_at = Some(now);
            true
        } else {
            false
        }
    }

    /// Пауза/стоп сбрасывает гейт: вернуться к игре того же трека
    /// нужно успеть без пятнадцатисекундной тишины в профиле.
    fn reset(&mut self) {
        self.last_at = None;
    }
}

/// Задание для потока с Discord-клиентом.
enum Op {
    Set(PresenceData),
    Clear,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Полный цикл подсистемы.
pub async fn run(app: Arc<crate::app::App>) -> anyhow::Result<()> {
    let app_id = std::env::var(APP_ID_ENV).with_context(|| {
        format!(
            "переменная {APP_ID_ENV} не задана: создайте приложение в Discord Developer \
             Portal и укажите его Application ID — вымышленный ID показывал бы в профиле \
             чужое приложение"
        )
    })?;

    // Канал без границ: отправитель — асинхронная задача, приёмник —
    // блокирующий поток. Переполнение невозможно по построению:
    // события приходят только на смену трека и статуса.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("tmus-discord".to_owned())
        .spawn(move || ipc_loop(app_id, rx))?;

    let mut events = app.subscribe();
    let mut gate = UpdateGate::default();
    while let Ok(ev) = events.recv().await {
        // `Position` приходит ежесекундно и для presence бесполезен:
        // прогресс Discord рисует сам по таймстампам.
        if !matches!(ev, Event::TrackChanged { .. } | Event::StateChanged { .. }) {
            continue;
        }
        let Ok(Payload::State(st)) = app.handle(Cmd::State).await else {
            continue;
        };
        // На паузе и остановке presence очищается: зависший трек врал
        // бы друзьям, что трек играет, целыми часами.
        let Some(track) = st.track else {
            gate.reset();
            let _ = tx.send(Op::Clear);
            continue;
        };
        if st.status != PlaybackStatus::Playing {
            gate.reset();
            let _ = tx.send(Op::Clear);
            continue;
        }
        if !gate.allows(Some(&track.id), Instant::now()) {
            continue;
        }
        let position = st.position.unwrap_or_default();
        let _ = tx.send(Op::Set(presence_for(&track, position, st.duration, now_ms())));
    }
    Ok(())
}

/// Владеет клиентом и переподключается молча. Живёт в отдельном
/// потоке, поэтому `blocking_recv` и синхронный сон здесь законны.
fn ipc_loop(app_id: String, mut rx: UnboundedReceiver<Op>) {
    let mut backoff = RECONNECT_MIN;
    let mut client: Option<DiscordIpcClient> = None;
    loop {
        if client.is_none() {
            match connect(&app_id) {
                Ok(c) => {
                    client = Some(c);
                    backoff = RECONNECT_MIN;
                }
                Err(e) => {
                    tracing::debug!(
                        "Discord RPC недоступен, повтор через {backoff:?}: {e}"
                    );
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                    continue;
                }
            }
        }
        let Some(op) = rx.blocking_recv() else {
            return;
        };
        let Some(c) = client.as_mut() else {
            continue;
        };
        let result = match op {
            Op::Set(p) => c.set_activity(build_activity(p)).map(|_| ()),
            Op::Clear => c.clear_activity().map(|_| ()),
        };
        if let Err(e) = result {
            // Discord перезапустили или сокет умер: молча перепод-
            // ключаемся в начале цикла с растущей паузой.
            tracing::debug!("обновление presence не прошло: {e}");
            client = None;
        }
    }
}

fn connect(app_id: &str) -> anyhow::Result<DiscordIpcClient> {
    // `new` валиден всегда; упасть может только `connect`.
    let mut client = DiscordIpcClient::new(app_id);
    client.connect().context("Discord не принял соединение")?;
    Ok(client)
}

fn build_activity(p: PresenceData) -> activity::Activity<'static> {
    let mut act = activity::Activity::new().details(p.details);
    if let Some(state) = p.state {
        act = act.state(state);
    }
    let mut assets = activity::Assets::new();
    if let Some(image) = p.large_image {
        assets = assets.large_image(image);
    }
    if let Some(text) = p.large_text {
        assets = assets.large_text(text);
    }
    act = act.assets(assets);
    if p.start_ms.is_some() || p.end_ms.is_some() {
        let mut ts = activity::Timestamps::new();
        if let Some(start) = p.start_ms {
            ts = ts.start(start);
        }
        if let Some(end) = p.end_ms {
            ts = ts.end(end);
        }
        act = act.timestamps(ts);
    }
    act
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_track() -> Track {
        Track {
            id: TrackId::new(tmus_core::model::ProviderId::YTMUSIC, "abc"),
            title: "Song".into(),
            artists: vec!["Artist".into()],
            album: Some("Album".into()),
            duration: Some(Duration::from_secs(90)),
            art_url: Some("https://example/art".into()),
            page_url: None,
        }
    }

    #[test]
    fn timestamps_span_the_track() {
        let p = presence_for(&sample_track(), Duration::from_secs(30), Some(Duration::from_secs(90)), 1_000_000);
        let start = p.start_ms.expect("start");
        let end = p.end_ms.expect("end");
        assert_eq!(start, 1_000_000 - 30_000); // сдвинут на позицию
        assert_eq!(end - start, 90_000); // равен длительности
    }

    #[test]
    fn without_duration_end_is_absent() {
        let mut track = sample_track();
        track.duration = None;
        let p = presence_for(&track, Duration::from_secs(10), None, 1_000_000);
        assert!(p.end_ms.is_none());
        assert!(p.start_ms.is_some());
    }

    #[test]
    fn empty_artists_leave_state_absent() {
        let mut track = sample_track();
        track.artists.clear();
        let p = presence_for(&track, Duration::ZERO, None, 0);
        assert!(p.state.is_none());
        assert_eq!(p.details, "Song");
    }

    #[test]
    fn gate_drops_rapid_updates_but_allows_track_change() {
        let mut gate = UpdateGate::default();
        let id_a = TrackId::new(tmus_core::model::ProviderId::YTMUSIC, "a");
        let id_b = TrackId::new(tmus_core::model::ProviderId::SOUNDCLOUD, "b");
        let t0 = Instant::now();
        assert!(gate.allows(Some(&id_a), t0));
        // Тот же трек через секунду — отбрасывается.
        assert!(!gate.allows(Some(&id_a), t0 + Duration::from_secs(1)));
        // Смена трека — пропускается немедленно.
        assert!(gate.allows(Some(&id_b), t0 + Duration::from_secs(2)));
        // Тот же трек по истечении интервала от ПОСЛЕДНЕЙ отправки —
        // пропускается (последняя была на t0+2с).
        assert!(gate.allows(Some(&id_b), t0 + Duration::from_secs(2) + MIN_UPDATE_INTERVAL));
    }
}
