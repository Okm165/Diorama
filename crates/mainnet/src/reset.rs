//! The mainnet profile's reset detector: the cursor hash.
//!
//! The cursor's block must carry its recorded hash, and a cursor below the
//! chain's fork base is stale by position (a re-pin moved the base past it).
//! The detector is the mainnet's implementation of the spine's
//! [`ResetDetector`]: the venue's is the pin canary (in
//! `crates/venue/src/reset.rs` — ADR-0002: heights below a fork's pin resolve
//! to the source, so cursor hashes are unsound there). The divergence is
//! declared in `crates/mainnet/src/profile.rs` and this module holds its
//! mechanics.
use futures_util::future::BoxFuture;
use diorama_anvil::Rpc;
use diorama_devnet::shared::Shared;
use diorama_devnet::spine::{ResetDetector, ResetSignal};
use diorama_devnet::state::StateFile;

/// The cursor-hash detector: the cursor's block must carry its recorded hash,
/// and a cursor below the chain's fork base is stale by position.
pub struct CursorHash {
    /// The cursor state (the plane's state file) the check reads.
    state: Shared<StateFile>,
}

impl CursorHash {
    pub fn new(state: Shared<StateFile>) -> Self {
        Self { state }
    }
}

/// Is the cursor below the chain's fork base? A re-pin moves the base past the
/// cursor, so the cursor points at blocks the current chain never carried —
/// a stale derivation by position, no read needed. `cursor == base` is the
/// legitimate rest state right after a rebase and must not fire.
pub fn below_fork_base(cursor_block: u64, base: Option<u64>) -> bool {
    base.is_some_and(|base| cursor_block < base)
}

impl ResetDetector for CursorHash {
    fn detect<'a>(&'a self, target: &'a Rpc) -> BoxFuture<'a, diorama_devnet::Result<ResetSignal>> {
        Box::pin(async move {
            // No cursor: nothing to invalidate (the fresh-chain path).
            let Some(cursor) = self.state.cursor()? else {
                return Ok(ResetSignal::Steady);
            };
            let base = target.fork_block().await?;
            if below_fork_base(cursor.block, base) {
                return Ok(ResetSignal::BaseMoved {
                    base,
                    cursor: cursor.block,
                });
            }
            let found = diorama_devnet::scan::block_hash(target, cursor.block).await?;
            Ok(if found == Some(cursor.hash) {
                ResetSignal::Steady
            } else {
                ResetSignal::HashMismatch {
                    block: cursor.block,
                    expected: cursor.hash,
                    found,
                }
            })
        })
    }
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
    fn the_fork_base_rule_is_strict() {
        assert!(below_fork_base(99, Some(100)), "a re-pin moved the base past the cursor");
        assert!(!below_fork_base(100, Some(100)), "cursor at the base: the rest state");
        assert!(!below_fork_base(101, Some(100)), "cursor above the base");
        assert!(!below_fork_base(99, None), "a real chain has no base");
    }
}
