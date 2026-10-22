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

/// Scan the chain's blocks in `(from, to]`, advancing the high-water mark only
/// past blocks that scanned cleanly. A trace fault stops the walk: re-scanning
/// is cheap (the marks are idempotent), while advancing past a failed block
/// would lose its devnet writes silently — and the next parent diff would then
/// overwrite the devnet's state, the exact COW failure the registry exists to
/// prevent.
pub async fn scan_range(
    target: &Rpc,
    shadow: &Shared<Shadow>,
    scanned_upto: &AtomicU64,
    from: u64,
    to: u64,
) -> Result<()> {
    for n in (from + 1)..=to {
        scan_block(target, shadow, n).await?;
        scanned_upto.fetch_max(n, Ordering::Relaxed);
    }
    Ok(())
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
