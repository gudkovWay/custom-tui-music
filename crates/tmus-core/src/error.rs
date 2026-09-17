//! Ошибки ядра.
//!
//! Провайдер-специфичные ошибки сюда не попадают: у провайдеров свой
//! тип (`tmus_provider::ProviderError`). Здесь только то, что может
//! сломаться в конфиге, учётных данных, базе и кэше.

use std::io;
use std::path::PathBuf;

/// Ошибка ядра.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("не нашлись каталоги XDG (HOME не задан?)")]
    NoHome,

    #[error("конфиг {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("конфиг {path} не разбирается: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// Профиль браузера найден, но cookies из него не читаются.
    ///
    /// Отдельный вариант, а не общий io: у Chromium значения шифруются
    /// ключом из keyring, и «нет доступа к keyring» чинится не так, как
    /// «файла нет».
    #[error("cookies из {path} не читаются: {reason}")]
    Cookies { path: PathBuf, reason: String },

    /// Ни один профиль браузера не подошёл.
    #[error("профиль браузера не найден; укажите его в config.toml")]
    NoBrowserProfile,

    #[error("база {path}: {source}")]
    Database {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("кэш {path}: {source}")]
    Cache {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;
