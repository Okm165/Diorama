//! Engine cursor persistence + rebase detection.
//!
//! The cursor is what makes restarts and re-pins safe:
//!   - a normal restart resumes at cursor+1 after verifying that the chain still
//!     has the cursor block with the recorded hash;
//!   - a chain reset (re-pin drill, wiped state) is detected by the same check
//!     and the cursor rebases to the chain head;
//!   - `reth_last_hash` detects upstream reorgs between consecutive blocks.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainCursor {
    pub block: u64,
    pub hash: String,
    /// First block this chain ever processed in the current epoch (pin+1 in
    /// practice); used for the pin-age metric.
    pub first_block: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persisted {
    #[serde(default)]
    pub m: Option<ChainCursor>,
    #[serde(default)]
    pub t: Option<ChainCursor>,
    /// Hash of the last processed reth block (reorg continuity check).
    #[serde(default)]
    pub reth_last_hash: Option<String>,
    /// Height of the last processed reth block (continuity check guard).
    #[serde(default)]
    pub reth_last_block: Option<u64>,
    /// Learned blob wrapper form ("legacy" | "eip7594" | "bare").
    #[serde(default)]
    pub blob_wrapper: Option<String>,
    /// Fork pin of the current epoch (anvil forkBlockNumber). A change means a
    /// re-pin: the shadow set is cleared so the devnet starts fresh.
    #[serde(default)]
    pub epoch_pin: Option<u64>,
}

pub struct StateFile {
    path: PathBuf,
    pub data: Persisted,
}

impl StateFile {
    pub fn open(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, data }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.data)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("renaming {}", self.path.display()))
    }
}

/// Decide which block to process next for a chain.
///
/// Replay must be contiguous. A hash-validated cursor always wins: resume at
/// cursor + 1, even when the cursor sits *behind* the chain head - that happens
/// legitimately (T's height advances independently of parent heights; a cursor
/// write can also race a restart), and jumping to `chain_head + 1` silently
/// skipped parent blocks and stalled T. A fresh or reset chain (no cursor)
/// starts at `chain_head + 1`; on first start that is the pin + 1, so the whole
/// pin window is replayed as the warm-up.
pub fn next_block(cursor: Option<&ChainCursor>, chain_head: u64) -> (u64, bool) {
    match cursor {
        Some(c) => (c.block + 1, false),
        None => (chain_head.saturating_add(1), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_is_contiguous() {
        let c = ChainCursor {
            block: 100,
            hash: "0x1".into(),
            first_block: 1,
        };
        assert_eq!(next_block(Some(&c), 100), (101, false));
        // a valid cursor behind the chain head still resumes right after it
        // (T's height advances independently; skipping ahead loses diffs)
        assert_eq!(next_block(Some(&c), 500), (101, false));
        // and a cursor ahead of the head (chain reset) is handled by the
        // caller's hash check, which discards it
        assert_eq!(next_block(Some(&c), 50), (101, false));
    }

    #[test]
    fn fresh_or_reset_chain_starts_at_head_plus_one() {
        // fresh chain at pin 900 -> 901 (the pin window is the warm-up)
        assert_eq!(next_block(None, 900), (901, true));
        // follow behind the chain head (reth catching up) -> still head + 1
        assert_eq!(next_block(None, 1_000), (1_001, true));
    }
}
