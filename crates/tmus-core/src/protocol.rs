//! Control-протокол демона: newline-delimited JSON поверх unix-сокета.
//!
//! Один протокол на всех потребителей. MPRIS, SNI-трей, Discord RPC,
//! TUI и одноразовые подкоманды `tmus` — все клиенты этого протокола;
//! прямого доступа к плееру нет ни у кого. Причина не стилистическая:
//! так добавление провайдера или фичи не требует новой точки входа в
//! плеер, и состояние не расходится между потребителями.
//!
//! Кадр — одна строка JSON, завершённая `\n`. Запрос несёт `id`; ответ
//! повторяет его. `id = 0` зарезервирован под `subscribe`: после него
//! соединение становится односторонним потоком событий.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model::{
    EqState, LoopMode, PlaybackStatus, PlaylistId, Rating, SearchKind, SearchResult, Track,
    TrackId,
};

/// `id`, после которого соединение переходит в режим потока событий.
pub const SUBSCRIBE_ID: u64 = 0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    /// `Cmd` сам несёт тег `cmd`, поэтому поле раскрывается в тот же
    /// объект: кадр выглядит как `{"id":7,"cmd":"next"}`. Без `flatten`
    /// получалось бы `{"id":7,"cmd":{"cmd":"next"}}` — лишний уровень,
    /// который пришлось бы знать каждому клиенту, включая Luau-плагин.
    #[serde(flatten)]
    pub cmd: Cmd,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Ok { id: u64, ok: Payload },
    Err { id: u64, err: String },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogSource {
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Cmd {
    /// Перевести соединение в поток событий.
    Subscribe,

    // --- воспроизведение ---
    PlayTrack { track: TrackId },
    /// Поставить плейлист в очередь целиком; `start` — индекс трека,
    /// с которого начать.
    PlayPlaylist { playlist: PlaylistId, start: Option<usize> },
    Toggle,
    Play,
    Pause,
    Stop,
    Next,
    Prev,
    /// Абсолютная позиция от начала трека.
    Seek { position: Duration },
    /// Относительный сдвиг в секундах; отрицательный — назад.
    SeekBy { delta: f64 },
    SetVolume { volume: f64 },
    /// Инкрементальное обновление эквалайзера: `None` — поле не менять.
    ///
    /// Почему Option-поля, а не отдельные команды: клиент (TUI-слайдер,
    /// CLI-однострочник) обычно правит одно поле за раз, и при трёх
    /// отдельных командах демону пришлось бы блокировать состояние
    /// трижды либо рисковать разъехавшимся `(enabled, preset, bands)`.
    /// Один кадр — одна атомарная правка.
    Equalizer {
        enabled: Option<bool>,
        preset: Option<String>,
        /// Усиления полос в дБ; короче/длиннее 10 значений — ошибка демона.
        bands: Option<Vec<f64>>,
    },
    SetLoop { mode: LoopMode },
    SetShuffle { shuffle: bool },

    // --- очередь ---
    Queue,
    QueueAppend { tracks: Vec<TrackId> },
    QueueClear,
    /// Перейти к треку очереди по индексу.
    QueueGoto { index: usize },

    // --- состояние и каталог ---
    State,
    /// Провайдеры и состояние их авторизации.
    Providers,
    /// `provider = None` — искать во всех подключённых сразу.
    Search { query: String, kind: SearchKind, provider: Option<String> },
    /// Плейлисты библиотеки; `provider = None` — из всех.
    Library { provider: Option<String> },
    LibraryTracks { playlist: PlaylistId },
    Liked { provider: Option<String> },
    /// Поставить оценку треку: провайдер получает лайк/дизлайк, демон
    /// сохраняет её локально и рассылает [`Event::RatingChanged`].
    Rate { track: TrackId, rating: Rating },
    /// Все известные локально оценки. Провайдеры не опрашиваются:
    /// источник истины — локальное хранилище демона.
    Ratings,
    GetCatalogSource,
    SetCatalogSource { source: CatalogSource },

    // --- офлайн-кэш ---
    CacheStats,
    CachePin { tracks: Vec<TrackId> },
    CacheUnpin { tracks: Vec<TrackId> },
    /// Вытеснить всё незакреплённое сверх лимита.
    CacheGc,
    /// Докачать список треков в офлайн-кэш в фоне. Демон сразу
    /// отвечает `Ack`, прогресс идёт потоком `CacheProgress`.
    CacheWarm { tracks: Vec<TrackId> },

    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Payload {
    Ack(Ack),
    State(PlayerState),
    Queue(QueueView),
    // `Results` обязан идти раньше `Tracks`: элементы поиска несут тег
    // `kind`, поэтому `Vec<SearchResult>` в untagged-enum иначе
    // ошибочно декодируется как `Vec<Track>`.
    Results(Vec<SearchResult>),
    // `Ratings` — по той же причине раньше `Tracks`: пара
    // `(TrackId, Rating)` в JSON — массив из двух элементов, а serde
    // умеет разбирать структуру и из массива (по порядку полей), так
    // что пары иначе уходят в `Tracks`.
    Ratings(Vec<(TrackId, Rating)>),
    Tracks(Vec<Track>),
    Playlists(Vec<crate::model::Playlist>),
    Providers(Vec<ProviderView>),
    Cache(CacheStats),
    Catalog(CatalogSource),
}

/// Ответ на команду без данных. Отдельный тип вместо `null`, чтобы
/// клиент отличал «сделано» от «поле отсутствует».
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Ack {
    pub done: bool,
}

impl Default for Ack {
    fn default() -> Self {
        Self { done: true }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlayerState {
    pub status: PlaybackStatus,
    #[serde(default)]
    pub track: Option<Track>,
    #[serde(default)]
    pub position: Option<Duration>,
    #[serde(default)]
    pub duration: Option<Duration>,
    pub volume: f64,
    pub loop_mode: LoopMode,
    pub shuffle: bool,
    /// Индекс текущего трека в очереди.
    #[serde(default)]
    pub queue_index: Option<usize>,
    pub queue_len: usize,
    /// Играем из офлайн-кэша, а не из сети.
    pub offline: bool,
    /// Состояние эквалайзера. `default` нужен, чтобы снапшоты и клиенты,
    /// написанные до появления эквалайзера, продолжали парситься.
    #[serde(default)]
    pub equalizer: EqState,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueueView {
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub index: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderView {
    pub id: String,
    pub name: String,
    pub auth: crate::model::AuthStatus,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CacheStats {
    pub tracks: u64,
    pub bytes: u64,
    pub limit_bytes: u64,
    pub pinned_tracks: u64,
    pub pinned_bytes: u64,
}

/// События потока. Отправляются всем подписчикам; порядок сохраняется
/// в пределах одного подписчика.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    TrackChanged { track: Option<Track>, queue_index: Option<usize> },
    /// Частое событие: подписчики, которым нужен только трек, его
    /// игнорируют.
    Position { position: Duration, duration: Option<Duration> },
    StateChanged { state: PlayerState },
    QueueChanged { len: usize, index: Option<usize> },
    /// Прогресс докачивания в офлайн-кэш.
    CacheProgress { track: TrackId, bytes: u64, total: Option<u64> },
    /// Авторизация провайдера отвалилась — повод показать это в баре,
    /// а не молча ловить «bot check».
    AuthChanged { provider: String, auth: crate::model::AuthStatus },
    /// Оценка трека изменилась (локально или на стороне провайдера) —
    /// клиенты перерисовывают значок лайка/дизлайка.
    RatingChanged { track: TrackId, rating: Rating },
}

/// Обёртка кадра события: событие всегда приходит отдельным объектом,
/// без `id`, поэтому клиент различает ответ и событие по наличию `id`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Frame {
    Response(Response),
    Event(Event),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProviderId;

    fn line<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).expect("serialize")
    }

    #[test]
    fn request_is_a_flat_tagged_object() {
        let got = line(&Request { id: 7, cmd: Cmd::Next });
        assert_eq!(got, r#"{"id":7,"cmd":"next"}"#);
    }

    #[test]
    fn commands_with_fields_stay_flat() {
        let got = line(&Request { id: 1, cmd: Cmd::SeekBy { delta: -5.0 } });
        assert_eq!(got, r#"{"id":1,"cmd":"seek_by","delta":-5.0}"#);
    }

    #[test]
    fn frame_tells_response_from_event() {
        let response = line(&Response::Err { id: 3, err: "no such track".into() });
        let event = line(&Event::QueueChanged { len: 2, index: Some(0) });

        match serde_json::from_str::<Frame>(&response).expect("parse response") {
            Frame::Response(Response::Err { id, err }) => {
                assert_eq!(id, 3);
                assert_eq!(err, "no such track");
            }
            other => panic!("expected an error response, got {other:?}"),
        }
        match serde_json::from_str::<Frame>(&event).expect("parse event") {
            Frame::Event(Event::QueueChanged { len, index }) => {
                assert_eq!(len, 2);
                assert_eq!(index, Some(0));
            }
            other => panic!("expected a queue event, got {other:?}"),
        }
    }

    #[test]
    fn catalog_source_defaults_to_all() {
        let all = serde_json::from_str::<CatalogSource>("{}").expect("parse");
        assert_eq!(all, CatalogSource { provider: None });
        let saved = serde_json::from_str::<CatalogSource>(r#"{"provider":"soundcloud"}"#).expect("parse");
        assert_eq!(saved.provider.as_deref(), Some("soundcloud"));
    }

    #[test]
    fn catalog_source_commands_stay_flat() {
        let get = line(&Request { id: 2, cmd: Cmd::GetCatalogSource });
        assert_eq!(get, r#"{"id":2,"cmd":"get_catalog_source"}"#);

        let set = line(&Request {
            id: 3,
            cmd: Cmd::SetCatalogSource { source: CatalogSource { provider: Some("ytmusic".into()) } },
        });
        assert_eq!(set, r#"{"id":3,"cmd":"set_catalog_source","source":{"provider":"ytmusic"}}"#);
    }

    #[test]
    fn catalog_payload_roundtrips() {
        let response = line(&Response::Ok { id: 5, ok: Payload::Catalog(CatalogSource { provider: None }) });
        match serde_json::from_str::<Frame>(&response).expect("parse response") {
            Frame::Response(Response::Ok { id, ok: Payload::Catalog(source) }) => {
                assert_eq!(id, 5);
                assert_eq!(source, CatalogSource { provider: None });
            }
            other => panic!("expected a catalog response, got {other:?}"),
        }
    }

    #[test]
    fn cache_payload_stays_cache_despite_catalog_variant() {
        let stats = CacheStats {
            tracks: 12,
            bytes: 4096,
            limit_bytes: 1 << 30,
            pinned_tracks: 2,
            pinned_bytes: 512,
        };
        let response = line(&Response::Ok { id: 9, ok: Payload::Cache(stats) });
        match serde_json::from_str::<Frame>(&response).expect("parse response") {
            Frame::Response(Response::Ok { id, ok: Payload::Cache(stats) }) => {
                assert_eq!(id, 9);
                assert_eq!(stats.tracks, 12);
            }
            other => panic!("cache payload must not degrade to Catalog, got {other:?}"),
        }
    }

    #[test]
    fn rate_command_stays_flat() {
        let track = TrackId::new(ProviderId::YTMUSIC, "abc");
        let got = line(&Request { id: 4, cmd: Cmd::Rate { track, rating: Rating::Liked } });
        assert_eq!(
            got,
            r#"{"id":4,"cmd":"rate","track":{"provider":"ytmusic","id":"abc"},"rating":"liked"}"#
        );

        let list = line(&Request { id: 5, cmd: Cmd::Ratings });
        assert_eq!(list, r#"{"id":5,"cmd":"ratings"}"#);
    }

    #[test]
    fn ratings_payload_roundtrips() {
        // Tuple-vec сериализуется как массив пар — фиксируем форму:
        let pairs = vec![
            (TrackId::new(ProviderId::YTMUSIC, "abc"), Rating::Liked),
            (TrackId::new(ProviderId::SOUNDCLOUD, "xyz"), Rating::Disliked),
        ];
        let response = line(&Response::Ok { id: 6, ok: Payload::Ratings(pairs.clone()) });
        assert!(
            response.contains(r#""ok":[["#), "tuple-vec must stay a JSON array of pairs: {response}"
        );
        match serde_json::from_str::<Frame>(&response).expect("parse response") {
            Frame::Response(Response::Ok { id, ok: Payload::Ratings(back) }) => {
                assert_eq!(id, 6);
                assert_eq!(back, pairs);
            }
            other => panic!("expected a ratings payload, got {other:?}"),
        }
    }

    #[test]
    fn rating_changed_event_is_tagged() {
        let track = TrackId::new(ProviderId::YTMUSIC, "abc");
        let event = line(&Event::RatingChanged { track, rating: Rating::Disliked });
        let tag = r#""event":"rating_changed""#;
        assert!(event.contains(tag), "event tag must be rating_changed: {event}");
        match serde_json::from_str::<Frame>(&event).expect("parse event") {
            Frame::Event(Event::RatingChanged { track, rating }) => {
                assert_eq!(rating, Rating::Disliked);
                assert_eq!(track.to_string(), "ytmusic:abc");
            }
            other => panic!("expected a rating event, got {other:?}"),
        }
    }

    #[test]
    fn equalizer_command_serializes_flat_with_nulls_for_none() {
        // Фиксируем фактическую форму контракта: serde для Option-полей
        // без skip_serializing_if пишет null, а не выбрасывает ключ.
        // Полный кадр — все три поля:
        let full = line(&Request {
            id: 11,
            cmd: Cmd::Equalizer {
                enabled: Some(true),
                preset: Some("Rock".into()),
                bands: Some(vec![5.0, 4.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 4.0, 5.0]),
            },
        });
        assert_eq!(
            full,
            r#"{"id":11,"cmd":"equalizer","enabled":true,"preset":"Rock","bands":[5.0,4.0,2.0,0.0,-1.0,-1.0,0.0,2.0,4.0,5.0]}"#
        );

        // Частичное обновление: незаданные поля остаются на месте с null.
        let partial = line(&Request { id: 12, cmd: Cmd::Equalizer { enabled: Some(false), preset: None, bands: None } });
        assert_eq!(partial, r#"{"id":12,"cmd":"equalizer","enabled":false,"preset":null,"bands":null}"#);

        // Обратный разбор: ключи можно опускать целиком — это None.
        let parsed: Request = serde_json::from_str(r#"{"id":13,"cmd":"equalizer","bands":[1.0,2.0]}"#).expect("parse");
        assert_eq!(
            parsed.cmd,
            Cmd::Equalizer { enabled: None, preset: None, bands: Some(vec![1.0, 2.0]) }
        );
    }

    #[test]
    fn player_state_parses_with_and_without_equalizer() {
        let without: PlayerState = serde_json::from_str(
            r#"{"status":"stopped","volume":70.0,"loop_mode":"none","shuffle":false,"queue_len":0,"offline":false}"#,
        )
        .expect("старый снапшот без equalizer обязан парситься");
        assert_eq!(without.equalizer, EqState::default());

        let with: PlayerState = serde_json::from_str(
            r#"{"status":"playing","volume":50.0,"loop_mode":"track","shuffle":true,"queue_len":3,"offline":false,"equalizer":{"enabled":true,"preset":"Bass Boost","bands":[6.0,5.0,4.0,2.0,0.0,0.0,0.0,0.0,0.0,0.0]}}"#,
        )
        .expect("снапшот с equalizer обязан парситься");
        assert!(with.equalizer.enabled);
        assert_eq!(with.equalizer.preset, "Bass Boost");
        assert_eq!(with.equalizer.bands[0], 6.0);
    }

    #[test]
    fn frames_carry_no_embedded_newline() {
        let event = line(&Event::Position {
            position: Duration::from_millis(1500),
            duration: Some(Duration::from_secs(300)),
        });
        assert!(!event.contains('\n'), "framing is newline-delimited: {event}");
    }

    fn track(id: &str) -> Track {
        Track {
            id: TrackId { provider: crate::model::ProviderId::YTMUSIC, id: id.into() },
            title: format!("track {id}"),
            artists: vec!["artist".into()],
            album: None,
            duration: None,
            art_url: None,
            page_url: None,
        }
    }

    #[test]
    fn tagged_search_results_stay_results_and_plain_tracks_stay_tracks() {
        let results = Payload::Results(vec![SearchResult::Track(track("a")), SearchResult::Track(track("b"))]);
        let wire = line(&results);
        match serde_json::from_str::<Payload>(&wire).expect("parse results") {
            Payload::Results(items) => {
                assert_eq!(items.len(), 2);
                assert!(items.iter().all(|r| matches!(r, SearchResult::Track(_))),
                    "tagged search array must decode as Results, not Tracks");
            }
            other => panic!("search results degraded to {other:?}"),
        }

        let tracks = Payload::Tracks(vec![track("c")]);
        let wire = line(&tracks);
        match serde_json::from_str::<Payload>(&wire).expect("parse tracks") {
            Payload::Tracks(items) => assert_eq!(items.len(), 1, "plain track array must stay Tracks"),
            other => panic!("plain tracks degraded to {other:?}"),
        }
        assert_eq!(line(&Payload::Tracks(vec![track("c")])), wire, "wire shape is unchanged");
    }
}
