//! The devnet's own blocks, handled before the parent's diffs land: mine the
//! pending txs in their own block (the gas-estimate safeguard), mark their
//! writes devnet-owned (the COW filter's input), and walk them with the
//! shared scanner.
use crate::ctx::Ctx;
use crate::error::Result;
use mirror_anvil::Rpc;
use mirror_devnet::engine::Engine as _;
use std::sync::atomic::Ordering;

/// Step 0 of one apply: the prepend rule's mine(s), then mark the devnet's
/// own writes — the COW filter reads the marks, and the parent's diff must
/// not overwrite the devnet's state.
pub(super) async fn prepend_and_mark(ctx: &Ctx, n: u64) -> Result<()> {
    if let Some(rounds) = ctx.mirror.policies().prepend_rounds {
        prepend_devnet_txs(&ctx.anvil_t, n, rounds).await?;
    }
    mark_own_writes(ctx).await
}

/// Prepend rule (plan, "Prepend rules"): mine the devnet's pending txs in
/// their own block *before* this parent's diffs land. Then the parent's block
/// on T carries only the parent's effects, and a devnet tx executes against
/// the same state its gas was estimated on (measured: anvil's auto-gas for
/// `eth_sendTransaction` came out ~5% short when the diffs landed between
/// estimate and execution, so the tx ran out of gas). Bounded loop: a tx that
/// arrives *while* this apply is writing diffs would otherwise be mined
/// together with them, executing against a state its gas was never estimated
/// on (measured: an Aave borrow estimated pre-diff came out ~5% short and
/// reverted). Mine until the pool drains, at most 3 rounds so a steady stream
/// cannot starve the parent block.
async fn prepend_devnet_txs(anvil_t: &Rpc, n: u64, rounds_max: u8) -> Result<()> {
    let mut rounds = 0;
    while rounds < rounds_max {
        let status = match anvil_t.txpool_status().await {
            Ok(status) => status,
            Err(e) => {
                // The prepend is a gas-estimate safeguard; a txpool fault skips
                // it (named here, bounded: the next apply tries again).
                tracing::debug!(block = n, error = %e, "prepend skipped: txpool unavailable");
                return Ok(());
            }
        };
        if status.pending == 0 {
            return Ok(());
        }
        rounds += 1;
        let last = anvil_t
            .block_by_number(anvil_t.block_number().await?, false)
            .await?;
        if let Err(e) = anvil_t
            .anvil_set_next_block_timestamp(last.header.timestamp + 1)
            .await
        {
            // The prepend block's timestamp drifts to anvil's default; the
            // parent's own context alignment below still sets the *applied*
            // block's timestamp, so this is a named, bounded degradation.
            tracing::debug!(block = n, error = %e, "prepend timestamp not set");
        }
        anvil_t.anvil_mine(1).await?;
        tracing::info!(
            block = n,
            pending = status.pending,
            round = rounds,
            "mined devnet txs before the parent diffs"
        );
    }
    Ok(())
}

/// Mark the devnet's own writes *before* the parent's are filtered. The scan
/// used to run only after `write_and_mine`, which left a hole with two shapes:
/// a devnet tx the prepend mined in *this* pass is written before the parent's
/// writes are applied with the *previous* pass's marks — deterministic
/// clobbering when the parent block wrote the same key (a pool's `slot0` is
/// written by essentially every block); and a block the harness mined
/// out-of-band waits for the shadow loop's next tick, so a parent apply
/// landing in that window clobbers it too. A scan fault holds the apply —
/// writing on stale marks is exactly the clobber this prevents.
async fn mark_own_writes(ctx: &Ctx) -> Result<()> {
    let watermark = ctx.plane.shadow_scanned_upto.load(Ordering::Relaxed);
    scan_own_blocks(ctx, watermark).await
}

/// Scan T's blocks from `from` up to the current head (the apply's own devnet
/// blocks included). Anchored at the pin like every scan: a re-pin mid-apply
/// must not walk the source's blocks.
pub(super) async fn scan_own_blocks(ctx: &Ctx, from: u64) -> Result<()> {
    let now = ctx.anvil_t.block_number().await?;
    let pin = ctx.anvil_t.fork_block().await?.unwrap_or(0);
    let from = mirror_devnet::scan::scan_anchor(from, pin);
    let walk = mirror_devnet::scan::scan_range(&ctx.plane, &ctx.anvil_t, from, now).await?;
    ctx.record_scan(&walk);
    Ok(())
}
