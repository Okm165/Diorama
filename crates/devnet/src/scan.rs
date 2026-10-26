//! The shadow scanner: every devnet transaction's writes become shadowed
//! (copy-on-write), so parent diffs never overwrite them.
//!
//! Chain-generic by design: the mainnet profile scans T, the venue profile
//! scans V — same code, different anvil client. The high-water mark lives with
//! the caller (it shares it with its inline scans).
use crate::error::{Error, Result};
use crate::plane::Plane;
use crate::shadow::Shadow;
use crate::shared::Shared;
use alloy_consensus::Transaction as _;
use alloy_network_primitives::TransactionResponse as _;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::{Block, Transaction};
use mirror_anvil::Rpc;
use std::sync::atomic::Ordering;

/// Transactions of a full block (the scanner always requests full=true).
pub fn block_txs(block: &Block) -> &[Transaction] {
    block.transactions.as_transactions().unwrap_or(&[])
}

/// The scan's anchor: the watermark, never below the fork pin (the pin's own
/// block is the source's state; the devnet's blocks start at pin + 1). After a
/// re-pin the watermark can sit below the new base — scanning there would read
/// the *source's* real blocks (anvil forwards pre-pin ranges upstream) and mark
/// mainnet's writes devnet-owned (measured: 311 real txs scanned as devnet
/// writes after a re-pin). `0` = no pin (a real chain): the watermark stands.
pub fn scan_anchor(watermark: u64, pin: u64) -> u64 {
    watermark.max(pin)
}

/// Scan the chain's blocks in `(from, to]`, advancing the high-water mark only
/// past blocks that scanned cleanly. A trace fault stops the walk: re-scanning
/// is cheap (the marks are idempotent), while advancing past a failed block
/// would lose its devnet writes silently — and the next parent diff would then
/// overwrite the devnet's state, the exact COW failure the registry exists to
/// prevent.
///
/// Returns the walk's counts — the scan-cost reading both profiles count. An
/// empty block is one call and no traces, and the harness mines one block per
/// poll, so the empty share is what the counters are for (plan §17.4).
pub async fn scan_range(plane: &Plane, target: &Rpc, from: u64, to: u64) -> Result<ScanWalk> {
    let mut walk = ScanWalk {
        walked: 0,
        empty: 0,
        shadowed: 0,
    };
    let mut pruned = 0u64;
    let mut n = from + 1;
    while n <= to {
        let scanned = scan_block(target, &plane.shadow, n).await?;
        if matches!(scanned, BlockScan::Pruned) {
            // The prefix is beyond anvil's history window. Walking it costs an
            // RPC round trip per block (measured: an 8k-block lag meant hours
            // of walking, with the *newer* devnet writes — the ones COW must
            // protect — still unmarked). Find the window's edge once and
            // resume there; the skipped blocks are named in the summary below.
            let edge = find_history_edge(target, n, to).await?;
            pruned += edge - n;
            plane
                .shadow_scanned_upto
                .fetch_max(edge.saturating_sub(1), Ordering::Relaxed);
            if edge > to {
                break;
            }
            n = edge;
            continue;
        }
        match scanned {
            BlockScan::Marked { writes } => {
                if writes == 0 {
                    walk.empty += 1;
                }
                walk.shadowed += writes as u64;
            }
            // The pruned arm is handled above (it continues the walk).
            BlockScan::Pruned => {}
        }
        plane.shadow_scanned_upto.fetch_max(n, Ordering::Relaxed);
        walk.walked += 1;
        n += 1;
    }
    if pruned > 0 {
        // One summary per walk: a stale watermark after a restart can sit
        // thousands of blocks behind anvil's `--prune-history` window
        // (measured: one block re-logged 150 times in five minutes while the
        // scan starved). The blocks are named and skipped — their writes
        // cannot be recovered, and the watermark must advance past them.
        tracing::warn!(
            pruned,
            from = from + 1,
            to,
            "shadow scan skipped blocks older than anvil's history window"
        );
    }
    Ok(walk)
}

/// The first block after `pruned` whose traces can succeed — or `to + 1`
/// when every block up to `to` is beyond the window. A trace at `pruned`
/// already failed, so the state after `pruned - 1` is gone: the search's
/// lower bound is that predecessor. The probe is a zero-slot storage read —
/// it needs the same state replay a trace does, without a transaction to find
/// (an empty block has none), and a dozen probes cover thousands of blocks.
async fn find_history_edge(target: &Rpc, pruned: u64, to: u64) -> Result<u64> {
    let mut lo = pruned.saturating_sub(1);
    if !history_reaches(target, to).await? {
        return Ok(to + 1);
    }
    let mut hi = to;
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if history_reaches(target, mid).await? {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Ok(hi + 1)
}

/// Whether anvil can replay block `n`'s state: the zero-slot read needs the
/// same history a tx trace does, and the pruned refusal is the classifier's
/// ([`mirror_anvil::Error::is_history_unavailable`]). Any other fault
/// propagates: the walk fails closed, exactly as on a trace fault.
async fn history_reaches(target: &Rpc, n: u64) -> Result<bool> {
    match target.storage_at(Address::ZERO, U256::ZERO, n).await {
        Ok(_) => Ok(true),
        Err(e) if e.is_history_unavailable() => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// One scan walk's counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanWalk {
    pub walked: u64,
    /// Blocks with no txs: one call, no traces.
    pub empty: u64,
    /// Devnet writes marked across the walk (the `shadow_scanned` fact's
    /// count).
    pub shadowed: u64,
}

/// Publish the `shadow_scanned` fact when the walk moved blocks (an empty
/// walk says nothing new). The one fact construction both drivers report; the
/// applier's inline scan and the hand-backs' pre-scans record the counters
/// alone (`Registry::record_scan`), because their own fact or response
/// carries the walk.
pub fn publish(plane: &Plane, chain: &'static str, walk: &ScanWalk) {
    if walk.walked > 0 {
        plane.facts.publish(crate::facts::Fact::ShadowScanned {
            chain,
            blocks: walk.walked,
            empty: walk.empty,
            shadowed: walk.shadowed,
        });
    }
}

/// The one shadow-scan walk (both profiles, both call shapes): from the
/// high-water mark — or `first` when the mark is unset (a fresh chain) —
/// anchored at the pin, up to `head`. Returns the blocks walked; the caller
/// records them in its own metrics.
///
/// `first` carries each profile's fresh-chain policy: the mainnet engine
/// starts at the head (its registry was persisted; T does not advance while
/// the engine is down), the venue starts at its cursor (V advances when
/// anyone mines, so blocks mined while the driver was down must be scanned).
/// A fresh mark is *set*, never left at 0: the policy is the watermark's
/// starting value, and leaving it unset would let the next scan re-run the
/// policy against a moved head and skip blocks written in between.
pub async fn scan_to(plane: &Plane, target: &Rpc, pin: u64, head: u64, first: u64) -> Result<ScanWalk> {
    let mark = plane.shadow_scanned_upto.load(Ordering::Relaxed);
    let from = if mark == 0 {
        let from = scan_anchor(first, pin);
        plane.shadow_scanned_upto.store(from, Ordering::Relaxed);
        from
    } else {
        scan_anchor(mark, pin)
    };
    scan_range(plane, target, from, head).await
}

/// Mark one transaction's writes: the sender's account fields (its nonce
/// moves), value recipients, and the storage writes from its trace
/// (prestateTracer diffMode — measured on anvil 1.8.6). Returns the marks
/// added; a trace fault is returned so the caller can keep the block
/// unscanned (the watermark must not advance past it).
async fn mark_tx(target: &Rpc, shadow: &Shared<Shadow>, tx: &Transaction) -> Result<usize> {
    let mut marked = 0;
    // A tx from an account stops its account-field sync (nonce -> balance too).
    shadow.lock()?.mark_account_fields(tx.from());
    marked += 1;
    // Value transfers shadow the recipient's balance.
    if tx.inner.value() > U256::ZERO {
        if let Some(to) = tx.inner.to() {
            shadow.lock()?.mark_account_fields(to);
        }
    }
    // Storage writes come from the tx trace.
    let trace = target.trace_tx_prestate(tx.tx_hash()).await?;
    for w in mirror_anvil::flatten_trace(&trace) {
        shadow.lock()?.mark(&w);
        marked += 1;
    }
    Ok(marked)
}

/// One block's scan outcome: marked writes, or a history-pruned refusal (the
/// block's writes cannot be recovered, and retrying would starve every later
/// scan — the block is named and skipped).
enum BlockScan {
    /// Writes marked (`0` = the empty-block skip the caller counts).
    Marked { writes: usize },
    /// Anvil's history window no longer reaches the block.
    Pruned,
}

/// Mark one block's writes devnet-owned: every sender's account fields, the
/// recipients of value transfers, and the storage writes from the tx traces.
/// Returns how many writes were marked; `0` is the empty-block skip the
/// caller counts.
async fn scan_block(target: &Rpc, shadow: &Shared<Shadow>, n: u64) -> Result<BlockScan> {
    let block = target.block_by_number(n, mirror_anvil::Txs::Full).await?;
    let txs = block_txs(&block);
    if txs.is_empty() {
        return Ok(BlockScan::Marked { writes: 0 });
    }
    let (shadowed, trace_failures, pruned) = mark_block(target, shadow, txs).await;
    if trace_failures > 0 {
        return Err(Error::ShadowScan {
            block: n,
            failed: trace_failures,
        });
    }
    tracing::info!(
        block = n,
        txs = txs.len(),
        shadowed_writes = shadowed,
        "scanned devnet block for shadow writes"
    );
    Ok(match pruned {
        0 => BlockScan::Marked { writes: shadowed },
        _ => BlockScan::Pruned,
    })
}

/// Mark every tx's writes, returning (marks added, traces that failed). A
/// failed trace hides its tx's writes: the block is *not* cleanly scanned and
/// the watermark must not advance past it.
async fn mark_block(
    target: &Rpc,
    shadow: &Shared<Shadow>,
    txs: &[Transaction],
) -> (usize, usize, usize) {
    let mut shadowed = 0usize;
    let mut trace_failures = 0usize;
    let mut pruned = 0usize;
    for tx in txs {
        match mark_tx(target, shadow, tx).await {
            Ok(marked) => shadowed += marked,
            // The history window no longer reaches the block: a retry can
            // never succeed (the range's summary names the count).
            Err(e) if e.is_history_unavailable() => {
                pruned += 1;
                tracing::debug!(tx = %tx.tx_hash(), "shadow trace pruned by anvil's history window");
            }
            Err(e) => {
                trace_failures += 1;
                tracing::warn!(tx = %tx.tx_hash(), error = %e, "shadow trace failed");
            }
        }
    }
    (shadowed, trace_failures, pruned)
}

/// Read a block's hash (for cursor validation).
pub async fn block_hash(rpc: &Rpc, n: u64) -> Result<Option<B256>> {
    let b = rpc.block_by_number_opt(n, mirror_anvil::Txs::Hashes).await?;
    Ok(b.map(|b| b.header.hash))
}

#[cfg(test)]
mod tests {
    use super::scan_anchor;

    /// The scan anchor never dips below the pin: a rewound or stale watermark
    /// must not walk the source's blocks (their txs are untraceable — anvil
    /// answers an unknown tx with -32601 — and pre-pin reads are the source's
    /// own state, not this devnet's).
    #[test]
    fn scan_anchor_clamps_at_the_pin() {
        assert_eq!(scan_anchor(0, 100), 100, "a fresh watermark starts at the pin");
        assert_eq!(scan_anchor(90, 100), 100, "a cursor below the pin is lifted");
        assert_eq!(scan_anchor(120, 100), 120, "a cursor above the pin is kept");
        assert_eq!(scan_anchor(7, 0), 7, "no pin: the watermark is the anchor");
    }
}
