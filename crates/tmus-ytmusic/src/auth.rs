//! Учётные данные YouTube Music: cookies, SAPISID и подпись запросов.
//!
//! Ключ API InnerTube не нужен вовсе. Замерено 17.09.2026: `browse` с
//! cookie-сессией браузера и заголовком `Authorization: SAPISIDHASH …`
//! отвечает HTTP 200 и отдаёт библиотеку, а тот же запрос без подписи
//! отбивается бот-гейтом.
//!
//! Здесь же производится разделение `Missing` и `Expired`, которого не
//! умеет yt-dlp: он отвечает одинаковым «Sign in to confirm you're not a
//! bot» и когда cookies нет, и когда сессия протухла, — а чинят эти
//! случаи по-разному.

use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use tmus_core::cookies::{CookieJar, CookieSource, jar_for_domain};
use tmus_core::model::AuthStatus;

/// Origin, который подписывается в `sapisid_hash` и уходит заголовками
/// `Origin`/`X-Origin`. Живёт в одном месте с подписью: разъехавшись с
/// заголовками, подпись перестанет проверяться сервисом.
pub(crate) const ORIGIN: &str = "https://music.youtube.com";

/// Домен отбора cookies: `music.youtube.com` ходит с cookies, выставленными
/// на `.youtube.com`, а не на конкретный поддомен.
const DOMAIN_SUFFIX: &str = "youtube.com";

/// Порядок важен: подписывается `__Secure-3PAPISID`, `SAPISID` — запасной
/// вариант для сессий, где secure-кука не выставилась.
const SAPISID_COOKIES: [&str; 2] = ["__Secure-3PAPISID", "SAPISID"];

const MISSING_HINT: &str =
    "сессия YouTube Music не найдена; залогиньтесь в браузере или укажите профиль в config.toml";

const EXPIRED_HINT: &str =
    "сессия YouTube Music отвергнута сервисом; залогиньтесь заново и обновите cookies";

/// Подпись запроса: `SAPISIDHASH <ts>_<sha1_hex>`, где sha1 берётся от
/// строки `"<ts> <sapisid> <origin>"`.
///
/// Замерено рабочим: с этой подписью авторизованный `browse` отдаёт
/// HTTP 200.
#[must_use]
pub fn sapisid_hash(sapisid: &str, origin: &str, now_unix: u64) -> String {
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(format!("{now_unix} {sapisid} {origin}").as_bytes());
    format!("SAPISIDHASH {now_unix}_{}", hex::encode(hasher.finalize()))
}

/// Cookies и состояние сессии YouTube Music.
///
/// `CookieSource` здесь не хранится: им владеет [`crate::YtMusic`], потому
/// что те же cookies уходят ещё и в yt-dlp, а держать вторую копию —
/// значит рассинхронизировать их при обновлении.
pub struct YtmAuth {
    jar: RwLock<CookieJar>,
    status: RwLock<AuthStatus>,
}

impl YtmAuth {
    /// Прочитать cookies и выставить стартовый статус.
    ///
    /// `Ready` тут — утверждение «SAPISID на месте», а не «сервис
    /// подтвердил»: подтверждение стоит сетевого запроса, и его делает
    /// [`crate::YtMusic`] при `refresh`. До тех пор оптимизм безопасен —
    /// отвергнутая сессия пометится `Expired` на первом же отказе.
    pub fn load(source: &CookieSource) -> tmus_core::Result<Self> {
        let jar = jar_for_domain(source, DOMAIN_SUFFIX)?;
        let status = status_of(&jar);
        Ok(Self {
            jar: RwLock::new(jar),
            status: RwLock::new(status),
        })
    }

    /// Перечитать cookies из источника: пользователь мог залогиниться
    /// после старта демона.
    pub fn reload(&self, source: &CookieSource) -> tmus_core::Result<AuthStatus> {
        let jar = jar_for_domain(source, DOMAIN_SUFFIX)?;
        let status = status_of(&jar);
        *write(&self.jar) = jar;
        *write(&self.status) = status.clone();
        Ok(status)
    }

    /// Кэшированное состояние. Сети не трогает — вызывается из UI.
    #[must_use]
    pub fn cached(&self) -> AuthStatus {
        read(&self.status).clone()
    }

    /// Заголовок `Cookie` целиком. Сервису нужна вся сессия, а не пара
    /// значений: по одним cookies он узнаёт аккаунт.
    #[must_use]
    pub fn cookie_header(&self) -> Option<String> {
        let header = read(&self.jar).header();
        (!header.is_empty()).then_some(header)
    }

    /// Заголовок `Authorization` — если есть чем подписывать.
    #[must_use]
    pub fn authorization(&self, now_unix: u64) -> Option<String> {
        self.sapisid()
            .map(|sapisid| sapisid_hash(&sapisid, ORIGIN, now_unix))
    }

    /// Сессия подтверждена сервисом запросом.
    pub fn mark_ready(&self) {
        *write(&self.status) = AuthStatus::Ready;
    }

    /// Сервис отказал в авторизации (401/403 или тело с кодом 401).
    ///
    /// Если SAPISID не было вовсе, статус остаётся `Missing`: истёкшим
    /// нечего быть, а подсказки у этих состояний разные.
    pub fn mark_expired(&self) {
        if self.sapisid().is_none() {
            return;
        }
        *write(&self.status) = AuthStatus::Expired {
            hint: EXPIRED_HINT.to_owned(),
        };
    }

    #[must_use]
    pub fn sapisid(&self) -> Option<String> {
        let jar = read(&self.jar);
        sapisid_of(&jar)
    }
}

fn status_of(jar: &CookieJar) -> AuthStatus {
    if sapisid_of(jar).is_some() {
        AuthStatus::Ready
    } else {
        AuthStatus::Missing {
            hint: MISSING_HINT.to_owned(),
        }
    }
}

fn sapisid_of(jar: &CookieJar) -> Option<String> {
    SAPISID_COOKIES
        .iter()
        .find_map(|name| jar.get(name))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Отравленный лок не повод падать: под ним лежат cookies и статус, а не
/// инвариант, который можно нарушить наполовину.
fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sha1 от `"1700000000 sapisid-value https://music.youtube.com"`,
    /// посчитанный снаружи (sha1sum), а не этим же кодом: тест ловит
    /// перепутанный порядок полей в подписываемой строке.
    const EXPECTED_DIGEST: &str = "11055f89efabedf355d8a9cb9845ef0187ec5fdc";

    #[test]
    fn sapisid_hash_has_the_measured_shape() {
        let hash = sapisid_hash("sapisid-value", ORIGIN, 1_700_000_000);

        let (scheme, value) = hash.split_once(' ').expect("схема и значение");
        assert_eq!(scheme, "SAPISIDHASH");

        let (ts, digest) = value.split_once('_').expect("отметка времени и хеш");
        assert_eq!(ts, "1700000000");
        assert_eq!(digest, EXPECTED_DIGEST);
    }

    #[test]
    fn sapisid_hash_changes_with_the_timestamp() {
        let first = sapisid_hash("sapisid-value", ORIGIN, 1_700_000_000);
        let second = sapisid_hash("sapisid-value", ORIGIN, 1_700_000_001);
        assert_ne!(first, second);
    }
}
