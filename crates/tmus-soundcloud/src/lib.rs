//! Провайдер SoundCloud: каталог через api-v2, поток через yt-dlp.
//!
//! Правило крейта, нарушение которого = брак: **ни одна структура
//! ответа SoundCloud не покидает этот крейт**. Наружу уходят только
//! типы `tmus-core`, общие для всех провайдеров (см. `tmus-core/src/
//! model.rs`).
//!
//! Замеры, на которых держатся решения (21.09.2026):
//! - резолв `yt-dlp` по `https://api.soundcloud.com/tracks/<id>` →
//!   m4a 160k;
//! - auth: POST `api-auth.soundcloud.com/connect/session` → 200/401;
//! - api-v2: `/search/tracks|albums|playlists|users`,
//!   `/search/queries`, `/me/playlists`, `/playlists/{id}`,
//!   `/e1/me/track_likes` (PUT/DELETE), `/stream`, POST/PUT/DELETE
//!   `/playlists`. Все — с `client_id` в query; auth-эндпоинты без
//!   oauth-параметра отвечают 404, а не 401.

mod api;
mod auth;
mod parse;

use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use tmus_core::config::Config;
use tmus_core::cookies::CookieSource;
use tmus_core::error::CoreError;
use tmus_core::model::{
    AuthStatus, CatalogShelf, Playlist, PlaylistId, ProviderId, Rating, SearchKind, SearchResult,
    StreamSource, Track, TrackId,
};
use tmus_provider::ytdlp::{YtDlp, YtDlpRequest};
use tmus_provider::{
    Account, Catalog, Provider, ProviderError, Resolver, Result, TrackPage,
};

use crate::api::Api;
use crate::auth::ScAuth;

/// Имя для UI. Не из конфига: это имя сервиса, а не пользовательская
/// настройка.
const DISPLAY_NAME: &str = "SoundCloud";

/// Селектор формата для yt-dlp: прогрессивный HTTP-аудио, HLS — только
/// фолбэком. См. поле `SoundCloud::format`.
const SC_FORMAT: &str = "bestaudio[protocol*=http]/bestaudio";

/// Страница лайков при полном дочитывании: SoundCloud отдаёт лайки
/// страницами без понятия «всё сразу», и без предела цикл теоретически
/// не кончается. Предел — тот же, что у YouTube Music: 10 страниц по
/// 24 записи хватает подавляющему большинству аккаунтов.
const MAX_PAGES: usize = 10;

/// Провайдер SoundCloud.
pub struct SoundCloud {
    id: ProviderId,
    auth: Arc<ScAuth>,
    api: Api,
    yt_dlp: YtDlp,
    /// Формат резолва: прогрессивный MP3, а не HLS. Замерено 21.09.2026:
    /// yt-dlp на SoundCloud выбирает HLS-манифест (aac 160k), и офлайн-кэш
    /// сохранил бы `playlist.m3u8` вместо аудио — «скачано, но не играет
    /// офлайн». `protocol*=http` отдаёт прямой `cf-media.sndcdn.com/….mp3`
    /// (128k), который кэшируется честно. Конфиг `audio_format` для
    /// SoundCloud не применяется: его селекторы написаны под YouTube.
    format: &'static str,
    /// Сессия для yt-dlp. Cookies у api-v2 берёт [`ScAuth`] — из того
    /// же источника, но своим jar'ом; источнику здесь принадлежит
    /// последнее слово, потому что его перечитывает `refresh`.
    cookies: CookieSource,
}

impl SoundCloud {
    /// Собрать провайдера.
    ///
    /// Cookies приходят снаружи готовым [`CookieSource`]: автодетект
    /// профиля браузера — дело ядра, а не провайдера, и одна ошибка
    /// настройки должна выглядеть одинаково у всех провайдеров.
    pub fn new(config: &Config, cookies: CookieSource) -> tmus_core::Result<Self> {
        let auth = Arc::new(ScAuth::load(&cookies)?);
        let api = Api::new(Arc::clone(&auth), config)
            // Сборка HTTP-клиента — отказ окружения (TLS-бэкенд), а не
            // сервиса: `ProviderError::Network` здесь соврал бы про
            // причину (образец — `YtMusic::new`).
            .map_err(|error| CoreError::Io(io::Error::other(error.to_string())))?;

        Ok(Self {
            id: ProviderId::SOUNDCLOUD,
            auth,
            api,
            yt_dlp: YtDlp::new(config.yt_dlp.clone()),
            format: SC_FORMAT,
            cookies,
        })
    }
}

#[async_trait]
impl Account for SoundCloud {
    fn provider(&self) -> ProviderId {
        self.id
    }

    fn display_name(&self) -> &str {
        DISPLAY_NAME
    }

    fn glyph(&self) -> &'static str {
        "☁"
    }

    fn color(&self) -> &'static str {
        "#ff5500"
    }

    fn auth(&self) -> AuthStatus {
        self.auth.cached()
    }

    async fn refresh(&self) -> Result<AuthStatus> {
        // Сначала перечитываем cookies: пользователь мог залогиниться
        // уже после старта демона, и тогда чинить нечего.
        let reloaded =
            self.auth
                .reload(&self.cookies)
                .map_err(|error| ProviderError::Auth {
                    provider: self.id,
                    reason: error.to_string(),
                })?;
        if matches!(reloaded, AuthStatus::Missing { .. }) {
            return Ok(reloaded);
        }

        // Токен на месте — но живой ли он, знает только сервис.
        match self.api.verify_session().await {
            // Отказ уже помечен внутри клиента (401 → `Expired`), а
            // для UI это не ошибка, а состояние.
            Ok(AuthStatus::Ready) => {
                self.auth.mark_ready();
                Ok(AuthStatus::Ready)
            }
            Err(ProviderError::Auth { .. }) => Ok(self.auth.cached()),
            Ok(status) => Ok(status),
            Err(other) => Err(other),
        }
    }
}

#[async_trait]
impl Catalog for SoundCloud {
    fn provider(&self) -> ProviderId {
        self.id
    }

    async fn search(&self, query: &str, kind: SearchKind) -> Result<Vec<SearchResult>> {
        let page = self.api.search(query, kind).await?;
        let found = parse::search_results(&page);
        // Вид уже отфильтрован сервисом (отдельный эндпоинт), но отбор
        // дублируется: ответ без учёта вида не должен подсунуть TUI
        // артистов в список треков.
        Ok(found
            .into_iter()
            .filter(|result| matches_kind(result, kind))
            .collect())
    }

    async fn suggest(&self, query: &str) -> Result<Vec<String>> {
        let page = self.api.suggest(query).await?;
        Ok(parse::suggestions_of(&page))
    }

    async fn playlists(&self) -> Result<Vec<Playlist>> {
        let page = self.api.me_playlists().await?;
        Ok(parse::playlists(&page))
    }

    async fn playlist_tracks(&self, playlist: &PlaylistId) -> Result<Vec<Track>> {
        // Дочитываем целиком через тот же курсор, что и страница:
        // плейлист SoundCloud может быть длиннее одной порции.
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .playlist_tracks_page(playlist, cursor.as_deref())
                .await?;
            all.extend(page.tracks);
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(all),
            }
        }
    }

    /// Страница треков плейлиста. Первая (`cursor: None`) идёт через
    /// `/playlists/{id}`: там вложенный `tracks` с собственным
    /// `next_href`. Продолжение (`Some(url)`) — готовый `next_href`,
    /// чья коллекция уже плоская. Разные формы ответа разбирают
    /// разные функции `parse` — сервису так проще ломаться по-своему
    /// на каждом конце.
    async fn playlist_tracks_page(
        &self,
        playlist: &PlaylistId,
        cursor: Option<&str>,
    ) -> Result<TrackPage> {
        if playlist.provider != self.id {
            return Err(ProviderError::NoSuchPlaylist(playlist.clone()));
        }
        let (tracks, next) = match cursor {
            None => {
                let page = self.api.playlist(&playlist.id).await?;
                parse::playlist_tracks(&page)
            }
            Some(url) => {
                let page = self.api.get_url(url).await?;
                parse::collection_page(&page)
            }
        };
        Ok(TrackPage { tracks, next })
    }

    /// Лайки. Звуковый лайк — не плейлист (в отличие от YouTube Music):
    /// отдельный эндпоинт с пагинацией `next_href`. Страницы — по 200
    /// (лимит сервиса), продолжения приходят такими же like-объектами,
    /// поэтому разбор у всех страниц один — [`parse::liked_of`].
    async fn liked(&self) -> Result<Vec<Track>> {
        let mut all = Vec::new();
        let (mut page_items, mut next) = {
            let page = self.api.likes(200).await?;
            parse::liked_of(&page)
        };
        all.append(&mut page_items);
        let mut pages = 1;
        while let Some(url) = next {
            if pages >= MAX_PAGES {
                tracing::warn!(
                    provider = "soundcloud",
                    limit = MAX_PAGES,
                    "лайки не дочитаны: достигнут предел страниц"
                );
                break;
            }
            let page = self.api.get_url(&url).await?;
            let (page_items, next_href) = parse::liked_of(&page);
            all.extend(page_items);
            next = next_href;
            pages += 1;
        }
        Ok(all)
    }

    async fn home(&self) -> Result<Vec<CatalogShelf>> {
        let page = self.api.stream().await?;
        Ok(parse::stream_shelves(&page))
    }

    async fn rate(&self, id: &TrackId, rating: Rating) -> Result<()> {
        // Чужой трек — не наша оценка: провайдер отвечает только за
        // свои идентификаторы.
        if id.provider != self.id {
            return Err(ProviderError::NoSuchTrack(id.clone()));
        }
        match rating {
            Rating::Liked => self.api.like(&id.id).await,
            Rating::None => self.api.unlike(&id.id).await,
            // У SoundCloud нет дизлайка: оценка остаётся локальной у
            // демона. Это не ошибка — вызывающий обязан трактовать
            // отсутствие поддержки как штатный случай.
            Rating::Disliked => Ok(()),
        }
    }

    async fn playlist_create(&self, title: &str) -> Result<Playlist> {
        let page = self.api.create_playlist(title).await?;
        // Сервис обязан вернуть собранный плейлист; без него дальше
        // работать нечем — это поломка формата, а не пустой результат.
        parse::playlist(&page).ok_or_else(|| ProviderError::Format {
            provider: self.id,
            reason: "ответ на создание плейлиста не содержит плейлиста".into(),
        })
    }

    async fn playlist_add(&self, playlist: &PlaylistId, track: &TrackId) -> Result<()> {
        // И плейлист, и трек обязаны быть нашими: править чужой
        // идентификатор провайдер не может.
        if playlist.provider != self.id {
            return Err(ProviderError::NoSuchPlaylist(playlist.clone()));
        }
        if track.provider != self.id {
            return Err(ProviderError::NoSuchTrack(track.clone()));
        }
        // Правка состава (PUT /playlists/:id) переехала на JSON-протокол
        // веб-клиента, формат которого не подтверждён живым запросом:
        // все опробованные тела (id, urn, полные объекты треков) сервис
        // отвергает 400. Честный Unsupported лучше молчаливой лжи.
        Err(ProviderError::Unsupported {
            provider: self.id,
            what: "добавление в плейлист",
        })
    }

    async fn playlist_remove(&self, playlist: &PlaylistId, track: &TrackId) -> Result<()> {
        if playlist.provider != self.id {
            return Err(ProviderError::NoSuchPlaylist(playlist.clone()));
        }
        if track.provider != self.id {
            return Err(ProviderError::NoSuchTrack(track.clone()));
        }
        // См. playlist_add: PUT полного состава не подтверждён.
        Err(ProviderError::Unsupported {
            provider: self.id,
            what: "удаление из плейлиста",
        })
    }

    async fn playlist_delete(&self, playlist: &PlaylistId) -> Result<()> {
        if playlist.provider != self.id {
            return Err(ProviderError::NoSuchPlaylist(playlist.clone()));
        }
        self.api.delete_playlist(&playlist.id).await
    }
}

#[async_trait]
impl Resolver for SoundCloud {
    fn provider(&self) -> ProviderId {
        self.id
    }

    async fn resolve(&self, track: &TrackId) -> Result<StreamSource> {
        if track.provider != self.id {
            return Err(ProviderError::NoSuchTrack(track.clone()));
        }
        // `TrackId` — только числовой id, а yt-dlp нужен URL. Замерено
        // живым прогоном 21.09.2026: yt-dlp резолвит api-URL трека в
        // m4a 160k, страница не обязательна.
        let page_url = format!("https://api.soundcloud.com/tracks/{}", track.id);
        // extractor_args пустой: пин клиента нужен только YouTube,
        // у SoundCloud дефолтный экстрактор yt-dlp стабилен.
        let media = self
            .yt_dlp
            .media(&YtDlpRequest {
                provider: self.id,
                page_url: &page_url,
                format: self.format,
                extractor_args: &[],
                cookies: Some(&self.cookies),
            })
            .await?;
        tracing::info!(
            provider = "soundcloud",
            track_id = %track.id,
            ext = %media.ext,
            "поток резолвлен через yt-dlp"
        );
        Ok(StreamSource::Remote {
            url: media.url,
            // Единственный заголовок, который разрешено передавать в
            // mpv, — тот же, каким ссылку выпрашивали.
            user_agent: media.user_agent,
            expires_at: media.expires_at,
        })
    }
}

impl Provider for SoundCloud {
    fn account(&self) -> &dyn Account {
        self
    }

    fn catalog(&self) -> &dyn Catalog {
        self
    }

    fn resolver(&self) -> &dyn Resolver {
        self
    }
}

/// Совпадает ли найденное с запрошенным видом.
///
/// Альбом выражается плейлистом (в модели нет типа «альбом»), поэтому
/// `Albums` и `Playlists` проверяются одинаково — разделяет их сервис.
fn matches_kind(result: &SearchResult, kind: SearchKind) -> bool {
    matches!(
        (kind, result),
        (SearchKind::Tracks, SearchResult::Track(_))
            | (SearchKind::Artists, SearchResult::Artist { .. })
            | (SearchKind::Albums | SearchKind::Playlists, SearchResult::Playlist(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmus_core::model::PlaylistId;

    #[test]
    fn kind_filter_matches_catalog_types() {
        use tmus_core::model::{Playlist, Track, TrackId};

        let track = SearchResult::Track(Track {
            id: TrackId::new(ProviderId::SOUNDCLOUD, "1"),
            title: "t".into(),
            artists: vec![],
            album: None,
            duration: None,
            art_url: None,
            page_url: None,
        });
        let pl = SearchResult::Playlist(Playlist {
            id: PlaylistId::new(ProviderId::SOUNDCLOUD, "2"),
            title: "p".into(),
            subtitle: None,
            art_url: None,
            track_count: None,
        });
        let artist = SearchResult::Artist {
            provider: ProviderId::SOUNDCLOUD,
            id: "3".into(),
            name: "a".into(),
        };

        assert!(matches_kind(&track, SearchKind::Tracks));
        assert!(!matches_kind(&pl, SearchKind::Tracks));
        // Альбомы выражаются плейлистом.
        assert!(matches_kind(&pl, SearchKind::Albums));
        assert!(matches_kind(&pl, SearchKind::Playlists));
        assert!(matches_kind(&artist, SearchKind::Artists));
        assert!(!matches_kind(&artist, SearchKind::Tracks));
    }

    #[tokio::test]
    async fn resolve_rejects_foreign_track() {
        // Провайдер собирается без сети: cookies из файла не читаются
        // до load, поэтому здесь проверяется только граница чужого id.
        // Сборка Api требует Config — берём дефолтный.
        let dir = std::env::temp_dir().join(format!("sc-resolve-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("временный каталог создаётся");
        let path = dir.join("cookies.txt");
        std::fs::write(&path, ".soundcloud.com\tTRUE\t/\tFALSE\t0\toauth_token\ttok")
            .expect("файл cookies пишется");
        let provider = SoundCloud::new(
            &Config::default(),
            CookieSource::File { path },
        )
        .expect("провайдер собирается");
        let foreign = TrackId::new(ProviderId::YTMUSIC, "1");
        let error = provider
            .resolve(&foreign)
            .await
            .expect_err("чужой трек отвергается без yt-dlp");
        assert!(matches!(error, ProviderError::NoSuchTrack(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
