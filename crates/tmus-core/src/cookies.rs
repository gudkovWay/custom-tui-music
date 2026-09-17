//! Cookies браузера: чем авторизоваться у провайдера.
//!
//! Замерено, зачем это отдельным модулем: без cookie-сессии официальные
//! треки отбиваются бот-гейтом («Sign in to confirm you're not a bot») на
//! всех клиентах, а само сообщение не различает «cookies нет» и «cookies
//! истекли». Поэтому здесь две разные точки отказа: [`source_from_config`]
//! ищет профиль и говорит, что профиля нет, а [`jar_for_domain`] читает
//! значения и говорит, что именно в профиле сломалось.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::aes::Aes128;
use aes_gcm::aes::cipher::{Block, BlockCipherDecrypt, BlockSizeUser};
use aes_gcm::{Aes128Gcm, Nonce};
use pbkdf2::pbkdf2_hmac;
use sha1::Sha1;

use crate::config::Config;
use crate::error::{CoreError, Result};

/// Пароль, которым Chromium шифрует cookies. Замерено на живом профиле
/// YTM Desktop: им расшифровываются 62 cookies из 71.
const PASSWORD: &[u8] = b"peanuts";

/// Запасной пароль: в Chromium был баг, из-за которого хранилище
/// паролей оказывалось пустым, и cookies шифровались пустым паролем.
/// Обе попытки делает и yt-dlp.
const PASSWORD_EMPTY: &[u8] = b"";

/// Соль и число итераций — из `os_crypt_linux.cc` Chromium.
const SALT: &[u8] = b"saltysalt";
const ITERATIONS: u32 = 1;
const KEY_LEN: usize = 16;

/// IV у CBC-схемы Chromium — 16 пробелов, он не хранится рядом с
/// значением.
const IV: [u8; 16] = [b' '; 16];

/// Замерено: в базе `meta.version = 24`, и с этой версии Chromium
/// приписывает к открытому значению 32 байта хеша домена. Их надо
/// отбросить, иначе получится мусор в начале значения.
const HASH_PREFIX_LEN: usize = 32;
const HASH_PREFIX_SINCE_META_VERSION: i64 = 24;

/// Схема на Linux — AES-128-CBC, а не GCM: замерено на живых cookies
/// (совпадает с yt-dlp 2026.08.19, `LinuxChromeCookieDecryptor`). GCM
/// оставлен запасной попыткой: он даёт ложных срабатываний не больше,
/// чем CBC, потому что проверяет тег аутентификации.
const GCM_NONCE_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;

const KEYRING_REASON: &str = "значение зашифровано ключом keyring (v11); укажите профиль Firefox или экспортируйте cookies в файл";

/// Куда передать cookies: профиль браузера или готовый файл.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookieSource {
    /// Значение для `--cookies-from-browser`, например `chromium:/путь`.
    Browser { spec: String },

    /// Значение для `--cookies`: файл в формате Netscape.
    File { path: PathBuf },
}

impl CookieSource {
    /// Готовые аргументы для yt-dlp. Отдельным методом, а не
    /// форматированием строки на месте вызова: формат спецификации
    /// профиля — знание этого модуля, и оно не должно расползаться по
    /// провайдерам.
    #[must_use]
    pub fn as_ytdlp_args(&self) -> Vec<String> {
        match self {
            Self::Browser { spec } => vec!["--cookies-from-browser".to_owned(), spec.clone()],
            Self::File { path } => vec![
                "--cookies".to_owned(),
                path.to_string_lossy().into_owned(),
            ],
        }
    }
}

/// Значения cookies: имя → значение.
///
/// `BTreeMap`, а не `HashMap`: порядок обхода задан, а значит
/// [`CookieJar::header`] не меняется между запусками. Иначе один и тот
/// же запрос уходил бы с разным порядком заголовка без причины — и
/// отлаживать расхождения было бы не с чем сравнить.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CookieJar {
    values: BTreeMap<String, String>,
}

impl CookieJar {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Значение для заголовка `Cookie`: `"k=v; k2=v2"`.
    #[must_use]
    pub fn header(&self) -> String {
        let mut header = String::new();
        for (name, value) in &self.values {
            if !header.is_empty() {
                header.push_str("; ");
            }
            header.push_str(name);
            header.push('=');
            header.push_str(value);
        }
        header
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Пустые имена и значения не кладём: в заголовке они дают
    /// бессмысленный `k=`, а сервер на такое отвечает отказом.
    fn push(&mut self, name: String, value: String) {
        if name.is_empty() || value.is_empty() {
            return;
        }
        self.values.insert(name, value);
    }
}

/// Откуда брать cookies: явный профиль из конфига, иначе — найденный
/// на диске.
pub fn source_from_config(cfg: &Config) -> Result<CookieSource> {
    // Профиль из конфига отдаём как есть: человек мог написать и
    // `chromium:/путь`, и `firefox:/путь`, и разбирать это здесь значило
    // бы угадывать за него.
    if let Some(spec) = cfg.browser.profile.as_deref() {
        return Ok(CookieSource::Browser {
            spec: spec.to_owned(),
        });
    }

    autodetect().ok_or(CoreError::NoBrowserProfile)
}

/// Порядок поиска профиля — по убыванию вероятности живой сессии.
fn autodetect() -> Option<CookieSource> {
    let config_dir = directories::BaseDirs::new()?.config_dir().to_path_buf();

    // 1. Профиль приложения YTM Desktop. Замерено: здесь лежит живая
    //    сессия YouTube Music, файл `Cookies` — прямо в каталоге.
    let ytm = config_dir
        .join("YouTube Music Desktop App")
        .join("Partitions")
        .join("ytmview");
    if let Some(db) = chromium_db(&ytm) {
        return Some(chromium_source(&db));
    }

    // 2. Zen: у него свой корень профилей, но база та же, что у Firefox.
    if let Some(db) = profile_db(&config_dir.join("zen"), |_| true) {
        return Some(firefox_source(&db));
    }

    // 3. Firefox: берём только релизный профиль, он у обычного человека
    //    один.
    if let Some(db) = profile_db(&config_dir.join("mozilla").join("firefox"), |name| {
        name.ends_with(".default-release")
    }) {
        return Some(firefox_source(&db));
    }

    // 4. Chromium.
    if let Some(db) = chromium_db(&config_dir.join("chromium")) {
        return Some(chromium_source(&db));
    }

    None
}

/// База cookies Chromium: и в корне профиля, и в `Network/`, и в
/// `Default/` — раскладка зависит от версии и от того, что это за
/// каталог (профиль или корень браузера).
fn chromium_db(profile: &Path) -> Option<PathBuf> {
    const LAYOUTS: [&str; 4] = [
        "Cookies",
        "Network/Cookies",
        "Default/Cookies",
        "Default/Network/Cookies",
    ];
    LAYOUTS
        .iter()
        .map(|rel| profile.join(rel))
        .find(|path| path.is_file())
}

/// Спецификация профиля Chromium для yt-dlp: он ждёт каталог профиля, а
/// не файл базы, поэтому из пути базы вычисляем каталог.
fn chromium_source(db: &Path) -> CookieSource {
    let profile = match db.parent().and_then(Path::file_name).and_then(|n| n.to_str()) {
        Some("Network") => db.parent().and_then(Path::parent).unwrap_or(db),
        _ => db.parent().unwrap_or(db),
    };
    CookieSource::Browser {
        spec: format!("chromium:{}", profile.display()),
    }
}

fn firefox_source(db: &Path) -> CookieSource {
    let profile = db.parent().unwrap_or(db);
    CookieSource::Browser {
        spec: format!("firefox:{}", profile.display()),
    }
}

/// Первый (по имени, чтобы выбор не плавал между запусками) каталог под
/// `root`, в котором лежит `cookies.sqlite`.
fn profile_db(root: &Path, matches: impl Fn(&str) -> bool) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(&matches)
        })
        .map(|dir| dir.join("cookies.sqlite"))
        .filter(|db| db.is_file())
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// Прочитать cookies для домена.
///
/// Домен — суффикс: замерено, что cookies YouTube Music лежат на
/// `.youtube.com`, а не на `music.youtube.com`, поэтому провайдер должен
/// спрашивать `"youtube.com"`.
///
/// Неудача — это не «нет cookies для домена» (их может не быть, и это
/// штатно: провайдер сам решит, что авторизации нет), а «cookies есть, но
/// не читаются». Во втором случае возвращается [`CoreError::Cookies`] с
/// причиной, пригодной для показа человеку.
pub fn jar_for_domain(src: &CookieSource, domain_suffix: &str) -> Result<CookieJar> {
    match src {
        CookieSource::Browser { spec } => {
            let (kind, path) = spec.split_once(':').ok_or_else(|| {
                cookies_err(
                    PathBuf::from(spec),
                    "у профиля нет пути; ожидается «chromium:/путь» или «firefox:/путь»",
                )
            })?;
            let path = Path::new(path);

            // Семейство Chromium разделяет формат cookies, поэтому
            // «chrome» и «brave» разбираются тем же кодом.
            match kind {
                "chromium" | "chrome" | "brave" | "edge" | "vivaldi" => {
                    let db = if path.is_file() {
                        path.to_path_buf()
                    } else {
                        chromium_db(path).ok_or_else(|| {
                            cookies_err(
                                path,
                                "в профиле нет базы cookies (Cookies или Network/Cookies)",
                            )
                        })?
                    };
                    read_chromium(&db, domain_suffix)
                }
                "firefox" | "zen" => {
                    let db = if path.is_file() {
                        path.to_path_buf()
                    } else {
                        path.join("cookies.sqlite")
                    };
                    read_firefox(&db, domain_suffix)
                }
                other => Err(cookies_err(
                    path,
                    format!("неизвестный браузер «{other}»; поддерживаются chromium и firefox"),
                )),
            }
        }
        CookieSource::File { path } => {
            let text = std::fs::read_to_string(path).map_err(|err| {
                cookies_err(path, format!("файл не читается: {err}"))
            })?;
            Ok(parse_netscape(&text, domain_suffix))
        }
    }
}

fn read_chromium(db: &Path, domain_suffix: &str) -> Result<CookieJar> {
    let copy = TempCopy::of(db).map_err(|reason| cookies_err(db, reason))?;
    let conn = rusqlite::Connection::open_with_flags(
        copy.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .map_err(|err| cookies_err(db, format!("база не открылась: {err}")))?;

    // Версия схемы говорит, приписан ли к значению хеш домена.
    let meta_version = conn
        .query_row("SELECT value FROM meta WHERE key = 'version'", [], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);

    let pattern = format!("%{domain_suffix}");
    let mut stmt = conn
        .prepare("SELECT name, value, encrypted_value FROM cookies WHERE host_key LIKE ?1")
        .map_err(|err| cookies_err(db, format!("таблица cookies читается не так: {err}")))?;
    let rows = stmt
        .query_map([pattern], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(|err| cookies_err(db, format!("запрос к cookies не выполнился: {err}")))?;

    let mut jar = CookieJar::default();
    let mut failed = 0usize;
    let mut first_reason: Option<String> = None;

    for row in rows {
        let (name, plain, blob) =
            row.map_err(|err| cookies_err(db, format!("строка cookies не прочиталась: {err}")))?;

        // Незашифрованное значение Chromium пишет в `value`, шифрованное
        // — только в `encrypted_value`.
        let value = match plain.filter(|value| !value.is_empty()) {
            Some(plain) => Some(plain),
            None if blob.is_empty() => None,
            None => match decrypt_chromium(&blob, meta_version) {
                Ok(value) => Some(value),
                Err(reason) => {
                    failed += 1;
                    first_reason.get_or_insert(reason);
                    None
                }
            },
        };

        if let Some(value) = value {
            jar.push(name, value);
        }
    }

    // Замерено, почему пропуск, а не ошибка на каждое значение: в живом
    // профиле 9 cookies из 71 зашифрованы ключом keyring (v11) и
    // прочитать их нельзя, но остальные 62 рабочие. Ошибка на первой
    // такой cookies убила бы всю авторизацию.
    if failed > 0 {
        tracing::warn!(
            db = %db.display(),
            failed,
            "часть cookies не расшифровалась и пропущена"
        );
    }

    if jar.is_empty() {
        if let Some(reason) = first_reason {
            return Err(cookies_err(db, reason));
        }
    }

    Ok(jar)
}

fn read_firefox(db: &Path, domain_suffix: &str) -> Result<CookieJar> {
    let copy = TempCopy::of(db).map_err(|reason| cookies_err(db, reason))?;
    let conn = rusqlite::Connection::open_with_flags(
        copy.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .map_err(|err| cookies_err(db, format!("база не открылась: {err}")))?;

    let pattern = format!("%{domain_suffix}");
    let mut stmt = conn
        .prepare("SELECT name, value FROM moz_cookies WHERE host LIKE ?1")
        .map_err(|err| cookies_err(db, format!("таблица moz_cookies читается не так: {err}")))?;
    let rows = stmt
        .query_map([pattern], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| cookies_err(db, format!("запрос к moz_cookies не выполнился: {err}")))?;

    let mut jar = CookieJar::default();
    for row in rows {
        let (name, value) =
            row.map_err(|err| cookies_err(db, format!("строка cookies не прочиталась: {err}")))?;
        jar.push(name, value);
    }

    Ok(jar)
}

/// Расшифровать значение cookie Chromium.
///
/// Ошибка — строка с причиной, потому что решение «пропустить значение»
/// или «упасть» принимает вызывающий.
fn decrypt_chromium(blob: &[u8], meta_version: i64) -> std::result::Result<String, String> {
    let Some((version, body)) = blob.split_at_checked(3) else {
        return Err("значение короче заголовка версии".to_owned());
    };

    match version {
        b"v10" => cbc_try(PASSWORD, body, meta_version)
            .or_else(|| cbc_try(PASSWORD_EMPTY, body, meta_version))
            .or_else(|| gcm_try(PASSWORD, body, meta_version))
            .ok_or_else(|| {
                "значение v10 не расшифровалось: ни один из известных ключей не подошёл"
                    .to_owned()
            }),
        b"v11" => Err(KEYRING_REASON.to_owned()),
        other => Err(format!(
            "неизвестный формат значения (префикс «{}»)",
            String::from_utf8_lossy(other)
        )),
    }
}

fn derive_key(password: &[u8]) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    pbkdf2_hmac::<Sha1>(password, SALT, ITERATIONS, &mut key);
    key
}

fn cbc_try(password: &[u8], body: &[u8], meta_version: i64) -> Option<String> {
    let key = derive_key(password);
    let plain = cbc_decrypt(&key, &IV, body)?;
    finish(unpad_pkcs7(plain)?, meta_version)
}

fn gcm_try(password: &[u8], body: &[u8], meta_version: i64) -> Option<String> {
    if body.len() < GCM_NONCE_LEN + GCM_TAG_LEN {
        return None;
    }
    let key = derive_key(password);
    let cipher = Aes128Gcm::new_from_slice(&key).ok()?;
    let nonce = Nonce::try_from(&body[..GCM_NONCE_LEN]).ok()?;
    let plain = cipher.decrypt(&nonce, &body[GCM_NONCE_LEN..]).ok()?;
    finish(plain, meta_version)
}

/// Отбросить хеш домена и проверить, что осталось текстом.
fn finish(plain: Vec<u8>, meta_version: i64) -> Option<String> {
    let body = if meta_version >= HASH_PREFIX_SINCE_META_VERSION {
        plain.get(HASH_PREFIX_LEN..)?
    } else {
        &plain
    };
    let text = std::str::from_utf8(body).ok()?;
    if text.is_empty() {
        return None;
    }
    Some(text.to_owned())
}

/// AES-128-CBC вручную: крейт `cbc` в манифесте не объявлен, а из
/// блочного шифра цепочка собирается в четыре строки.
fn cbc_decrypt(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    let block_len = Aes128::block_size();
    if data.is_empty() || data.len() % block_len != 0 {
        return None;
    }

    let cipher = Aes128::new_from_slice(key).ok()?;
    let mut out = Vec::with_capacity(data.len());
    let mut prev = *iv;

    for chunk in data.chunks_exact(block_len) {
        let mut block = Block::<Aes128>::default();
        block.copy_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        for (byte, link) in block.iter_mut().zip(prev.iter()) {
            *byte ^= link;
        }
        out.extend_from_slice(&block);
        prev.copy_from_slice(chunk);
    }

    Some(out)
}

fn unpad_pkcs7(mut data: Vec<u8>) -> Option<Vec<u8>> {
    let pad = usize::from(*data.last()?);
    let block_len = Aes128::block_size();
    if pad == 0 || pad > block_len || pad > data.len() {
        return None;
    }
    if !data[data.len() - pad..].iter().all(|byte| usize::from(*byte) == pad) {
        return None;
    }
    data.truncate(data.len() - pad);
    Some(data)
}

/// Разбор файла в формате Netscape: 7 полей через TAB, `#` — комментарий,
/// 0 — домен, 5 — имя, 6 — значение.
fn parse_netscape(text: &str, domain_suffix: &str) -> CookieJar {
    let mut jar = CookieJar::default();

    for line in text.lines() {
        // `#HttpOnly_` — не комментарий, а Cookie с флагом HttpOnly;
        // именно так помечены сессионные cookies, ради которых всё и
        // делается, поэтому строку разбираем, а префикс отбрасываем.
        let line = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => rest,
            None if line.starts_with('#') => continue,
            None => line,
        };
        if line.trim().is_empty() {
            continue;
        }

        let mut fields = line.splitn(7, '\t');
        let (Some(host), Some(_flag), Some(_path), Some(_secure), Some(_expiry), Some(name), Some(value)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };

        if !host_matches(host, domain_suffix) {
            continue;
        }
        jar.push(name.to_owned(), value.to_owned());
    }

    jar
}

/// Домен из файла — с ведущей точкой (`.youtube.com`), поэтому её
/// отбрасываем; сравнение по границе точки, иначе `evilyoutube.com`
/// сошёл бы за `youtube.com`.
fn host_matches(host: &str, suffix: &str) -> bool {
    let host = host.strip_prefix('.').unwrap_or(host);
    host == suffix
        || (host.len() > suffix.len()
            && host.ends_with(suffix)
            && host.as_bytes()[host.len() - suffix.len() - 1] == b'.')
}

fn cookies_err(path: impl AsRef<Path>, reason: impl Into<String>) -> CoreError {
    CoreError::Cookies {
        path: path.as_ref().to_path_buf(),
        reason: reason.into(),
    }
}

/// Копия чужой базы во временном каталоге.
///
/// Замерено, почему копия, а не чтение на месте: база лежит рядом с
/// `-journal`/`-wal`, её держит живой браузер, и открытие оригинала либо
/// упирается в блокировку, либо теряет свежие записи из журнала. Копия
/// уносит и сам файл, и его журналы, а оригинал остаётся нетронутым.
struct TempCopy {
    path: PathBuf,
    dir: PathBuf,
}

impl TempCopy {
    fn of(db: &Path) -> std::result::Result<Self, String> {
        let name = db
            .file_name()
            .ok_or_else(|| "у пути к базе нет имени файла".to_owned())?
            .to_os_string();

        let dir = std::env::temp_dir().join(format!(
            "tmus-cookies-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("временный каталог {} не создался: {err}", dir.display()))?;

        // Копия создаётся до попытки чтения: при ошибке Drop уберёт
        // каталог.
        let copy = Self {
            path: dir.join(&name),
            dir,
        };
        std::fs::copy(db, &copy.path)
            .map_err(|err| format!("база не скопировалась: {err}"))?;

        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = name.clone();
            sidecar.push(suffix);
            let src = db.with_file_name(&sidecar);
            if src.is_file() {
                let _ = std::fs::copy(&src, copy.dir.join(&sidecar));
            }
        }

        Ok(copy)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    /// Значение, зашифрованное openssl (AES-128-CBC, ключ
    /// PBKDF2-SHA1("peanuts", "saltysalt", 1), IV — 16 пробелов,
    /// PKCS7) поверх открытого текста из 32 байт хеша и `abc123XYZ`.
    /// Отдельная реализация, а не своя же: так проверяются и ключ, и IV,
    /// и цепочка блоков.
    const OPENSSL_CIPHERTEXT: &str =
        "0b9cf0a6eee9cf1aaac601b85ca37cf9c99903ddda1456a74e2f66567254c243b1e0c60428c8c73f0a730785db20db67";

    fn netscape_file(text: &str) -> (tempfile::TempDir, CookieSource) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cookies.txt");
        std::fs::write(&path, text).expect("write");
        (dir, CookieSource::File { path })
    }

    /// База Chromium с одной строкой cookies и версией схемы.
    fn write_chromium_db(dir: &Path, meta_version: i64, encrypted: &[u8]) -> PathBuf {
        let path = dir.join("Cookies");
        let conn = rusqlite::Connection::open(&path).expect("open");
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT NOT NULL, value TEXT NOT NULL);
             CREATE TABLE cookies(
                 host_key TEXT NOT NULL, name TEXT NOT NULL,
                 value TEXT NOT NULL, encrypted_value BLOB NOT NULL);",
        )
        .expect("schema");
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('version', ?1)",
            [meta_version.to_string()],
        )
        .expect("meta");
        conn.execute(
            "INSERT INTO cookies(host_key, name, value, encrypted_value) VALUES(?1, ?2, '', ?3)",
            rusqlite::params![".youtube.com", "SAPISID", encrypted],
        )
        .expect("row");
        path
    }

    #[test]
    fn ytdlp_args_name_the_source() {
        let browser = CookieSource::Browser {
            spec: "chromium:/профиль с пробелами".to_owned(),
        };
        assert_eq!(
            browser.as_ytdlp_args(),
            vec![
                "--cookies-from-browser".to_owned(),
                "chromium:/профиль с пробелами".to_owned()
            ]
        );

        let file = CookieSource::File {
            path: PathBuf::from("/tmp/cookies.txt"),
        };
        assert_eq!(
            file.as_ytdlp_args(),
            vec!["--cookies".to_owned(), "/tmp/cookies.txt".to_owned()]
        );
    }

    #[test]
    fn configured_profile_is_used_as_is() {
        let cfg = Config {
            browser: crate::config::BrowserConfig {
                profile: Some("chromium:/home/q/.config/YouTube Music Desktop App/Partitions/ytmview".to_owned()),
            },
            ..Config::default()
        };

        let source = source_from_config(&cfg).expect("профиль задан явно");

        assert_eq!(
            source,
            CookieSource::Browser {
                spec: "chromium:/home/q/.config/YouTube Music Desktop App/Partitions/ytmview"
                    .to_owned()
            }
        );
    }

    #[test]
    fn header_is_sorted_regardless_of_insertion_order() {
        let mut first = CookieJar::default();
        first.push("SID".to_owned(), "b".to_owned());
        first.push("APISID".to_owned(), "a".to_owned());
        first.push("HSID".to_owned(), "c".to_owned());

        let mut second = CookieJar::default();
        second.push("HSID".to_owned(), "c".to_owned());
        second.push("SID".to_owned(), "b".to_owned());
        second.push("APISID".to_owned(), "a".to_owned());

        assert_eq!(first.header(), "APISID=a; HSID=c; SID=b");
        assert_eq!(first.header(), second.header());
    }

    #[test]
    fn netscape_file_skips_noise_and_keeps_full_values() {
        let (_dir, source) = netscape_file(
            "# Netscape HTTP Cookie File\n\
             \n\
             #HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tdb=equal=signs\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tHSID\tкороткое\n\
             evilyoutube.com\tTRUE\t/\tTRUE\t0\tFAKE\tнет\n\
             .example.com\tTRUE\t/\tTRUE\t0\tOTHER\tнет\n\
             .youtube.com\tTRUE\t/\tTRUE\t0\tSSID\n",
        );

        let jar = jar_for_domain(&source, "youtube.com").expect("разбор файла");

        // `=` внутри значения не режет поле, короткая строка пропущена,
        // чужой домен не попал, а `#HttpOnly_` — это cookie, не
        // комментарий.
        assert_eq!(jar.get("SID"), Some("db=equal=signs"));
        assert_eq!(jar.get("HSID"), Some("короткое"));
        assert_eq!(jar.get("FAKE"), None);
        assert_eq!(jar.get("OTHER"), None);
        assert_eq!(jar.get("SSID"), None);
    }

    #[test]
    fn chromium_cookies_are_decrypted_with_the_browser_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ciphertext = hex::decode(OPENSSL_CIPHERTEXT).expect("hex");
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&ciphertext);
        let db = write_chromium_db(dir.path(), 24, &blob);
        let profile = db.parent().expect("каталог профиля");

        let jar = jar_for_domain(
            &CookieSource::Browser {
                spec: format!("chromium:{}", profile.display()),
            },
            "youtube.com",
        )
        .expect("профиль читается");

        assert_eq!(jar.get("SAPISID"), Some("abc123XYZ"));
    }

    #[test]
    fn v11_without_keyring_is_reported_not_silently_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = write_chromium_db(dir.path(), 24, b"v11\x00\x01\x02binary");
        let profile = db.parent().expect("каталог профиля");

        let err = jar_for_domain(
            &CookieSource::Browser {
                spec: format!("chromium:{}", profile.display()),
            },
            "youtube.com",
        )
        .expect_err("расшифровать нечем");

        assert!(
            matches!(&err, CoreError::Cookies { reason, .. } if reason.contains("keyring")),
            "ожидалась причина про keyring, получено {err:?}"
        );
    }

    #[test]
    fn domain_without_cookies_is_empty_jar_not_an_error() {
        let (_dir, source) = netscape_file(".youtube.com\tTRUE\t/\tTRUE\t0\tSID\tзначение\n");

        let jar = jar_for_domain(&source, "soundcloud.com").expect("разбор файла");

        assert!(jar.is_empty());
    }
}
