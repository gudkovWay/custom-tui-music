//! Провайдер-независимые типы. Всё, что пересекает границу крейта
//! провайдера, выражено здесь и только здесь.
//!
//! Правило проекта: ни одна структура ответа InnerTube / yt-dlp /
//! SoundCloud не покидает свой крейт. Эти API неофициальны и меняются
//! молча; если их типы протекут в плеер, MPRIS или TUI — ломаться будет
//! всё сразу, а не один провайдер.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Идентификатор провайдера: `"ytmusic"`, `"soundcloud"`, `"spotify"`, …
///
/// Строка, а не enum: типы, на которые смотрят плеер, демон и TUI, не
/// обязаны знать список провайдеров, и `match` по нему в них запрещён.
///
/// Почему `&'static str`, а разбор строки — через [`ProviderId::ALL`].
/// Провайдер — это крейт, то есть сущность времени компиляции; хранить
/// его имя в `String` значило бы платить аллокацией в каждом `TrackId`,
/// а их в очереди тысячи. Обратная сторона: имя, пришедшее из JSON или
/// из SQLite, нельзя превратить в `&'static str` иначе как сверкой с
/// литералом. `Box::leak` здесь запрещён — на неизвестном имени из
/// внешних данных он течёт без предела.
///
/// Цена решения — одна строка в [`ProviderId::ALL`] на новый провайдер.
/// Это единственное место в ядре, которое провайдер правит о себе.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProviderId(pub &'static str);

impl ProviderId {
    pub const YTMUSIC: Self = Self("ytmusic");
    pub const SOUNDCLOUD: Self = Self("soundcloud");

    /// Все известные ядру провайдеры. Служит таблицей разбора имён из
    /// внешних данных — протокола и базы.
    pub const ALL: &'static [Self] = &[Self::YTMUSIC, Self::SOUNDCLOUD];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    /// Разобрать имя. Сравнение точное: `"YTMusic"` — не `"ytmusic"`,
    /// потому что это же имя служит ключом в SQLite и в путях кэша, и
    /// регистронезависимое совпадение развело бы одну запись на две.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|known| known.0 == name)
    }
}

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let name = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Self::from_name(&name).ok_or_else(|| {
            let known = Self::ALL
                .iter()
                .map(|p| p.0)
                .collect::<Vec<_>>()
                .join(", ");
            D::Error::custom(format!(
                "неизвестный провайдер {name:?}; известны: {known}"
            ))
        })
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl fmt::Debug for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProviderId({})", self.0)
    }
}

/// Трек, адресуемый в пределах провайдера.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct TrackId {
    pub provider: ProviderId,
    pub id: String,
}

impl TrackId {
    pub fn new(provider: ProviderId, id: impl Into<String>) -> Self {
        Self { provider, id: id.into() }
    }
}

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider, self.id)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct PlaylistId {
    pub provider: ProviderId,
    pub id: String,
}

impl PlaylistId {
    pub fn new(provider: ProviderId, id: impl Into<String>) -> Self {
        Self { provider, id: id.into() }
    }
}

impl fmt::Display for PlaylistId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider, self.id)
    }
}

/// Единица воспроизведения. Достаточна и для MPRIS, и для Discord RPC,
/// и для отрисовки в TUI — эти потребители не знают о провайдерах.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub title: String,
    /// Может быть пустым: у части источников артист не отделён от
    /// названия. Потребители обязаны это выдерживать, а не
    /// разыменовывать первый элемент.
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration: Option<Duration>,
    /// Удалённый URL обложки. Локальный файл не нужен: noctalia тянет
    /// обложку своим HttpClient из `mpris:artUrl`.
    #[serde(default)]
    pub art_url: Option<String>,
    /// Страница трека у провайдера — уходит в `xesam:url`.
    #[serde(default)]
    pub page_url: Option<String>,
}

impl Track {
    /// Артисты одной строкой. Пусто, когда провайдер их не отдал.
    #[must_use]
    pub fn artist_line(&self) -> String {
        self.artists.join(", ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: PlaylistId,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub art_url: Option<String>,
    /// `None`, когда провайдер не сообщил размер в списке плейлистов.
    #[serde(default)]
    pub track_count: Option<u32>,
}

/// Откуда играть. `Local` возвращается, когда трек уже лежит в
/// офлайн-кэше, и тогда провайдер не опрашивается вовсе.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamSource {
    Local(PathBuf),
    Remote {
        url: String,
        /// Единственный заголовок, который разрешено передавать в mpv.
        ///
        /// `mpv --http-header-fields` разрезает значения по запятым, а
        /// `Accept` от yt-dlp содержит запятые — запрос распадается на
        /// мусорные поля и googlevideo отвечает `400 Bad Request`, тогда
        /// как тот же URL в curl даёт `206`. Замерено 17.09.2026.
        user_agent: Option<String>,
        /// У googlevideo-ссылок есть `expire=` (~6 ч). Поэтому URL
        /// никогда не кэшируется — кэшируются метаданные и файлы.
        expires_at: Option<SystemTime>,
    },
}

impl StreamSource {
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// Истёк ли резолв. Локальный файл не истекает никогда.
    #[must_use]
    pub fn is_expired(&self, now: SystemTime) -> bool {
        match self {
            Self::Local(_) => false,
            Self::Remote { expires_at, .. } => expires_at.is_some_and(|at| at <= now),
        }
    }
}

/// Состояние авторизации провайдера.
///
/// `Expired` и `Missing` разделены намеренно: чинят их по-разному, а
/// сообщение «Sign in to confirm you're not a bot» от yt-dlp не
/// различает эти случаи вовсе — именно на этом спотыкаются все готовые
/// клиенты.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthStatus {
    /// Сессия есть и рабочая.
    Ready,
    /// Учётные данные не найдены — нужно подключить аккаунт.
    Missing { hint: String },
    /// Учётные данные найдены, но отвергнуты провайдером.
    Expired { hint: String },
    /// Провайдер работает без авторизации (публичный каталог).
    Anonymous,
}

impl AuthStatus {
    #[must_use]
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Ready | Self::Anonymous)
    }
}

/// Что искать. Провайдер, не умеющий вид, возвращает пустой результат,
/// а не ошибку.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    Tracks,
    Albums,
    Artists,
    Playlists,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchResult {
    Track(Track),
    Playlist(Playlist),
    Artist { provider: ProviderId, id: String, name: String },
}

/// Режим повтора. Единый для всех провайдеров: очередь смешанная.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopMode {
    #[default]
    None,
    Track,
    Queue,
}

impl LoopMode {
    /// Значение для MPRIS-свойства `LoopStatus`.
    #[must_use]
    pub const fn as_mpris(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Track => "Track",
            Self::Queue => "Playlist",
        }
    }

    #[must_use]
    pub fn from_mpris(value: &str) -> Option<Self> {
        match value {
            "None" => Some(Self::None),
            "Track" => Some(Self::Track),
            "Playlist" => Some(Self::Queue),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl PlaybackStatus {
    /// Значение для MPRIS-свойства `PlaybackStatus`.
    #[must_use]
    pub const fn as_mpris(self) -> &'static str {
        match self {
            Self::Playing => "Playing",
            Self::Paused => "Paused",
            Self::Stopped => "Stopped",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpris_loop_names_round_trip() {
        for mode in [LoopMode::None, LoopMode::Track, LoopMode::Queue] {
            assert_eq!(LoopMode::from_mpris(mode.as_mpris()), Some(mode));
        }
        assert_eq!(LoopMode::from_mpris("Nonsense"), None);
    }

    #[test]
    fn local_stream_never_expires() {
        let local = StreamSource::Local(PathBuf::from("/tmp/a.webm"));
        assert!(!local.is_expired(SystemTime::UNIX_EPOCH));
        assert!(local.is_local());
    }

    #[test]
    fn remote_stream_expires_at_deadline() {
        let deadline = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let remote = StreamSource::Remote {
            url: "https://example/v".into(),
            user_agent: None,
            expires_at: Some(deadline),
        };
        assert!(!remote.is_expired(deadline - Duration::from_secs(1)));
        assert!(remote.is_expired(deadline));
    }

    #[test]
    fn track_ids_are_namespaced_by_provider() {
        let a = TrackId::new(ProviderId::YTMUSIC, "abc");
        let b = TrackId::new(ProviderId::SOUNDCLOUD, "abc");
        assert_ne!(a, b);
        assert_eq!(a.to_string(), "ytmusic:abc");
    }
}
