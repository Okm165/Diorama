//! Per-block journal of before-values, so applied parent blocks can be rewound.
//!
//! The journal records what the applier is about to overwrite on T (storage
//! slots and account fields). Files are `journal/<block>.jsonl`, ring-bounded.
//!
//! Entries are typed and serde's tag *is* the file format: an unknown kind, a
//! storage entry without a slot, or a malformed value fails the load as
//! [`Error::Corrupt`]. A skipped entry would make a rewind report success while
//! leaving part of the rolled-back range un-restored.
use crate::error::{Error, Result};
use alloy_primitives::{Address, Bytes, U256};
use mirror_anvil::{Rpc, Write};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One recorded before-value, tagged by kind in the JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum JournalEntry {
    Storage {
        address: Address,
        slot: U256,
        before: U256,
    },
    Balance {
        address: Address,
        before: U256,
    },
    Nonce {
        address: Address,
        /// The node's own quantity encoding (`alloy_serde::quantity`: `0x`-hex,
        /// zero as `0x0`), so a nonce before-value round-trips like the others.
        #[serde(with = "alloy_serde::quantity")]
        before: u64,
    },
    Code {
        address: Address,
        before: Bytes,
    },
}

impl JournalEntry {
    /// The undo write this entry restores.
    fn to_write(&self) -> Write {
        match self {
            JournalEntry::Storage {
                address,
                slot,
                before,
            } => Write::Storage {
                address: *address,
                slot: *slot,
                value: *before,
            },
            JournalEntry::Balance { address, before } => Write::Balance {
                address: *address,
                value: *before,
            },
            JournalEntry::Nonce { address, before } => Write::Nonce {
                address: *address,
                value: *before,
            },
            JournalEntry::Code { address, before } => Write::Code {
                address: *address,
                value: before.clone(),
            },
        }
    }
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
    /// The ring size: the rewindable window. `/admin/rewind` refuses beyond
    /// it — the older before-values are pruned, and restoring only part of the
    /// requested range while reporting success would be a lie.
    pub fn keep(&self) -> u64 {
        self.keep
    }

    /// The journaled blocks within `[from, to]`, newest first. The rewind
    /// walks only what exists: a block with no recorded writes has no file
    /// (empty batches are not recorded), so a per-block probe would fail on a
    /// quiet block — and a huge range would cost one lookup per block.
    pub fn blocks_in(&self, from: u64, to: u64) -> Result<Vec<u64>> {
        let entries = std::fs::read_dir(&self.dir).map_err(|source| Error::Io {
            what: "journal directory",
            path: self.dir.clone(),
            source,
        })?;
        let mut out: Vec<u64> = Vec::new();
        for e in entries.flatten() {
            let Some(stem) = e
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".jsonl").map(str::to_string))
            else {
                continue;
            };
            if let Ok(block) = stem.parse::<u64>() {
                if block >= from && block <= to {
                    out.push(block);
                }
            }
        }
        out.sort_unstable_by(|a, b| b.cmp(a));
        Ok(out)
    }

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
        // tmp+rename, like the other state files: a crash mid-write must not
        // leave a truncated file that parses as a shorter, valid journal.
        let tmp = self.dir.join(format!("{block}.jsonl.tmp"));
        std::fs::write(&tmp, buf).map_err(|source| Error::Io {
            what: "journal entry",
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &path).map_err(|source| Error::Io {
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
            prune_entry(&entry, floor);
        }
    }

    /// Write every recorded before-value back to the chain (newest block first
    /// is the caller's order). Returns the number of accepted writes.
    pub async fn undo_block(&self, block: u64, t: &Rpc, chunk: usize) -> Result<usize> {
        let entries = self.load(block)?;
        let writes: Vec<Write> = entries.iter().map(JournalEntry::to_write).collect();
        mirror_anvil::apply_writes(t, &writes, chunk)
            .await
            .map_err(Error::from)
    }
}

/// One directory entry: a crashed `record` left a partial temp file — the
/// real entry is the renamed one, so the temp is always garbage. A `.jsonl`
/// entry below the floor is removed; anything else is left alone.
fn prune_entry(entry: &std::fs::DirEntry, floor: u64) {
    let name = entry.file_name();
    let Some(stem) = name.to_str() else {
        return;
    };
    if stem.ends_with(".jsonl.tmp") {
        remove_logged(&entry.path(), "journal prune: temp file could not be removed");
        return;
    }
    let Some(stem) = stem.strip_suffix(".jsonl") else {
        return;
    };
    if stem.parse::<u64>().is_ok_and(|n| n < floor) {
        remove_logged(&entry.path(), "journal prune: entry could not be removed");
    }
}

/// A housekeeping removal: a fault is logged, never silent.
fn remove_logged(path: &std::path::Path, what: &'static str) {
    if let Err(source) = std::fs::remove_file(path) {
        tracing::warn!(path = %path.display(), error = %source, "{what}");
    }
}

/// Journal before-values for a batch of writes (read from T first).
///
/// The pairing of a write with its before-value is a caller contract; a
/// mismatch is surfaced as [`Error::KindMismatch`], never a panic (the journal
/// is a library — a caller bug must not abort the engine mid-apply).
pub fn entry_for_write(w: &Write, before: BeforeValue) -> Result<JournalEntry> {
    match (w, before) {
        (Write::Storage { address, slot, .. }, BeforeValue::Storage(v)) => Ok(JournalEntry::Storage {
            address: *address,
            slot: *slot,
            before: v,
        }),
        (Write::Balance { address, .. }, BeforeValue::Balance(v)) => Ok(JournalEntry::Balance {
            address: *address,
            before: v,
        }),
        (Write::Nonce { address, .. }, BeforeValue::Nonce(v)) => Ok(JournalEntry::Nonce {
            address: *address,
            before: v,
        }),
        (Write::Code { address, .. }, BeforeValue::Code(v)) => Ok(JournalEntry::Code {
            address: *address,
            before: v,
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
    Code(Bytes),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_roundtrip_and_prune() {
        let dir = crate::test_dir("journal-test");
        let j = Journal::new(dir.clone(), 10).unwrap();
        let e = JournalEntry::Storage {
            address: Address::repeat_byte(1),
            slot: U256::from(5u64),
            before: U256::from(42u64),
        };
        j.record(100, std::slice::from_ref(&e)).unwrap();
        let loaded = j.load(100).unwrap();
        assert_eq!(loaded.len(), 1);
        // prune keeps last `keep` blocks
        j.record(200, std::slice::from_ref(&e)).unwrap();
        j.prune(205);
        assert!(j.load(100).is_err(), "block 100 pruned at head 205 keep 10");
        assert!(j.load(200).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rewind walks only the blocks that have before-values: sparse files
    /// (a quiet block is never recorded) are skipped, the order is newest
    /// first, and a temp file is not a block.
    #[test]
    fn blocks_in_is_sparse_and_newest_first() {
        let dir = crate::test_dir("journal-range");
        let j = Journal::new(dir.clone(), 1024).unwrap();
        let e = JournalEntry::Storage {
            address: Address::repeat_byte(2),
            slot: U256::from(1u64),
            before: U256::from(7u64),
        };
        for block in [100u64, 103, 105] {
            j.record(block, std::slice::from_ref(&e)).unwrap();
        }
        // A crash-mid-write temp file must not read as a journaled block.
        std::fs::write(dir.join("104.jsonl.tmp"), "partial").unwrap();
        assert_eq!(j.keep(), 1024);
        assert_eq!(j.blocks_in(99, 106).unwrap(), vec![105, 103, 100]);
        assert_eq!(j.blocks_in(101, 104).unwrap(), vec![103], "the range clips");
        assert_eq!(j.blocks_in(200, 300).unwrap(), Vec::<u64>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The entries an older engine wrote must still parse: a rollout must not
    /// brick a rewind that spans the old journal's files.
    #[test]
    fn legacy_entries_parse() {
        let dir = crate::test_dir("journal-legacy");
        let j = Journal::new(dir.clone(), 10).unwrap();
        let addr = format!("0x{}", alloy_primitives::hex::encode([0x11u8; 20]));
        let legacy = [
            format!(r#"{{"address":"{addr}","kind":"storage","slot":"0x5","before":"0x000000000000000000000000000000000000000000000000000000000000002a"}}"#),
            format!(r#"{{"address":"{addr}","kind":"balance","before":"0x2a"}}"#),
            format!(r#"{{"address":"{addr}","kind":"nonce","before":"0x3"}}"#),
        ]
        .join("\n");
        std::fs::write(dir.join("7.jsonl"), legacy).unwrap();
        let loaded = j.load(7).unwrap();
        assert_eq!(loaded.len(), 3);
        assert!(matches!(
            loaded[0],
            JournalEntry::Storage { before, .. } if before == U256::from(42u64)
        ));
        assert!(matches!(loaded[2], JournalEntry::Nonce { before, .. } if before == 3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A nonce before-value is *written* in the node's own quantity encoding
    /// (`alloy_serde::quantity`, which the field borrows): `0x`-hex, zero as
    /// `0x0` — the exact text the older hand-rolled newtype wrote, so a
    /// journal spanning the swap stays one format (the read side is pinned by
    /// `legacy_entries_parse`).
    #[test]
    fn nonce_before_values_write_as_quantities() {
        let dir = crate::test_dir("journal-nonce-text");
        let j = Journal::new(dir.clone(), 10).unwrap();
        let address = Address::repeat_byte(3);
        j.record(
            9,
            std::slice::from_ref(&JournalEntry::Nonce { address, before: 0x1f }),
        )
        .unwrap();
        j.record(
            10,
            std::slice::from_ref(&JournalEntry::Nonce { address, before: 0 }),
        )
        .unwrap();
        let text = std::fs::read_to_string(dir.join("9.jsonl")).unwrap();
        assert!(text.contains(r#""before":"0x1f""#), "unexpected text: {text}");
        let text = std::fs::read_to_string(dir.join("10.jsonl")).unwrap();
        assert!(text.contains(r#""before":"0x0""#), "unexpected text: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unknown kind or a storage entry without a slot is corrupt, never a
    /// silently skipped entry: a partial rewind must not report success.
    #[test]
    fn malformed_entries_are_corrupt() {
        let dir = crate::test_dir("journal-bad");
        let j = Journal::new(dir.clone(), 10).unwrap();
        let addr = format!("0x{}", alloy_primitives::hex::encode([0x11u8; 20]));
        std::fs::write(
            dir.join("7.jsonl"),
            format!(r#"{{"address":"{addr}","kind":"mystery","before":"0x1"}}"#),
        )
        .unwrap();
        assert!(matches!(j.load(7), Err(Error::Corrupt { .. })));
        std::fs::write(
            dir.join("7.jsonl"),
            format!(r#"{{"address":"{addr}","kind":"storage","before":"0x1"}}"#),
        )
        .unwrap();
        assert!(matches!(j.load(7), Err(Error::Corrupt { .. })));
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
