//! Atomic JSON state files (the `shadow.json` pattern): a load has three
//! answers — absent (a fact: the caller starts empty), fault, or corrupt
//! (dies at the load point, path named) — and a save never leaves a torn file
//! (write temp, rename).
use crate::error::{Error, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

/// Load a JSON state file. `what` names the state in the errors.
pub fn load<T: DeserializeOwned>(path: &Path, what: &'static str) -> Result<Option<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Io {
                what,
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let value = serde_json::from_str(&text).map_err(|source| Error::Corrupt {
        what,
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(value))
}

/// Save a JSON state file atomically (temp + rename). `what` names the state
/// in the errors; missing parent directories are created.
pub fn save<T: Serialize>(path: &Path, value: &T, what: &'static str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            what,
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::Encode { what, source })?;
    std::fs::write(&tmp, bytes).map_err(|source| Error::Io {
        what,
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| Error::Io {
        what,
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Sample {
        n: u32,
    }

    #[test]
    fn absent_is_a_fact_faults_and_corruption_are_errors() {
        let dir = std::env::temp_dir().join(format!("mirror-store-test-{}", std::process::id()));
        let path = dir.join("state.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load::<Sample>(&path, "test state").unwrap(), None);
        save(&path, &Sample { n: 7 }, "test state").unwrap();
        assert_eq!(load::<Sample>(&path, "test state").unwrap(), Some(Sample { n: 7 }));
        std::fs::write(&path, b"{not json").unwrap();
        let err = load::<Sample>(&path, "test state").unwrap_err();
        assert!(matches!(err, Error::Corrupt { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
