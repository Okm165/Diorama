//! Per-block journal of before-values, so applied parent blocks can be rewound.
//!
//! The journal records what the applier is about to overwrite on T (storage
//! slots and account fields). Files are `journal/<block>.jsonl`, ring-bounded.
use mirror_anvil::Rpc;
use mirror_anvil as bal;
use mirror_anvil::Write;
use alloy_primitives::{Address, U256};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub address: Address,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<U256>,
    /// Value before the write (hex as returned by the node).
    pub before: String,
}

pub struct Journal {
    dir: PathBuf,
    keep: u64,
}

impl Journal {
    /// Create the journal directory (a boot failure here is typed, not
    /// swallowed: a journal that cannot exist silently disables rewind).
    pub fn new(dir: PathBuf, keep: u64) -> Result<Self> {
        std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
            what: "journal directory",
            path: dir.clone(),
            source,
        })?;
        Ok(Self { dir, keep })
    }

    /// Drop every recorded block. Used when T is re-forked under us: the
    /// before-values belong to a chain that no longer exists, so a rewind
    /// against the new one would write values that were never there. A file
    /// that cannot be removed is logged loudly: a survivor is a rewind hazard.
    pub fn clear(&self) -> Result<()> {
        let entries = std::fs::read_dir(&self.dir).map_err(|source| Error::Io {
            what: "journal directory",
            path: self.dir.clone(),
            source,
        })?;
        for e in entries.flatten() {
            if let Err(source) = std::fs::remove_file(e.path()) {
                tracing::warn!(
                    path = %e.path().display(),
                    error = %source,
                    "journal clear: entry could not be removed"
                );
            }
        }
        Ok(())
    }

    pub fn record(&self, block: u64, entries: &[JournalEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let path = self.dir.join(format!("{block}.jsonl"));
        let mut buf = String::new();
        for e in entries {
            let line = serde_json::to_string(e).map_err(|source| Error::Encode {
                what: "journal entry",
                source,
            })?;
            buf.push_str(&line);
            buf.push('\n');
        }
        std::fs::write(&path, buf).map_err(|source| Error::Io {
            what: "journal entry",
            path,
            source,
        })
    }

    /// Does a journal file exist for this block?
    ///
    /// Three answers: **absent** is a fact (the block recorded no writes);
    /// a **fault** is an error. Guessing "no journal" on a fault would make the
    /// applier re-apply and re-journal the block, recording already-applied
    /// values as before-values — poisoning the rewind data silently.
    pub fn has_entry(&self, block: u64) -> Result<bool> {
        match std::fs::metadata(self.dir.join(format!("{block}.jsonl"))) {
            Ok(_) => Ok(true),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Io {
                what: "journal entry",
                path: self.dir.join(format!("{block}.jsonl")),
                source,
            }),
        }
    }

    pub fn load(&self, block: u64) -> Result<Vec<JournalEntry>> {
        let path = self.dir.join(format!("{block}.jsonl"));
        let s = std::fs::read_to_string(&path).map_err(|source| Error::Io {
            what: "journal entry",
            path: path.clone(),
            source,
        })?;
        let mut out = Vec::new();
        for line in s.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let entry = serde_json::from_str(line).map_err(|source| Error::Corrupt {
                what: "journal entry",
                path: path.clone(),
                source,
            })?;
            out.push(entry);
        }
        Ok(out)
    }

    /// Drop journal files older than `keep` blocks behind `head`. Housekeeping:
    /// a fault is logged, never silent (an unreadable directory means the ring
    /// is not being bounded).
    pub fn prune(&self, head: u64) {
        let floor = head.saturating_sub(self.keep);
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(source) => {
                tracing::warn!(
                    dir = %self.dir.display(),
                    error = %source,
                    "journal prune: directory unreadable"
                );
                return;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|s| s.strip_suffix(".jsonl")) else {
                continue;
            };
            if let Ok(n) = stem.parse::<u64>() {
                if n < floor {
                    if let Err(source) = std::fs::remove_file(entry.path()) {
                        tracing::warn!(
                            path = %entry.path().display(),
                            error = %source,
                            "journal prune: entry could not be removed"
                        );
                    }
                }
            }
        }
    }

    /// Write every recorded before-value back to the chain (newest block first).
    /// Returns the number of restored values.
    pub async fn undo_block(&self, block: u64, t: &Rpc, chunk: usize) -> Result<usize> {
        let entries = self.load(block)?;
        let mut calls: Vec<(String, serde_json::Value)> = Vec::with_capacity(entries.len());
        for e in &entries {
            let call = match e.kind.as_str() {
                "storage" => (
                    "anvil_setStorageAt".to_string(),
                    json!([e.address, bal::pad_data32(e.slot.unwrap_or_default()), pad_storage(&e.before)]),
                ),
                "balance" => ("anvil_setBalance".to_string(), json!([e.address, e.before])),
                "nonce" => ("anvil_setNonce".to_string(), json!([e.address, e.before])),
                "code" => ("anvil_setCode".to_string(), json!([e.address, e.before])),
                other => {
                    tracing::warn!(kind = other, "unknown journal entry kind");
                    continue;
                }
            };
            calls.push(call);
        }
        let mut restored = 0usize;
        for chunk_calls in calls.chunks(chunk.max(1)) {
            let results = t.batch_raw(chunk_calls).await?;
            for r in results {
                match r {
                    Ok(_) => restored += 1,
                    Err(e) => tracing::warn!(error = %e, "journal undo: write failed"),
                }
            }
        }
        Ok(restored)
    }
}

/// Journal before-values for a batch of writes (read from T first).
///
/// The pairing of a write with its before-value is a caller contract; a
/// mismatch is surfaced as [`Error::KindMismatch`], never a panic (the journal
/// is a library — a caller bug must not abort the engine mid-apply).
pub fn entry_for_write(w: &Write, before: BeforeValue) -> Result<JournalEntry> {
    match (w, before) {
        (
            Write::Storage { address, slot, .. },
            BeforeValue::Storage(v),
        ) => Ok(JournalEntry {
            address: *address,
            kind: "storage".into(),
            slot: Some(*slot),
            before: bal::pad_data32(v),
        }),
        (Write::Balance { address, .. }, BeforeValue::Balance(v)) => Ok(JournalEntry {
            address: *address,
            kind: "balance".into(),
            slot: None,
            before: bal::quant_hex_u256(v),
        }),
        (Write::Nonce { address, .. }, BeforeValue::Nonce(v)) => Ok(JournalEntry {
            address: *address,
            kind: "nonce".into(),
            slot: None,
            before: bal::quant_hex_u64(v),
        }),
        (Write::Code { address, .. }, BeforeValue::Code(v)) => Ok(JournalEntry {
            address: *address,
            kind: "code".into(),
            slot: None,
            before: bal::bytes_hex(&v),
        }),
        (w, b) => Err(Error::KindMismatch {
            write_kind: write_kind(w),
            before_kind: b.kind(),
        }),
    }
}

fn write_kind(w: &Write) -> &'static str {
    match w {
        Write::Storage { .. } => "storage",
        Write::Balance { .. } => "balance",
        Write::Nonce { .. } => "nonce",
        Write::Code { .. } => "code",
    }
}

#[derive(Debug, Clone)]
pub enum BeforeValue {
    Storage(U256),
    Balance(U256),
    Nonce(u64),
    Code(alloy_primitives::Bytes),
}

impl BeforeValue {
    fn kind(&self) -> &'static str {
        match self {
            BeforeValue::Storage(_) => "storage",
            BeforeValue::Balance(_) => "balance",
            BeforeValue::Nonce(_) => "nonce",
            BeforeValue::Code(_) => "code",
        }
    }
}

/// Storage before-values are already 32-byte data; keep the padding rule in one place.
fn pad_storage(s: &str) -> String {
    let t = s.trim_start_matches("0x");
    format!("0x{t:0>64}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_roundtrip_and_prune() {
        let dir = std::env::temp_dir().join(format!("mirror-journal-test-{}", std::process::id()));
        let j = Journal::new(dir.clone(), 10).unwrap();
        let e = JournalEntry {
            address: Address::repeat_byte(1),
            kind: "storage".into(),
            slot: Some(U256::from(5u64)),
            before: "0x000000000000000000000000000000000000000000000000000000000000002a".into(),
        };
        j.record(100, std::slice::from_ref(&e)).unwrap();
        let loaded = j.load(100).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].before, e.before);
        // prune keeps last `keep` blocks
        j.record(200, std::slice::from_ref(&e)).unwrap();
        j.prune(205);
        assert!(j.load(100).is_err(), "block 100 pruned at head 205 keep 10");
        assert!(j.load(200).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_mismatched_write_before_pair_is_a_typed_fault() {
        let w = Write::Balance {
            address: Address::repeat_byte(1),
            value: U256::ZERO,
        };
        let err = entry_for_write(&w, BeforeValue::Nonce(1)).unwrap_err();
        assert!(matches!(
            err,
            Error::KindMismatch {
                write_kind: "balance",
                before_kind: "nonce"
            }
        ));
    }
}
