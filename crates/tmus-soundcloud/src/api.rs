//! HTTP-клиент api-v2 SoundCloud.
//!
//! Структуры ответов наружу не уходят: [`request`] возвращает
//! `serde_json::Value`, разбор — дело [`crate::parse`]. То же правило
//! крейта, что и у InnerTube: неофициальный API меняется молча, и
//! поломка должна оставаться внутри провайдера.
//!
//! Замеры, на которых держатся решения (21.09.2026):
//! - `POST api-auth.soundcloud.com/connect/session` с телом
//!   `{"session":{"access_token":…}}` → 200 на живой сессии, 401 на
//!   отвергнутой;
//! - `client_id` не нужен в заголовках, но обязателен в query каждого
//!   запроса к api-v2; без oauth-параметра auth-эндпоинты отвечают
//!   404, а не 401.

use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use serde_json::{Value, json};
use tmus_core::config::Config;
use tmus_core::model::{ProviderId, SearchKind};
use tmus_provider::ProviderError;

use crate::auth::{ScAuth, MISSING_HINT};

const API_BASE: &str = "https://api-v2.soundcloud.com/";
const SESSION_URL: &str = "https://api-auth.soundcloud.com/connect/session";

/// Живой client_id на 21.09.2026. Извлечение с сайта — ленивое: ходить
/// в сеть за JS-ассетами на старте демона нельзя, поэтому стартуем с
/// известного рабочего значения и переизвлекаем только после отказа.
const FALLBACK_CLIENT_ID: &str = "Pb72ranhoyt6gw7hM7TkzUItXlMWSNSo";

/// Десктопный Chrome: на «пустой» User-Agent сервис отвечает иначе
/// (та же причина, что в `tmus-ytmusic/src/innertube.rs`).
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                          Chrome/124.0.0.0 Safari/537.36";

/// Зависший запрос хуже ошибки: каталог за ним просто не отвечает.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Сниппет тела в ошибку разбора. Режется по символам, а не по байтам:
/// в теле бывает UTF-8.
const SNIPPET_CHARS: usize = 240;

/// HTTP-клиент api-v2 SoundCloud.
pub struct Api {
    http: reqwest::Client,
    auth: std::sync::Arc<ScAuth>,
    /// client_id кэшируется: строка живёт месяцами, а переизвлечение
    /// стоит похода в сеть за страницей и JS-ассетами. Пустая строка —
    /// маркер «кэш отравлен, надо переизвлечь».
    client_id: RwLock<String>,
    /// Числовой id аккаунта из `/me`. Пути библиотеки строятся как
    /// `users/:userId/…`: `me/…` у SoundCloud отвалился (404).
    uid: RwLock<Option<String>>,
}

impl Api {
    pub fn new(auth: std::sync::Arc<ScAuth>, config: &Config) -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ProviderError::Network(error.to_string()))?;
        // Оверрайд из конфига сильнее фолбэка: секция
        // `[providers.soundcloud] client_id = "…"` существует ровно на
        // случай, когда фолбэк умер, а ленивое извлечение не сработало.
        let client_id = config
            .provider("soundcloud")
            .client_id
            .clone()
            .unwrap_or_else(|| FALLBACK_CLIENT_ID.to_owned());
        Ok(Self {
            http,
            auth,
            client_id: RwLock::new(client_id),
            uid: RwLock::new(None),
        })
    }

    /// Числовой id аккаунта из `/me`, кэшируется.
    async fn uid(&self) -> Result<String, ProviderError> {
        if let Some(uid) = read(&self.uid) {
            return Ok(uid);
        }
        let me = self.request(reqwest::Method::GET, "me", &[], None, true).await?;
        let uid = me
            .get("id")
            .and_then(Value::as_u64)
            .map(|id| id.to_string())
            .ok_or_else(|| ProviderError::Format {
                provider: ProviderId::SOUNDCLOUD,
                reason: "в ответе /me нет id".into(),
            })?;
        *write(&self.uid) = Some(uid.clone());
        Ok(uid)
    }

    /// oauth-токен из auth — заголовок `Authorization: OAuth …`.
    fn oauth(&self) -> Option<String> {
        self.auth.token().map(|token| format!("OAuth {token}"))
    }

    fn client_id(&self) -> String {
        read(&self.client_id)
    }

    fn poison_client_id(&self) {
        *write(&self.client_id) = String::new();
    }

    /// Обёртка запроса api-v2. `path_or_url` — путь относительно
    /// [`API_BASE`] либо абсолютный URL (продолжения пагинации приходят
    /// готовыми `next_href` с уже вшитым `client_id`, поэтому их
    /// склеивать нельзя).
    ///
    /// `form` — тело как форма (`POST/PUT/DELETE` плейлистов SoundCloud
    /// принимает form-urlencoded, а не JSON). `auth_required` — ход с
    /// `Authorization: OAuth`; его отказ трактуется как сбой сессии, а
    /// не как мёртвый client_id.
    async fn request(
        &self,
        method: reqwest::Method,
        path_or_url: &str,
        query: &[(&str, &str)],
        json: Option<&Value>,
        auth_required: bool,
    ) -> Result<Value, ProviderError> {
        let url = self.build_url(path_or_url);
        // Аутентифицированные эндпоинты без oauth-параметра вообще не
        // маршрутизируются (замерено: 404 вместо 401) — честный Auth до
        // запроса избавляет от ложного «HTTP 404» для человека.
        if auth_required && self.oauth().is_none() {
            return Err(ProviderError::Auth {
                provider: ProviderId::SOUNDCLOUD,
                reason: "нужна авторизация".into(),
            });
        }
        let mut attempt = 0;
        loop {
            let mut req = self.http.request(method.clone(), &url).query(query);
            // Токен цепляется всегда, когда есть, а не только на
            // «аутентифицированных» эндпоинтах: продолжения пагинации
            // (next_href) тоже требуют сессию, а их ходят через get_url
            // с auth_required=false (замерено 21.09.2026: 401 на хвосте
            // лайков без заголовка).
            if let Some(oauth) = self.oauth() {
                req = req.header(reqwest::header::AUTHORIZATION, oauth);
            }
            // Браузерные cookies обязательны: бот-защита (datadome)
            // сверяет их и на пишущих запросах отвечает 403-капчей без
            // них (замерено 21.09.2026 на PUT-лайке).
            if let Some(cookie) = self.auth.cookie_header() {
                req = req.header(reqwest::header::COOKIE, cookie);
            }
            if let Some(json) = json {
                req = req.json(json);
            }
            let response = req
                .send()
                .await
                .map_err(|error| ProviderError::Network(error.to_string()))?;
            let status = response.status();
            // 200/201 — с телом; 204 (DELETE плейлистов) — без тела.
            match status.as_u16() {
                200 | 201 => {
                    let value: Value = response
                        .json()
                        .await
                        .map_err(|error| ProviderError::Format {
                            provider: ProviderId::SOUNDCLOUD,
                            reason: format!("тело {status} не разбирается как JSON: {error}"),
                        })?;
                    return Ok(value);
                }
                204 => return Ok(Value::Null),
                _ => {}
            }

            let body = response.text().await.unwrap_or_default();
            if matches!(status.as_u16(), 401 | 403) && auth_required {
                // Сессия отвергнута — client_id тут ни при чём, и
                // повторять бессмысленно: чинится перелогином.
                self.auth.mark_expired();
                return Err(ProviderError::Auth {
                    provider: ProviderId::SOUNDCLOUD,
                    reason: "сессия отвергнута".into(),
                });
            }
            if matches!(status.as_u16(), 401 | 403) && attempt == 0 {
                // Скорее всего умер client_id (или запрос анонимный).
                // Отравляем кэш, переизвлекаем и повторяем ОДИН раз:
                // если снова отказ — наружу уходит Auth, потому что у
                // SoundCloud нет различимого ответа «мёртвый client_id»
                // против «мёртвая сессия» на этих кодах.
                attempt += 1;
                self.poison_client_id();
                let fresh = self.extract_client_id().await?;
                *write(&self.client_id) = fresh;
                tracing::debug!(
                    provider = "soundcloud",
                    status = status.as_u16(),
                    "отказ с client_id, переизвлекли и повторяем"
                );
                continue;
            }
            return Err(ProviderError::Format {
                provider: ProviderId::SOUNDCLOUD,
                reason: format!("HTTP {status}; тело: {}", snippet(&body)),
            });
        }
    }

    /// URL запроса: абсолютный — как есть, путь — приклеенный к базе с
    /// `client_id` в query. У `next_href` client_id уже вшит сервисом.
    fn build_url(&self, path_or_url: &str) -> String {
        if path_or_url.starts_with("https://") {
            return path_or_url.to_owned();
        }
        let mut url = format!("{API_BASE}{path_or_url}");
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str("client_id=");
        url.push_str(&self.client_id());
        url
    }

    /// Извлечь живой client_id со страницы soundcloud.com: он зашит в
    /// JS-ассетах. По требованию — только после 401/403, не на старте.
    async fn extract_client_id(&self) -> Result<String, ProviderError> {
        let page = self
            .http
            .get("https://soundcloud.com/")
            .send()
            .await
            .map_err(|error| ProviderError::Network(error.to_string()))?
            .text()
            .await
            .map_err(|error| ProviderError::Network(error.to_string()))?;

        // Порядок ассетов в HTML не гарантирован — берём первый, в чьём
        // коде нашёлся client_id.
        for asset in asset_urls(&page) {
            let Ok(code) = self.http.get(&asset).send().await else {
                continue;
            };
            let Ok(text) = code.text().await else {
                continue;
            };
            if let Some(id) = client_id_in(&text) {
                return Ok(id.to_owned());
            }
        }
        // Ни один ассет не дал id — вернёмся к известному рабочему.
        Ok(FALLBACK_CLIENT_ID.to_owned())
    }

    /// Проверка сессии: сервис сам говорит, жив ли токен.
    pub async fn verify_session(&self) -> Result<tmus_core::model::AuthStatus, ProviderError> {
        use tmus_core::model::AuthStatus;

        let Some(token) = self.auth.token() else {
            return Ok(AuthStatus::Missing {
                hint: MISSING_HINT.to_owned(),
            });
        };
        let url = format!("{SESSION_URL}?client_id={}", self.client_id());
        let response = self
            .http
            .post(&url)
            .json(&json!({ "session": { "access_token": token } }))
            .send()
            .await
            .map_err(|error| ProviderError::Network(error.to_string()))?;
        match response.status().as_u16() {
            200 => {
                self.auth.mark_ready();
                Ok(AuthStatus::Ready)
            }
            // Отказ уже помечен внутри (mark_expired), наружу —
            // состояние, а не ошибка: для UI это не сбой.
            401 => Ok(self.auth.cached()),
            status => Err(ProviderError::Auth {
                provider: ProviderId::SOUNDCLOUD,
                reason: format!("проверка сессии вернула HTTP {status}"),
            }),
        }
    }

    /// Поиск по виду. Вид уже отфильтрован сервисом эндпоинтом, но
    /// отбор дублируется в [`crate::SoundCloud::search`] — ответ без
    /// учёта вида не должен подсунуть TUI артистов в список треков.
    pub async fn search(&self, query: &str, kind: SearchKind) -> Result<Value, ProviderError> {
        let path = match kind {
            SearchKind::Tracks => "search/tracks",
            SearchKind::Albums => "search/albums",
            SearchKind::Playlists => "search/playlists",
            SearchKind::Artists => "search/users",
        };
        self.request(
            reqwest::Method::GET,
            path,
            &[("q", query), ("limit", "20"), ("linked_partitioning", "1")],
            None,
            false,
        )
        .await
    }

    /// Подсказки поисковой строки.
    pub async fn suggest(&self, query: &str) -> Result<Value, ProviderError> {
        self.request(
            reqwest::Method::GET,
            "search/queries",
            &[("q", query)],
            None,
            false,
        )
        .await
    }

    /// Плейлисты аккаунта. `/me/playlists` умер (404) — жив
    /// `users/:userId/playlists` (замерено 21.09.2026).
    pub async fn me_playlists(&self) -> Result<Value, ProviderError> {
        let uid = self.uid().await?;
        let path = format!("users/{uid}/playlists");
        self.request(
            reqwest::Method::GET,
            &path,
            &[("limit", "50"), ("linked_partitioning", "1")],
            None,
            true,
        )
        .await
    }

    /// Плейлист по id с пагинацией (`linked_partitioning`). Треки
    /// приходят целиком полем `tracks` — массивом, без пагинации.
    pub async fn playlist(&self, id: &str) -> Result<Value, ProviderError> {
        let path = format!("playlists/{id}?linked_partitioning=1");
        self.request(reqwest::Method::GET, &path, &[], None, false)
            .await
    }

    /// GET абсолютного продолжения (`next_href`) как есть.
    pub async fn get_url(&self, url: &str) -> Result<Value, ProviderError> {
        self.request(reqwest::Method::GET, url, &[], None, false)
            .await
    }

    /// Страница лайков. Живой путь — `users/:userId/likes`: элементы
    /// `{kind:"like", track:{…}}`, `next_href`, до 200 на страницу
    /// (проверено live 21.09.2026; `/e1/me/track_likes` — 404).
    pub async fn likes(&self, limit: u32) -> Result<Value, ProviderError> {
        let uid = self.uid().await?;
        let path = format!("users/{uid}/likes");
        self.request(
            reqwest::Method::GET,
            &path,
            &[("limit", &limit.to_string()), ("linked_partitioning", "1")],
            None,
            true,
        )
        .await
    }

    /// Лайк: `PUT users/:userId/track_likes/:id` (шаблон из JS-бандла
    /// веб-клиента; проверен live net-zero 21.09.2026).
    pub async fn like(&self, id: &str) -> Result<(), ProviderError> {
        let uid = self.uid().await?;
        let path = format!("users/{uid}/track_likes/{id}");
        self.request(reqwest::Method::PUT, &path, &[], None, true)
            .await?;
        Ok(())
    }

    /// Снять лайк: `DELETE users/:userId/track_likes/:id`.
    pub async fn unlike(&self, id: &str) -> Result<(), ProviderError> {
        let uid = self.uid().await?;
        let path = format!("users/{uid}/track_likes/{id}");
        self.request(reqwest::Method::DELETE, &path, &[], None, true)
            .await?;
        Ok(())
    }

    /// Лента рекомендаций.
    pub async fn stream(&self) -> Result<Value, ProviderError> {
        self.request(
            reqwest::Method::GET,
            "stream",
            &[("limit", "30"), ("linked_partitioning", "1")],
            None,
            true,
        )
        .await
    }

    /// Создать приватный плейлист. POST принимает JSON
    /// `{"playlist":{"title","sharing","tracks":[]}}` — без `tracks`
    /// сервис отвечает 404 (замерено 21.09.2026).
    pub async fn create_playlist(&self, title: &str) -> Result<Value, ProviderError> {
        let body = json!({
            "playlist": {
                "title": title,
                "sharing": "private",
                "tracks": [],
            }
        });
        self.request(reqwest::Method::POST, "playlists", &[], Some(&body), true)
            .await
    }

    /// Удалить плейлист. Отвечает 204 без тела.
    pub async fn delete_playlist(&self, id: &str) -> Result<(), ProviderError> {
        let path = format!("playlists/{id}");
        self.request(reqwest::Method::DELETE, &path, &[], None, true)
            .await?;
        Ok(())
    }
}

/// URL JS-ассетов страницы. Regex ассетов ограничен доменом
/// `a-v2.sndcdn.com`: чужие скрипты аналитики client_id не содержат.
fn asset_urls(page: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = page;
    // Без crate regex: одна зависимость ради двух поисков по HTML
    // излишня — обе сигнатуры литеральны, и ручной скан дешевле.
    const NEEDLE: &str = "https://a-v2.sndcdn.com/assets/";
    while let Some(start) = rest.find(NEEDLE) {
        let tail = &rest[start..];
        // URL заканчивается кавычкой (src="…") — до неё и режем.
        let url = match tail.find('"') {
            Some(end) if end > NEEDLE.len() => &tail[..end],
            _ => {
                rest = &rest[start + NEEDLE.len()..];
                continue;
            }
        };
        if url.ends_with(".js") {
            found.push(url.to_owned());
        }
        rest = &rest[start + NEEDLE.len()..];
    }
    found
}

/// client_id в коде ассета. Значение — 32 символа `[0-9a-zA-Z]`, ровно
/// как в живых ассетах; короче/длиннее — другое поле, не трогаем.
fn client_id_in(code: &str) -> Option<&str> {
    const NEEDLE: &str = "client_id";
    let mut rest = code;
    while let Some(pos) = rest.find(NEEDLE) {
        let tail = &rest[pos + NEEDLE.len()..];
        // После `client_id` ждём `:` (возможно с пробелами), кавычку и
        // ровно 32 символа алфавита. Несовпадение — повод искать
        // следующее вхождение, а не сдаваться: в ассетах есть и другие
        // `client_id`-строки (обращения к полям), и они не последние.
        let Some(tail) = tail.trim_start().strip_prefix(':') else {
            rest = &rest[pos + NEEDLE.len()..];
            continue;
        };
        let Some(quoted) = tail.trim_start().strip_prefix('"') else {
            rest = &rest[pos + NEEDLE.len()..];
            continue;
        };
        let value: String = quoted.chars().take(32).collect();
        if value.len() == 32
            && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
            // Индекс по байтам: многобайтовый хвост или обрезанная
            // строка не должны паниковать — их просто пропускаем.
            && quoted.get(32..).is_some_and(|s| s.starts_with('"'))
        {
            return Some(&quoted[..32]);
        }
        rest = &rest[pos + NEEDLE.len()..];
    }
    None
}

/// Сниппет тела в ошибку разбора: по символам, не по байтам (в теле
/// бывает UTF-8).
fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= SNIPPET_CHARS {
        return trimmed.to_owned();
    }
    let head: String = trimmed.chars().take(SNIPPET_CHARS).collect();
    format!("{head}…")
}

/// Отравленный лок не повод падать: под ним строка client_id.
fn read<T: Clone>(lock: &RwLock<T>) -> T {
    lock.read().unwrap_or_else(PoisonError::into_inner).clone()
}

fn write<'a, T>(lock: &'a RwLock<T>) -> std::sync::RwLockWriteGuard<'a, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_are_collected_from_html() {
        let page = r#"<html><script src="https://a-v2.sndcdn.com/assets/0-abc123.js">
        <script src="https://a-v2.sndcdn.com/assets/1-def456.js">
        <script src="https://google.com/analytics.js">"#;
        let urls = asset_urls(page);
        assert_eq!(urls.len(), 2);
        assert!(urls[0].starts_with("https://a-v2.sndcdn.com/assets/"));
        assert!(urls[0].ends_with(".js"));
    }

    #[test]
    fn client_id_is_read_from_asset_code() {
        let id = "Pb72ranhoyt6gw7hM7TkzUItXlMWSNSo";
        let code = format!(r#"var t = {{client_id: "{id}", other: 1}}"#);
        assert_eq!(client_id_in(&code), Some(id));
    }

    #[test]
    fn wrong_length_or_not_quoted_is_rejected() {
        assert_eq!(client_id_in("client_id: \"short\""), None);
        assert_eq!(client_id_in("client_id_x: \"Pb72ranhoyt6gw7hM7TkzUItXlMWSNSo\""), None);
    }

    #[test]
    fn snippet_cuts_by_chars_not_bytes() {
        let long = "ё".repeat(400);
        let cut = snippet(&long);
        assert_eq!(cut.chars().count(), SNIPPET_CHARS + 1); // + многоточие
    }
}
