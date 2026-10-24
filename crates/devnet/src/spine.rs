//! The spine's divergence seams: the five decisions that may differ per
//! profile, hidden so the pipeline itself has none.
//!
//! 1. the diff source ([`crate::diff::DiffComposer`]);
//! 2. the reset signal ([`ResetDetector`]: cursor hash on both anvils, the
//!    venue's fork-instance canary lands here too);
//! 3. the clock rule ([`ClockPolicy`]);
//! 4. block production ([`MinePolicy`]);
//! 5. probe cadence ([`ProbeSchedule`]).
//!
//! The policies are data (the profile's config maps to them); the detector is
//! a trait because it performs reads.
use crate::error::Result;
use crate::source::Fut;
use crate::state::ChainCursor;
use mirror_anvil::Rpc;

/// What a reset check saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetSignal {
    /// The cursor still belongs to this chain instance.
    Steady,
    /// Reconcile before any write (the epoch rule owns what happens next).
    Moved {
        expected: Option<u64>,
        found: Option<u64>,
    },
}

/// How a chain reset is detected.
///
/// `CursorHash`: the cursor's block must carry its recorded hash, and a cursor
/// below the fork base is stale by position (the base proves it — the hash
/// read would forward a pre-pin request to the fork source). `Canary`: the
/// venue's one storage word must read back the recorded pin (heights below a
/// fork's pin resolve to the source, so hashes are unsound there — ADR 0002).
pub trait ResetDetector: Send + Sync {
    fn detect<'a>(
        &'a self,
        target: &'a Rpc,
        cursor: &'a ChainCursor,
    ) -> Fut<'a, Result<ResetSignal>>;
}

/// The cursor-hash detector (both anvils; the venue adds its canary beside it).
pub struct CursorHash;

/// Is the cursor below the chain's fork base? A re-pin moves the base past the
/// cursor, so the cursor points at blocks the current chain never carried —
/// a stale derivation by position, no read needed. `cursor == base` is the
/// legitimate rest state right after a rebase and must not fire.
pub fn below_fork_base(cursor_block: u64, base: Option<u64>) -> bool {
    base.is_some_and(|base| cursor_block < base)
}

impl ResetDetector for CursorHash {
    fn detect<'a>(
        &'a self,
        target: &'a Rpc,
        cursor: &'a ChainCursor,
    ) -> Fut<'a, Result<ResetSignal>> {
        Box::pin(async move {
            let base = target.fork_block().await?;
            if below_fork_base(cursor.block, base) {
                return Ok(ResetSignal::Moved {
                    expected: base,
                    found: Some(cursor.block),
                });
            }
            let found = crate::scan::block_hash(target, cursor.block).await?;
            Ok(if found == Some(cursor.hash) {
                ResetSignal::Steady
            } else {
                ResetSignal::Moved {
                    expected: base,
                    found: cursor.block.into(),
                }
            })
        })
    }
}

/// The clock rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockPolicy {
    /// The applied block's context is the source's: timestamp + offset,
    /// clamped to last+1 so a deliberate jump can never go non-monotonic.
    AlignToSource { offset_seconds: u64 },
    /// The target's own clock (the venue; nothing re-aligns it).
    Native,
}

/// Who produces blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinePolicy {
    /// One target block per applied source block (T: the parent stream is the
    /// devnet's block cadence).
    PerAppliedBlock,
    /// Mine only when the devnet's own pool is non-empty (V: an idle chain
    /// stays static; the harness's mines are extra empty blocks).
    PoolDrain,
}

/// When probes run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeSchedule {
    /// After every apply (T: reth can serve the applied block).
    PerBlock,
    /// Only while the pin is the source's head (V after a re-pin; the public
    /// source serves its latest state, so an older pin cannot be compared).
    AtPin,
    /// After every apply, against the composer's own cached source values
    /// (V live: the poll cache is the expected side).
    LiveParity,
}

/// The profile's decisions, as data — the only per-profile behavior.
#[derive(Debug, Clone, Copy)]
pub struct Policies {
    pub clock: ClockPolicy,
    pub mine: MinePolicy,
    pub probes: ProbeSchedule,
    /// Rounds of prepend (devnet txs mined before the source's writes land):
    /// 3 on T (a steady stream must not starve the parent block), 1 on V.
    pub prepend_rounds: u8,
    /// Pause when the follow head runs this far ahead; `None` = no guard.
    pub gap: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::below_fork_base;

    /// The cursor-below-base rule: a re-pin moves the base past the cursor,
    /// which makes the cursor a stale derivation by position — no hash read
    /// (the read would forward a pre-pin request to the fork source, where the
    /// public RPCs time out; measured: a re-pin while the engine ran left the
    /// rebase unfired). `cursor == base` is the rest state right after a
    /// rebase and must not fire.
    #[test]
    fn below_fork_base_is_strict() {
        assert!(below_fork_base(99, Some(100)), "a re-pin moved the base past the cursor");
        assert!(!below_fork_base(100, Some(100)), "cursor at the base: the rest state");
        assert!(!below_fork_base(101, Some(100)), "cursor above the base");
        assert!(!below_fork_base(99, None), "a real chain has no base");
    }
}
