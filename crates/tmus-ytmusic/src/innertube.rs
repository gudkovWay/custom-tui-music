//! Клиент InnerTube: POST на `music.youtube.com/youtubei/v1/…`.
//!
//! Ключ API не нужен — замерено 17.09.2026: авторизованный `browse` уходит
//! с cookie-сессией и подписью `SAPISIDHASH` и отвечает HTTP 200.
//!
//! Наружу отсюда уходят только `serde_json::Value` и [`ProviderError`]:
//! структуры ответа не покидают крейт, и разбирает их [`crate::parse`].

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use tmus_core::model::{ProviderId, Rating, SearchKind};
use tmus_provider::{ProviderError, Result};

use crate::auth::{ORIGIN, YtmAuth};
use crate::parse;

/// Префикс всех эндпоинтов InnerTube.
const BASE: &str = "https://music.youtube.com/youtubei/v1/";

/// Контекст запроса: `WEB_REMIX` — клиент веб-плеера YouTube Music.
/// `hl=en` запинен намеренно: разбор опирается на английские служебные
/// строки («12 songs», «1.2M views»), а локализованные он бы не узнал.
const CLIENT_NAME: &str = "WEB_REMIX";
const CLIENT_VERSION: &str = "1.20240401.01.00";
const HL: &str = "en";
const GL: &str = "US";

/// Клиент VISIONOS для прямого player-резолва (станза сверена с
/// yt-dlp 2026.09): не требует JS-плеера и PO-токена, отдаёт прямые
/// googlevideo-ссылки по обычной cookie-сессии. Известное ограничение:
/// клиент не отдаёт «made for kids» — такие треки это штатный случай
/// фолбэка на yt-dlp в [`crate::YtMusic::resolve`].
const FAST_CLIENT_NAME: &str = "VISIONOS";
const FAST_CLIENT_VERSION: &str = "1.02";
/// UA VISIONOS-клиента: googlevideo сверяет User-Agent с тем, от кого
/// ссылка выпущена, поэтому он же уходит в `StreamSource::Remote`.
pub(crate) const FAST_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) \
                               AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
/// Числовой идентификатор клиента VISIONOS для `X-YouTube-Client-Name`.
const FAST_CLIENT_NUM: &str = "101";

/// Десктопный Chrome: на «пустой» User-Agent сервис отвечает иначе.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                          Chrome/124.0.0.0 Safari/537.36";

/// Зависший запрос хуже ошибки: каталог за ним просто не отвечает.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Предел страниц продолжений.
///
/// Число страниц не замерялось: в библиотеке хозяина 18 плейлистов, а
/// лайков много. Предел взят с запасом (десяток страниц — порядка тысячи
/// записей) и жёстко: без него обход библиотеки тянулся бы минутами, а
/// пользователь в это время видел бы пустой список.
const MAX_PAGES: usize = 10;

/// Провайдер для сообщений об ошибках.
const PROVIDER: ProviderId = ProviderId::YTMUSIC;

/// Сколько символов тела ответа попадает в сообщение об ошибке: целиком
/// ответ сервиса в строку не влезает, но и «что-то пошло не так» —
/// бесполезно. Режется по символам, а не по байтам: в теле бывает UTF-8.
const SNIPPET_CHARS: usize = 240;

pub struct InnerTube {
    http: reqwest::Client,
    auth: Arc<YtmAuth>,
}

impl InnerTube {
    pub fn new(auth: Arc<YtmAuth>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ProviderError::Network(error.to_string()))?;
        Ok(Self { http, auth })
    }

    pub async fn search(&self, query: &str, kind: SearchKind) -> Result<Value> {
        self.post("search", Self::search_body(query, kind)).await
    }

    pub async fn browse(&self, browse_id: &str) -> Result<Value> {
        self.post("browse", json!({ "browseId": browse_id })).await
    }

    /// Полный ответ страницы watch/очереди плейлиста.
    ///
    /// Очередь воспроизведения и радио: их продолжения живут в
    /// `playlistPanelRenderer`, которого в ответе `browse` нет. Пока
    /// вызывающего внутри крейта нет — очередь ведёт демон, — но метод
    /// обязан работать, а не быть заглушкой.
    #[expect(
        dead_code,
        reason = "эндпоинт очереди: внутри крейта пока не вызывается, нужен вызывающему сверху"
    )]
    pub async fn next(&self, video_id: &str, playlist_id: Option<&str>) -> Result<Value> {
        let mut body = json!({ "videoId": video_id });
        if let Some(playlist_id) = playlist_id {
            body["playlistId"] = Value::String(playlist_id.to_owned());
        }
        self.post("next", body).await
    }

    /// Ответ `player` для прямого резолва потока: клиент VISIONOS
    /// отдаёт googlevideo-ссылки без JS-плеера и PO-токена. Cookies —
    /// обязательны: анонимный запрос на официальном треке отбивается
    /// бот-гейтом (замер 17.09.2026, общий для всех клиентов).
    pub async fn player(&self, video_id: &str) -> Result<Value> {
        self.post_client(
            "player",
            player_body(video_id),
            visionos_client(),
            Some(FAST_CLIENT_NUM),
        )
        .await
    }

    pub async fn suggest(&self, query: &str) -> Result<Value> {
        self.post("music/get_search_suggestions", json!({ "input": query }))
            .await
    }

    /// Поставить оценку видео у YouTube Music.
    ///
    /// `like/like`, `like/dislike` и `like/removelike` — один и тот же
    /// эндпоинт-семейство: тело одно (`target.videoId`), различие только
    /// в имени эндпоинта. Плейлист `LM` (список лайкнутого) здесь не
    /// трогается — это отдельная задача редактирования плейлистов.
    pub async fn like(&self, video_id: &str, rating: Rating) -> Result<()> {
        self.post(like_endpoint(rating), like_body(video_id)).await?;
        Ok(())
    }

    /// Создать закрытый плейлист, вернуть его id.
    ///
    /// Эндпоинт `playlist/create` в ответе отдаёт только `playlistId`:
    /// остальное описание (заголовок, размер) вызывающий собирает сам —
    /// перечитывать библиотеку ради одной строки сервис не заставляет.
    pub async fn playlist_create(&self, title: &str) -> Result<String> {
        let value = self
            .post("playlist/create", playlist_create_body(title))
            .await?;
        playlist_id_from_create(&value).ok_or_else(|| ProviderError::Format {
            provider: PROVIDER,
            reason: format!("playlist/create: нет playlistId: {}", snippet(&value.to_string())),
        })
    }

    /// Удалить плейлист аккаунта.
    ///
    /// В отличие от `edit_playlist`, эндпоинт не отвечает
    /// `STATUS_SUCCEEDED`: живой замер 19.09 вернул HTTP 200 с телом-эхом
    /// `commandExecutorCommand` — и плейлист реально исчез из библиотеки.
    /// Поэтому успех — отсутствие `error`/признака разлогина в теле;
    /// `ensure_edit_succeeded` здесь неприменим.
    pub async fn playlist_delete(&self, playlist_id: &str) -> Result<()> {
        let value = self
            .post("playlist/delete", json!({ "playlistId": playlist_id }))
            .await?;
        if value.get("error").is_none() && unauth_reason(&value).is_none() {
            return Ok(());
        }
        Err(ProviderError::Format {
            provider: PROVIDER,
            reason: format!("playlist/delete: {}", snippet(&value.to_string())),
        })
    }

    /// Добавить видео в плейлист (`browse/edit_playlist`).
    pub async fn playlist_edit_add(&self, playlist_id: &str, video_id: &str) -> Result<()> {
        let value = self
            .post(
                "browse/edit_playlist",
                playlist_edit_body(
                    playlist_id,
                    json!([{ "action": "ACTION_ADD_VIDEO", "addedVideoId": video_id }]),
                ),
            )
            .await?;
        ensure_edit_succeeded("browse/edit_playlist", &value)
    }

    /// Убрать видео из плейлиста.
    ///
    /// `setVideoId` обязателен: это внутренний идентификатор записи
    /// внутри плейлиста, и без него сервис не знает, какую из одинаковых
    /// записей убрать. Он не возвращается никаким эндпоинтом редактирования
    /// — только разбором перечисления плейлиста, поэтому его приносит
    /// вызывающий.
    pub async fn playlist_edit_remove(
        &self,
        playlist_id: &str,
        set_video_id: &str,
    ) -> Result<()> {
        let value = self
            .post(
                "browse/edit_playlist",
                playlist_edit_body(
                    playlist_id,
                    // Тело сверено с youtubei.js (PlaylistManager.
                    // removeVideos): ACTION_REMOVE_VIDEO несёт ТОЛЬКО
                    // setVideoId — removedVideoId там нет. Живой замер
                    // 19.09 не дошёл до валидного запроса (поле svid не
                    // читалось), поэтому идём за проверенным клиентом.
                    json!([{
                        "action": "ACTION_REMOVE_VIDEO",
                        "setVideoId": set_video_id
                    }]),
                ),
            )
            .await?;
        ensure_edit_succeeded("browse/edit_playlist", &value)
    }

    /// Первая страница `browse` плейлиста и токен продолжения.
    ///
    /// Ленивая пагинация нужна потому, что LM у хозяина больше тысячи
    /// треков, а [`MAX_PAGES`] тянет лишь ~1000: хвост плейлиста должен
    /// дозагружаться по требованию, а не выбрасываться вместе с токеном.
    pub async fn browse_first(&self, browse_id: &str) -> Result<(Value, Option<String>)> {
        let page = self.browse(browse_id).await?;
        let next = parse::continuation_token(&page);
        Ok((page, next))
    }

    /// Одна страница `browse` по токену продолжения и токен следующей.
    pub async fn browse_continue(&self, token: &str) -> Result<(Value, Option<String>)> {
        let page = self.post("browse", json!({ "continuation": token })).await?;
        let next = parse::continuation_token(&page);
        Ok((page, next))
    }

    /// Страница `browse` со всеми продолжениями, но не глубже [`MAX_PAGES`].
    pub async fn browse_pages(&self, browse_id: &str) -> Result<Vec<Value>> {
        let (first, mut token) = self.browse_first(browse_id).await?;
        let mut pages = vec![first];

        while pages.len() < MAX_PAGES {
            let Some(current) = token.take() else {
                break;
            };
            let (page, next) = self.browse_continue(&current).await?;
            token = next;
            pages.push(page);
        }
        Ok(pages)
    }

    /// Поиск со всеми продолжениями, но не глубже [`MAX_PAGES`].
    pub async fn search_pages(&self, query: &str, kind: SearchKind) -> Result<Vec<Value>> {
        let first = self.search(query, kind).await?;
        self.continuations("search", first).await
    }

    /// Обход продолжений: эндпоинт тот же, тело — только токен.
    async fn continuations(&self, endpoint: &str, first: Value) -> Result<Vec<Value>> {
        let mut token = parse::continuation_token(&first);
        let mut pages = vec![first];

        while pages.len() < MAX_PAGES {
            let Some(current) = token.take() else {
                break;
            };
            let page = self
                .post(endpoint, json!({ "continuation": current }))
                .await?;
            token = parse::continuation_token(&page);
            pages.push(page);
        }
        Ok(pages)
    }

    fn search_body(query: &str, kind: SearchKind) -> Value {
        json!({ "query": query, "params": search_params(kind) })
    }

    /// Запрос с контекстом клиента, cookies и подписью.
    ///
    /// `Content-Type: application/json` ставит `.json()`; заголовки
    /// `Origin`/`X-Origin`/`X-Goog-AuthUser` сервис требует для запросов с
    /// подписью — без них он отвечает отказом, хотя подпись верна.
    async fn post(&self, endpoint: &str, body: Value) -> Result<Value> {
        self.post_client(endpoint, body, web_remix_client(), None)
            .await
    }

    /// Тот же запрос на другом клиенте InnerTube. Существующие вызовы
    /// (`browse`/`search`/`like`) ходят от `WEB_REMIX`; player-резолв —
    /// от VISIONOS, потому что тот отдаёт прямые ссылки без JS-плеера
    /// и PO-токена.
    async fn post_client(
        &self,
        endpoint: &str,
        body: Value,
        client: Value,
        client_num: Option<&str>,
    ) -> Result<Value> {
        let url = format!("{BASE}{endpoint}?prettyPrint=false");
        let mut request = self
            .http
            .post(url)
            .header("Origin", ORIGIN)
            .header("X-Origin", ORIGIN)
            .header("X-Goog-AuthUser", "0")
            .json(&with_context(body, client));

        if let Some(num) = client_num {
            request = request.header("X-YouTube-Client-Name", num);
        }
        if let Some(cookie) = self.auth.cookie_header() {
            request = request.header("Cookie", cookie);
        }
        if let Some(authorization) = self.auth.authorization(now_unix()) {
            request = request.header("Authorization", authorization);
        }

        let response = request
            .send()
            .await
            .map_err(|error| ProviderError::Network(format!("{endpoint}: {error}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| ProviderError::Network(format!("{endpoint}: {error}")))?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(self.sign_out(endpoint, &body));
        }
        if !status.is_success() {
            return Err(ProviderError::Network(format!(
                "{endpoint} ответил HTTP {status}: {}",
                snippet(&body)
            )));
        }

        let value: Value = serde_json::from_str(&body).map_err(|error| ProviderError::Format {
            provider: PROVIDER,
            reason: format!("{endpoint}: {error}"),
        })?;

        // Разлогин приходит и телом с кодом 200 — по одному HTTP-статусу
        // его не поймать.
        if let Some(reason) = unauth_reason(&value) {
            return Err(self.sign_out(endpoint, &reason));
        }
        Ok(value)
    }

    /// Отбой по авторизации: помечаем сессию истёкшей, чтобы UI предложил
    /// переподключение, а не «попробуйте позже».
    fn sign_out(&self, endpoint: &str, reason: &str) -> ProviderError {
        self.auth.mark_expired();
        ProviderError::Auth {
            provider: PROVIDER,
            reason: format!("{endpoint}: {}", snippet(reason)),
        }
    }
}

/// Имя эндпоинта оценки для [`Rating`]: like/dislike/removelike.
fn like_endpoint(rating: Rating) -> &'static str {
    match rating {
        Rating::None => "like/removelike",
        Rating::Liked => "like/like",
        Rating::Disliked => "like/dislike",
    }
}

/// Тело запроса оценки: единственное поле — идентификатор видео.
fn like_body(video_id: &str) -> Value {
    json!({ "target": { "videoId": video_id } })
}

/// Тело создания плейлиста: заголовок плюс `PRIVATE` — плейлист создаётся
/// закрытым всегда, публичность у библиотеки хозяина не переключается из
/// плеера.
fn playlist_create_body(title: &str) -> Value {
    json!({ "title": title, "privacyStatus": "PRIVATE" })
}

/// Тело правки плейлиста: список действий над записями. Одним вызовом
/// сервис позволяет и добавить, и убрать — различие только в элементах
/// `actions`, поэтому тело собирает здесь один сборщик.
fn playlist_edit_body(playlist_id: &str, actions: Value) -> Value {
    json!({ "playlistId": playlist_id, "actions": actions })
}

/// `playlistId` из ответа `playlist/create`.
fn playlist_id_from_create(value: &Value) -> Option<String> {
    value
        .get("playlistId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Успех правки плейлиста/удаления: сервис отвечает `STATUS_SUCCEEDED`
/// в теле с HTTP 200, отказ приходит тоже телом — по статусу HTTP это
/// не отличить.
fn ensure_edit_succeeded(endpoint: &str, value: &Value) -> Result<()> {
    if value.get("status").and_then(Value::as_str) == Some("STATUS_SUCCEEDED") {
        Ok(())
    } else {
        Err(ProviderError::Format {
            provider: PROVIDER,
            reason: format!("{endpoint}: {}", snippet(&value.to_string())),
        })
    }
}

/// Тело запроса: контекст клиента плюс поля конкретного эндпоинта.
fn with_context(fields: Value, client: Value) -> Value {
    let mut body = json!({ "context": { "client": client } });
    if let (Some(map), Value::Object(fields)) = (body.as_object_mut(), fields) {
        map.extend(fields);
    }
    body
}

/// Станца клиента `WEB_REMIX`. `hl=en` запинен намеренно: разбор
/// опирается на английские служебные строки («12 songs», «1.2M views»),
/// а локализованные он бы не узнал.
fn web_remix_client() -> Value {
    json!({
        "clientName": CLIENT_NAME,
        "clientVersion": CLIENT_VERSION,
        "hl": HL,
        "gl": GL
    })
}

/// Станца клиента VISIONOS для player-резолва (сверена с yt-dlp 2026.09).
fn visionos_client() -> Value {
    json!({
        "clientName": FAST_CLIENT_NAME,
        "clientVersion": FAST_CLIENT_VERSION,
        "deviceMake": "Apple",
        "deviceModel": "RealityDevice17,1",
        "osName": "visionOS",
        "osVersion": "26.5.23O471",
        "hl": HL,
        "gl": GL
    })
}

/// Тело player-запроса. `contentCheckOk`/`racyCheckOk` снимают
/// промежуточные подтверждения возраста/контента, на которые клиенту
/// нечем ответить.
fn player_body(video_id: &str) -> Value {
    json!({
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true
    })
}

/// Играбельность по ответу player: сервис сам решает, отдаёт ли он
/// поток этому клиенту.
pub(crate) fn playable(value: &Value) -> bool {
    value["playabilityStatus"]["status"].as_str() == Some("OK")
}

/// Выбор аудио-формата из ответа player.
///
/// Приоритет фиксированный: itag 140 (m4a, совместим со всем), затем
/// 251 (opus, наш основной формат), затем просто максимальный битрейт
/// среди аудио — сервис мог убрать привычные itag'и, но играть должно
/// всё равно. Видео-форматы отсекаются по `mimeType`: в
/// `adaptiveFormats` они лежат вперемешку с аудио.
pub(crate) fn pick_audio_format(value: &Value) -> Option<(String, u64, u64)> {
    let formats = value["streamingData"]["adaptiveFormats"].as_array()?;
    let mut best: Option<(String, u64, u64)> = None;
    let by_itag = |itag: u64| {
        formats.iter().find_map(|f| {
            let mime = f["mimeType"].as_str()?;
            if !mime.contains("audio/") || f["itag"].as_u64() != Some(itag) {
                return None;
            }
            Some((
                f["url"].as_str()?.to_owned(),
                itag,
                f["bitrate"].as_u64().unwrap_or_default(),
            ))
        })
    };
    if let Some(found) = by_itag(140).or_else(|| by_itag(251)) {
        return Some(found);
    }
    for f in formats {
        if !f["mimeType"].as_str().is_some_and(|m| m.contains("audio/")) {
            continue;
        }
        let candidate = (
            f["url"].as_str()?.to_owned(),
            f["itag"].as_u64().unwrap_or_default(),
            f["bitrate"].as_u64().unwrap_or_default(),
        );
        if best.as_ref().is_none_or(|(_, _, b)| candidate.2 > *b) {
            best = Some(candidate);
        }
    }
    best
}

/// Срок жизни googlevideo-ссылки из её параметра `expire=` (unix-секунды).
///
/// Параметра нет — `None`: незнакомый URL считается короткоживущим,
/// и `StreamSource::Remote` с `expires_at: None` кэшироваться не будет.
pub(crate) fn expire_from_url(url: &str) -> Option<SystemTime> {
    let start = url.split(&['?', '&']).find_map(|q| q.strip_prefix("expire="))?;
    let ts: u64 = start.parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(ts))
}

/// Параметры фильтра поиска WEB_REMIX.
///
/// Раскодированные значения выглядят так: `field 17 { field 1: <код> }` плюс
/// постоянный хвост `10 09 10 05 10 0a 10 03 10 04`, где код означает вид:
/// 1 — треки, 2 — альбомы, 3 — артисты, 4 — плейлисты. Кодировать это
/// заново нечем: `base64` в зависимостях крейта нет, а значения стабильны.
fn search_params(kind: SearchKind) -> &'static str {
    match kind {
        SearchKind::Tracks => "EgWKAQIIAWoKEAkQBRAKEAMQBA==",
        SearchKind::Albums => "EgWKAQIYAWoKEAkQChAFEAMQBA==",
        SearchKind::Artists => "EgWKAQIgAWoKEAkQChAFEAMQBA==",
        SearchKind::Playlists => "EgWKAQIoAWoKEAkQChAFEAMQBA==",
    }
}

/// Признак разлогина в теле ответа: `{"error":{"code":403,…}}`.
fn unauth_reason(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    let status = error.get("status").and_then(Value::as_str).unwrap_or_default();
    let code = error.get("code").and_then(Value::as_u64);

    let unauthorized = status.contains("UNAUTHENTICATED")
        || status.contains("PERMISSION_DENIED")
        || matches!(code, Some(401 | 403));
    unauthorized.then(|| {
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(status)
            .to_owned()
    })
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Начало тела ответа: сообщение об ошибке должно помещаться в строку, а
/// ответ сервиса — быть видно.
fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= SNIPPET_CHARS {
        return trimmed.to_owned();
    }
    let head: String = trimmed.chars().take(SNIPPET_CHARS).collect();
    format!("{head}…")
}

/// Проба googlevideo-ссылки перед выдачей плееру: обычный GET с
/// `Range: bytes=0-0` и тем User-Agent, каким ссылку выпрашивали
/// (googlevideo сверяет UA — чужой получает отказ). Возвращает
/// HTTP-статус; живость статуса решает [`crate::probe_passed`].
///
/// Почему проба обязательна: с волны 20.09.2026 (yt-dlp #17682/#17705)
/// yt-dlp отвечает «успехом» и на мёртвую ссылку — WEB_REMIX-ссылки
/// резолвятся, но обычный GET по ним даёт 403; та же ссылка с
/// `Range: bytes=0-0` (≤ 1 MiB) даёт 206, весь файл разом — снова 403.
/// Проба в 1 байт ловит ровно этот класс отказа до того, как плеер
/// покажет «тихий stopped». Замерено 21.09.2026, 3/3 раунда.
pub(crate) async fn probe(url: &str, user_agent: Option<&str>) -> Result<u16> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(PROBE_TIMEOUT);
    if let Some(user_agent) = user_agent {
        builder = builder.user_agent(user_agent);
    }
    let http = builder
        .build()
        .map_err(|error| ProviderError::Network(error.to_string()))?;
    let response = http
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|error| ProviderError::Network(error.to_string()))?;
    Ok(response.status().as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_maps_to_like_endpoints() {
        // Три имени эндпоинта — часть контракта InnerTube: тело у всех
        // одно, различие только в пути. Проверяем и путь, и собранный
        // URL, каким его строит `post`.
        assert_eq!(like_endpoint(Rating::Liked), "like/like");
        assert_eq!(like_endpoint(Rating::Disliked), "like/dislike");
        assert_eq!(like_endpoint(Rating::None), "like/removelike");

        let url = format!("{}{}?prettyPrint=false", BASE, like_endpoint(Rating::Liked));
        assert_eq!(url, "https://music.youtube.com/youtubei/v1/like/like?prettyPrint=false");
    }

    #[test]
    fn like_body_targets_video_id() {
        // Тело должно быть ровно target.videoId: лишние поля InnerTube
        // у оценок игнорирует, а отсутствие target — валит.
        let body = like_body("dQw4w9WgXcQ");
        assert_eq!(
            body,
            json!({ "target": { "videoId": "dQw4w9WgXcQ" } })
        );
    }

    #[test]
    fn playlist_create_body_is_private_with_title() {
        // Схема сверена с youtubei: без privacyStatus сервис молча
        // создаёт плейлист с неопределённой видимостью — фиксируем
        // PRIVATE явно.
        assert_eq!(
            playlist_create_body("Мой плейлист"),
            json!({ "title": "Мой плейлист", "privacyStatus": "PRIVATE" })
        );
    }

    #[test]
    fn playlist_edit_bodies_match_actions() {
        // Добавление: только addedVideoId. Удаление: removedVideoId плюс
        // setVideoId — без него сервис не знает, какую запись убрать.
        assert_eq!(
            playlist_edit_body("PLabc", json!([{ "action": "ACTION_ADD_VIDEO", "addedVideoId": "vid1" }])),
            json!({ "playlistId": "PLabc", "actions": [
                { "action": "ACTION_ADD_VIDEO", "addedVideoId": "vid1" }
            ]})
        );
        // Удаление — ровно как у youtubei.js: только setVideoId,
        // без removedVideoId (сверка 19.09).
        assert_eq!(
            playlist_edit_body(
                "PLabc",
                json!([{ "action": "ACTION_REMOVE_VIDEO", "setVideoId": "sv1" }])
            ),
            json!({ "playlistId": "PLabc", "actions": [
                { "action": "ACTION_REMOVE_VIDEO", "setVideoId": "sv1" }
            ]})
        );
    }

    #[test]
    fn create_answer_yields_playlist_id() {
        assert_eq!(
            playlist_id_from_create(&json!({ "playlistId": "PLnew1" })),
            Some("PLnew1".to_owned())
        );
        // Пустой или отсутствующий id — не ответ: вызывающий обязан
        // увидеть ошибку формата, а не пустое имя плейлиста.
        assert_eq!(playlist_id_from_create(&json!({ "playlistId": "" })), None);
        assert_eq!(playlist_id_from_create(&json!({})), None);
    }

    #[test]
    fn edit_succeeded_only_on_status_succeeded() {
        assert!(ensure_edit_succeeded("e", &json!({ "status": "STATUS_SUCCEEDED" })).is_ok());
        // Отказ приходит с HTTP 200: статус в теле — единственный признак.
        let failed = ensure_edit_succeeded("e", &json!({ "status": "STATUS_FAILED" }));
        assert!(failed.is_err());
        assert!(ensure_edit_succeeded("e", &json!({})).is_err());
    }

    /// Компактный фейк ответа player: OK, два аудио-формата и один
    /// видео. Пропорции полей — как в живом ответе, значения — свои.
    fn player_ok_json() -> Value {
        json!({
            "playabilityStatus": { "status": "OK" },
            "streamingData": { "adaptiveFormats": [
                { "itag": 18, "mimeType": "video/mp4; codecs=\"avc1.42001E\"",
                  "bitrate": 500_000, "url": "https://gv/video18" },
                { "itag": 251, "mimeType": "audio/webm; codecs=\"opus\"",
                  "bitrate": 129_000, "url": "https://gv/audio251?expire=1893456000" },
                { "itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                  "bitrate": 130_000, "url": "https://gv/audio140?expire=1893456000" }
            ]}
        })
    }

    #[test]
    fn playable_only_on_ok_status() {
        assert!(playable(&player_ok_json()));
        // Бот-гейт и прочие отказы приходят статусом в теле с HTTP 200.
        let error = json!({ "playabilityStatus": { "status": "PLAYABILITY_ERROR" } });
        assert!(!playable(&error));
    }

    #[test]
    fn audio_format_prefers_itag_140_then_251() {
        let (_, itag, _) = pick_audio_format(&player_ok_json()).expect("аудио есть");
        assert_eq!(itag, 140, "m4a приоритетнее opus");

        // Без 140 берётся 251.
        let mut no_140 = player_ok_json();
        no_140["streamingData"]["adaptiveFormats"]
            .as_array_mut()
            .expect("массив")
            .retain(|f| f["itag"] != 140);
        let (url, itag, bitrate) = pick_audio_format(&no_140).expect("аудио есть");
        assert_eq!(itag, 251);
        assert_eq!(bitrate, 129_000);
        assert!(url.contains("audio251"));
    }

    #[test]
    fn audio_format_falls_back_to_max_bitrate_and_skips_video() {
        // Привычных itag'ов нет — берётся максимальный аудио-битрейт,
        // видео-формат игнорируется даже при большем bitrate.
        let value = json!({
            "playabilityStatus": { "status": "OK" },
            "streamingData": { "adaptiveFormats": [
                { "itag": 18, "mimeType": "video/mp4", "bitrate": 900_000,
                  "url": "https://gv/video18" },
                { "itag": 250, "mimeType": "audio/webm", "bitrate": 61_000,
                  "url": "https://gv/audio250" },
                { "itag": 249, "mimeType": "audio/webm", "bitrate": 48_000,
                  "url": "https://gv/audio249" }
            ]}
        });
        let (url, itag, _) = pick_audio_format(&value).expect("аудио есть");
        assert_eq!(itag, 250);
        assert!(url.contains("audio250"));

        // Аудио нет вовсе (или adaptiveFormats пуст) — None.
        let empty = json!({ "streamingData": { "adaptiveFormats": [] } });
        assert_eq!(pick_audio_format(&empty), None);
        assert_eq!(pick_audio_format(&json!({})), None);
    }

    #[test]
    fn expire_parsed_from_url_and_absent_is_none() {
        let at = expire_from_url("https://gv/audio140?expire=1893456000&itag=140")
            .expect("expire есть");
        assert_eq!(at, UNIX_EPOCH + Duration::from_secs(1_893_456_000));
        assert_eq!(expire_from_url("https://gv/audio140"), None);
    }

    /// Фейк страницы `browse` плейлиста: одна запись с `setVideoId` и,
    /// по желанию, токен продолжения. Пропорции полей — как в живом
    /// ответе (`singleColumnBrowseResultsRenderer` →
    /// `musicPlaylistShelfRenderer`), значения — свои.
    fn playlist_page_json(continuation: Option<&str>) -> Value {
        let mut page = json!({
            "contents": { "singleColumnBrowseResultsRenderer": { "contents": [
                { "musicPlaylistShelfRenderer": { "contents": [
                    { "musicResponsiveListItemRenderer": {
                        "playlistItemData": {
                            "videoId": "vid1",
                            "playlistSetVideoId": "SVabc123"
                        },
                        "flexColumns": [
                            { "musicResponsiveListItemFlexColumnRenderer": { "text": {
                                "runs": [{ "text": "Трек" }]
                            } } }
                        ]
                    } }
                ] } }
            ] } }
        });
        if let Some(token) = continuation {
            page["continuationItemRenderer"] = json!({
                "continuationEndpoint": { "continuationCommand": { "token": token } }
            });
        }
        page
    }

    #[test]
    fn browse_first_fixture_yields_entry_and_token() {
        // Контракт `browse_first`: страница разбирается `playlist_entries`
        // вместе с `playlistSetVideoId`, токен продолжения выживает.
        let (page, next) = (playlist_page_json(Some("tok-2")), Some("tok-2".to_owned()));
        let entries = parse::playlist_entries(&page);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1.as_deref(), Some("SVabc123"));
        assert_eq!(parse::continuation_token(&page), next);
    }

    #[test]
    fn browse_continue_fixture_ends_without_token() {
        // Последняя страница токена не несёт: пагинация обязана
        // остановиться именно на этом сигнале, а не на счётчике страниц.
        let page = playlist_page_json(None);
        assert!(parse::playlist_entries(&page).len() == 1);
        assert_eq!(parse::continuation_token(&page), None);
    }
}
