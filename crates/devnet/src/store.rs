//! Atomic JSON state files: one body for the substrate's `shadow.json`
//! pattern, shared by the registries ([`crate::state`], [`crate::shadow`]) and
//! delegated to by the venue's own state files (`hyperliquid`'s sim and feed
//! caches).
//!
//! A load has four answers (boundary-discipline): **absent** is a fact — the
//! caller starts empty, never an error; a **zero-byte** file is a crash
//! artifact (a sandbox kill between the rename metadata and the data fsync) and
//! is treated as absent with a loud warning, because crash-looping on a file
//! the process itself truncated is worse than rebasing; an **IO fault** is
//! named with its path; **corrupt** bytes die here, at the load point, with the
//! path named — a corrupt state file must never silently become an empty one.
//! A save never leaves a torn file: write a temp sibling, fsync it, rename it
//! over the target, and fsync the parent directory.
use crate::error::{Error, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// The process's write sequence: every save's temp name is unique, so
/// concurrent savers of one path never collide on the temp file (measured:
/// a fixed `.tmp` name made 5 of 8 concurrent feed persists fail on the
/// rename — the loser's temp was already renamed away).
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

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
    if text.trim().is_empty() {
        tracing::warn!(
            what,
            path = %path.display(),
            "state file is empty (a crash-truncated artifact) — treating it as absent; the caller rebases/rebuilds"
        );
        return Ok(None);
    }
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
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|source| Error::Encode { what, source })?;
    save_bytes(path, &bytes, what)
}

/// The stores' first write step: the directory must exist before the temp
/// file lands in it. `Err` carries the directory, so each caller names its
/// own `what` in its own error (`drift`, `journal`, [`save_bytes`]).
pub(crate) fn ensure_dir(dir: &Path) -> Result<(), (std::path::PathBuf, std::io::Error)> {
    std::fs::create_dir_all(dir).map_err(|source| (dir.to_path_buf(), source))
}

/// Write bytes atomically (temp + rename), creating missing parent
/// directories. The temp sibling is `<name>.tmp.<seq>` — unique per write, so
/// concurrent savers of one path cannot race each other's rename (the last
/// rename wins with a complete file); a crash between the write and the
/// rename leaves a temp behind, and the reader of the *target* name never
/// sees it. The raw half of [`save`], for bodies that are not one serialized
/// value (the journal's JSONL block files).
///
/// `what` names the state in the errors (the path names the file or, for a
/// directory fault, the parent).
pub fn save_bytes(path: &Path, bytes: &[u8], what: &'static str) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent).map_err(|(path, source)| Error::Io { what, path, source })?;
    }
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{seq}"));
    let mut file = std::fs::File::create(&tmp).map_err(|source| Error::Io {
        what,
        path: tmp.clone(),
        source,
    })?;
    use std::io::Write;
    file.write_all(bytes).map_err(|source| Error::Io {
        what,
        path: tmp.clone(),
        source,
    })?;
    // The rename is atomic in the namespace, but not durable: without this
    // fsync a sandbox kill can leave a zero-byte target after the rename
    // metadata commits. The measured 2026-11-09 diorama-mainnet engine crash
    // loop was exactly this shape (`shadow.json` and `cursor.json` both 0 B).
    file.sync_all().map_err(|source| Error::Io {
        what,
        path: tmp.clone(),
        source,
    })?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|source| Error::Io {
        what,
        path: path.to_path_buf(),
        source,
    })?;
    // Persist the rename itself. A directory fsync failure is logged, not
    // returned: the target is already complete and visible, so failing the
    // caller after the save would be a false negative.
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            if let Err(source) = dir.sync_all() {
                tracing::warn!(
                    what,
                    path = %parent.display(),
                    error = %source,
                    "state directory fsync failed — the file is visible but the rename may not survive a power loss"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Sample {
        n: u32,
    }

    /// Concurrent saves of one path all succeed (the temp name is unique per
    /// write): a fixed temp name made the losers' renames fail (measured with
    /// 8 concurrent feed persists).
    #[test]
    fn concurrent_saves_of_one_path_never_collide() {
        let dir = crate::test_dir("store-race-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state.json");
        let mut handles = Vec::new();
        for n in 0..16u32 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                save(&path, &Sample { n }, "test state").unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let got = load::<Sample>(&path, "test state").unwrap().unwrap();
        assert!(got.n < 16, "the last rename wins with a complete file");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A sandbox kill can leave the target zero-byte after the rename metadata
    /// commits but before the data is durable. That is not a corrupt state to
    /// crash-loop on; it is an absent state, and the caller rebases/rebuilds.
    /// Whitespace-only is the same artifact (a partial pretty-print).
    #[test]
    fn a_zero_byte_state_file_is_a_crash_artifact_and_reads_as_absent() {
        let dir = crate::test_dir("store-empty-test");
        let path = dir.join("state.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"").unwrap();
        assert_eq!(load::<Sample>(&path, "test state").unwrap(), None);
        std::fs::write(&path, b"\n").unwrap();
        assert_eq!(load::<Sample>(&path, "test state").unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
