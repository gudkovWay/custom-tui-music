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

/// Origin подписывается вместе с cookies и должен совпадать с HTTP-заголовками.
pub(crate) const ORIGIN: &str = "https://music.youtube.com";

/// Домен отбора cookies: `music.youtube.com` ходит с cookies, выставленными
/// на `.youtube.com`, а не на конкретный поддомен.
const DOMAIN_SUFFIX: &str = "youtube.com";

const SAPISID_COOKIES: [&str; 3] = ["SAPISID", "__Secure-1PAPISID", "__Secure-3PAPISID"];

const MISSING_HINT: &str =
    "сессия YouTube Music не найдена; залогиньтесь в браузере или укажите профиль в config.toml";

const EXPIRED_HINT: &str =
    "сессия YouTube Music отвергнута сервисом; залогиньтесь заново и обновите cookies";

fn append_sid_authorization(
    header: &mut String,
    scheme: &str,
    sid: &str,
    origin: &str,
    now_unix: u64,
) {
    use std::fmt::Write;
    use sha1::{Digest, Sha1};

    if !header.is_empty() {
        header.push(' ');
    }
    write!(header, "{scheme} ").expect("write to String");
    let timestamp_start = header.len();
    write!(header, "{now_unix}").expect("write to String");
    let mut hasher = Sha1::new();
    hasher.update(&header.as_bytes()[timestamp_start..]);
    hasher.update(b" ");
    hasher.update(sid.as_bytes());
    hasher.update(b" ");
    hasher.update(origin.as_bytes());
    let mut digest = [0; 40];
    hex::encode_to_slice(hasher.finalize(), &mut digest).expect("SHA-1 hex buffer");
    header.push('_');
    header.push_str(std::str::from_utf8(&digest).expect("hex is ASCII"));
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

    /// Каждая SID-cookie подписывается своей схемой; SAPISID может отсутствовать.
    #[must_use]
    pub fn authorization(&self, now_unix: u64) -> Option<String> {
        let jar = read(&self.jar);
        let third_party = jar.get("__Secure-3PAPISID");
        let cookies = [
            ("SAPISIDHASH", jar.get("SAPISID").or(third_party)),
            ("SAPISID1PHASH", jar.get("__Secure-1PAPISID")),
            ("SAPISID3PHASH", third_party),
        ];
        let count = cookies.iter().filter(|(_, cookie)| cookie.is_some()).count();
        if count == 0 {
            return None;
        }
        let mut header = String::with_capacity(count * 76);
        for (scheme, cookie) in cookies {
            if let Some(sid) = cookie {
                append_sid_authorization(&mut header, scheme, sid, ORIGIN, now_unix);
            }
        }
        Some(header)
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
        if !has_signing_cookie(&read(&self.jar)) {
            return;
        }
        *write(&self.status) = AuthStatus::Expired {
            hint: EXPIRED_HINT.to_owned(),
        };
    }
}

fn status_of(jar: &CookieJar) -> AuthStatus {
    if has_signing_cookie(jar) {
        AuthStatus::Ready
    } else {
        AuthStatus::Missing {
            hint: MISSING_HINT.to_owned(),
        }
    }
}

fn has_signing_cookie(jar: &CookieJar) -> bool {
    SAPISID_COOKIES.iter().any(|name| jar.get(name).is_some())
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

    fn fixture_auth(cookies: &[(&str, &str)]) -> YtmAuth {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tmus-ytm-auth-{}-{}.txt",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::File::create_new(&path).expect("create cookie fixture");
        for (name, value) in cookies {
            writeln!(file, ".youtube.com\tTRUE\t/\tTRUE\t0\t{name}\t{value}")
                .expect("write cookie fixture");
        }
        drop(file);
        let auth = YtmAuth::load(&CookieSource::File { path: path.clone() });
        std::fs::remove_file(path).expect("remove cookie fixture");
        auth.expect("load cookie fixture")
    }

    #[test]
    fn authorization_signs_each_cookie_with_its_own_scheme() {
        let auth = fixture_auth(&[
            ("SAPISID", "sapisid-value"),
            ("__Secure-1PAPISID", "1p-value"),
            ("__Secure-3PAPISID", "3p-value"),
        ]);
        assert_eq!(
            auth.authorization(1_700_000_000).as_deref(),
            Some(concat!(
                "SAPISIDHASH 1700000000_11055f89efabedf355d8a9cb9845ef0187ec5fdc ",
                "SAPISID1PHASH 1700000000_4ac9f2f5f73f41f060d704c0bb16bf59dd8702c5 ",
                "SAPISID3PHASH 1700000000_ff98d3a745009fd6f85e630de13986eec10156d0"
            ))
        );
    }

    #[test]
    fn authorization_uses_third_party_cookie_when_sapisid_is_missing() {
        let auth = fixture_auth(&[("__Secure-3PAPISID", "3p-value")]);
        assert_eq!(
            auth.authorization(1_700_000_000).as_deref(),
            Some(concat!(
                "SAPISIDHASH 1700000000_ff98d3a745009fd6f85e630de13986eec10156d0 ",
                "SAPISID3PHASH 1700000000_ff98d3a745009fd6f85e630de13986eec10156d0"
            ))
        );
    }

    #[test]
    fn first_party_cookie_can_authorize_and_expire() {
        let auth = fixture_auth(&[("__Secure-1PAPISID", "1p-value")]);
        assert_eq!(
            auth.authorization(1_700_000_000).as_deref(),
            Some("SAPISID1PHASH 1700000000_4ac9f2f5f73f41f060d704c0bb16bf59dd8702c5")
        );
        assert!(matches!(auth.cached(), AuthStatus::Ready));
        auth.mark_expired();
        assert!(matches!(auth.cached(), AuthStatus::Expired { .. }));
    }

    #[test]
    fn unsigned_session_stays_missing_after_rejection() {
        let auth = fixture_auth(&[]);
        assert!(auth.authorization(1_700_000_000).is_none());
        auth.mark_expired();
        assert!(matches!(auth.cached(), AuthStatus::Missing { .. }));
    }

    /// sha1 от `"1700000000 sapisid-value https://music.youtube.com"`,
    /// посчитанный снаружи (sha1sum), а не этим же кодом: тест ловит
    /// перепутанный порядок полей в подписываемой строке.
    const EXPECTED_DIGEST: &str = "11055f89efabedf355d8a9cb9845ef0187ec5fdc";

    #[test]
    fn sapisid_hash_has_the_measured_shape() {
        let auth = fixture_auth(&[("SAPISID", "sapisid-value")]);
        let hash = auth.authorization(1_700_000_000).expect("signed request");

        let (scheme, value) = hash.split_once(' ').expect("схема и значение");
        assert_eq!(scheme, "SAPISIDHASH");

        let (ts, digest) = value.split_once('_').expect("отметка времени и хеш");
        assert_eq!(ts, "1700000000");
        assert_eq!(digest, EXPECTED_DIGEST);
    }

    #[test]
    fn sapisid_hash_changes_with_the_timestamp() {
        let auth = fixture_auth(&[("SAPISID", "sapisid-value")]);
        let first = auth.authorization(1_700_000_000);
        let second = auth.authorization(1_700_000_001);
        assert_ne!(first, second);
    }
}
