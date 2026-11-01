//! Parked-message archives: durable snapshots of records an operator has
//! removed from the live store.
//!
//! The live store's `/health` gauge only counts what is in `cctp.json`; an
//! archived record is still a fact (the 2026-10-31 re-pin left eight parked
//! burns that disappeared from the gauge after they were archived). Boot
//! therefore reads every `cctp-parked-archive-*.json` beside the state file
//! and exposes their parked count as a metric.
//!
//! Reads are tolerant by design: one truncated archive must not stop the
//! bridge or silently erase history. A corrupt file is reported by path and
//! parse error to the caller (which logs it and counts it), while the other
//! archives still load. Writes go through [`crate::atomic::write_atomic`], so
//! a reader can never observe the unterminated-string shape measured on
//! `cctp-parked-archive-20261030.json`.
use crate::error::{Error, Result};
use crate::store::{CctpState, MessageRecord, RelayState, Store};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The filename prefix every parked archive carries.
pub const ARCHIVE_PREFIX: &str = "cctp-parked-archive-";

/// One archive file that could not be read. The path is kept so the operator
/// can inspect/repair it; the parse error is kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptArchive {
    pub path: PathBuf,
    pub error: String,
}

/// What a state directory's archives contain right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveSummary {
    /// Number of archive files successfully parsed.
    pub archives: usize,
    /// Parked records in those files (the metric's value).
    pub parked: usize,
    /// Files that exist but could not be read/parsed, reported not dropped.
    pub corrupt: Vec<CorruptArchive>,
}

/// The archive's on-disk shapes: the historical bare array and a full
/// `CctpState` snapshot. Both carry the same `MessageRecord`s.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ArchiveFile {
    Records(Vec<MessageRecord>),
    State(CctpState),
}

/// Write parked `records` to `path` atomically (temp + fsync + rename).
pub(crate) fn write(path: &Path, records: &[MessageRecord]) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(records).map_err(|source| Error::Encode {
        what: "cctp parked archive",
        source,
    })?;
    crate::atomic::write_atomic(path, &bytes).map_err(|source| Error::Io {
        what: "cctp parked archive",
        path: path.to_path_buf(),
        source,
    })
}

/// Read one archive. Absent is an IO fault (the caller named a file it
/// expected); corrupt bytes are [`Error::Corrupt`] with the path named.
pub(crate) fn read(path: &Path) -> Result<Vec<MessageRecord>> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        what: "cctp parked archive",
        path: path.to_path_buf(),
        source,
    })?;
    let parsed: ArchiveFile = serde_json::from_str(&text).map_err(|source| Error::Corrupt {
        what: "cctp parked archive",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(match parsed {
        ArchiveFile::Records(records) => records,
        ArchiveFile::State(state) => state.messages,
    })
}

/// Read every parked archive in `dir`. A missing directory is an empty
/// archive set (a fresh deployment); a corrupt file is returned in
/// [`ArchiveSummary::corrupt`] rather than aborting the whole scan.
pub fn load_all(dir: &Path) -> ArchiveSummary {
    let mut summary = ArchiveSummary::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return summary,
        Err(error) => {
            summary.corrupt.push(CorruptArchive {
                path: dir.to_path_buf(),
                error: error.to_string(),
            });
            return summary;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(ARCHIVE_PREFIX) || !name.ends_with(".json") {
            continue;
        }
        match read(&path) {
            Ok(records) => {
                summary.archives += 1;
                summary.parked += records
                    .iter()
                    .filter(|record| record.state == RelayState::Parked)
                    .count();
            }
            Err(error) => summary.corrupt.push(CorruptArchive {
                path,
                error: error.to_string(),
            }),
        }
    }
    summary
}

impl Store {
    /// Snapshot the live parked records to `path` atomically. The live store
    /// is unchanged: archiving is a durable copy for the operator/metrics,
    /// not a second lifecycle transition.
    pub fn archive_parked(&self, path: &Path) -> Result<usize> {
        let parked: Vec<MessageRecord> = self.with(|state| {
            Ok(state
                .messages
                .iter()
                .filter(|record| record.state == RelayState::Parked)
                .cloned()
                .collect())
        })?;
        if parked.is_empty() {
            return Ok(0);
        }
        write(path, &parked)?;
        Ok(parked.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MessageRecord, RelayState};
    use alloy_primitives::B256;

    fn record(index: u8, state: RelayState) -> MessageRecord {
        MessageRecord {
            source_domain: crate::chains::Domain(0),
            destination_domain: crate::chains::Domain(19),
            nonce: B256::repeat_byte(index),
            burn_tx: B256::repeat_byte(index),
            burn_log_index: 0,
            burn_block: u64::from(index),
            raw_message: Default::default(),
            message: Default::default(),
            message_hash: B256::ZERO,
            attestation: None,
            state,
            created_ms: 1,
            attest_after_ms: 1,
            relay_tx: None,
            relayed_ms: None,
            attempts: 0,
            park_reason: Some("re-pinned".into()),
        }
    }

    #[test]
    fn archive_round_trips_atomically_and_counts_only_parked_records() {
        let dir = crate::test_dir("archive");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{ARCHIVE_PREFIX}20261101.json"));
        let records = vec![
            record(1, RelayState::Parked),
            record(2, RelayState::Parked),
            record(3, RelayState::Signed),
        ];
        write(&path, &records).unwrap();
        assert_eq!(read(&path).unwrap(), records);
        let summary = load_all(&dir);
        assert_eq!(summary.archives, 1);
        assert_eq!(summary.parked, 2, "only parked records feed the gauge");
        assert!(summary.corrupt.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_archive_is_reported_and_does_not_hide_the_others() {
        let dir = crate::test_dir("archive-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join(format!("{ARCHIVE_PREFIX}20261101.json"));
        let bad = dir.join(format!("{ARCHIVE_PREFIX}20261030.json"));
        write(&good, &[record(1, RelayState::Parked)]).unwrap();
        // The measured shape: an unterminated string.
        std::fs::write(&bad, br#"[{"nonce":"0xdeadbeef"#).unwrap();
        let summary = load_all(&dir);
        assert_eq!(summary.archives, 1);
        assert_eq!(summary.parked, 1);
        assert_eq!(summary.corrupt.len(), 1, "{summary:?}");
        assert_eq!(summary.corrupt[0].path, bad);
        assert!(
            summary.corrupt[0].error.contains("cctp parked archive"),
            "{}",
            summary.corrupt[0].error
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_archive_snapshots_parked_records() {
        let dir = crate::test_dir("archive-store");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::load(dir.join("cctp.json")).unwrap();
        store
            .with(|state| {
                state.messages.push(record(1, RelayState::Parked));
                state.messages.push(record(2, RelayState::Signed));
                Ok(())
            })
            .unwrap();
        let path = dir.join(format!("{ARCHIVE_PREFIX}20261101.json"));
        assert_eq!(store.archive_parked(&path).unwrap(), 1);
        assert_eq!(read(&path).unwrap().len(), 1);
        // The live store keeps its record: archiving is a snapshot.
        assert_eq!(store.counts().unwrap().parked, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
