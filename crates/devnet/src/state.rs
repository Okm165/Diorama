//! Engine cursor persistence + rebase detection.
//!
//! The cursor is what makes restarts and re-pins safe:
//!   - a normal restart resumes at cursor+1 after verifying that the chain still
//!     has the cursor block with the recorded hash;
//!   - a chain reset (re-pin drill, wiped state) is detected by the same check
//!     and the cursor rebases to the chain head;
//!   - `reth_last_hash` detects upstream reorgs between consecutive blocks.
use crate::error::Result;
use crate::store;
use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainCursor {
    pub block: u64,
    /// The block's hash: the chain-instance check (a cursor whose block no
    /// longer carries this hash belongs to a chain that was reset).
    pub hash: B256,
    /// First block this chain ever processed in the current epoch (pin+1 in
    /// practice); used for the pin-age metric.
    pub first_block: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persisted {
    /// The devnet chain's cursor: T on the mainnet profile, V on the venue
    /// profile. One field, both profiles — the chain is the profile's, the
    /// cursor's meaning is the same. (`alias = "t"`: pre-rename state files.)
    #[serde(default, alias = "t")]
    pub devnet: Option<ChainCursor>,
    /// Hash of the last processed source block (the reorg continuity check on
    /// both mirrors: the mainnet applier's parent hash, the venue's follow
    /// window). `alias`: pre-rename state files spelled this `reth_last_hash`.
    #[serde(default, alias = "reth_last_hash")]
    pub source_last_hash: Option<B256>,
    /// Height of the last processed source block (continuity check guard).
    #[serde(default, alias = "reth_last_block")]
    pub source_last_block: Option<u64>,
    /// Fork pin of the current epoch (anvil forkBlockNumber). A change starts
    /// a new epoch: the shadow registry is cleared (marks *and* values - the
    /// old base's derivations must not be replayed onto the moved base) and
    /// the product re-creates its positions from its own journal.
    #[serde(default)]
    pub epoch_pin: Option<u64>,
    /// The devnet's clock offset (seconds ahead of mainnet, `/admin/time`).
    /// Persisted: T's blocks carry it, so a restart must keep it or
    /// time-dependent contracts see a clock behind their own state.
    #[serde(default)]
    pub time_offset_seconds: Option<u64>,
}

pub struct StateFile {
    path: PathBuf,
    pub data: Persisted,
}

impl StateFile {
    /// Load the cursor file ([`crate::store`]'s three answers: **absent** is
    /// first boot; an **IO fault** is not emptiness — a cursor that cannot be
    /// read must be fixed, not silently re-derived; **corrupt** bytes die
    /// here, with the path named, because a silently defaulted cursor would
    /// re-run or skip blocks with no trace of why).
    pub fn open(path: PathBuf) -> Result<Self> {
        let data = store::load(&path, crate::CURSOR_ARTIFACT)?.unwrap_or_default();
        Ok(Self { path, data })
    }

    pub fn save(&self) -> Result<()> {
        store::save(&self.path, &self.data, crate::CURSOR_ARTIFACT)
    }
}

/// The state file's named accessors — one definition per field, on the shared
/// handle itself: every read and write of the cursor, the follow markers, the
/// epoch pin and the clock offset goes through them (the engines' contexts,
/// the reset detectors and the admin surface alike). The field names are
/// serde's (`devnet` is the chain's cursor on both profiles); the accessors
/// carry the meaning.
impl crate::shared::Shared<StateFile> {
    /// The devnet chain's cursor (T-height on the mainnet — one block per
    /// applied parent — V's on the venue).
    pub fn cursor(&self) -> Result<Option<ChainCursor>> {
        Ok(self.lock()?.data.devnet.clone())
    }

    pub fn set_cursor(&self, cursor: ChainCursor) -> Result<()> {
        self.lock()?.data.devnet = Some(cursor);
        Ok(())
    }

    /// Forget the cursor (a re-pin/rebase: the chain instance moved, the old
    /// height means nothing on the new base).
    pub fn clear_cursor(&self) -> Result<()> {
        self.lock()?.data.devnet = None;
        Ok(())
    }

    /// The highest source block composed onto the devnet (absent until the
    /// first anchor).
    pub fn source_last_block(&self) -> Result<Option<u64>> {
        Ok(self.lock()?.data.source_last_block)
    }

    /// That block's hash (the reorg-continuity anchor).
    pub fn source_last_hash(&self) -> Result<Option<B256>> {
        Ok(self.lock()?.data.source_last_hash)
    }

    /// Record the source block the devnet now mirrors. Height and hash are one
    /// anchor (the reorg-continuity check reads the pair; a reorg clears it
    /// whole), so they are written together — never one without the other.
    pub fn set_source_last(&self, n: u64, hash: B256) -> Result<()> {
        let mut state = self.lock()?;
        state.data.source_last_block = Some(n);
        state.data.source_last_hash = Some(hash);
        Ok(())
    }

    /// Forget the follow-marker pair (a detected reorg: the recorded source
    /// block no longer belongs to this chain's branch).
    pub fn clear_source_last(&self) -> Result<()> {
        let mut state = self.lock()?;
        state.data.source_last_hash = None;
        state.data.source_last_block = None;
        Ok(())
    }

    /// The base the registry was built against (anvil's `forkBlockNumber`): a
    /// change is a new epoch.
    pub fn epoch_pin(&self) -> Result<Option<u64>> {
        Ok(self.lock()?.data.epoch_pin)
    }

    pub fn set_epoch_pin(&self, pin: Option<u64>) -> Result<()> {
        self.lock()?.data.epoch_pin = pin;
        Ok(())
    }

    /// The devnet's clock offset (seconds ahead of the parent, persisted).
    pub fn set_time_offset_seconds(&self, seconds: u64) -> Result<()> {
        self.lock()?.data.time_offset_seconds = Some(seconds);
        Ok(())
    }
}

/// Convenience: build a `ChainCursor` from a block (the block's own number and
/// hash, plus the epoch's first processed block).
pub fn cursor_from(block: &alloy_rpc_types_eth::Block, first_block: u64) -> ChainCursor {
    ChainCursor {
        block: block.header.number,
        hash: block.header.hash,
        first_block,
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
            hash: B256::with_last_byte(1),
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
