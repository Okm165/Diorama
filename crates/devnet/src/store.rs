//! Atomic JSON state files: one body for the substrate's `shadow.json`
//! pattern, shared by the registries ([`crate::state`], [`crate::shadow`]) and
//! delegated to by the venue's own state files (`hyperliquid`'s sim and feed
//! caches).
//!
//! A load has three answers (boundary-discipline): **absent** is a fact — the
//! caller starts empty, never an error; an **IO fault** is named with its
//! path; **corrupt** bytes die here, at the load point, with the path named —
//! a corrupt state file must never silently become an empty one. A save never
//! leaves a torn file: write a temp sibling, rename it over the target.
use crate::error::{Error, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

/// Load a JSON state file. `None` means absent (a fact, never an error);
/// `what` names the state in the errors.
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

/// Save a JSON state file atomically: [`save_bytes`] over the pretty-printed
/// value. `what` names the state in the errors.
pub fn save<T: Serialize>(path: &Path, value: &T, what: &'static str) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::Encode { what, source })?;
    save_bytes(path, &bytes, what)
}

/// Write bytes atomically (temp + rename), creating missing parent
/// directories. The temp sibling is `<name>.tmp`; a crash between the write
/// and the rename leaves it behind, and the reader of the *target* name never
/// sees it. The raw half of [`save`], for bodies that are not one serialized
/// value (the journal's JSONL block files).
///
/// `what` names the state in the errors (the path names the file or, for a
/// directory fault, the parent).
pub fn save_bytes(path: &Path, bytes: &[u8], what: &'static str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            what,
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = path.with_extension("tmp");
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
        let dir = crate::test_dir("store-test");
        let path = dir.join("state.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load::<Sample>(&path, "test state").unwrap(), None);
        save(&path, &Sample { n: 7 }, "test state").unwrap();
        assert_eq!(
            load::<Sample>(&path, "test state").unwrap(),
            Some(Sample { n: 7 })
        );
        std::fs::write(&path, b"{not json").unwrap();
        let err = load::<Sample>(&path, "test state").unwrap_err();
        assert!(matches!(err, Error::Corrupt { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
