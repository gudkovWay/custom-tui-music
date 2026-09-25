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
    CatalogShelf, EqState, LoopMode, PlaybackStatus, PlaylistId, Rating, SearchKind, SearchResult,
    Track, TrackId,
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
    /// Поставить плейлист в очередь целиком, начиная с выбранного стабильного id.
    PlayPlaylist { playlist: PlaylistId, #[serde(default, skip_serializing_if = "Option::is_none")] track: Option<TrackId> },
    /// Играть готовый список треков как новый контекст: очередь
    /// заменяется списком, `start` — индекс трека, с которого начать.
    /// Клиент сам владеет списком (страница поиска, лайк), поэтому
    /// плейлист демоном не читается.
    PlayContext { tracks: Vec<TrackId>, start: usize },
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
    /// Перечитать cookies и проверить сессии у провайдеров.
    /// `None` — у всех. Ответ [`Payload::Ack`]; результаты приходят
    /// событиями [`Event::AuthChanged`].
    RefreshAuth { provider: Option<String> },
    /// Провайдеры и состояние их авторизации.
    Providers,
    /// `provider = None` — искать во всех подключённых сразу.
    Search { query: String, kind: SearchKind, provider: Option<String> },
    /// Плейлисты библиотеки; `provider = None` — из всех.
    Library { provider: Option<String> },
    LibraryTracks { playlist: PlaylistId },
    /// Следующая страница состава плейлиста по сохранённому курсору
    /// догрузки. Ответ [`Payload::TracksPage`]; пустая страница с
    /// `next: None` значит «дочитано».
    LibraryTracksPage { playlist: PlaylistId },
    Liked { provider: Option<String> },
    /// Домашняя лента рекомендаций; provider = None — из всех.
    Home { provider: Option<String> },
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

    // --- плейлисты ---
    /// Создать пустой приватный плейлист. Полей приватности здесь нет
    /// сознательно: провайдер сам решает, каким создаёт плейлист по
    /// умолчанию (YouTube Music — `PRIVATE`, чтобы пользовательский
    /// выбор приватности не приходилось тащить через весь протокол).
    /// Демон отвечает [`Payload::PlaylistCreated`] с новым id.
    PlaylistCreate {
        title: String,
        /// Адресат создания: None значит «выбрать по правилу демона» —
        /// сохранённый источник каталога, иначе первый подключённый.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    /// Добавить трек в плейлист. И плейлист, и трек нужны явно: одна и
    /// та же операция осмысленна для любого плейлиста библиотеки, а
    /// «текущего плейлиста» в демоне нет — он играет очередь.
    PlaylistAdd { playlist: PlaylistId, track: TrackId },
    /// Убрать трек из плейлиста. Плейлист указывается по той же
    /// причине, что и в `PlaylistAdd`, — команда не привязана к тому,
    /// что сейчас играет или открыто на экране.
    PlaylistRemove { playlist: PlaylistId, track: TrackId },
    /// Удалить плейлист целиком вместе с содержимым.
    PlaylistDelete { playlist: PlaylistId },

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
    /// Страница ленивой догрузки состава: свежие треки и курсор
    /// следующей страницы. `next: None` — плейлист дочитан. Безопасно
    /// после `Tracks` (объект ≠ массив) и после `Queue`: у `QueueView`
    /// стоит `deny_unknown_fields`, поэтому поле `next` не даёт ответу
    /// догрузки задекодироваться как зеркало очереди — без этого
    /// untagged-перебор молча съедал страницу в `Queue` (замер 20.09:
    /// клиент видел `index` вместо `next`).
    TracksPage { tracks: Vec<Track>, next: Option<String> },
    Playlists(Vec<crate::model::Playlist>),
    // Полка домашней ленты безопасна после Playlists: {title, subtitle,
    // items} структурно не матчится ни с Results (нет тега kind), ни с
    // Tracks/Playlists (нет обязательного id), ни с Ratings (не пары).
    Home(Vec<CatalogShelf>),
    /// Ответ на `PlaylistCreate`: id нового плейлиста, по которому его
    /// можно сразу пополнять и открывать.
    PlaylistCreated { playlist: PlaylistId },
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
    /// Причина последнего сбоя воспроизведения (HTTP 403 от googlevideo
    /// и т.п.). Ставится при асинхронном отказе mpv (`end-file` с
    /// reason вне eof/stop/quit), сбрасывается при успешной загрузке
    /// следующего трека. `default` — чтобы старые клиенты без знания об
    /// этом поле продолжали разбирать снапшоты, как с equalizer.
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Бейдж провайдера из `Account::glyph/color` — клиенты рисуют
    /// иконку клиента, не зная список провайдеров.
    pub glyph: String,
    pub color: String,
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
    /// Состав плейлистов изменился (создан, удалён, пополнен) —
    /// получатель сам перечитывает список через `Cmd::Library`:
    /// событие — только сигнал, чтобы не дублировать в кадре весь
    /// список плейлистов при каждой мелкой правке.
    PlaylistsChanged,
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
    fn playlist_commands_stay_flat_and_roundtrip() {
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "PL1");
        let track_id = TrackId::new(ProviderId::YTMUSIC, "abc");

        // create: только title, приватность решает провайдер.
        let create = line(&Request { id: 20, cmd: Cmd::PlaylistCreate { title: "Chill".into(), provider: None } });
        assert_eq!(create, r#"{"id":20,"cmd":"playlist_create","title":"Chill"}"#);
        let back: Request = serde_json::from_str(&create).expect("parse");
        assert_eq!(back.id, 20);
        assert_eq!(back.cmd, Cmd::PlaylistCreate { title: "Chill".into(), provider: None });

        // add/remove: кадр плоский, оба id разворачиваются на месте.
        let add = line(&Request {
            id: 21,
            cmd: Cmd::PlaylistAdd { playlist: playlist.clone(), track: track_id.clone() },
        });
        assert_eq!(
            add,
            r#"{"id":21,"cmd":"playlist_add","playlist":{"provider":"ytmusic","id":"PL1"},"track":{"provider":"ytmusic","id":"abc"}}"#
        );
        let back: Request = serde_json::from_str(&add).expect("parse");
        assert_eq!(back.cmd, Cmd::PlaylistAdd { playlist: playlist.clone(), track: track_id.clone() });

        let remove = line(&Request {
            id: 22,
            cmd: Cmd::PlaylistRemove { playlist: playlist.clone(), track: track_id.clone() },
        });
        assert_eq!(
            remove,
            r#"{"id":22,"cmd":"playlist_remove","playlist":{"provider":"ytmusic","id":"PL1"},"track":{"provider":"ytmusic","id":"abc"}}"#
        );
        let back: Request = serde_json::from_str(&remove).expect("parse");
        assert_eq!(back.cmd, Cmd::PlaylistRemove { playlist: playlist.clone(), track: track_id.clone() });

        let delete = line(&Request { id: 23, cmd: Cmd::PlaylistDelete { playlist: playlist.clone() } });
        assert_eq!(delete, r#"{"id":23,"cmd":"playlist_delete","playlist":{"provider":"ytmusic","id":"PL1"}}"#);
        let back: Request = serde_json::from_str(&delete).expect("parse");
        assert_eq!(back.cmd, Cmd::PlaylistDelete { playlist });
    }

    #[test]
    fn playlists_changed_event_is_tagged_and_bare() {
        let event = line(&Event::PlaylistsChanged);
        // Событие без полей — объект только с тегом; тег в snake_case.
        assert_eq!(event, r#"{"event":"playlists_changed"}"#);
        match serde_json::from_str::<Frame>(&event).expect("parse event") {
            Frame::Event(Event::PlaylistsChanged) => {}
            other => panic!("expected a playlists_changed event, got {other:?}"),
        }
    }

    #[test]
    fn playlist_created_payload_roundtrips() {
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "PLnew");
        let response = line(&Response::Ok { id: 24, ok: Payload::PlaylistCreated { playlist: playlist.clone() } });
        // Фиксируем форму untagged-контракта: объект с ключом playlist.
        assert!(response.contains(r#""ok":{"playlist""#), "playlist_created must stay an object: {response}");
        match serde_json::from_str::<Frame>(&response).expect("parse response") {
            Frame::Response(Response::Ok { id, ok: Payload::PlaylistCreated { playlist } }) => {
                assert_eq!(id, 24);
                assert_eq!(playlist.to_string(), "ytmusic:PLnew");
            }
            other => panic!("expected a playlist_created payload, got {other:?}"),
        }
    }

    #[test]
    fn home_request_serializes() {
        let got = line(&Request { id: 30, cmd: Cmd::Home { provider: None } });
        assert_eq!(got, r#"{"id":30,"cmd":"home","provider":null}"#);
        let back: Request = serde_json::from_str(&got).expect("parse");
        assert_eq!(back.cmd, Cmd::Home { provider: None });
    }

    #[test]
    fn refresh_auth_serializes() {
        let got = line(&Request { id: 40, cmd: Cmd::RefreshAuth { provider: None } });
        assert_eq!(got, r#"{"id":40,"cmd":"refresh_auth","provider":null}"#);
        let back: Request = serde_json::from_str(&got).expect("parse");
        assert_eq!(back.cmd, Cmd::RefreshAuth { provider: None });

        let scoped = line(&Request { id: 41, cmd: Cmd::RefreshAuth { provider: Some("soundcloud".into()) } });
        assert_eq!(scoped, r#"{"id":41,"cmd":"refresh_auth","provider":"soundcloud"}"#);
        let back: Request = serde_json::from_str(&scoped).expect("parse");
        assert_eq!(back.cmd, Cmd::RefreshAuth { provider: Some("soundcloud".into()) });
    }

    #[test]
    fn home_payload_roundtrips() {
        let shelf = CatalogShelf {
            title: "Quick picks".into(),
            subtitle: None,
            items: vec![SearchResult::Track(Track {
                id: TrackId::new(ProviderId::YTMUSIC, "abc"),
                title: "Song".into(),
                artists: vec!["Artist".into()],
                album: None,
                duration: None,
                art_url: None,
                page_url: None,
            })],
        };
        let response = line(&Response::Ok { id: 31, ok: Payload::Home(vec![shelf.clone()]) });
        // Фиксируем форму untagged-контракта: полка — объект с ключом title.
        assert!(response.contains(r#""ok":[{"title""#), "shelf must stay an object: {response}");
        match serde_json::from_str::<Frame>(&response).expect("parse response") {
            Frame::Response(Response::Ok { id, ok: Payload::Home(shelves) }) => {
                assert_eq!(id, 31);
                assert_eq!(shelves, vec![shelf]);
            }
            other => panic!("home payload must not degrade to Results/Tracks, got {other:?}"),
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
    fn player_state_last_error_is_backward_compatible() {
        // Снапшот старого демона без last_error обязан парситься.
        let without: PlayerState = serde_json::from_str(
            r#"{"status":"stopped","volume":70.0,"loop_mode":"none","shuffle":false,"queue_len":0,"offline":false}"#,
        )
        .expect("снапшот без last_error обязан парситься");
        assert_eq!(without.last_error, None);

        // И поле обязано уходить в провод при наличии.
        let mut with = PlayerState { last_error: Some("HTTP 403".into()), ..Default::default() };
        with.status = crate::model::PlaybackStatus::Stopped;
        let json = serde_json::to_string(&with).expect("serialize");
        assert!(json.contains(r#""last_error":"HTTP 403""#), "json={json}");
        let back: PlayerState = serde_json::from_str(&json).expect("roundtrip");
        assert_eq!(back.last_error.as_deref(), Some("HTTP 403"));
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

    /// Страница догрузки обязана переживать untagged-перебор вариантов:
    /// `QueueView` стоит раньше и без `deny_unknown_fields` съедал её
    /// молча (клиент видел `index` вместо `next` — живой замер 20.09).
    #[test]
    fn tracks_page_survives_untagged_and_queue_stays_queue() {
        let page = Payload::TracksPage {
            tracks: vec![track("x")],
            next: Some("tok".into()),
        };
        match serde_json::from_str::<Payload>(&line(&page)).expect("parse page") {
            Payload::TracksPage { tracks, next } => {
                assert_eq!(tracks.len(), 1);
                assert_eq!(next.as_deref(), Some("tok"));
            }
            other => panic!("страница догрузки деградировала до {other:?}"),
        }

        let exhausted = Payload::TracksPage { tracks: Vec::new(), next: None };
        match serde_json::from_str::<Payload>(&line(&exhausted)).expect("parse empty page") {
            Payload::TracksPage { tracks, next } => {
                assert!(tracks.is_empty());
                assert_eq!(next, None);
            }
            other => panic!("пустая страница деградировала до {other:?}"),
        }
        // Зеркало очереди при этом остаётся собой: strict-режим не
        // обязан резать собственную форму.
        let queue = Payload::Queue(QueueView { tracks: vec![track("q")], index: Some(3) });
        match serde_json::from_str::<Payload>(&line(&queue)).expect("parse queue") {
            Payload::Queue(view) => {
                assert_eq!(view.index, Some(3));
                assert_eq!(view.tracks.len(), 1);
            }
            other => panic!("зеркало очереди деградировало до {other:?}"),
        }
    }
}
