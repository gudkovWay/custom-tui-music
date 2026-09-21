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

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use tmus_core::CoreError;
use tmus_core::config::Config;
use tmus_core::cookies::CookieSource;
use tmus_core::model::{
    AuthStatus, CatalogShelf, Playlist, PlaylistId, ProviderId, Rating, SearchKind, SearchResult,
    StreamSource, Track, TrackId,
};
use tmus_provider::ytdlp::{YtDlp, YtDlpRequest};
use tmus_provider::{Account, Catalog, Provider, ProviderError, Resolver, Result, TrackPage};

use crate::auth::YtmAuth;
use crate::innertube::InnerTube;

/// Имя для UI. Не из конфига: это имя сервиса, а не пользовательская
/// настройка.
const DISPLAY_NAME: &str = "YouTube Music";

/// Раздел библиотеки с плейлистами. Замерено: HTTP 200, 18 плейлистов.
const LIKED_PLAYLISTS_BROWSE: &str = "FEmusic_liked_playlists";

/// Лента рекомендаций на главной. Замерено 19.09: HTTP 200, карусели
/// `musicCarouselShelfRenderer` с шапками `musicCarouselShelfBasicHeaderRenderer`.
const HOME_BROWSE: &str = "FEmusic_home";

/// Служебный плейлист «Мне нравится». Для модели это обычный плейлист:
/// вызывающий не должен знать, что у YouTube Music лайки — плейлист.
const LIKED_PLAYLIST: &str = "LM";

/// Цепочка клиентов резолва вместо одного. Почему цепочка: волна 403 от
/// googlevideo 20.09.2026 (yt-dlp #17682, #17705) показала, что ссылка,
/// которую yt-dlp успешно выпросил, может не играть. Замеры 21.09.2026:
/// `web_music`/`mweb`/`web_creator` отдают ссылки `c=WEB_REMIX`, на
/// которые обычный GET → 403, а GET с `Range: bytes=0-0` → 206; при этом
/// `web_embedded` (`c=WEB_EMBEDDED_PLAYER`) отдаёт ссылки, играющие и
/// обычным GET → 200. Резолвим по очереди, каждую ссылку пробуем и
/// берём первую живую; yt-dlp-отказ клиента — не конец, а переход к
/// следующему. Порядок: `web_embedded` первым (битрейт ниже — itag 251
/// ~139k против 774 ~266k у WEB_REMIX, — но единственный, чьи ссылки
/// реально играют), прежние клиенты — фолбэком на случай, если Google
/// откатит политику.
/// Пара `(client, база страницы watch)`. База у `web_embedded` —
/// обычный сайт: на `music.youtube.com/watch?v=` yt-dlp этот клиент не
/// берёт и молча уходит в `web_music` (замерено 21.09.2026: тот же
/// трек, флаг `player_client=web_embedded`, а ответ пришёл `c=WEB_REMIX`
/// с 403-ссылкой).
const RESOLVE_CHAIN: &[(&str, &str)] = &[
    ("web_embedded", "https://www.youtube.com/watch?v="),
    ("web_music", "https://music.youtube.com/watch?v="),
    ("mweb", "https://music.youtube.com/watch?v="),
];

/// Провайдер YouTube Music.
pub struct YtMusic {
    id: ProviderId,
    auth: Arc<YtmAuth>,
    tube: InnerTube,
    yt_dlp: YtDlp,
    /// Формат из конфига (`audio_format`): он же уходит в yt-dlp при
    /// резолве, поэтому хранится здесь, а не читается на каждый трек.
    format: String,
    /// Прямой player-резолв (`fast_resolve` секции
    /// `[providers.ytmusic]`). Выключен по умолчанию: клиент VISIONOS
    /// неофициален и не проходит апдейты yt-dlp, а фолбэк страхует.
    fast_resolve: bool,
    /// Сессия для yt-dlp. Cookies у InnerTube берёт [`YtmAuth`] — из того
    /// же источника, но своим jar'ом; источнику здесь принадлежит
    /// последнее слово, потому что его перечитывает `refresh`.
    cookies: CookieSource,
    /// `setVideoId` записей плейлистов: (id плейлиста, id видео) →
    /// служебный идентификатор записи. В памяти, а не в БД, потому что
    /// карта наполняется штатным листингом плейлиста: после рестарта
    /// демона достаточно один раз открыть плейлист, и правка снова
    /// работает — постоянное хранение ничего бы не добавило, зато
    /// потребовало бы инвалидации при чужих правках из веб-интерфейса.
    set_video_ids: Mutex<HashMap<(String, String), String>>,
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
            fast_resolve: config.provider("ytmusic").fast_resolve,
            cookies,
            set_video_ids: Mutex::new(HashMap::new()),
        })
    }

    /// Прямой источник через player-запрос VISIONOS. Ошибка здесь —
    /// любой негатив (сеть, не OK, нет аудио-формата): вызывающий
    /// (`Resolver::resolve`) логирует её в debug и уходит в yt-dlp.
    async fn fast_source(&self, video_id: &str) -> Result<StreamSource> {
        let page = self.tube.player(video_id).await?;
        if !innertube::playable(&page) {
            return Err(ProviderError::Format {
                provider: self.id,
                reason: "player ответил не OK".into(),
            });
        }
        let Some((url, itag, _bitrate)) = innertube::pick_audio_format(&page) else {
            return Err(ProviderError::Format {
                provider: self.id,
                reason: "в ответе player нет аудио-формата".into(),
            });
        };
        let expires_at = innertube::expire_from_url(&url);
        tracing::debug!(provider = "ytmusic", video_id, itag, "fast_resolve: ссылка получена");
        Ok(StreamSource::Remote {
            url,
            // Googlevideo отдаёт 403 чужому User-Agent — отдаём тот же,
            // каким ссылку выпрашивали.
            user_agent: Some(innertube::FAST_USER_AGENT.to_owned()),
            // Ссылка без `expire=` считается короткоживущей (`None`):
            // кэшировать её нельзя, `StreamSource` сам это разрулит.
            expires_at,
        })
    }

    /// Перечитать плейлист и наполнить карту `setVideoId`.
    ///
    /// Вызывается и листингом, и — при промахе карты — удалением трека:
    /// демон кэширует треки плейлиста (TTL), свежий кэш отвечает БЕЗ
    /// захода в провайдера, поэтому «сначала открой плейлист» —
    /// ненадёжный инвариант (живой замер 19.09: rm падал сразу после
    /// рестарта демона при тёплом кэше). Ленивый дозапрос дешевле
    /// честной ошибки: один browse вместо пользовательского отказа.
    /// Устаревшие записи выцветают — перечисление перезаписывает срез
    /// плейлиста целиком.
    async fn refresh_playlist_entries(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<(Track, Option<String>)>> {
        // Префикс `VL` обязателен: browse по сырому id плейлист не открывает.
        let browse_id = parse::playlist_browse_id(playlist_id);
        let pages = self.tube.browse_pages(&browse_id).await?;
        let entries: Vec<_> = pages.iter().flat_map(|page| parse::playlist_entries(page)).collect();

        {
            let mut map = self
                .set_video_ids
                .lock()
                .expect("карта setVideoId не может быть отравлена");
            map.retain(|(pl, _), _| pl != playlist_id);
            for (track, set_video_id) in &entries {
                if let Some(set_video_id) = set_video_id {
                    map.insert((playlist_id.to_owned(), track.id.id.clone()), set_video_id.clone());
                }
            }
        }
        Ok(entries)
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

    fn glyph(&self) -> &'static str {
        "♪"
    }

    fn color(&self) -> &'static str {
        "#ff0000"
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
        let entries = self.refresh_playlist_entries(&playlist.id).await?;
        Ok(entries.into_iter().map(|(track, _)| track).collect())
    }

    /// Страница треков плейлиста. Первая (`cursor: None`) идёт через
    /// `refresh_playlist_entries`-подобный путь: она перезаписывает карту
    /// `setVideoId` целиком, как перечисление и обязано — устаревшие
    /// записи выцветают. Продолжение (`Some(token)`) карту только
    /// пополняет: `retain` по плейлисту стёр бы уже догруженные
    /// страницы, и хвост снова оказался бы недоступен для удаления.
    async fn playlist_tracks_page(
        &self,
        playlist: &PlaylistId,
        cursor: Option<&str>,
    ) -> Result<TrackPage> {
        if playlist.provider != self.id {
            return Err(ProviderError::NoSuchPlaylist(playlist.clone()));
        }
        let (entries, next) = match cursor {
            None => {
                // Префикс `VL` обязателен: browse по сырому id плейлист
                // не открывает (см. refresh_playlist_entries).
                let browse_id = parse::playlist_browse_id(&playlist.id);
                let (page, next) = self.tube.browse_first(&browse_id).await?;
                let entries: Vec<_> = parse::playlist_entries(&page);
                {
                    let mut map = self
                        .set_video_ids
                        .lock()
                        .expect("карта setVideoId не может быть отравлена");
                    map.retain(|(pl, _), _| pl != &playlist.id);
                    for (track, set_video_id) in &entries {
                        if let Some(set_video_id) = set_video_id {
                            map.insert(
                                (playlist.id.clone(), track.id.id.clone()),
                                set_video_id.clone(),
                            );
                        }
                    }
                }
                (entries, next)
            }
            Some(token) => {
                let (page, next) = self.tube.browse_continue(token).await?;
                let entries: Vec<_> = parse::playlist_entries(&page);
                {
                    let mut map = self
                        .set_video_ids
                        .lock()
                        .expect("карта setVideoId не может быть отравлена");
                    // Без retain: только новые записи, прежние страницы
                    // остаются в карте.
                    for (track, set_video_id) in &entries {
                        if let Some(set_video_id) = set_video_id {
                            map.entry((playlist.id.clone(), track.id.id.clone()))
                                .or_insert_with(|| set_video_id.clone());
                        }
                    }
                }
                (entries, next)
            }
        };
        Ok(TrackPage {
            tracks: entries.into_iter().map(|(track, _)| track).collect(),
            next,
        })
    }


    async fn liked(&self) -> Result<Vec<Track>> {
        self.playlist_tracks(&PlaylistId::new(self.id, LIKED_PLAYLIST))
            .await
    }

    async fn home(&self) -> Result<Vec<CatalogShelf>> {
        // Одна страница: продолжения удваивают латентность ради внеэкранных
        // полок (см. browse_pages, если понадобится глубже).
        let page = self.tube.browse(HOME_BROWSE).await?;
        Ok(parse::home(&page))
    }

    async fn rate(&self, track: &TrackId, rating: Rating) -> Result<()> {
        // Чужой трек — не наша оценка: провайдер отвечает только за свои
        // идентификаторы, тот же приём, что в `playlist_tracks`.
        if track.provider != self.id {
            return Err(ProviderError::NoSuchTrack(track.clone()));
        }
        self.tube.like(&track.id, rating).await
    }

    async fn playlist_create(&self, title: &str) -> Result<Playlist> {
        let id = self.tube.playlist_create(title).await?;
        // Описание собирается локально: сервис в ответе отдаёт только id,
        // а ход за перечитыванием библиотеки ради одной строки не нужен.
        // Размер известен точно — плейлист создан пустым.
        Ok(Playlist {
            id: PlaylistId::new(self.id, id),
            title: title.to_owned(),
            subtitle: None,
            art_url: None,
            track_count: Some(0),
        })
    }

    async fn playlist_add(&self, playlist: &PlaylistId, track: &TrackId) -> Result<()> {
        // И плейлист, и трек обязаны быть нашими: править чужой
        // идентификатор провайдер не может, см. `playlist_tracks`/`rate`.
        if playlist.provider != self.id {
            return Err(ProviderError::NoSuchPlaylist(playlist.clone()));
        }
        if track.provider != self.id {
            return Err(ProviderError::NoSuchTrack(track.clone()));
        }
        self.tube.playlist_edit_add(&playlist.id, &track.id).await
    }

    async fn playlist_remove(&self, playlist: &PlaylistId, track: &TrackId) -> Result<()> {
        if playlist.provider != self.id {
            return Err(ProviderError::NoSuchPlaylist(playlist.clone()));
        }
        if track.provider != self.id {
            return Err(ProviderError::NoSuchTrack(track.clone()));
        }
        // `setVideoId` негде взять, кроме перечисления плейлиста. Карта
        // — оптимизация: при промахе (рестарт демона, тёплый кэш
        // треков, ни разу не открытый плейлист) перечитываем плейлист
        // здесь же одним browse и продолжаем. Отказ — только когда
        // трека действительно нет в свежем перечислении.
        let mut set_video_id = self
            .set_video_ids
            .lock()
            .expect("карта setVideoId не может быть отравлена")
            .get(&(playlist.id.clone(), track.id.clone()))
            .cloned();
        if set_video_id.is_none() {
            let entries = self.refresh_playlist_entries(&playlist.id).await?;
            let present = entries
                .iter()
                .any(|(t, svid)| t.id == *track && svid.is_some());
            if !present {
                return Err(ProviderError::NoSuchTrack(track.clone()));
            }
            set_video_id = self
                .set_video_ids
                .lock()
                .expect("карта setVideoId не может быть отравлена")
                .get(&(playlist.id.clone(), track.id.clone()))
                .cloned();
        }
        let Some(set_video_id) = set_video_id else {
            // Перечисление знает трек, но setVideoId у записи нет —
            // сервис отдаёт его не для всех типов записей; вслепую
            // удалять нельзя.
            return Err(ProviderError::Format {
                provider: self.id,
                reason: "у записи нет setVideoId".into(),
            });
        };
        self.tube
            .playlist_edit_remove(&playlist.id, &set_video_id)
            .await
    }

    async fn playlist_delete(&self, playlist: &PlaylistId) -> Result<()> {
        if playlist.provider != self.id {
            return Err(ProviderError::NoSuchPlaylist(playlist.clone()));
        }
        self.tube.playlist_delete(&playlist.id).await?;
        // Плейлиста больше нет — и его setVideoId тоже.
        self.set_video_ids
            .lock()
            .expect("карта setVideoId не может быть отравлена")
            .retain(|(playlist_id, _), _| playlist_id != &playlist.id);
        Ok(())
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
        // страница трека собирается в цикле ниже — база зависит от
        // клиента цепочки.

        // Быстрый путь: player-запрос VISIONOS отдаёт прямую ссылку без
        // yt-dlp (замер: yt-dlp ~4 с и ~335 МБ на процесс). Любой негатив
        // — не ошибка для вызывающего, а повод вернуться к yt-dlp, поэтому
        // причина только в debug-лог. «Made for kids» VISIONOS не отдаёт —
        // штатный случай фолбэка, см. `innertube`.
        if self.fast_resolve {
            match self.fast_source(&track.id).await {
                Ok(source) => return Ok(source),
                Err(reason) => tracing::debug!(
                    provider = "ytmusic",
                    video_id = %track.id,
                    "fast_resolve не сработал, фолбэк на yt-dlp: {reason}"
                ),
            }
        }

        // Цепочка клиентов: первый, чья ссылка прошла пробу, выигрывает.
        // Живость решает [`probe_passed`] (см. тест), здесь только io:
        // резолв, проба, лог. Не победил никто — наружу уходит ошибка с
        // причиной последнего отказа: прежние причины к этому моменту
        // уже история.
        let mut last_error: Option<ProviderError> = None;
        for (client, base) in RESOLVE_CHAIN {
            let page_url = format!("{base}{}", track.id);
            // yt-dlp требует пару `IE_KEY:ARGS`, а не голое имя клиента:
            // `--extractor-args "mweb"` он отвергает с «wrong
            // --extractor-args formatting» (поймано живым прогоном
            // 21.09.2026, юнит-тесты этого не видели). Отдельная
            // привязка — срез обязан жить через .await, а временный
            // массив в выражении запроса жил бы только до него.
            let arg = extractor_arg(client);
            let extractor_args = [arg.as_str()];
            let media = match self
                .yt_dlp
                .media(&YtDlpRequest {
                    provider: self.id,
                    page_url: &page_url,
                    format: &self.format,
                    // Без пина `player_client` yt-dlp уходит в дефолт,
                    // чьи ссылки Google режет 403 — см. `RESOLVE_CHAIN`.
                    extractor_args: &extractor_args,
                    cookies: Some(&self.cookies),
                })
                .await
            {
                Ok(media) => media,
                Err(error) => {
                    tracing::debug!(
                        provider = "ytmusic",
                        video_id = %track.id,
                        client,
                        "клиент цепочки не отдал ссылку, следующий: {error}"
                    );
                    last_error = Some(error);
                    continue;
                }
            };

            // Проба обязательна: yt-dlp отвечает «успехом» и на ссылку,
            // которая при воспроизведении упрётся в 403 (замер 21.09.2026:
            // WEB_REMIX-ссылки, обычный GET 403, с Range: bytes=0-0 — 206).
            let probe_status = crate::innertube::probe(&media.url, media.user_agent.as_deref()).await;
            match probe_status {
                Ok(status) if probe_passed(status) => {
                    tracing::info!(
                        provider = "ytmusic",
                        video_id = %track.id,
                        client,
                        itag = url_param(&media.url, "itag").unwrap_or("?"),
                        ext = %media.ext,
                        probe_status = status,
                        "ссылка резолвлена и прошла пробу"
                    );
                    return Ok(YtDlp::to_stream_source(media));
                }
                Ok(status) => {
                    tracing::debug!(
                        provider = "ytmusic",
                        video_id = %track.id,
                        client,
                        "проба ссылки не прошла (HTTP {status}), следующий клиент"
                    );
                    last_error = Some(ProviderError::Format {
                        provider: self.id,
                        reason: format!("клиент {client}: проба ссылки вернула HTTP {status}"),
                    });
                }
                Err(error) => {
                    tracing::debug!(
                        provider = "ytmusic",
                        video_id = %track.id,
                        client,
                        "проба ссылки не удалась: {error}"
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(ProviderError::Format {
            provider: self.id,
            reason: format!(
                "ни один клиент цепочки ({}) не отдал рабочую ссылку; последняя причина: {}",
                RESOLVE_CHAIN
                    .iter()
                    .map(|(c, _)| *c)
                    .collect::<Vec<_>>()
                    .join(", "),
                last_error
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "нет попыток".into()),
            ),
        })
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

/// Значение параметра из query-части URL. Нужно только для info-лога
/// (itag резолва), поэтому без полного парсинга.
fn url_param<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    url.split(['?', '&'])
        .find_map(|q| q.strip_prefix(key)?.strip_prefix('='))
}

/// Прошла ли ссылка пробу. 200 — полный ответ; 206 — частичный по
/// `Range: bytes=0-0`, и это тоже успех: замер 21.09.2026 показал, что
/// живая googlevideo-ссылка на `bytes=0-0` отвечает именно 206, а
/// мёртвая (класс отказа той волны) — 403.
fn probe_passed(status: u16) -> bool {
    matches!(status, 200 | 206)
}

/// Аргумент `--extractor-args` для клиента цепочки: формат обязан быть
/// `IE_KEY:ARGS`, голое имя клиента yt-dlp отвергает («wrong
/// --extractor-args formatting; it should be IE_KEY:ARGS, not `mweb`»).
/// Ошибка в этом месте валит резолв целиком, а юнит-тесты её не видели —
/// поймал живой прогон 21.09.2026, поэтому формат закреплён тестом.
fn extractor_arg(client: &str) -> String {
    format!("youtube:player_client={client}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_accepts_full_and_range_responses() {
        assert!(probe_passed(200));
        assert!(probe_passed(206));
        // 403 — класс отказа волны 20.09.2026: yt-dlp ссылку отдал,
        // googlevideo воспроизведение зарезал.
        assert!(!probe_passed(403));
        assert!(!probe_passed(404));
        assert!(!probe_passed(500));
    }

    #[test]
    fn extractor_arg_carries_the_ie_key_prefix() {
        // Живой прогон 21.09.2026: без префикса все клиенты цепочки
        // падали на «wrong --extractor-args formatting», и резолв не
        // работал вовсе — при зелёных тестах.
        assert_eq!(extractor_arg("mweb"), "youtube:player_client=mweb");
        assert_eq!(
            extractor_arg("web_embedded"),
            "youtube:player_client=web_embedded"
        );
    }

    #[test]
    fn url_param_reads_query() {
        let url = "https://rr3---sn.googlevideo.com/videoplayback?id=abc&expire=1893456000&itag=251";
        assert_eq!(url_param(url, "itag"), Some("251"));
        assert_eq!(url_param(url, "expire"), Some("1893456000"));
        assert_eq!(url_param(url, "missing"), None);
    }
}
