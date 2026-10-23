//! The shadow scanner: every devnet transaction's writes become shadowed
//! (copy-on-write), so parent diffs never overwrite them.
//!
//! Chain-generic by design: the ethereum profile scans T, the venue profile
//! scans V — same code, different anvil client. The high-water mark lives with
//! the caller (it shares it with its inline scans).
use crate::error::{Error, Result};
use crate::shadow::Shadow;
use crate::shared::Shared;
use alloy_primitives::U256;
use alloy_rpc_types_eth::{Block, Transaction};
use mirror_anvil::Rpc;
use std::sync::atomic::{AtomicU64, Ordering};

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
/// Returns the number of blocks walked (an empty block costs one header read,
/// no traces — the harness mines one block per poll, so this is the scan-cost
/// reading both profiles count).
pub async fn scan_range(
    target: &Rpc,
    shadow: &Shared<Shadow>,
    scanned_upto: &AtomicU64,
    from: u64,
    to: u64,
) -> Result<u64> {
    let mut walked = 0u64;
    for n in (from + 1)..=to {
        scan_block(target, shadow, n).await?;
        scanned_upto.fetch_max(n, Ordering::Relaxed);
        walked += 1;
    }
    Ok(walked)
}

/// The one shadow-scan walk (both profiles, both call shapes): from the
/// high-water mark — or `first` when the mark is unset (a fresh chain) —
/// anchored at the pin, up to `head`. Returns the blocks walked; the caller
/// records them in its own metrics.
///
/// `first` carries each profile's fresh-chain policy: the ethereum engine
/// starts at the head (its registry was persisted; T does not advance while
/// the engine is down), the venue starts at its cursor (V advances when
/// anyone mines, so blocks mined while the driver was down must be scanned).
/// A fresh mark is *set*, never left at 0: the policy is the watermark's
/// starting value, and leaving it unset would let the next scan re-run the
/// policy against a moved head and skip blocks written in between.
pub async fn scan_to(
    target: &Rpc,
    shadow: &Shared<Shadow>,
    scanned_upto: &AtomicU64,
    pin: u64,
    head: u64,
    first: u64,
) -> Result<u64> {
    let mark = scanned_upto.load(Ordering::Relaxed);
    let from = if mark == 0 {
        let from = scan_anchor(first, pin);
        scanned_upto.store(from, Ordering::Relaxed);
        from
    } else {
        scan_anchor(mark, pin)
    };
    scan_range(target, shadow, scanned_upto, from, head).await
}

/// Mark one block's writes devnet-owned: every sender's account fields, the
/// recipients of value transfers, and the storage writes from the tx traces
/// (prestateTracer diffMode — measured on anvil 1.8.6).
pub async fn scan_block(target: &Rpc, shadow: &Shared<Shadow>, n: u64) -> Result<()> {
    let block = target.block_by_number(n, true).await?;
    let txs = block_txs(&block);
    if txs.is_empty() {
        return Ok(());
    }
    let mut shadowed = 0usize;
    let mut trace_failures = 0usize;
    for tx in txs {
        use alloy_consensus::Transaction as _;
        use alloy_network_primitives::TransactionResponse as _;
        // A tx from an account stops its account-field sync (nonce -> balance too).
        let from = tx.from();
        shadow.lock()?.mark_account_fields(from);
        shadowed += 1;
        // Value transfers shadow the recipient's balance.
        if tx.inner.value() > U256::ZERO {
            if let Some(to) = tx.inner.to() {
                shadow.lock()?.mark_account_fields(to);
            }
        }
        // Storage writes come from the tx trace.
        match target.trace_tx_prestate(tx.tx_hash()).await {
            Ok(trace) => {
                for w in mirror_anvil::flatten_trace(&trace) {
                    shadow.lock()?.mark(&w);
                    shadowed += 1;
                }
            }
            Err(e) => {
                // A tx whose trace failed hides its writes: the block is *not*
                // cleanly scanned and the watermark must not advance past it.
                trace_failures += 1;
                tracing::warn!(tx = %tx.tx_hash(), error = %e, "shadow trace failed");
            }
        }
    }
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
    Ok(())
}

/// Read a block's hash (for cursor validation).
pub async fn block_hash(rpc: &Rpc, n: u64) -> Result<Option<String>> {
    let b = rpc.block_by_number_opt(n, false).await?;
    Ok(b.map(|b| format!("0x{:x}", b.header.hash)))
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
