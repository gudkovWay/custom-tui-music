//! Конфиг приложения: один файл `~/.config/tmus/config.toml`.
//!
//! Отсутствие файла — штатный случай, а не ошибка: после установки
//! приложение обязано запускаться сразу. Поэтому здесь нет ни одного
//! обязательного поля, а разбор идёт с `#[serde(default)]` — частично
//! заполненный конфиг тоже валиден.
//!
//! Провайдеры настраиваются секциями (`[providers.ytmusic]`), а не
//! полями с префиксом вроде `ytm_browser_profile`. Причина замерена
//! количеством, а не вкусом: провайдеров будет несколько (soundcloud,
//! spotify, yandex), и префиксные поля пришлось бы дублировать на
//! каждого, а общий код — ветвить по имени поля.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::paths::Paths;

/// Громкость по умолчанию.
const DEFAULT_VOLUME: f64 = 70.0;

/// Лимит офлайн-кэша по умолчанию: 8 GiB.
const DEFAULT_CACHE_LIMIT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Настройки приложения целиком.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Путь к `mpv`. Именно путь, а не только имя: у части людей mpv
    /// собран локально или лежит в `~/.local/bin`, которого нет в PATH
    /// у systemd-юнита демона.
    pub mpv: PathBuf,

    /// Путь к `yt-dlp`.
    pub yt_dlp: PathBuf,

    /// Громкость в процентах. Значение вне 0..=100 — мусор для mpv,
    /// поэтому наружу отдаётся только через [`Config::volume_clamped`].
    pub volume: f64,

    /// Формат потока для yt-dlp.
    ///
    /// `web_music` отдаёт opus; замерено, что запрошенный
    /// `bestaudio[acodec=opus]/bestaudio` даёт itag 774 с abr 251.
    pub audio_format: String,

    /// Показывать текущий трек в Discord.
    pub discord_rpc: bool,

    pub cache: CacheConfig,

    pub browser: BrowserConfig,

    /// Секция на провайдера: `[providers.ytmusic]`.
    ///
    /// `BTreeMap`, а не `HashMap`: порядок обхода детерминирован, иначе
    /// один и тот же конфиг давал бы разные последовательности
    /// (например, при автодетекте профилей) между запусками.
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mpv: PathBuf::from("mpv"),
            yt_dlp: PathBuf::from("yt-dlp"),
            volume: DEFAULT_VOLUME,
            audio_format: "bestaudio[acodec=opus]/bestaudio".to_owned(),
            discord_rpc: true,
            cache: CacheConfig::default(),
            browser: BrowserConfig::default(),
            providers: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Прочитать конфиг. Файла нет — значения по умолчанию.
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_file();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => return Err(CoreError::ConfigRead { path, source }),
        };

        toml::from_str(&text).map_err(|source| CoreError::ConfigParse { path, source })
    }

    /// Настройки провайдера. Секции нет — дефолт, а не ошибка:
    /// провайдер включён по умолчанию, иначе добавление нового
    /// провайдера требовало бы правки конфига у всех.
    #[must_use]
    pub fn provider(&self, id: &str) -> ProviderConfig {
        self.providers.get(id).cloned().unwrap_or_default()
    }

    /// Профиль браузера для провайдера: свой, если задан, иначе общий.
    ///
    /// Замерено, почему это разные вещи: у YouTube Music cookies лежат в
    /// профиле `Partitions/ytmview` приложения YTM Desktop, а у
    /// SoundCloud они окажутся в обычном профиле браузера.
    #[must_use]
    pub fn browser_profile_for(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .and_then(|cfg| cfg.browser_profile.as_deref())
            .or(self.browser.profile.as_deref())
    }

    /// Громкость, пригодная для mpv.
    ///
    /// NaN отсекается отдельно: `f64::clamp` пропускает его насквозь, и
    /// mpv получил бы `volume=NaN` — ровно тот мусор, от которого этот
    /// метод и защищает.
    #[must_use]
    pub fn volume_clamped(&self) -> f64 {
        if self.volume.is_nan() {
            DEFAULT_VOLUME
        } else {
            self.volume.clamp(0.0, 100.0)
        }
    }
}

/// Офлайн-кэш аудио.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Предел занимаемого места. Кэш — не главное хранилище: при
    /// переполнении вытесняется самое старое, а не отказывает запись.
    pub limit_bytes: u64,

    /// Качать следующий трек очереди заранее.
    pub prefetch_next: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            limit_bytes: DEFAULT_CACHE_LIMIT_BYTES,
            prefetch_next: true,
        }
    }
}

/// Браузер, из которого берутся cookies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// Явная спецификация профиля в формате yt-dlp: `chromium:/путь`
    /// или `firefox:/путь`. `None` — искать профиль самим.
    pub profile: Option<String>,
}

/// Настройки одного провайдера.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Выключенный провайдер не попадает в реестр: его треки не
    /// находятся и не играются, но настройки остаются на месте.
    pub enabled: bool,

    /// Профиль браузера именно для этого провайдера. `None` — общий
    /// [`BrowserConfig::profile`].
    pub browser_profile: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            browser_profile: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_under(dir: &std::path::Path) -> Paths {
        Paths::under(dir)
    }

    #[test]
    fn missing_file_is_defaults_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");

        let config = Config::load(&paths_under(dir.path())).expect("конфига нет — это не ошибка");

        assert_eq!(config.mpv, PathBuf::from("mpv"));
        assert_eq!(config.yt_dlp, PathBuf::from("yt-dlp"));
        assert_eq!(config.volume_clamped(), 70.0);
        assert_eq!(config.audio_format, "bestaudio[acodec=opus]/bestaudio");
        assert!(config.discord_rpc);
        assert_eq!(config.cache.limit_bytes, 8 * 1024 * 1024 * 1024);
        assert!(config.cache.prefetch_next);
        assert!(config.browser.profile.is_none());
        assert!(config.providers.is_empty());
        // Провайдер без секции всё равно включён.
        assert!(config.provider("ytmusic").enabled);
    }

    #[test]
    fn broken_toml_is_a_parse_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_under(dir.path());
        std::fs::create_dir_all(paths.config_dir()).expect("config dir");
        std::fs::write(paths.config_file(), "volume = = 70\n").expect("write");

        let err = Config::load(&paths).expect_err("битый TOML обязан быть ошибкой");

        assert!(
            matches!(&err, CoreError::ConfigParse { path, .. } if path == &paths.config_file()),
            "ожидался ConfigParse, получено {err:?}"
        );
    }

    #[test]
    fn partial_file_keeps_defaults_for_other_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_under(dir.path());
        std::fs::create_dir_all(paths.config_dir()).expect("config dir");
        std::fs::write(
            paths.config_file(),
            "[providers.ytmusic]\nbrowser_profile = \"chromium:/tmp/ytm\"\n\n[cache]\nlimit_bytes = 1024\n",
        )
        .expect("write");

        let config = Config::load(&paths).expect("разбор");

        assert_eq!(config.cache.limit_bytes, 1024);
        assert!(config.cache.prefetch_next, "незаданное поле — дефолт");
        assert_eq!(config.volume_clamped(), 70.0);
        assert!(config.provider("ytmusic").enabled);
    }

    #[test]
    fn provider_profile_wins_over_general() {
        let mut config = Config {
            browser: BrowserConfig {
                profile: Some("chromium:/tmp/общий".to_owned()),
            },
            ..Config::default()
        };
        config.providers.insert(
            "ytmusic".to_owned(),
            ProviderConfig {
                enabled: true,
                browser_profile: Some("firefox:/tmp/свой".to_owned()),
            },
        );

        assert_eq!(
            config.browser_profile_for("ytmusic"),
            Some("firefox:/tmp/свой")
        );
        assert_eq!(
            config.browser_profile_for("soundcloud"),
            Some("chromium:/tmp/общий"),
            "у провайдера без своего профиля берётся общий"
        );
    }

    #[test]
    fn volume_is_clamped_and_nan_is_replaced() {
        let clamp = |volume: f64| Config {
            volume,
            ..Config::default()
        }
        .volume_clamped();

        assert_eq!(clamp(-20.0), 0.0);
        assert_eq!(clamp(1000.0), 100.0);
        assert_eq!(clamp(35.5), 35.5);
        assert_eq!(clamp(f64::NAN), 70.0, "NaN для mpv — мусор");
    }
}
