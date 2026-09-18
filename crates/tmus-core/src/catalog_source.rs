use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::paths::Paths;
use crate::protocol::CatalogSource;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StateFile {
    catalog_source: CatalogSource,
}

pub fn load(paths: &Paths) -> Option<CatalogSource> {
    let bytes = std::fs::read(paths.state_file()).ok()?;
    match serde_json::from_slice::<StateFile>(&bytes) {
        Ok(state) => Some(state.catalog_source),
        Err(_) => None,
    }
}

pub fn save(paths: &Paths, source: &CatalogSource) -> Result<()> {
    std::fs::create_dir_all(paths.state_dir())?;
    let file = paths.state_file();
    let tmp = file.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(&StateFile { catalog_source: source.clone() })
            .map_err(|err| std::io::Error::other(err.to_string()))?,
    )?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("tmus-cs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Paths::under(&root)
    }

    #[test]
    fn missing_state_file_yields_no_source() {
        let paths = scratch("missing");
        assert_eq!(load(&paths), None);
    }

    #[test]
    fn saved_source_survives_reload() {
        let paths = scratch("roundtrip");
        let saved = CatalogSource { provider: Some("ytmusic".into()) };
        save(&paths, &saved).expect("save");
        let loaded = load(&paths);
        assert_eq!(loaded, Some(saved));
    }

    #[test]
    fn saved_all_survives_reload() {
        let paths = scratch("all");
        save(&paths, &CatalogSource { provider: None }).expect("save");
        assert_eq!(load(&paths), Some(CatalogSource { provider: None }));
    }

    #[test]
    fn corrupt_state_file_yields_no_source() {
        let paths = scratch("corrupt");
        std::fs::create_dir_all(paths.state_dir()).expect("mkdir");
        std::fs::write(paths.state_file(), b"not json").expect("write");
        assert_eq!(load(&paths), None);
    }

    #[test]
    fn state_file_without_catalog_source_key_yields_no_source() {
        let paths = scratch("nokey");
        std::fs::create_dir_all(paths.state_dir()).expect("mkdir");
        std::fs::write(paths.state_file(), b"{}").expect("write");
        assert_eq!(load(&paths), None);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let paths = scratch("atomic");
        save(&paths, &CatalogSource { provider: Some("soundcloud".into()) }).expect("save");
        let names: Vec<_> = std::fs::read_dir(paths.state_dir())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "только state.json, без временных файлов: {names:?}");
    }
}
