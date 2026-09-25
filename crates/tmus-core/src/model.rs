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

/// Идентификатор стабильного пространства источника: `"ytmusic"`,
/// `"soundcloud"`, `"local"`, …
///
/// Строка, а не enum: типы, на которые смотрят плеер, демон и TUI, не
/// обязаны знать список источников, и `match` по нему в них запрещён.
///
/// `local` — пространство имён локальных коллекций приложения, а не
/// зарегистрированный аккаунт или внешний провайдер.
///
/// Почему `&'static str`, а разбор строки — через [`ProviderId::ALL`].
/// Провайдер — это крейт, то есть сущность времени компиляции; хранить
/// его имя в `String` значило бы платить аллокацией в каждом `TrackId`,
/// а их в очереди тысячи. Обратная сторона: имя, пришедшее из JSON или
/// из SQLite, нельзя превратить в `&'static str` иначе как сверкой с
/// литералом. `Box::leak` здесь запрещён — на неизвестном имени из
/// внешних данных он течёт без предела.
///
/// Цена решения — одна строка в [`ProviderId::ALL`] на новый источник.
/// Это единственное место в ядре, которое источник правит о себе.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProviderId(pub &'static str);

impl ProviderId {
    pub const LOCAL: Self = Self("local");
    pub const YTMUSIC: Self = Self("ytmusic");
    pub const SOUNDCLOUD: Self = Self("soundcloud");

    /// Все известные источники, включая локальное пространство приложения.
    pub const ALL: &'static [Self] = &[Self::LOCAL, Self::YTMUSIC, Self::SOUNDCLOUD];

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

/// Центральные частоты полос эквалайзера, Гц.
///
/// ISO-октавный ряд: каждая следующая полоса вдвое больше предыдущей,
/// поэтому усиление распределяется по спектру равномерно в
/// логарифмической шкале — так слышит человек. Десять полос покрывают
/// весь слышимый диапазон от нижнего баса до верхней воздушной
/// полки; именно эти значения mpv-эквалайзер и клиенты показывают
/// подписями полос.
pub const EQ_FREQUENCIES_HZ: [u32; 10] =
    [32, 64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];

/// Максимальное усиление полосы эквалайзера, дБ. Дальше клиппинг.
pub const EQ_GAIN_LIMIT_DB: f64 = 15.0;

/// Состояние эквалайзера: включённость, выбранный пресет и усиления
/// десяти полос в дБ по [`EQ_FREQUENCIES_HZ`].
///
/// `preset` — строка, а не enum: пресеты описаны в ядре таблицей
/// [`eq_presets`], но пользовательский (набранный вручную) набор
/// полос не обязан совпадать ни с одним из них, и хранить его пришлось
/// бы как отдельный вариант enum во всех слоях. Строка `"Custom"` —
/// дешевле. `bands` — массив фиксированной длины: число полос
/// зашито в протокол и в mpv-фильтр, `Vec` добавил бы аллокацию
/// без гибкости.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EqState {
    /// Применять ли эквалайзер к воспроизведению. Выключенный
    /// эквалайзер сохраняет полосы — пользователь ожидает вернуть
    /// настройку, а не набирать её заново.
    pub enabled: bool,
    /// Имя последнего применённого пресета из [`eq_presets`] либо
    /// `"Custom"`, если полосы правились вручную.
    pub preset: String,
    /// Усиления полос в дБ, диапазон −15..=+15
    /// ([`EQ_GAIN_LIMIT_DB`]). Индекс соответствует
    /// [`EQ_FREQUENCIES_HZ`].
    pub bands: [f64; 10],
}

impl Default for EqState {
    fn default() -> Self {
        Self { enabled: false, preset: "Flat".to_owned(), bands: [0.0; 10] }
    }
}

/// Таблица типовых пресетов эквалайзера: `(имя, усиления полос в дБ)`.
///
/// Значения — распространённые кривые для десятиполосной схемы; ядро
/// не обязано знать, какой провайдер или фильтр их применит. Возвращается
/// срез статических данных: чтение не аллоцирует, а клиенты (TUI-меню,
/// CLI-подсказка) могут перечислить пресеты без копирования.
#[must_use]
pub fn eq_presets() -> &'static [(&'static str, [f64; 10])] {
    &[
        ("Flat", [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        ("Bass Boost", [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        ("Vocal Boost", [-2.0, -2.0, -1.0, 0.0, 4.0, 5.0, 4.0, 2.0, 0.0, -1.0]),
        ("Pop", [-1.0, 1.0, 3.0, 4.0, 3.0, 1.0, -1.0, -1.0, 2.0, 2.0]),
        ("Rock", [5.0, 4.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 4.0, 5.0]),
        ("Hip-Hop", [6.0, 5.0, 3.0, 1.0, -1.0, -1.0, 1.0, 2.0, 3.0, 3.0]),
        ("Jazz", [3.0, 2.0, 1.0, 0.0, -1.0, -1.0, 0.0, 1.0, 2.0, 3.0]),
        ("Classical", [4.0, 3.0, 2.0, 1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0]),
        ("Electronic", [5.0, 4.0, 2.0, 0.0, -2.0, -1.0, 0.0, 2.0, 4.0, 5.0]),
        ("Acoustic", [3.0, 2.0, 1.0, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 2.0]),
        ("Deep Bass", [8.0, 6.0, 3.0, 0.0, -2.0, -2.0, -1.0, 0.0, 0.0, 0.0]),
        ("Bright", [-2.0, -1.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 5.0]),
        ("Crisp", [-1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 4.0]),
        ("Live", [-2.0, 0.0, 2.0, 3.0, 3.0, 3.0, 2.0, 1.0, 2.0, 2.0]),
        ("Headphones", [3.0, 4.0, 3.0, 1.0, -1.0, -1.0, 0.0, 2.0, 3.0, 4.0]),
        ("Small Speakers", [6.0, 5.0, 4.0, 2.0, 1.0, 0.0, 0.0, 1.0, 2.0, 2.0]),
        ("Laptop", [5.0, 4.0, 3.0, 1.0, 0.0, 0.0, 1.0, 2.0, 3.0, 3.0]),
        ("Car", [4.0, 3.0, 1.0, 0.0, -1.0, -1.0, 0.0, 2.0, 4.0, 5.0]),
        ("Earbuds", [4.0, 3.0, 2.0, 1.0, 0.0, 0.0, 1.0, 2.0, 3.0, 3.0]),
        ("Podcast", [-3.0, -2.0, 0.0, 2.0, 4.0, 5.0, 4.0, 2.0, 0.0, -2.0]),
    ]
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

/// Оценка трека: лайк, дизлайк или её отсутствие.
///
/// `Default = None` нужен, чтобы история без оценок не писала в SQLite
/// и в JSON ничего лишнего: отсутствие оценки — нормальное состояние
/// большинства треков, а не отдельный признак. `Copy` — оценка
/// передаётся по значению в протоколе и в провайдер, клонировать её
/// незачем. Имена вариантов в JSON — `snake_case`, как у остальных
/// перечислений протокола.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rating {
    #[default]
    None,
    Liked,
    Disliked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchResult {
    Track(Track),
    Playlist(Playlist),
    Artist { provider: ProviderId, id: String, name: String },
}

/// Полка рекомендаций домашней ленты. Элементы переиспользуют
/// [`SearchResult`]: виды ровно те же, что в поиске, и панель/remember()
/// уже работают с ними.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CatalogShelf {
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    pub items: Vec<SearchResult>,
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
    fn eq_presets_are_well_formed() {
        let presets = eq_presets();
        assert!(presets.iter().any(|(name, _)| *name == "Flat"), "Flat обязан существовать");

        for (name, bands) in presets {
            assert_eq!(bands.len(), EQ_FREQUENCIES_HZ.len(), "{name}: полос не 10");
            for (i, gain) in bands.iter().enumerate() {
                assert!(
                    (-EQ_GAIN_LIMIT_DB..=EQ_GAIN_LIMIT_DB).contains(gain),
                    "{name}: полоса {} ({}) Гц вне −15..=15 дБ: {gain}",
                    i,
                    EQ_FREQUENCIES_HZ[i],
                );
            }
        }

        let (flat, bands) = presets.iter().find(|(name, _)| *name == "Flat").expect("Flat");
        assert_eq!(*flat, "Flat");
        assert!(bands.iter().all(|gain| *gain == 0.0), "Flat — нули");
    }

    #[test]
    fn eq_state_default_is_flat_and_disabled() {
        let state = EqState::default();
        assert!(!state.enabled);
        assert_eq!(state.preset, "Flat");
        assert_eq!(state.bands, [0.0; 10]);
    }

    #[test]
    fn eq_state_round_trips_through_json() {
        let state = EqState {
            enabled: true,
            preset: "Rock".to_owned(),
            bands: eq_presets().iter().find(|(n, _)| *n == "Rock").expect("Rock").1,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let back: EqState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);
    }

    #[test]
    fn rating_serializes_with_snake_case_names() {
        // Имена вариантов — часть контракта протокола и схемы SQLite:
        // клиент и демон должны видеть ровно "none"/"liked"/"disliked".
        assert_eq!(serde_json::to_string(&Rating::None).expect("serialize"), r#""none""#);
        assert_eq!(serde_json::to_string(&Rating::Liked).expect("serialize"), r#""liked""#);
        assert_eq!(serde_json::to_string(&Rating::Disliked).expect("serialize"), r#""disliked""#);
        assert_eq!(Rating::default(), Rating::None);
    }

    #[test]
    fn track_ids_are_namespaced_by_provider() {
        let a = TrackId::new(ProviderId::YTMUSIC, "abc");
        let b = TrackId::new(ProviderId::SOUNDCLOUD, "abc");
        assert_ne!(a, b);
        assert_eq!(a.to_string(), "ytmusic:abc");
    }
}
