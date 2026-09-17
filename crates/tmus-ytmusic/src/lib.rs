//! Провайдер YouTube Music: каталог через InnerTube, поток через yt-dlp.
//!
//! Правило крейта, нарушение которого = брак: **ни одна структура ответа
//! InnerTube не покидает этот крейт**. Наружу уходят только типы
//! `tmus-core`, общие для всех провайдеров. Причина замеренная: InnerTube
//! неофициальный и ломается молча; пока его поля не видны никому, кроме
//! этого крейта, поломка сервиса остаётся поломкой одного провайдера, а не
//! всего приложения.
//!
//! Замеры, на которых держатся решения (17.09.2026):
//! - авторизованный `browse` по `FEmusic_liked_playlists` → HTTP 200,
//!   18 плейлистов библиотеки;
//! - анонимный `search` → HTTP 200, 26 результатов;
//! - `yt-dlp -J … -f 'bestaudio[acodec=opus]/bestaudio'` → itag 774, opus.

mod auth;
mod innertube;
mod parse;

use std::io;
use std::sync::Arc;

use async_trait::async_trait;

use tmus_core::CoreError;
use tmus_core::config::Config;
use tmus_core::cookies::CookieSource;
use tmus_core::model::{
    AuthStatus, Playlist, PlaylistId, ProviderId, SearchKind, SearchResult, StreamSource, Track,
    TrackId,
};
use tmus_provider::ytdlp::{YtDlp, YtDlpRequest};
use tmus_provider::{Account, Catalog, Provider, ProviderError, Resolver, Result};

use crate::auth::YtmAuth;
use crate::innertube::InnerTube;

/// Имя для UI. Не из конфига: это имя сервиса, а не пользовательская
/// настройка.
const DISPLAY_NAME: &str = "YouTube Music";

/// Раздел библиотеки с плейлистами. Замерено: HTTP 200, 18 плейлистов.
const LIKED_PLAYLISTS_BROWSE: &str = "FEmusic_liked_playlists";

/// Служебный плейлист «Мне нравится». Для модели это обычный плейлист:
/// вызывающий не должен знать, что у YouTube Music лайки — плейлист.
const LIKED_PLAYLIST: &str = "LM";

/// `player_client` обязателен, и это не украшение: дефолтный выбор yt-dlp
/// уходит в `web_creator`, URL резолвится, а GET по нему даёт
/// `403 Forbidden`. Замерено 17.09.2026.
const EXTRACTOR_ARGS: &[&str] = &["youtube:player_client=web_music"];

/// Провайдер YouTube Music.
pub struct YtMusic {
    id: ProviderId,
    auth: Arc<YtmAuth>,
    tube: InnerTube,
    yt_dlp: YtDlp,
    /// Формат из конфига (`audio_format`): он же уходит в yt-dlp при
    /// резолве, поэтому хранится здесь, а не читается на каждый трек.
    format: String,
    /// Сессия для yt-dlp. Cookies у InnerTube берёт [`YtmAuth`] — из того
    /// же источника, но своим jar'ом; источнику здесь принадлежит
    /// последнее слово, потому что его перечитывает `refresh`.
    cookies: CookieSource,
}

impl YtMusic {
    /// Собрать провайдера.
    ///
    /// Cookies приходят снаружи готовым [`CookieSource`]: автодетект
    /// профиля браузера — дело ядра, а не провайдера, и одна ошибка
    /// настройки должна выглядеть одинаково у всех провайдеров.
    pub fn new(config: &Config, cookies: CookieSource) -> tmus_core::Result<Self> {
        let auth = Arc::new(YtmAuth::load(&cookies)?);
        let tube = InnerTube::new(Arc::clone(&auth))
            // Сборка HTTP-клиента — отказ окружения (TLS-бэкенд), а не
            // сервиса: `ProviderError::Network` здесь соврал бы про причину.
            .map_err(|error| CoreError::Io(io::Error::other(error.to_string())))?;

        Ok(Self {
            id: ProviderId::YTMUSIC,
            auth,
            tube,
            yt_dlp: YtDlp::new(config.yt_dlp.clone()),
            format: config.audio_format.clone(),
            cookies,
        })
    }
}

#[async_trait]
impl Account for YtMusic {
    fn provider(&self) -> ProviderId {
        self.id
    }

    fn display_name(&self) -> &str {
        DISPLAY_NAME
    }

    fn auth(&self) -> AuthStatus {
        self.auth.cached()
    }

    async fn refresh(&self) -> Result<AuthStatus> {
        // Сначала перечитываем cookies: пользователь мог залогиниться уже
        // после старта демона, и тогда чинить нечего.
        let reloaded = self
            .auth
            .reload(&self.cookies)
            .map_err(|error| ProviderError::Auth {
                provider: self.id,
                reason: error.to_string(),
            })?;
        if matches!(reloaded, AuthStatus::Missing { .. }) {
            return Ok(reloaded);
        }

        // SAPISID на месте — но живой ли он, знает только сервис.
        match self.tube.browse(LIKED_PLAYLISTS_BROWSE).await {
            Ok(_) => {
                self.auth.mark_ready();
                Ok(AuthStatus::Ready)
            }
            // Отказ уже помечен внутри клиента (401/403 → `Expired`), а
            // для UI это не ошибка, а состояние.
            Err(ProviderError::Auth { .. }) => Ok(self.auth.cached()),
            Err(other) => Err(other),
        }
    }
}

#[async_trait]
impl Catalog for YtMusic {
    fn provider(&self) -> ProviderId {
        self.id
    }

    async fn search(&self, query: &str, kind: SearchKind) -> Result<Vec<SearchResult>> {
        let pages = self.tube.search_pages(query, kind).await?;
        let found: Vec<SearchResult> = pages
            .iter()
            .flat_map(|page| parse::search_results(page))
            .collect();
        // Вид уже отфильтрован сервисом (параметры запроса), но отбор
        // дублируется: ответ без учёта вида не должен подсунуть TUI
        // артистов в список треков.
        Ok(found
            .into_iter()
            .filter(|result| matches_kind(result, kind))
            .collect())
    }

    async fn suggest(&self, query: &str) -> Result<Vec<String>> {
        let page = self.tube.suggest(query).await?;
        Ok(parse::suggestions(&page))
    }

    async fn playlists(&self) -> Result<Vec<Playlist>> {
        let pages = self.tube.browse_pages(LIKED_PLAYLISTS_BROWSE).await?;
        Ok(pages.iter().flat_map(|page| parse::playlists(page)).collect())
    }

    async fn playlist_tracks(&self, playlist: &PlaylistId) -> Result<Vec<Track>> {
        if playlist.provider != self.id {
            return Err(ProviderError::NoSuchPlaylist(playlist.clone()));
        }
        // Префикс `VL` обязателен: browse по сырому id плейлист не открывает.
        let browse_id = parse::playlist_browse_id(&playlist.id);
        let pages = self.tube.browse_pages(&browse_id).await?;
        Ok(pages.iter().flat_map(|page| parse::tracks(page)).collect())
    }

    async fn liked(&self) -> Result<Vec<Track>> {
        self.playlist_tracks(&PlaylistId::new(self.id, LIKED_PLAYLIST))
            .await
    }
}

#[async_trait]
impl Resolver for YtMusic {
    fn provider(&self) -> ProviderId {
        self.id
    }

    async fn resolve(&self, track: &TrackId) -> Result<StreamSource> {
        if track.provider != self.id {
            return Err(ProviderError::NoSuchTrack(track.clone()));
        }
        // `TrackId` — это только идентификатор видео, а yt-dlp нужен URL:
        // собираем страницу трека, она у yt-dlp же и разрешается.
        let page_url = format!("https://music.youtube.com/watch?v={}", track.id);

        let media = self
            .yt_dlp
            .media(&YtDlpRequest {
                provider: self.id,
                page_url: &page_url,
                format: &self.format,
                // Без этого аргумента ссылка резолвится, а воспроизведение
                // упирается в `403 Forbidden` — см. `EXTRACTOR_ARGS`.
                extractor_args: EXTRACTOR_ARGS,
                cookies: Some(&self.cookies),
            })
            .await?;

        Ok(YtDlp::to_stream_source(media))
    }
}

impl Provider for YtMusic {
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
