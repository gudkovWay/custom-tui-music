//! Иконка в трее через StatusNotifierItem (SNI).
//!
//! Реализация требования «чтобы не закрывался, а сворачивался в трей»:
//! демон живёт всегда, иконка остаётся после закрытия TUI. Протокол —
//! `org.kde.StatusNotifierItem` на своём объекте плюс регистрация у
//! `org.kde.StatusNotifierWatcher`. Меню через DBusMenu сознательно не
//! делается: это отдельный протокол `com.canonical.dbusmenu`, и в v1
//! он не нужен — `ItemIsMenu = false`, чтобы левый клик дошёл до
//! `Activate`, а не уходил в меню.
//!
//! Состояние иконки обновляется ТОЛЬКО из шины событий [`App`]:
//! подсистема не знает ни плеера, ни провайдера.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt as _;
use anyhow::Context as _;
use tmus_core::model::{PlaybackStatus, Track};
use tmus_core::protocol::{Cmd, Event, Payload};
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::ObjectPath;
use zbus::{Connection, interface};

/// Имя объекта с интерфейсом SNI. Хосты ждут ровно его по умолчанию.
const ITEM_PATH: ObjectPath<'static> = ObjectPath::from_static_str_unchecked("/StatusNotifierItem");

/// Чистый выбор иконки по состоянию воспроизведения. Отдельная функция —
/// чтобы правило «иконка не мигает» и все три ветки были проверяемы
/// без D-Bus.
fn icon_for(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Playing => "media-playback-start",
        PlaybackStatus::Paused => "media-playback-pause",
        PlaybackStatus::Stopped => "multimedia-player",
    }
}

/// Шаг громкости от прокрутки на иконке: 5 процентов, отрицательный
/// `delta` (прокрутка вниз по спецификации SNI у нас — вверх) повышает.
/// Результат зажат в 0..=100: плеер объём вне диапазона не примет.
fn volume_after_scroll(current: f64, delta: i32) -> f64 {
    let step = if delta < 0 { 5.0 } else { -5.0 };
    (current + step).clamp(0.0, 100.0)
}

/// `Title` и заголовок тултипа: название трека, а без трека — `tmus`,
/// чтобы иконка не оставалась полностью анонимной на пустом старте.
fn title_for(track: Option<&Track>) -> String {
    track.map_or_else(|| "tmus".to_owned(), |t| t.title.clone())
}

/// Тело тултипа: артист и альбом, что из них есть. Пустая строка —
/// норма: провайдер не обязан отдавать ни того, ни другого.
fn tooltip_body_for(track: Option<&Track>) -> String {
    let Some(t) = track else { return String::new() };
    let artist = t.artist_line();
    let album = t.album.as_deref().unwrap_or_default();
    match (artist.is_empty(), album.is_empty()) {
        (true, true) => String::new(),
        (false, true) => artist,
        (true, false) => album.to_owned(),
        (false, false) => format!("{artist} — {album}"),
    }
}

/// Изменяемая часть свойств SNI. Отдельно от интерфейса, чтобы поток
/// событий менял её под коротким `Mutex`, не трогая D-Bus.
struct SniState {
    title: String,
    icon: &'static str,
    tip_title: String,
    tip_body: String,
}

impl SniState {
    fn from_track(track: Option<&Track>, status: PlaybackStatus) -> Self {
        Self {
            title: title_for(track),
            icon: icon_for(status),
            tip_title: title_for(track),
            tip_body: tooltip_body_for(track),
        }
    }
}

struct Sni {
    app: Arc<crate::app::App>,
    state: Mutex<SniState>,
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl Sni {
    #[zbus(property)]
    fn category(&self) -> String {
        "Multimedia".to_owned()
    }

    #[zbus(property)]
    fn id(&self) -> String {
        "tmus".to_owned()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        self.state.lock().expect("state").title.clone()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        // Демон жив всегда, «пассивной» иконки не бывает.
        "Active".to_owned()
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.state.lock().expect("state").icon.to_owned()
    }

    // Меню не делаем: иначе хосты перехватывают левый клик под меню
    // и `Activate` не вызывается вовсе.
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    /// Кортеж SNI: имя иконки, пиксмапы (не используем — хватает
    /// тематической иконки из темы), заголовок, тело.
    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<(i32, i32, Vec<u8>)>, String, String) {
        let s = self.state.lock().expect("state");
        (
            s.icon.to_owned(),
            Vec::new(),
            s.tip_title.clone(),
            s.tip_body.clone(),
        )
    }

    /// Левый клик — пауза/продолжить. Самое частое действие на иконке.
    async fn activate(&self, _x: i32, _y: i32) {
        if let Err(e) = self.app.handle(Cmd::Toggle).await {
            tracing::warn!("toggle из трея не прошёл: {e:#}");
        }
    }

    /// Правый клик (у большинства хостов — на иконке без меню) — дальше.
    async fn secondary_activate(&self, _x: i32, _y: i32) {
        if let Err(e) = self.app.handle(Cmd::Next).await {
            tracing::warn!("next из трея не прошёл: {e:#}");
        }
    }

    /// Меню в v1 не входит: DBusMenu — отдельный протокол, тянуть его
    /// ради двух пунктов не за что.
    async fn context_menu(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "DBusMenu не поддерживается в v1".to_owned(),
        ))
    }

    /// Прокрутка на иконке — громкость. Текущее значение берём через
    /// протокол, а не из плеера: единственный законный источник
    /// состояния — [`App::handle`].
    async fn scroll(&self, delta: i32, _orientation: &str) {
        let Ok(Payload::State(st)) = self.app.handle(Cmd::State).await else {
            return;
        };
        let volume = volume_after_scroll(st.volume, delta);
        if let Err(e) = self.app.handle(Cmd::SetVolume { volume }).await {
            tracing::warn!("громкость из трея не применилась: {e:#}");
        }
    }

    #[zbus(signal)]
    async fn new_icon(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_title(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Прокси вочера. Нужен только один метод — остальной интерфейс вочера
/// (хосты, версии) демону не интересен.
#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
}

/// Полный цикл подсистемы: объект на шине, регистрация у вочера
/// с повторами, обновление свойств из шины событий.
pub async fn run(app: Arc<crate::app::App>) -> anyhow::Result<()> {
    let conn = zbus::Connection::session()
        .await
        .context("нет сессионной шины D-Bus — трей недоступен")?;

    // Шаблон имени из стандарта: хосты (включая noctalia) перебирают
    // имена `org.kde.StatusNotifierItem-*` и берут тот, что
    // зарегистрирован у вочера.
    let bus_name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());

    let sni = Sni {
        app: app.clone(),
        state: Mutex::new(SniState::from_track(None, PlaybackStatus::Stopped)),
    };
    conn.object_server().at(&ITEM_PATH, sni).await?;
    conn.request_name(bus_name.as_str()).await?;

    let iface: InterfaceRef<Sni> = conn.object_server().interface(&ITEM_PATH).await?;

    // Поток событий: трек/статус меняют свойства и будят хосты.
    // `Position` игнорируем сознательно: иконка, мигающая раз в секунду
    // на смене тултипа, — баг, а не фича.
    {
        let app = app.clone();
        let iface = iface.clone();
        tokio::spawn(async move {
            let mut rx = app.subscribe();
            loop {
                match rx.recv().await {
                    Ok(Event::TrackChanged { .. }) | Ok(Event::StateChanged { .. }) => {
                        let Ok(Payload::State(st)) = app.handle(Cmd::State).await else {
                            continue;
                        };
                        {
                            let fresh = SniState::from_track(st.track.as_ref(), st.status);
                            let s = iface.get_mut().await;
                            *s.state.lock().expect("state") = fresh;
                        }
                        // Сигналы без аргументов: хост сам перечитывает
                        // свойства, что он закэшировал.
                        let emitter = iface.signal_emitter();
                        let _ = Sni::new_icon(emitter).await;
                        let _ = Sni::new_title(emitter).await;
                        let _ = Sni::new_tool_tip(emitter).await;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("трей отстал на {n} событий — догоняем снимком позже");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    register_loop(&conn, &bus_name).await
}

/// Регистрация у вочера с бесконечными повторами.
///
/// Вочер может ещё не подняться (оболочка стартует позже демона) и
/// уйти с шины (перезапуск noctalia). Без повторов иконка пропадала бы
/// навсегда после перезагрузки оболочки. Ошибки — debug, а не error:
/// отсутствие вочера — обычное состояние, а не авария.
async fn register_loop(conn: &Connection, bus_name: &str) -> anyhow::Result<()> {
    loop {
        let watcher = StatusNotifierWatcherProxy::builder(conn).build().await;
        match watcher {
            Ok(w) => match w.register_status_notifier_item(bus_name).await {
                Ok(()) => {
                    tracing::info!("иконка в трее зарегистрирована ({bus_name})");
                    // Ждём, пока вочер исчезнет с шины, — тогда
                    // регистрируемся заново. Пока он жив, повторная
                    // регистрация не нужна.
                    wait_for_watcher_loss(conn).await;
                }
                Err(e) => tracing::debug!("вочер отклонил регистрацию: {e}"),
            },
            Err(e) => tracing::debug!("вочера ещё нет на шине: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// Блокирующе ждать исчезновения вочера: сигнал `NameOwnerChanged`
/// с пустым новым владельцем. Возврат — повод перерегистрироваться.
async fn wait_for_watcher_loss(conn: &Connection) {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
        // Нет шины имён — вернёмся в цикл регистрации, там же retry.
        return;
    };
    let Ok(mut stream) = dbus
        .receive_name_owner_changed_with_args(&[(0, "org.kde.StatusNotifierWatcher")])
        .await
    else {
        return;
    };
    while let Some(signal) = stream.next().await {
        if let Ok(args) = signal.args() {
            // Пустой (или отсутствующий) новый владелец — вочер ушёл.
            let gone = args
                .new_owner()
                .as_ref()
                .map(|n| n.as_str())
                .unwrap_or("")
                .is_empty();
            if gone {
                tracing::info!("вочер трея ушёл с шины — перерегистрируемся");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmus_core::model::{ProviderId, TrackId};

    #[test]
    fn icon_matches_all_statuses() {
        assert_eq!(icon_for(PlaybackStatus::Playing), "media-playback-start");
        assert_eq!(icon_for(PlaybackStatus::Paused), "media-playback-pause");
        assert_eq!(icon_for(PlaybackStatus::Stopped), "multimedia-player");
    }

    #[test]
    fn scroll_volume_clamped_on_both_edges() {
        assert_eq!(volume_after_scroll(50.0, -1), 55.0); // отрицательный delta — вверх
        assert_eq!(volume_after_scroll(50.0, 1), 45.0);
        assert_eq!(volume_after_scroll(100.0, -5), 100.0);
        assert_eq!(volume_after_scroll(0.0, 5), 0.0);
        assert_eq!(volume_after_scroll(98.0, -5), 100.0);
    }

    #[test]
    fn tooltip_body_combines_artist_and_album() {
        let mut track = Track {
            id: TrackId::new(ProviderId::YTMUSIC, "abc"),
            title: "Song".into(),
            artists: vec!["A".into()],
            album: Some("Album".into()),
            duration: None,
            art_url: None,
            page_url: None,
        };
        assert_eq!(tooltip_body_for(Some(&track)), "A — Album");
        track.album = None;
        assert_eq!(tooltip_body_for(Some(&track)), "A");
        track.artists.clear();
        track.album = Some("Album".into());
        assert_eq!(tooltip_body_for(Some(&track)), "Album");
        assert_eq!(tooltip_body_for(None), "");
    }

    #[test]
    fn title_falls_back_to_daemon_name() {
        assert_eq!(title_for(None), "tmus");
    }
}
