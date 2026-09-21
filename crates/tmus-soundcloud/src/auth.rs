//! Учётные данные SoundCloud: cookies браузера и oauth-токен.
//!
//! SoundCloud авторизуется одной кукой `oauth_token` с домена
//! `soundcloud.com` — остальная сессия сервису не нужна. Разделение
//! `Missing` и `Expired` здесь по той же причине, что у YouTube Music:
//! чинятся они по-разному, а без кэшированного статуса UI не отличит
//! «нечем логиниться» от «сессию отвергли».

use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use tmus_core::cookies::{CookieJar, CookieSource, jar_for_domain};
use tmus_core::model::AuthStatus;

/// Домен отбора cookies: oauth_token выставлен на `.soundcloud.com`.
pub(crate) const DOMAIN_SUFFIX: &str = "soundcloud.com";

/// Кука с токеном api-v2.
pub(crate) const TOKEN_COOKIE: &str = "oauth_token";

pub(crate) const MISSING_HINT: &str =
    "сессия SoundCloud не найдена; залогиньтесь на soundcloud.com в браузере или укажите профиль в config.toml";

pub(crate) const EXPIRED_HINT: &str =
    "сессия SoundCloud отвергнута сервисом; залогиньтесь заново на soundcloud.com";

/// Cookies и состояние сессии SoundCloud.
///
/// `CookieSource` здесь не хранится: им владеет [`crate::SoundCloud`],
/// потому что те же cookies уходят ещё и в yt-dlp, а держать вторую
/// копию — значит рассинхронизировать их при обновлении.
pub struct ScAuth {
    jar: RwLock<CookieJar>,
    token: RwLock<Option<String>>,
    status: RwLock<AuthStatus>,
}

impl ScAuth {
    /// Прочитать cookies и выставить стартовый статус.
    ///
    /// `Ready` тут — утверждение «токен на месте», а не «сервис
    /// подтвердил»: подтверждение стоит сетевого запроса, и его делает
    /// [`crate::SoundCloud`] при `refresh`.
    pub fn load(source: &CookieSource) -> tmus_core::Result<Self> {
        let jar = jar_for_domain(source, DOMAIN_SUFFIX)?;
        let token = token_of(&jar);
        let status = status_of(&token);
        Ok(Self {
            jar: RwLock::new(jar),
            token: RwLock::new(token),
            status: RwLock::new(status),
        })
    }

    /// Перечитать cookies из источника: пользователь мог залогиниться
    /// после старта демона.
    pub fn reload(&self, source: &CookieSource) -> tmus_core::Result<AuthStatus> {
        let jar = jar_for_domain(source, DOMAIN_SUFFIX)?;
        let token = token_of(&jar);
        let status = status_of(&token);
        *write(&self.jar) = jar;
        *write(&self.token) = token;
        *write(&self.status) = status.clone();
        Ok(status)
    }

    /// Кэшированное состояние. Сети не трогает — вызывается из UI.
    #[must_use]
    pub fn cached(&self) -> AuthStatus {
        read(&self.status).clone()
    }

    /// oauth-токен из cookies, если он есть.
    #[must_use]
    pub fn token(&self) -> Option<String> {
        read(&self.token).clone()
    }

    /// Заголовок `Cookie` целиком. Бот-защита SoundCloud (datadome)
    /// сверяет браузерные cookies и без них пишущие запросы ловят
    /// 403-капчу (замерено 21.09.2026 на PUT-лайке).
    #[must_use]
    pub fn cookie_header(&self) -> Option<String> {
        let header = read(&self.jar).header();
        (!header.is_empty()).then_some(header)
    }

    /// Сессия подтверждена сервисом запросом.
    pub fn mark_ready(&self) {
        *write(&self.status) = AuthStatus::Ready;
    }

    /// Сервис отказал в авторизации (401/403).
    ///
    /// Если токена не было вовсе, статус не меняется: нечему истекать,
    /// а подсказки у `Missing` и `Expired` разные.
    pub fn mark_expired(&self) {
        if self.token().is_none() {
            return;
        }
        *write(&self.status) = AuthStatus::Expired {
            hint: EXPIRED_HINT.to_owned(),
        };
    }
}

/// Логика статуса из наличия токена. Отдельной функцией, чтобы её
/// можно было покрыть тестами без сети и без чтения cookies.
#[must_use]
pub(crate) fn status_of(token: &Option<String>) -> AuthStatus {
    if token.is_some() {
        AuthStatus::Ready
    } else {
        AuthStatus::Missing {
            hint: MISSING_HINT.to_owned(),
        }
    }
}

fn token_of(jar: &CookieJar) -> Option<String> {
    jar.get(TOKEN_COOKIE)
        .map(str::to_owned)
        .filter(|token| !token.is_empty())
}

/// Отравленный лок не повод падать: под ним лежат cookies и статус, а
/// не инвариант, который можно нарушить наполовину.
fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_on_place_is_ready() {
        let token = Some("abc".to_owned());
        assert_eq!(status_of(&token), AuthStatus::Ready);
    }

    #[test]
    fn no_token_is_missing() {
        assert_eq!(
            status_of(&None),
            AuthStatus::Missing {
                hint: MISSING_HINT.to_owned()
            }
        );
    }

    #[test]
    fn empty_token_is_not_a_session() {
        // Кука `oauth_token=` с пустым значением — не сессия: status_of
        // получает уже отфильтрованный token_of, который пустоту
        // отбрасывает. Проверяем фильтр через CookieSource::File:
        // публичного сеттера у CookieJar нет, а Netscape-файл — его
        // штатный тестовый способ наполнения (см. tmus-core::cookies).
        let dir = std::env::temp_dir().join(format!("sc-auth-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("временный каталог создаётся");
        let path = dir.join("cookies.txt");
        std::fs::write(
            &path,
            ".soundcloud.com\tTRUE\t/\tFALSE\t0\toauth_token\t",
        )
        .expect("файл cookies пишется");
        let source = CookieSource::File { path };
        let auth = ScAuth::load(&source).expect("пустые cookies читаются без ошибки");
        assert!(matches!(auth.cached(), AuthStatus::Missing { .. }));
        assert!(auth.token().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_from_netscape_file_is_ready() {
        let dir = std::env::temp_dir().join(format!("sc-auth-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("временный каталог создаётся");
        let path = dir.join("cookies.txt");
        std::fs::write(
            &path,
            ".soundcloud.com\tTRUE\t/\tFALSE\t0\toauth_token\t3-123456-abcdef",
        )
        .expect("файл cookies пишется");
        let source = CookieSource::File { path };
        let auth = ScAuth::load(&source).expect("cookies читаются без ошибки");
        assert_eq!(auth.cached(), AuthStatus::Ready);
        assert_eq!(auth.token().as_deref(), Some("3-123456-abcdef"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_expired_without_token_keeps_status() {
        let auth = ScAuth {
            jar: RwLock::new(CookieJar::default()),
            token: RwLock::new(None),
            status: RwLock::new(AuthStatus::Missing {
                hint: MISSING_HINT.to_owned(),
            }),
        };
        auth.mark_expired();
        assert!(matches!(auth.cached(), AuthStatus::Missing { .. }));
    }
}
