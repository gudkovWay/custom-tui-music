//! Разбор ответов SoundCloud. Весь и только здесь.
//!
//! Ответы — JSON без схемы: сервис может убрать или переименовать поле
//! в любой момент, поэтому обход через `Value::get`, а не через
//! serde-модели — ошибка формата должна стоить одной пропущенной
//! записи, а не падения всего плеера. Наружу уходят только типы
//! `tmus-core`.

use std::time::Duration;

use serde_json::Value;
use tmus_core::model::{
    CatalogShelf, Playlist, PlaylistId, ProviderId, SearchResult, Track, TrackId,
};

const PROVIDER: ProviderId = ProviderId::SOUNDCLOUD;

/// Трек из объекта api-v2. Числовые id приходят числами — приводятся к
/// строке, потому что [`TrackId`] хранит строку.
///
/// `artwork_url` трека может отсутствовать — тогда берётся
/// `avatar_url` автора (замер живого ответа: одно из двух есть всегда,
/// но полагаться на «всегда» не стоит — вернём `None`).
#[must_use]
pub(crate) fn track(v: &Value) -> Option<Track> {
    let id = v.get("id")?.as_u64()?.to_string();
    let title = v.get("title")?.as_str()?.to_owned();
    let user = v.get("user")?;
    let artist = user.get("username")?.as_str()?.to_owned();
    let duration_ms = v.get("duration").and_then(Value::as_u64);
    let art_url = v
        .get("artwork_url")
        .and_then(Value::as_str)
        .or_else(|| user.get("avatar_url").and_then(Value::as_str))
        .map(str::to_owned);
    let page_url = v
        .get("permalink_url")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(Track {
        id: TrackId::new(PROVIDER, id),
        title,
        artists: vec![artist],
        album: None,
        duration: duration_ms.map(Duration::from_millis),
        art_url,
        page_url,
    })
}

/// Плейлист из объекта api-v2.
#[must_use]
pub(crate) fn playlist(v: &Value) -> Option<Playlist> {
    let id = v.get("id")?.as_u64()?.to_string();
    let title = v.get("title")?.as_str()?.to_owned();
    let user = v.get("user")?;
    let art_url = v
        .get("artwork_url")
        .and_then(Value::as_str)
        .or_else(|| user.get("avatar_url").and_then(Value::as_str))
        .map(str::to_owned);
    Some(Playlist {
        id: PlaylistId::new(PROVIDER, id),
        title,
        subtitle: user.get("username").and_then(Value::as_str).map(str::to_owned),
        art_url,
        track_count: v.get("track_count").and_then(Value::as_u64).map(|n| n as u32),
    })
}

/// Артист из объекта пользователя api-v2.
#[must_use]
pub(crate) fn artist(v: &Value) -> Option<SearchResult> {
    let id = v.get("id")?.as_u64()?.to_string();
    let name = v.get("username")?.as_str()?.to_owned();
    Some(SearchResult::Artist {
        provider: PROVIDER,
        id,
        name,
    })
}

/// Результаты поиска. Форматы разных эндпоинтов различаются:
/// `/search/tracks` отдаёт `{"collection":[{"kind":"track","track":{…}}…]}`,
/// `/search/users` — объекты-пользователи без обёртки. Различие
/// обрабатывается здесь, а не на вызывающем.
#[must_use]
pub(crate) fn search_results(value: &Value) -> Vec<SearchResult> {
    let Some(collection) = value.get("collection").and_then(Value::as_array) else {
        return Vec::new();
    };
    collection
        .iter()
        .filter_map(|item| match item.get("kind").and_then(Value::as_str) {
            // /search/* отдаёт объекты голыми (`kind` стоит на самом
            // объекте), поэтому обёртку `.track`/`.playlist` берём
            // только когда она есть: `unwrap_or(item)` покрывает обе
            // формы. Замерено 21.09.2026 на /search/tracks.
            Some("track") => track(item.get("track").unwrap_or(item)).map(SearchResult::Track),
            Some("playlist") | Some("album") => playlist(item.get("playlist").unwrap_or(item))
                .map(SearchResult::Playlist),
            // user-объекты приходят и с `kind: "user"`, и без обёртки.
            _ => artist(item.get("user").unwrap_or(item)),
        })
        .collect()
}

/// Плейлисты аккаунта из `me/playlists`.
#[must_use]
pub(crate) fn playlists(value: &Value) -> Vec<Playlist> {
    value
        .get("collection")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(playlist).collect())
        .unwrap_or_default()
}

/// Страница треков плейлиста: `{"tracks":{"collection":[…],"next_href":…}}`.
#[must_use]
pub(crate) fn playlist_tracks(value: &Value) -> (Vec<Track>, Option<String>) {
    let tracks = value.get("tracks");
    let collection = tracks
        .and_then(|t| t.get("collection"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(track).collect())
        .unwrap_or_default();
    let next = tracks
        .and_then(|t| t.get("next_href"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    (collection, next)
}

/// Страница общей коллекции треков: `{"collection":[…],"next_href":…}` —
/// продолжение пагинации и `liked_of`.
#[must_use]
pub(crate) fn collection_page(value: &Value) -> (Vec<Track>, Option<String>) {
    let collection = value
        .get("collection")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(track).collect())
        .unwrap_or_default();
    let next = value
        .get("next_href")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (collection, next)
}

/// Лайки аккаунта. Каждый элемент либо сам трек, либо `{"track":{…}}` —
/// бери `item.get("track").unwrap_or(item)`.
#[must_use]
pub(crate) fn liked_of(value: &Value) -> (Vec<Track>, Option<String>) {
    let collection = value
        .get("collection")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| track(item.get("track").unwrap_or(item)))
                .collect()
        })
        .unwrap_or_default();
    let next = value
        .get("next_href")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (collection, next)
}

/// Лента рекомендаций: элементы `collection` содержат `track` или
/// `playlist` (иногда прямо объект без обёртки). Две полки — «Tracks»
/// и «Playlists», пустые не выдаются: пустая полка в TUI — просто мусор
/// на экране.
#[must_use]
pub(crate) fn stream_shelves(value: &Value) -> Vec<CatalogShelf> {
    let Some(collection) = value.get("collection").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut tracks = Vec::new();
    let mut lists = Vec::new();
    for item in collection {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        if kind.contains("track") {
            if let Some(t) = track(item.get("track").unwrap_or(item)) {
                tracks.push(SearchResult::Track(t));
            }
        } else if kind.contains("playlist") {
            if let Some(p) = playlist(item.get("playlist").unwrap_or(item)) {
                lists.push(SearchResult::Playlist(p));
            }
        }
    }
    let mut shelves = Vec::new();
    if !tracks.is_empty() {
        shelves.push(CatalogShelf {
            title: "Tracks".to_owned(),
            subtitle: None,
            items: tracks,
        });
    }
    if !lists.is_empty() {
        shelves.push(CatalogShelf {
            title: "Playlists".to_owned(),
            subtitle: None,
            items: lists,
        });
    }
    shelves
}

/// Подсказки поисковой строки: `collection[].query`.
#[must_use]
pub(crate) fn suggestions_of(value: &Value) -> Vec<String> {
    value
        .get("collection")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("query").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_track(id: u64) -> Value {
        json!({
            "id": id,
            "title": "Song",
            "duration": 215_000,
            "permalink_url": "https://soundcloud.com/artist/song",
            "artwork_url": "https://i1.sndcdn.com/artwork.jpg",
            "user": {
                "id": 7,
                "username": "Artist",
                "avatar_url": "https://i1.sndcdn.com/avatar.jpg"
            }
        })
    }

    #[test]
    fn track_is_parsed_with_ms_duration() {
        let t = track(&sample_track(42)).expect("трек разбирается");
        assert_eq!(t.id, TrackId::new(PROVIDER, "42"));
        assert_eq!(t.title, "Song");
        assert_eq!(t.artists, vec!["Artist"]);
        assert_eq!(t.duration, Some(Duration::from_millis(215_000)));
        assert_eq!(t.art_url.as_deref(), Some("https://i1.sndcdn.com/artwork.jpg"));
        assert_eq!(
            t.page_url.as_deref(),
            Some("https://soundcloud.com/artist/song")
        );
    }

    #[test]
    fn track_falls_back_to_avatar_art() {
        let mut v = sample_track(1);
        v.as_object_mut()
            .expect("объект")
            .remove("artwork_url");
        let t = track(&v).expect("трек разбирается");
        assert_eq!(t.art_url.as_deref(), Some("https://i1.sndcdn.com/avatar.jpg"));
    }

    #[test]
    fn playlist_is_parsed() {
        let v = json!({
            "id": 9,
            "title": "Mix",
            "track_count": 12,
            "user": {"username": "Artist"}
        });
        let p = playlist(&v).expect("плейлист разбирается");
        assert_eq!(p.id, PlaylistId::new(PROVIDER, "9"));
        assert_eq!(p.title, "Mix");
        assert_eq!(p.subtitle.as_deref(), Some("Artist"));
        assert_eq!(p.track_count, Some(12));
    }

    #[test]
    fn search_results_handle_wrapped_and_bare_users() {
        let value = json!({
            "collection": [
                {"kind": "track", "track": sample_track(1)},
                {"kind": "playlist", "playlist": {"id": 2, "title": "P", "user": {"username": "U"}}},
                {"kind": "user", "user": {"id": 3, "username": "Bare User"}}
            ]
        });
        let results = search_results(&value);
        assert_eq!(results.len(), 3);
        assert!(matches!(&results[0], SearchResult::Track(_)));
        assert!(matches!(&results[1], SearchResult::Playlist(_)));
        assert!(matches!(&results[2], SearchResult::Artist { name, .. } if name == "Bare User"));
    }

    #[test]
    fn liked_handles_both_item_shapes() {
        let value = json!({
            "collection": [
                {"track": sample_track(1)},
                sample_track(2)
            ],
            "next_href": "https://api-v2.soundcloud.com/e1/me/track_likes?cursor=abc"
        });
        let (tracks, next) = liked_of(&value);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[1].id.id, "2");
        assert!(next.is_some());
    }

    #[test]
    fn playlist_tracks_reads_nested_collection() {
        let value = json!({
            "tracks": {
                "collection": [sample_track(1), sample_track(2)],
                "next_href": "https://api-v2.soundcloud.com/playlists/9?offset=2"
            }
        });
        let (tracks, next) = playlist_tracks(&value);
        assert_eq!(tracks.len(), 2);
        assert_eq!(next.as_deref(), Some("https://api-v2.soundcloud.com/playlists/9?offset=2"));
    }

    #[test]
    fn stream_shelves_split_tracks_and_playlists_and_skip_empty() {
        let value = json!({
            "collection": [
                {"type": "track", "track": sample_track(1)},
                {"type": "playlist-repost", "playlist": {"id": 5, "title": "P", "user": {"username": "U"}}}
            ]
        });
        let shelves = stream_shelves(&value);
        assert_eq!(shelves.len(), 2);
        assert_eq!(shelves[0].title, "Tracks");
        assert_eq!(shelves[0].items.len(), 1);
        assert_eq!(shelves[1].title, "Playlists");

        let empty = stream_shelves(&json!({"collection": []}));
        assert!(empty.is_empty());
    }

    #[test]
    fn suggestions_read_query_field() {
        let value = json!({"collection": [{"query": "lofi"}, {"query": "lofi beats"}]});
        assert_eq!(suggestions_of(&value), vec!["lofi", "lofi beats"]);
    }
}
