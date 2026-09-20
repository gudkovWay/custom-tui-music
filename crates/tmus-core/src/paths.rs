//! Пути. Одно место на весь проект — иначе демон, TUI и плагин
//! noctalia разойдутся в том, где лежит сокет, и это проявится как
//! «демон не отвечает».

use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

/// Имя приложения в XDG-каталогах и в именах сокетов.
pub const APP: &str = "tmus";

/// Имя шины MPRIS. Суффикс совпадает с `APP` намеренно: noctalia
/// показывает любой `org.mpris.MediaPlayer2.*`, а совпадение делает
/// процесс узнаваемым в `busctl --user list`.
pub const MPRIS_BUS_NAME: &str = "org.mpris.MediaPlayer2.tmus";

/// Набор путей приложения. Разрешается один раз при старте.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    config_dir: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
    runtime_dir: PathBuf,
}

impl Paths {
    /// Разрешить пути по XDG. `XDG_RUNTIME_DIR` отсутствует — падаем на
    /// `/tmp/tmus-<user>`: без него не будет сокета, а значит и связи
    /// между демоном и клиентом.
    pub fn resolve() -> Result<Self> {
        let dirs = directories::BaseDirs::new().ok_or(CoreError::NoHome)?;
        let runtime_dir = dirs.runtime_dir().map(Path::to_path_buf).unwrap_or_else(|| {
            let who = std::env::var("USER")
                .or_else(|_| std::env::var("LOGNAME"))
                .unwrap_or_else(|_| "nobody".to_owned());
            PathBuf::from(format!("/tmp/{APP}-{who}"))
        });

        Ok(Self {
            config_dir: dirs.config_dir().join(APP),
            cache_dir: dirs.cache_dir().join(APP),
            state_dir: dirs.state_dir().map_or_else(
                || dirs.data_local_dir().join(APP),
                |dir| dir.join(APP),
            ),
            runtime_dir,
        })
    }

    /// Пути внутри заданного корня. Для тестов и для прогонов на
    /// изолированном стенде.
    #[must_use]
    pub fn under(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            config_dir: root.join("config"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
        }
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// База метаданных: треки, плейлисты, учёт кэша.
    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.cache_dir.join("cache.sqlite")
    }

    /// Корень офлайн-кэша аудио. Раскладка внутри —
    /// `<provider>/<id>.<ext>`.
    #[must_use]
    pub fn audio_dir(&self) -> PathBuf {
        self.cache_dir.join("audio")
    }

    /// Каталог кэша для провайдера.
    #[must_use]
    pub fn audio_dir_for(&self, provider: &str) -> PathBuf {
        self.audio_dir().join(provider)
    }

    #[must_use]
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    /// Эксклюзивный замок демона. Лежит рядом с control-socket: та же
    /// жизненная зона runtime-каталога, чистится тем же tmpfiles-правилом.
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.runtime_dir.join(format!("{APP}.lock"))
    }

    /// Control-socket демона.
    #[must_use]
    pub fn control_socket(&self) -> PathBuf {
        self.runtime_dir.join(format!("{APP}.sock"))
    }

    /// IPC-сокет mpv. Отдельный от control-socket: по первому говорят
    /// клиенты, по второму — только демон с плеером.
    #[must_use]
    pub fn mpv_socket(&self) -> PathBuf {
        self.runtime_dir.join(format!("{APP}-mpv.sock"))
    }

    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Создать каталоги, которые приложение пишет. Конфиг не создаём:
    /// его отсутствие — штатный случай (работаем на значениях по
    /// умолчанию).
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.cache_dir, &self.state_dir, &self.runtime_dir] {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::create_dir_all(self.audio_dir())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_stable_under_a_root() {
        let paths = Paths::under("/stand");
        assert_eq!(paths.config_file(), PathBuf::from("/stand/config/config.toml"));
        assert_eq!(paths.database(), PathBuf::from("/stand/cache/cache.sqlite"));
        assert_eq!(
            paths.audio_dir_for("ytmusic"),
            PathBuf::from("/stand/cache/audio/ytmusic")
        );
        assert_eq!(paths.control_socket(), PathBuf::from("/stand/run/tmus.sock"));
        assert_eq!(paths.mpv_socket(), PathBuf::from("/stand/run/tmus-mpv.sock"));
        assert_eq!(paths.lock_file(), PathBuf::from("/stand/run/tmus.lock"));
    }

    #[test]
    fn control_and_mpv_sockets_never_collide() {
        let paths = Paths::under("/stand");
        assert_ne!(paths.control_socket(), paths.mpv_socket());
    }

    #[test]
    fn ensure_dirs_creates_writable_tree_but_not_config_file() {
        let root = std::env::temp_dir().join(format!("tmus-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::under(&root);

        paths.ensure_dirs().expect("create dirs");

        assert!(paths.audio_dir().is_dir());
        assert!(paths.state_dir().is_dir());
        assert!(paths.runtime_dir().is_dir());
        assert!(!paths.config_file().exists(), "конфиг создаётся только человеком");

        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
