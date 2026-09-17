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

use tmus_core::model::{ProviderId, SearchKind};
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

    pub async fn suggest(&self, query: &str) -> Result<Value> {
        self.post("music/get_search_suggestions", json!({ "input": query }))
            .await
    }

    /// Страница `browse` со всеми продолжениями, но не глубже [`MAX_PAGES`].
    pub async fn browse_pages(&self, browse_id: &str) -> Result<Vec<Value>> {
        let first = self.browse(browse_id).await?;
        self.continuations("browse", first).await
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
        let url = format!("{BASE}{endpoint}?prettyPrint=false");
        let mut request = self
            .http
            .post(url)
            .header("Origin", ORIGIN)
            .header("X-Origin", ORIGIN)
            .header("X-Goog-AuthUser", "0")
            .json(&with_context(body));

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

/// Тело запроса: контекст клиента плюс поля конкретного эндпоинта.
fn with_context(fields: Value) -> Value {
    let mut body = json!({
        "context": { "client": {
            "clientName": CLIENT_NAME,
            "clientVersion": CLIENT_VERSION,
            "hl": HL,
            "gl": GL
        } }
    });
    if let (Some(map), Value::Object(fields)) = (body.as_object_mut(), fields) {
        map.extend(fields);
    }
    body
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
