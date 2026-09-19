//! Разбор ответов InnerTube. Весь и только здесь.
//!
//! Ответы — глубоко вложенный JSON без схемы и без версии. Поэтому обход
//! идёт через `Value::get`, а не через индексацию и не через
//! `serde`-модели: сервис может убрать или переименовать поле в любой
//! момент, и это должно стоить одной пропущенной записи, а не падения
//! всего плеера.

use std::time::Duration;

use serde_json::Value;
use tmus_core::model::{CatalogShelf, Playlist, PlaylistId, ProviderId, SearchResult, Track, TrackId};

const PROVIDER: ProviderId = ProviderId::YTMUSIC;

/// `PlaylistId` хранит сырой id плейлиста, а browse-эндпоинт открывает его
/// только с префиксом `VL`. Одна конвенция на оба конца: разбор снимает
/// префикс, запрос его ставит.
const BROWSE_PREFIX: &str = "VL";

/// Страница трека. По ней потом резолвится поток — `TrackId` для этого
/// недостаточно, yt-dlp нужен URL.
const WATCH_URL: &str = "https://music.youtube.com/watch?v=";

/// Разделитель полей внутри `flexColumns`. В ответе это отдельный run,
/// причём ссылка на артиста живёт в своём run'е — поэтому «разделить текст
/// по точке» недостаточно, нужны границы run'ов.
const SEPARATOR: char = '•';

/// browse-id для запроса треков плейлиста.
#[must_use]
pub(crate) fn playlist_browse_id(playlist_id: &str) -> String {
    format!("{BROWSE_PREFIX}{playlist_id}")
}

/// Обратное преобразование: в ответах browse-id приходит уже с `VL`.
fn playlist_id_from_browse(browse_id: &str) -> Option<&str> {
    browse_id
        .strip_prefix(BROWSE_PREFIX)
        .filter(|id| !id.is_empty())
}

/// Длительность из `m:ss` или `h:mm:ss`.
///
/// Возвращает `None` на всём остальном: в тех же колонках лежат «12 songs»
/// и «2019», и принять их за длительность — значит показать трек на 12
/// секунд.
#[must_use]
pub(crate) fn parse_duration(text: &str) -> Option<Duration> {
    let mut seconds: u64 = 0;
    let mut count = 0;

    for part in text.trim().split(':') {
        let part = part.trim();
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let value: u64 = part.parse().ok()?;
        // Минуты и секунды больше 59 означают, что это не длительность
        // (а, например, «2024:1» из чужого формата).
        if count > 0 && value > 59 {
            return None;
        }
        seconds = seconds * 60 + value;
        count += 1;
    }

    // Одно поле — не длительность; четыре и больше — тоже.
    if !(2..=3).contains(&count) {
        return None;
    }
    Some(Duration::from_secs(seconds))
}

/// Треки страницы: все `musicResponsiveListItemRenderer` (так приходят и
/// результаты поиска, и содержимое плейлиста, и лайкнутое).
///
/// С переходом `playlist_tracks` на [`playlist_entries`] внутри крейта
/// живого вызывающего не осталось — записи плейлиста нужны вместе с
/// `setVideoId`. Оставлена как упрощённая выборка для тестов: в lib-сборе
/// отсутствует (`cfg(test)`), поэтому не стреляет ни dead_code, ни
/// unfulfilled-ожидание — оба срабатывали на разных целях сборки.
#[cfg(test)]
pub(crate) fn tracks(page: &Value) -> Vec<Track> {
    playlist_entries(page)
        .into_iter()
        .map(|(track, _set_video_id)| track)
        .collect()
}

/// Треки плейлиста вместе с `setVideoId` каждой записи.
///
/// `setVideoId` — служебный идентификатор записи внутри плейлиста
/// (`playlistItemData.playlistSetVideoId`): без него эндпоинт
/// `browse/edit_playlist` не даёт убрать трек. Он существует только
/// внутри конкретного плейлиста и приходит только при его перечислении,
/// поэтому добывается здесь, на месте, а не отдельным запросом.
///
/// Имя поля сверено с живым протоколом (19.09): в InnerTube оно
/// `playlistSetVideoId`, а НЕ `setVideoId` — первая реализация читала
/// несуществующее поле, карта оставалась пустой и удаление трека
/// всегда падало «нет такого трека» при видимых в листинге треках.
/// Для записей без `playlistItemData` (внеплейлистовые раскладки) —
/// `None`.
pub(crate) fn playlist_entries(page: &Value) -> Vec<(Track, Option<String>)> {
    renderers(page, "musicResponsiveListItemRenderer")
        .iter()
        .filter_map(|item| {
            let track = track_from_responsive(item)?;
            let set_video_id = item
                .pointer("/playlistItemData/playlistSetVideoId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            Some((track, set_video_id))
        })
        .collect()
}

/// Плейлисты библиотеки: `musicTwoRowItemRenderer` с типом страницы
/// плейлиста. Альбомы и артисты в этот список не попадают — они тоже
/// `musicTwoRowItemRenderer`.
pub(crate) fn playlists(page: &Value) -> Vec<Playlist> {
    renderers(page, "musicTwoRowItemRenderer")
        .iter()
        .filter_map(|item| two_row(item))
        .filter(is_playlist)
        .filter_map(playlist_of)
        .collect()
}

/// Результаты поиска одним списком: треки, плейлисты, артисты.
///
/// Тип страницы берётся из ответа, а не угадывается: один и тот же
/// `musicTwoRowItemRenderer` описывает и плейлист, и альбом, и артиста.
pub(crate) fn search_results(page: &Value) -> Vec<SearchResult> {
    let mut results: Vec<SearchResult> = Vec::new();

    // Под фильтром поиска сервис отдаёт артистов/плейлисты/альбомы
    // `musicResponsiveListItemRenderer`-списком, а не каруселью: замер
    // 18.09.2026 по `youtubei/v1/search` — artists 2 записи, playlists 20,
    // `musicTwoRowItemRenderer` ноль. Поэтому одного twoRow-разбора ниже
    // недостаточно. Responsive-элемент описывает и трек тоже, поэтому вид
    // определяем по pageType browse-эндпоинта и ветвимся один раз до
    // трекового разбора — иначе запись уходила бы в результат и как Track,
    // и как Playlist.
    for item in renderers(page, "musicResponsiveListItemRenderer") {
        let page_type = responsive_page_type(item);
        let is_browse = page_type.as_deref().is_some_and(|page| {
            page.contains("ARTIST") || page.contains("ALBUM") || page.contains("PLAYLIST")
        });
        if is_browse {
            let Some(row) = responsive_row(item) else {
                continue;
            };
            let page_type = page_type.as_deref().unwrap_or_default();
            if page_type.contains("ARTIST") {
                if !row.browse_id.is_empty() {
                    results.push(SearchResult::Artist {
                        provider: PROVIDER,
                        id: row.browse_id,
                        name: row.title,
                    });
                }
            } else if page_type.contains("ALBUM") {
                if let Some(album) = album_of(row) {
                    results.push(SearchResult::Playlist(album));
                }
            } else if let Some(playlist) = playlist_of(row) {
                results.push(SearchResult::Playlist(playlist));
            }
        } else if let Some(track) = track_from_responsive(item) {
            results.push(SearchResult::Track(track));
        }
    }

    // Нефильтрованный поиск и библиотека продолжают приходить каруселью
    // `musicTwoRowItemRenderer` — этот разбор сохранён.
    for item in renderers(page, "musicTwoRowItemRenderer") {
        let Some(row) = two_row(item) else {
            continue;
        };
        // Ветвление общее с лентой Home — вынесено в two_row_result.
        if let Some(result) = two_row_result(row) {
            results.push(result);
        }
    }

    results
}

/// Карточка `musicTwoRowItemRenderer` → результат поиска: артист, альбом
/// или плейлист по pageType. Битая карточка (нет адреса/имени) — `None`,
/// а не ошибка: одна мёртвая строка не должна ронять выдачу.
fn two_row_result(row: TwoRow) -> Option<SearchResult> {
    let artist = row.page_type.as_deref().is_some_and(|page| page.contains("ARTIST"));
    let album = row.page_type.as_deref().is_some_and(|page| page.contains("ALBUM"));

    if artist {
        if row.browse_id.is_empty() {
            return None;
        }
        Some(SearchResult::Artist {
            provider: PROVIDER,
            id: row.browse_id,
            name: row.title,
        })
    } else if album {
        album_of(row).map(SearchResult::Playlist)
    } else if is_playlist(&row) {
        playlist_of(row).map(SearchResult::Playlist)
    } else {
        None
    }
}

/// Лента Home: карусели `musicCarouselShelfRenderer` одной страницы.
///
/// Одна страница, а не browse_pages: продолжения удваивают латентность
/// ради полок, которые всё равно за пределами экрана. Артистов (и видео)
/// выбрасываем — поверхность артиста в плеере нет, мёртвая карточка хуже
/// отсутствующей. Полка без заголовка или без элементов не нужна вызывающему.
pub(crate) fn home(page: &Value) -> Vec<CatalogShelf> {
    renderers(page, "musicCarouselShelfRenderer")
        .iter()
        .filter_map(|shelf| {
            let header = shelf.pointer("/header/musicCarouselShelfBasicHeaderRenderer")?;
            let title = runs_text(header.get("title")?);
            if title.is_empty() {
                return None;
            }
            let subtitle = header.get("strapline").map(runs_text).filter(|text| !text.is_empty());

            let items: Vec<SearchResult> = shelf
                .get("contents")?
                .as_array()?
                .iter()
                .filter_map(|item| {
                    if let Some(entry) = item.get("musicResponsiveListItemRenderer") {
                        track_from_responsive(entry).map(SearchResult::Track)
                    } else if let Some(entry) = item.get("musicTwoRowItemRenderer") {
                        // Оставляем только плейлисты (включая альбомы в их
                        // представлении): остальное — мёртвые карточки.
                        let row = two_row(entry)?;
                        match two_row_result(row) {
                            Some(result @ SearchResult::Playlist(_)) => Some(result),
                            _ => None,
                        }
                    } else {
                        // Прочие ключи (continuationItemRenderer и т.п.) — мимо.
                        None
                    }
                })
                .collect();

            if items.is_empty() {
                None
            } else {
                Some(CatalogShelf { title, subtitle, items })
            }
        })
        .collect()
}

/// Подсказки поиска. Пустой ответ сервиса — пустой вектор, а не ошибка.
pub(crate) fn suggestions(page: &Value) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for item in renderers(page, "searchSuggestionRenderer") {
        let text = item
            .pointer("/navigationEndpoint/searchEndpoint/query")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| item.get("suggestion").map(runs_text))
            .unwrap_or_default();
        let text = text.trim();
        // Сервис повторяет одну и ту же подсказку в разных секциях.
        if !text.is_empty() && !found.iter().any(|known| known == text) {
            found.push(text.to_owned());
        }
    }
    found
}

/// Токен следующей страницы, если сервис её предложил.
pub(crate) fn continuation_token(page: &Value) -> Option<String> {
    renderers(page, "continuationItemRenderer")
        .iter()
        .find_map(|item| {
            item.pointer("/continuationEndpoint/continuationCommand/token")
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

/// Все renderer'ы с этим именем ключа на любой глубине ответа.
///
/// Обход по дереву, а не по известному пути: глубина вложенности у разных
/// эндпоинтов разная (`browse` и `search` кладут секции по-разному), а
/// твёрдый путь ломался бы на каждом изменении обёрток.
fn renderers<'a>(page: &'a Value, name: &str) -> Vec<&'a Value> {
    let mut found = Vec::new();
    walk(page, name, &mut found);
    found
}

fn walk<'a>(value: &'a Value, name: &str, found: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == name {
                    found.push(child);
                }
                walk(child, name, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, name, found);
            }
        }
        _ => {}
    }
}

/// «Песня» из `musicResponsiveListItemRenderer`.
///
/// Запись без `videoId` или без заголовка пропускается: адресовать и
/// показать её нечем, а падать из-за одной битой строки в ответе на две
/// тысячи строк нельзя.
fn track_from_responsive(item: &Value) -> Option<Track> {
    let id = video_id(item)?;
    let columns = flex_columns(item);
    let title = columns.first().map(|column| runs_text(column)).unwrap_or_default();
    if title.is_empty() {
        return None;
    }

    let (artists, album) = credit(&columns);
    Some(Track {
        id: TrackId::new(PROVIDER, &id),
        title,
        artists,
        album,
        duration: duration_of(&columns),
        art_url: largest_thumbnail(item),
        page_url: Some(format!("{WATCH_URL}{id}")),
    })
}

/// Идентификатор видео. Лежит в разных местах в зависимости от того, чем
/// страница его вернула: поиск кладёт `videoId`, плейлист — в
/// `playlistItemData`, часть раскладок — только в навигации.
fn video_id(item: &Value) -> Option<String> {
    const PATHS: [&str; 3] = [
        "/playlistItemData/videoId",
        "/navigationEndpoint/watchEndpoint/videoId",
        "/overlay/playButtonRenderer/navigationEndpoint/watchEndpoint/videoId",
    ];

    let direct = item.get("videoId").and_then(Value::as_str);
    let nested = PATHS
        .iter()
        .find_map(|path| item.pointer(path).and_then(Value::as_str));

    direct
        .or(nested)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Колонки записи: `[0]` — название, `[1]` — артисты и альбом, дальше —
/// длительность и прочие служебные поля.
fn flex_columns(item: &Value) -> Vec<&Value> {
    item.get("flexColumns")
        .and_then(Value::as_array)
        .map(|columns| {
            columns
                .iter()
                .filter_map(|column| {
                    column
                        .get("musicResponsiveListItemFlexColumnRenderer")
                        .and_then(|renderer| renderer.get("text"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Текст поля: либо `runs`, либо `simpleText` (одно из двух приходит).
fn runs_text(text: &Value) -> String {
    let joined = match text.get("runs").and_then(Value::as_array) {
        Some(runs) => runs
            .iter()
            .filter_map(|run| run.get("text").and_then(Value::as_str))
            .collect::<String>(),
        None => text
            .get("simpleText")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    };
    joined.trim().to_owned()
}

/// Кусок колонки со ссылками на артистов внутри.
struct Segment {
    text: String,
    /// Run ведёт на страницу артиста — единственный надёжный признак
    /// «это артист, а не альбом»: без него остаётся только позиция.
    artist: bool,
}

/// Колонка, разрезанная по `•`. Разделитель приходит отдельным run'ом, но
/// попадается и внутри текста — режем в обоих случаях.
fn segments(column: &Value) -> Vec<Segment> {
    let Some(runs) = column.get("runs").and_then(Value::as_array) else {
        return plain_segments(&runs_text(column));
    };

    let mut found = Vec::new();
    let mut buffer = String::new();
    let mut artist = false;

    for run in runs {
        let raw = run.get("text").and_then(Value::as_str).unwrap_or_default();
        let mut rest = raw;
        while let Some(position) = rest.find(SEPARATOR) {
            buffer.push_str(&rest[..position]);
            flush(&mut found, &mut buffer, &mut artist);
            rest = &rest[position + SEPARATOR.len_utf8()..];
        }
        buffer.push_str(rest);
        artist |= links_to_artist(run);
    }
    flush(&mut found, &mut buffer, &mut artist);

    found
}

fn plain_segments(text: &str) -> Vec<Segment> {
    text.split(SEPARATOR)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| Segment {
            text: part.to_owned(),
            artist: false,
        })
        .collect()
}

fn flush(found: &mut Vec<Segment>, buffer: &mut String, artist: &mut bool) {
    let text = buffer.trim();
    if !text.is_empty() {
        found.push(Segment {
            text: text.to_owned(),
            artist: *artist,
        });
    }
    buffer.clear();
    *artist = false;
}

fn links_to_artist(run: &Value) -> bool {
    run.pointer(
        "/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs\
         /browseEndpointContextMusicConfig/pageType",
    )
    .and_then(Value::as_str)
    .is_some_and(|page| page.contains("ARTIST"))
}

/// Артисты и альбом из второй колонки.
///
/// Разметка важнее позиции: у песен каждый артист — отдельная ссылка на
/// свою страницу, а альбом идёт без неё, поэтому «последний кусок — это
/// альбом» применяется только к разметке без ссылок.
fn credit(columns: &[&Value]) -> (Vec<String>, Option<String>) {
    let Some(column) = columns.get(1) else {
        return (Vec::new(), None);
    };
    let parts: Vec<Segment> = segments(column)
        .into_iter()
        .filter(|segment| !is_metadata(&segment.text))
        .collect();

    if parts.is_empty() {
        return (Vec::new(), None);
    }
    let marked = parts.iter().any(|part| part.artist);

    let mut artists = Vec::new();
    let mut album = None;
    for (index, part) in parts.iter().enumerate() {
        if marked {
            if part.artist {
                artists.push(part.text.clone());
            } else if album.is_none() {
                album = Some(part.text.clone());
            }
        } else if parts.len() > 1 && index + 1 == parts.len() {
            album = Some(part.text.clone());
        } else {
            artists.push(part.text.clone());
        }
    }

    (artists, album)
}

/// Служебные поля, которые стоят в тех же колонках, что артист и альбом.
///
/// Разбор опирается на английские формулировки намеренно: `hl=en` запинен в
/// контексте запроса, иначе строки локализовались бы и признак поехал.
fn is_metadata(text: &str) -> bool {
    if parse_duration(text).is_some() {
        return true;
    }
    let lowered = text.to_ascii_lowercase();
    let cleaned = lowered.trim().trim_end_matches('.');
    if cleaned.len() == 4 && cleaned.bytes().all(|byte| byte.is_ascii_digit()) {
        return true;
    }
    // Единица измерения — последнее слово: «12 songs», «3.2M subscribers».
    // По началу строки проверять нельзя — под него попадёт альбом
    // «Songs for the Deaf».
    const UNITS: [&str; 8] = [
        "song",
        "songs",
        "track",
        "tracks",
        "view",
        "views",
        "subscriber",
        "subscribers",
    ];
    cleaned
        .split_whitespace()
        .last()
        .is_some_and(|last| UNITS.contains(&last))
}

/// Длительность: последняя колонка целиком, а если её нет — служебный
/// кусок внутри какой-нибудь колонки (в части раскладок он приклеен к
/// артистам через тот же `•`).
fn duration_of(columns: &[&Value]) -> Option<Duration> {
    for column in columns.iter().rev() {
        if let Some(duration) = parse_duration(&runs_text(column)) {
            return Some(duration);
        }
        if let Some(duration) = segments(column)
            .iter()
            .rev()
            .find_map(|segment| parse_duration(&segment.text))
        {
            return Some(duration);
        }
    }
    None
}

/// Обложка наибольшего размера: у трека их несколько, и порядок в ответе
/// не гарантирован.
fn largest_thumbnail(item: &Value) -> Option<String> {
    const PATHS: [&str; 4] = [
        "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails",
        "/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails",
        "/thumbnail/croppedSquareThumbnailRenderer/thumbnail/thumbnails",
        "/thumbnail/thumbnails",
    ];

    let thumbnails = PATHS
        .iter()
        .find_map(|path| item.pointer(path).and_then(Value::as_array))?;

    thumbnails
        .iter()
        .filter_map(|thumbnail| {
            let url = thumbnail.get("url").and_then(Value::as_str)?;
            let width = thumbnail.get("width").and_then(Value::as_u64).unwrap_or(0);
            let height = thumbnail.get("height").and_then(Value::as_u64).unwrap_or(0);
            Some((width * height, url))
        })
        .max_by_key(|(area, _)| *area)
        .map(|(_, url)| url.to_owned())
}

/// Запись `musicTwoRowItemRenderer` в том виде, в каком она нужна модели.
struct TwoRow {
    title: String,
    subtitle: Option<String>,
    art_url: Option<String>,
    browse_id: String,
    page_type: Option<String>,
    /// Плейлист с треками альбома: приходит из play-кнопки обложки.
    audio_playlist_id: Option<String>,
}

fn two_row(item: &Value) -> Option<TwoRow> {
    let title = runs_text(item.get("title")?);
    if title.is_empty() {
        return None;
    }
    let subtitle = item.get("subtitle").map(runs_text).filter(|text| !text.is_empty());

    Some(TwoRow {
        title,
        subtitle,
        art_url: largest_thumbnail(item),
        browse_id: item
            .pointer("/navigationEndpoint/browseEndpoint/browseId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        page_type: item
            .pointer(
                "/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs\
                 /browseEndpointContextMusicConfig/pageType",
            )
            .and_then(Value::as_str)
            .map(str::to_owned),
        audio_playlist_id: item
            .pointer(
                "/thumbnailOverlay/musicItemThumbnailOverlayRenderer/content\
                 /musicPlayButtonRenderer/playNavigationEndpoint\
                 /watchPlaylistEndpoint/playlistId",
            )
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// pageType browse-эндпоинта responsive-записи. Единственный надёжный
/// признак вида: под фильтром и артист, и плейлист приходят одним и тем же
/// `musicResponsiveListItemRenderer`, различие только здесь.
fn responsive_page_type(item: &Value) -> Option<String> {
    item.pointer(
        "/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs\
         /browseEndpointContextMusicConfig/pageType",
    )
    .and_then(Value::as_str)
    .map(str::to_owned)
}

/// Responsive-запись (артист/плейлист/альбом) в виде `TwoRow`: имя — в
/// первой колонке, подпись — во второй, остальное совпадает с twoRow
/// поэлементно. Переиспользует `playlist_of`/`album_of`, чтобы не плодить
/// вторую конвенцию сборки модели.
fn responsive_row(item: &Value) -> Option<TwoRow> {
    let columns = flex_columns(item);
    let title = columns.first().map(|column| runs_text(column)).unwrap_or_default();
    if title.is_empty() {
        return None;
    }

    Some(TwoRow {
        subtitle: columns.get(1).map(|column| runs_text(column)).filter(|text| !text.is_empty()),
        title,
        art_url: largest_thumbnail(item),
        browse_id: item
            .pointer("/navigationEndpoint/browseEndpoint/browseId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        page_type: responsive_page_type(item),
        audio_playlist_id: item
            .pointer(
                "/thumbnailOverlay/musicItemThumbnailOverlayRenderer/content\
                 /musicPlayButtonRenderer/playNavigationEndpoint\
                 /watchPlaylistEndpoint/playlistId",
            )
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn is_playlist(row: &TwoRow) -> bool {
    match row.page_type.as_deref() {
        Some(page) => page.contains("PLAYLIST"),
        // Тип страницы не пришёл — остаётся признак адресации: плейлисты
        // открываются через `VL`, альбомы и артисты — нет.
        None => row.browse_id.starts_with(BROWSE_PREFIX),
    }
}

fn playlist_of(row: TwoRow) -> Option<Playlist> {
    let id = playlist_id_from_browse(&row.browse_id)?;
    let track_count = track_count(row.subtitle.as_deref());
    Some(Playlist {
        id: PlaylistId::new(PROVIDER, id),
        title: row.title,
        subtitle: row.subtitle,
        art_url: row.art_url,
        track_count,
    })
}

/// Альбом как плейлист.
///
/// Модель не знает типа «альбом», а у YouTube Music альбом и есть
/// плейлист — но адресуется он иначе: browse-id альбома (`MPREb_…`) под
/// `VL` не открывается, треки лежат в аудиоплейлисте из play-кнопки.
/// Нет этого id — запись пропускается: открыть её всё равно нечем.
fn album_of(row: TwoRow) -> Option<Playlist> {
    let id = row.audio_playlist_id.filter(|id| !id.is_empty())?;
    Some(Playlist {
        id: PlaylistId::new(PROVIDER, id),
        title: row.title,
        subtitle: row.subtitle,
        art_url: row.art_url,
        track_count: None,
    })
}

/// Размер плейлиста из подписи вида «42 songs». В подписи же стоят год,
/// тип и владелец, поэтому ищется не первое число, а число перед единицей
/// измерения.
fn track_count(subtitle: Option<&str>) -> Option<u32> {
    let words: Vec<&str> = subtitle?.split_whitespace().collect();
    for (index, word) in words.iter().enumerate() {
        let unit = word.trim_end_matches('.').to_ascii_lowercase();
        if !(unit.starts_with("song") || unit.starts_with("track")) {
            continue;
        }
        let previous = index.checked_sub(1).and_then(|index| words.get(index))?;
        return previous.replace(',', "").parse().ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Сокращённая форма реальной записи: `flexColumns`, обложка и
    /// `playlistItemData` — как в ответе `browse` по плейлисту.
    fn song_renderer() -> Value {
        let artist = |name: &str| {
            json!({
                "text": name,
                "navigationEndpoint": { "browseEndpoint": {
                    "browseEndpointContextSupportedConfigs": {
                        "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ARTIST" }
                    }
                } }
            })
        };
        let column = |value: Value| {
            json!({ "musicResponsiveListItemFlexColumnRenderer": { "text": value } })
        };

        json!({
            "musicResponsiveListItemRenderer": {
                "playlistItemData": { "videoId": "dQw4w9WgXcQ", "playlistSetVideoId": "SVabc123" },
                "flexColumns": [
                    column(json!({ "runs": [{ "text": "Заголовок" }] })),
                    column(json!({ "runs": [
                        artist("Artist A"),
                        { "text": " • " },
                        artist("Artist B"),
                        { "text": " • " },
                        { "text": "Album" }
                    ] })),
                    column(json!({ "simpleText": "3:07" }))
                ],
                "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                    { "url": "https://img.example/small.jpg", "width": 60, "height": 60 },
                    { "url": "https://img.example/big.jpg", "width": 544, "height": 544 }
                ] } } }
            }
        })
    }

    #[test]
    fn responsive_list_item_becomes_track() {
        let found = tracks(&song_renderer());
        let track = found.first().expect("запись разобрана");

        assert_eq!(track.id, TrackId::new(ProviderId::YTMUSIC, "dQw4w9WgXcQ"));
        assert_eq!(track.title, "Заголовок");
        assert_eq!(track.artists, ["Artist A", "Artist B"]);
        assert_eq!(track.album.as_deref(), Some("Album"));
        assert_eq!(track.duration, Some(Duration::from_secs(187)));
        assert_eq!(track.art_url.as_deref(), Some("https://img.example/big.jpg"));
        assert_eq!(
            track.page_url.as_deref(),
            Some("https://music.youtube.com/watch?v=dQw4w9WgXcQ")
        );
    }

    #[test]
    fn playlist_entry_carries_set_video_id() {
        // `setVideoId` нужен `playlist_remove`: без него эндпоинт
        // редактирования плейлиста трек не убирает. Разбор обязан
        // доставать его из той же записи, что и сам трек.
        let found = playlist_entries(&song_renderer());
        let (track, set_video_id) = found.first().expect("запись разобрана");

        assert_eq!(track.id, TrackId::new(ProviderId::YTMUSIC, "dQw4w9WgXcQ"));
        assert_eq!(set_video_id.as_deref(), Some("SVabc123"));
    }

    #[test]
    fn entry_without_playlist_item_data_has_no_set_video_id() {
        // Внеплейлистовые раскладки (поиск, лайкнутое без item-данных)
        // `setVideoId` не имеют — наружу уходит `None`, а не пустая строка.
        let stripped = json!({
            "musicResponsiveListItemRenderer": {
                "videoId": "dQw4w9WgXcQ",
                "flexColumns": song_renderer()["musicResponsiveListItemRenderer"]["flexColumns"]
            }
        });

        let (track, set_video_id) = playlist_entries(&stripped)
            .into_iter()
            .next()
            .expect("запись без item-данных разобрана");
        assert_eq!(track.id, TrackId::new(ProviderId::YTMUSIC, "dQw4w9WgXcQ"));
        assert_eq!(set_video_id, None);
    }

    #[test]
    fn record_without_video_id_is_skipped() {
        // Так выглядит запись, у которой сервис не отдал `videoId`:
        // разбор обязан её выбросить, а не паниковать.
        let broken = json!({
            "musicResponsiveListItemRenderer": {
                "flexColumns": [{ "musicResponsiveListItemFlexColumnRenderer": {
                    "text": { "runs": [{ "text": "Без идентификатора" }] }
                } }]
            }
        });
        let page = json!({ "contents": [song_renderer(), broken] });

        let found = tracks(&page);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Заголовок");
    }

    #[test]
    fn parse_duration_reads_minutes_and_hours() {
        assert_eq!(parse_duration("3:07"), Some(Duration::from_secs(187)));
        assert_eq!(parse_duration("1:02:03"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_duration("10:00"), Some(Duration::from_secs(600)));

        for garbage in ["", "не длительность", "3", "1:2:3:4", "3:07:99", "12 songs"] {
            assert_eq!(parse_duration(garbage), None, "{garbage}");
        }
    }

    #[test]
    fn library_playlist_keeps_raw_id_and_count() {
        let page = json!({ "contents": [{ "musicTwoRowItemRenderer": {
            "title": { "runs": [{ "text": "Плейлист" }] },
            "subtitle": { "runs": [
                { "text": "Playlist" },
                { "text": " • " },
                { "text": "42 songs" }
            ] },
            "navigationEndpoint": { "browseEndpoint": {
                "browseId": "VLPLabc123",
                "browseEndpointContextSupportedConfigs": {
                    "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_PLAYLIST" }
                }
            } },
            "thumbnailRenderer": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                { "url": "https://img.example/playlist.jpg", "width": 226, "height": 226 }
            ] } } }
        } }] });

        let found = playlists(&page);
        let playlist = found.first().expect("плейлист разобран");

        assert_eq!(playlist.id, PlaylistId::new(ProviderId::YTMUSIC, "PLabc123"));
        assert_eq!(playlist.title, "Плейлист");
        assert_eq!(playlist.track_count, Some(42));
        assert_eq!(playlist.subtitle.as_deref(), Some("Playlist • 42 songs"));
        assert_eq!(
            playlist.art_url.as_deref(),
            Some("https://img.example/playlist.jpg")
        );
    }

    #[test]
    fn playlist_browse_id_gets_the_vl_prefix() {
        // Так `playlist_tracks` собирает browse-id: без префикса эндпоинт
        // плейлист не открывает.
        assert_eq!(playlist_browse_id("PLabc123"), "VLPLabc123");
        assert_eq!(playlist_browse_id("LM"), "VLLM");
        assert_eq!(playlist_id_from_browse("VLPLabc123"), Some("PLabc123"));
        assert_eq!(playlist_id_from_browse("MPREb_album"), None);
    }

    /// Урезанная копия реального responsive-элемента артиста из ответа
    /// `search` под фильтром: pageType в browse-эндпоинте, имя в первой
    /// flex-колонке.
    fn responsive_artist_renderer() -> Value {
        json!({
            "musicResponsiveListItemRenderer": {
                "flexColumns": [
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": {
                        "runs": [{ "text": "Aphex Twin" }]
                    } } },
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": {
                        "runs": [{ "text": "Artist" }, { "text": " • " }, { "text": "576M monthly audience" }]
                    } } }
                ],
                "navigationEndpoint": { "browseEndpoint": {
                    "browseId": "UCWmnkYUzoOiOztmPBhIlZjg",
                    "browseEndpointContextSupportedConfigs": {
                        "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ARTIST" }
                    }
                } }
            }
        })
    }

    #[test]
    fn responsive_list_item_becomes_artist() {
        // Регресс: под фильтром artists сервис отдаёт responsive-список
        // (twoRow — ноль), и поиск раньше возвращал пустой результат.
        let found = search_results(&responsive_artist_renderer());

        assert_eq!(found.len(), 1);
        match found.into_iter().next().expect("разобран") {
            SearchResult::Artist { provider, id, name } => {
                assert_eq!(provider, ProviderId::YTMUSIC);
                assert_eq!(id, "UCWmnkYUzoOiOztmPBhIlZjg");
                assert_eq!(name, "Aphex Twin");
            }
            other => panic!("ожидался артист, получено: {other:?}"),
        }
    }

    #[test]
    fn responsive_playlist_and_track_are_not_double_counted() {
        // Плейлист и трек приходят одним и тем же responsive-элементом:
        // ветвление по pageType обязано отдать каждый ровно один раз.
        let playlist = json!({
            "musicResponsiveListItemRenderer": {
                "flexColumns": [
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": {
                        "runs": [{ "text": "Микс" }]
                    } } },
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": {
                        "runs": [{ "text": "Playlist" }, { "text": " • " }, { "text": "42 songs" }]
                    } } }
                ],
                "navigationEndpoint": { "browseEndpoint": {
                    "browseId": "VLPLoQ9abc123",
                    "browseEndpointContextSupportedConfigs": {
                        "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_PLAYLIST" }
                    }
                } }
            }
        });
        let page = json!({ "contents": [playlist, song_renderer()] });

        let found = search_results(&page);
        let playlists = found
            .iter()
            .filter(|result| matches!(result, SearchResult::Playlist(_)))
            .count();
        let tracks = found
            .iter()
            .filter(|result| matches!(result, SearchResult::Track(_)))
            .count();
        assert_eq!(playlists, 1, "плейлист ровно один: {found:?}");
        assert_eq!(tracks, 1, "трек ровно один: {found:?}");
    }

    // Фикстуры ниже кодируют ДОГОВОРНУЮ форму ответа FEmusic_home, а не
    // живой снимок: обёртки (singleColumnBrowseResultsRenderer и т.п.)
    // опущены — renderers() ходит по всему дереву. Форма сверяется живым
    // снимком FEmusic_home при интеграции: фикстура ≠ доказательство
    // контракта (урок KB).

    /// Карточка `musicTwoRowItemRenderer` с плейлистом.
    fn two_row_playlist() -> Value {
        json!({
            "musicTwoRowItemRenderer": {
                "title": { "runs": [{ "text": "Микс дня" }] },
                "subtitle": { "runs": [{ "text": "Playlist" }, { "text": " • " }, { "text": "25 songs" }] },
                "navigationEndpoint": { "browseEndpoint": {
                    "browseId": "VLPLhome1",
                    "browseEndpointContextSupportedConfigs": {
                        "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_PLAYLIST" }
                    }
                } },
                "thumbnailRenderer": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                    { "url": "https://img.example/mix.jpg", "width": 226, "height": 226 }
                ] } } }
            }
        })
    }

    /// Карточка `musicTwoRowItemRenderer` с альбомом: pageType ALBUM,
    /// адрес треков — аудиоплейлист из play-кнопки (browse-id `MPREb_…`
    /// под `VL` не открывается).
    fn two_row_album() -> Value {
        json!({
            "musicTwoRowItemRenderer": {
                "title": { "runs": [{ "text": "Альбом" }] },
                "subtitle": { "runs": [{ "text": "Artist" }, { "text": " • " }, { "text": "2024" }] },
                "navigationEndpoint": { "browseEndpoint": {
                    "browseId": "MPREb_album1",
                    "browseEndpointContextSupportedConfigs": {
                        "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ALBUM" }
                    }
                } },
                "thumbnailOverlay": { "musicItemThumbnailOverlayRenderer": { "content":
                    { "musicPlayButtonRenderer": { "playNavigationEndpoint":
                        { "watchPlaylistEndpoint": { "playlistId": "RDAMVMalbum1" } } } } } },
                "thumbnailRenderer": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                    { "url": "https://img.example/album.jpg", "width": 226, "height": 226 }
                ] } } }
            }
        })
    }

    /// Карточка `musicTwoRowItemRenderer` с артистом.
    fn two_row_artist() -> Value {
        json!({
            "musicTwoRowItemRenderer": {
                "title": { "runs": [{ "text": "Aphex Twin" }] },
                "subtitle": { "runs": [{ "text": "Artist" }] },
                "navigationEndpoint": { "browseEndpoint": {
                    "browseId": "UCartist1",
                    "browseEndpointContextSupportedConfigs": {
                        "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ARTIST" }
                    }
                } }
            }
        })
    }

    /// Карусель с шапкой и элементами — договорённая форма полки Home.
    fn carousel(title: &str, subtitle: Option<&str>, items: Value) -> Value {
        let mut header = json!({
            "musicCarouselShelfBasicHeaderRenderer": {
                "title": { "runs": [{ "text": title }] }
            }
        });
        if let Some(text) = subtitle {
            header["musicCarouselShelfBasicHeaderRenderer"]["strapline"] =
                json!({ "runs": [{ "text": text }] });
        }
        json!({
            "musicCarouselShelfRenderer": {
                "header": header,
                "contents": items
            }
        })
    }

    #[test]
    fn home_parses_carousel_shelves() {
        // Две полки: треки responsive-списком и карточки twoRow
        // (плейлист + альбом). Заголовки, подписи и состав проверяются
        // по полям контракта CatalogShelf.
        let page = json!({
            "contents": [
                carousel("Слушайте снова", Some("Для вас"), json!([song_renderer()])),
                carousel("Плейлисты", None, json!([two_row_playlist(), two_row_album()]))
            ]
        });

        let shelves = home(&page);
        assert_eq!(shelves.len(), 2, "{shelves:?}");

        assert_eq!(shelves[0].title, "Слушайте снова");
        assert_eq!(shelves[0].subtitle.as_deref(), Some("Для вас"));
        assert_eq!(shelves[0].items.len(), 1);
        assert!(matches!(&shelves[0].items[0], SearchResult::Track(track) if track.title == "Заголовок"));

        assert_eq!(shelves[1].title, "Плейлисты");
        assert_eq!(shelves[1].subtitle, None);
        let ids: Vec<_> = shelves[1]
            .items
            .iter()
            .map(|result| match result {
                SearchResult::Playlist(playlist) => playlist.id.id.clone(),
                other => panic!("ожидался плейлист, получено: {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["PLhome1", "RDAMVMalbum1"]);
    }

    #[test]
    fn home_drops_artists_and_empty_shelves() {
        // Артист — мёртвая карточка (поверхности артиста в плеере нет):
        // из живой полки выбрасывается запись, полка из одних артистов
        // выбрасывается целиком, как и полка без элементов вовсе.
        let page = json!({
            "contents": [
                carousel("Смешанная", None, json!([two_row_artist(), two_row_playlist()])),
                carousel("Одни артисты", None, json!([two_row_artist()])),
                carousel("Пустая", None, json!([]))
            ]
        });

        let shelves = home(&page);
        assert_eq!(shelves.len(), 1, "{shelves:?}");
        assert_eq!(shelves[0].title, "Смешанная");
        assert_eq!(shelves[0].items.len(), 1);
    }

    #[test]
    fn home_skips_unnamed_shelf() {
        // Полка без шапки (или с пустым заголовком) не показывается:
        // заголовок — единственное, чем полка адресуется в UI.
        let page = json!({
            "contents": [
                carousel("Безымянная", None, json!([two_row_playlist()])),
                { "musicCarouselShelfRenderer": { "contents": [two_row_playlist()] } }
            ]
        });
        let empty_title = json!({
            "contents": [
                { "musicCarouselShelfRenderer": {
                    "header": { "musicCarouselShelfBasicHeaderRenderer": {
                        "title": { "runs": [{ "text": "  " }] }
                    } },
                    "contents": [two_row_playlist()]
                } }
            ]
        });

        assert_eq!(home(&page).len(), 1);
        assert_eq!(home(&empty_title).len(), 0);
    }

    #[test]
    fn home_ignores_immersive_and_continuation() {
        // Промо-блок главной (musicImmersiveTopShelfRenderer) — не карусель
        // и полкой не собирается; continuationItemRenderer внутри contents
        // пропускается, соседние записи разбираются дальше.
        let page = json!({
            "contents": [
                { "musicImmersiveTopShelfRenderer": {
                    "header": { "musicImmersiveHeaderRenderer": {
                        "title": { "runs": [{ "text": "Промо" }] }
                    } }
                } },
                carousel("Слушайте снова", None, json!([
                    { "continuationItemRenderer": { "continuationEndpoint":
                        { "continuationCommand": { "token": "tok" } } } },
                    song_renderer()
                ]))
            ]
        });

        let shelves = home(&page);
        assert_eq!(shelves.len(), 1, "{shelves:?}");
        assert_eq!(shelves[0].title, "Слушайте снова");
        assert_eq!(shelves[0].items.len(), 1);
    }
}
