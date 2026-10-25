//! The engine's loop machinery: the devnet applier loop (one T block per
//! applied parent block), the shadow scanner, the source-sync supervisor, and
//! the on-demand probe run.
//!
//! One Anvil, one parent stream. The cursor/rebase/gap/reorg rules live here;
//! the epoch rule itself is shared (`mirror_devnet::epoch`).
use crate::ctx::Ctx;
use crate::apply::apply_block;
use crate::error::Result;
use crate::source_sync;
use mirror_anvil::Rpc;
use mirror_devnet::engine::Engine as _;
use mirror_devnet::facts::{Fact, ResetWitness};
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::spine::ResetSignal;
use mirror_devnet::state::{self, ChainCursor};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// The next parent block to apply, or `None`. Applies the cursor, rebase,
/// gap, and reorg rules.
async fn next_block(ctx: &Ctx) -> Result<Option<u64>> {
    let Some(follow) = ctx.follow_head().await? else {
        tracing::debug!("follow head unavailable");
        return Ok(None);
    };
    ctx.mainnet_metrics.set(&ctx.mainnet_metrics.reth_follow_head, follow);
    // Schedule a small margin behind the head: the applier mines one T block
    // per applied parent block, so the follow head *is* the devnet's cadence.
    let follow = follow.saturating_sub(ctx.l1.follow_margin_blocks);
    // Metrics-only read: a reth blip must not fail the poll (the gauge reads 0
    // until reth answers again; the next pass overwrites it).
    let reth_head = ctx.reth.block_number().await.unwrap_or(0);
    ctx.mainnet_metrics.set(&ctx.mainnet_metrics.reth_head, reth_head);

    let head = ctx.anvil_t.block_number().await?;
    ctx.mainnet_metrics.set(&ctx.mainnet_metrics.head, head);
    if let Ok(info) = ctx.anvil_t.anvil_node_info().await {
        if let Some(fork) = info.fork_config.fork_block_number {
            ctx.plane
                .metrics
                .set(&ctx.mainnet_metrics.t_fork_age_blocks, head.saturating_sub(fork));
        }
    }

    let cursor = validate_cursor(ctx, head).await?;
    let (start, rebased) = resume_point(&ctx.anvil_t, cursor.as_ref(), head).await?;
    if rebased {
        on_rebase(&ctx.anvil_t, head, follow, start).await?;
    }
    if start > follow {
        tracing::debug!(head, follow, start, "chain is at the follow head");
        return Ok(None);
    }
    if !gap_within_limit(follow.saturating_sub(start), ctx.mirror.policies().gap, rebased) {
        return Ok(None);
    }
    reorg_continuity(ctx, start).await?;
    Ok(Some(start))
}

/// Validate the cursor against T; a mismatch is a reset (re-pin / wiped
/// state): the epoch rule reconciles and the cursor rebases. Returns the
/// cursor when it is still valid, `None` after a rebase.
async fn validate_cursor(ctx: &Ctx, head: u64) -> Result<Option<ChainCursor>> {
    let Some(c) = ctx.plane.state.cursor()? else {
        return Ok(None);
    };
    match ctx.mirror.reset().detect(&ctx.anvil_t).await? {
        ResetSignal::Steady => Ok(Some(c)),
        signal => on_reset(ctx, head, signal).await,
    }
}

/// A witnessed reset: say what was seen, publish the witness, and rebase.
/// All three witnesses converge here (the detector's answer decides the fact).
async fn on_reset(ctx: &Ctx, head: u64, signal: ResetSignal) -> Result<Option<ChainCursor>> {
    let Some(witness) = witness_for(signal) else {
        return Ok(None);
    };
    ctx.plane.facts.publish(Fact::ResetDetected {
        chain: ctx.mirror.label(),
        witness,
    });
    rebase(ctx, head).await?;
    Ok(None)
}

/// The detector's signal as a witness, each with the log line that names what
/// was seen (`Steady` = no reset at all: `None`).
fn witness_for(signal: ResetSignal) -> Option<ResetWitness> {
    match signal {
        ResetSignal::BaseMoved { base, cursor } => {
            tracing::warn!(
                ?base,
                cursor,
                "cursor below the fork base (re-pin) - reconciling"
            );
            Some(ResetWitness::Position { base, cursor })
        }
        ResetSignal::HashMismatch {
            block,
            expected,
            found,
        } => {
            tracing::warn!(
                block,
                ?expected,
                ?found,
                "cursor hash not found on chain (reset or re-pin) - reconciling"
            );
            Some(ResetWitness::Hash {
                block,
                expected,
                found,
            })
        }
        ResetSignal::PinMismatch { expected, found } => {
            // Not this detector's witness (the venue's canary); fail closed if
            // it ever appears: an instance the registry does not know is a
            // reset by definition.
            tracing::error!(
                ?expected,
                ?found,
                "pin mismatch from the cursor-hash detector - reconciling"
            );
            Some(ResetWitness::Pin { expected, found })
        }
        ResetSignal::Steady => None,
    }
}

/// The invalid cursor's consequences, one step per effect: T's epoch rule (a
/// fresh pin drops marks and values, the same pin re-applies the recorded
/// writes), the reset counter, and the persisted cursor's drop (`first_block`
/// is an epoch marker; the next processed block sets a fresh one, or
/// `mirror_pin_age_blocks` counts across epochs).
async fn rebase(ctx: &Ctx, head: u64) -> Result<()> {
    let pin = ctx
        .anvil_t
        .anvil_node_info()
        .await?
        .fork_config
        .fork_block_number;
    ctx.mainnet_metrics.inc(&ctx.mainnet_metrics.t_resets_total);
    mirror_devnet::epoch::reconcile(&ctx.plane, &ctx.anvil_t, ctx.engine.loops.chunk_size, pin)
        .await?;
    ctx.plane.state.clear_cursor()?;
    ctx.plane.save_state()?;
    ctx.plane
        .facts
        .publish(Fact::EpochChanged { chain: ctx.mirror.label(), pin });
    tracing::info!(head, ?pin, "chain rebased; the epoch rule ran");
    Ok(())
}

/// The block to resume from, plus whether this is a rebase. A reset chain
/// (re-pin / wiped state): T's state is exactly the fork pin's state, so the
/// applier resumes from the pin. Using T's own height + 1 would be wrong - T
/// mines one block per applied parent block (plus one per journal rewind), so
/// its height is not a parent height and `head + 1` can land past the follow
/// head, stalling T forever (measured). Fail-closed on the pin read: a fault
/// here must not silently pick that stalled path.
async fn resume_point(anvil_t: &Rpc, cursor: Option<&ChainCursor>, head: u64) -> Result<(u64, bool)> {
    let reset_pin = if cursor.is_none() {
        anvil_t.anvil_node_info().await?.fork_config.fork_block_number
    } else {
        None
    };
    Ok(match reset_pin {
        Some(pin) => (pin + 1, true),
        None => state::next_block(cursor, head),
    })
}

/// A rebase means an anvil restart (re-pin, OOM) happened, which resets
/// automine to its default (on). Re-assert it: otherwise each raw tx mines its
/// own block and the engine's mine(1) produces empty blocks. (The epoch
/// decision - clear vs restore - lives with the reset handling above, where
/// the fork pin is compared before anything is written.)
async fn on_rebase(rpc: &mirror_anvil::Rpc, head: u64, follow: u64, start: u64) -> Result<()> {
    if let Err(e) = rpc.anvil_set_automine(false).await {
        tracing::warn!(error = %e, "could not re-assert automine=false");
    }
    tracing::info!(head, follow, start, "starting chain at block");
    Ok(())
}

/// Two regimes (plan §3 ops): normal operation pauses at the profile's gap
/// (`max_gap_blocks`; 300 = the plan's "> 300 -> pause and re-sync"), while a
/// *rebased* cursor (a fresh pin after a restart/re-fork) is allowed to replay
/// its whole pin window - bounded by reth's state window (10,064), the plan's
/// own limit.
fn gap_within_limit(gap: u64, limit: Option<u64>, rebased: bool) -> bool {
    let limit = if rebased {
        10_064
    } else {
        match limit {
            Some(limit) => limit,
            None => return true,
        }
    };
    if gap > limit {
        tracing::error!(
            gap,
            max = limit,
            rebased,
            "sync gap too large - paused; run the re-pin drill (make repin)"
        );
        return false;
    }
    true
}

/// Upstream reorg continuity: the parent hash of `start` must follow the last
/// processed block. On a mismatch the counter moves, the continuity marker is
/// cleared (so the next pass proceeds from the new branch), and the applier
/// continues: its writes are absolute post-values, so keys mainnet writes
/// again re-converge; orphan-only keys stay until their next mainnet write
/// (the tradeoff a single-Anvil mirror accepts, and the reason
/// `mirror_reorgs_detected_total` is an alarm).
async fn reorg_continuity(ctx: &Ctx, start: u64) -> Result<()> {
    let (Some(last_hash), Some(last_block)) = (ctx.plane.state.source_last_hash()?, ctx.plane.state.source_last_block()?) else {
        return Ok(());
    };
    if start > 1 && start - 1 == last_block {
        if let Some(prev) = mirror_devnet::scan::block_hash(&ctx.reth, start - 1).await? {
            if prev != last_hash {
                ctx.mainnet_metrics.inc(&ctx.mainnet_metrics.reorgs_detected);
                tracing::warn!(
                    block = start - 1,
                    "reth reorg detected; applied diffs self-heal on the new branch"
                );
                ctx.plane.state.clear_source_last()?;
                ctx.plane.save_state()?;
            }
        }
    }
    Ok(())
}

pub(crate) async fn apply_loop(ctx: Arc<Ctx>) {
    let poll = Duration::from_millis(ctx.engine.loops.poll_interval_ms.max(200));
    loop {
        if ctx.plane.is_halted() {
            tokio::time::sleep(poll).await;
            continue;
        }
        match next_block(&ctx).await {
            Ok(Some(n)) => {
                if let Err(e) = apply_block(&ctx, n).await {
                    tracing::error!(block = n, error = %e, "diff apply failed (will retry)");
                    tokio::time::sleep(poll * 5).await;
                }
            }
            Ok(None) => tokio::time::sleep(poll).await,
            Err(e) => {
                tracing::warn!(error = %e, "apply loop poll failed");
                tokio::time::sleep(poll * 5).await;
            }
        }
    }
}

/// Supervises the top-up loop. A panicking top-up task is invisible from the
/// outside (reth simply stops advancing), so restart it and say so loudly.
///
/// No halt gate on purpose: the top-up is read-only catch-up for reth — a
/// paused mirror must not let the source's head fall behind the applier.
pub(crate) async fn source_sync_supervisor(ctx: Arc<Ctx>) {
    loop {
        match tokio::spawn(source_sync::source_sync_loop(ctx.clone())).await {
            Ok(()) => tracing::warn!("top-up loop exited; restarting"),
            Err(e) => tracing::error!(error = %e, "top-up loop panicked; restarting"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Watch T's own blocks: every devnet transaction's writes become shadowed
/// (copy-on-write) so parent diffs never overwrite them.
///
/// No halt gate on purpose: while the engine is paused the harness mines T by
/// hand (OPERATIONS), and those writes must still be marked — the marks are
/// exactly what keeps the next parent diffs (after resume) from clobbering
/// them.
pub(crate) async fn shadow_loop(ctx: Arc<Ctx>) {
    let poll = Duration::from_millis(ctx.engine.loops.poll_interval_ms.max(200));
    loop {
        tokio::time::sleep(poll * 2).await;
        if let Err(e) = scan_once(&ctx).await {
            tracing::warn!(error = %e, "shadow scan failed; will retry from the last clean block");
        }
        // A save fault is logged, not swallowed: the marks are in memory and
        // the next pass retries the write (bounded: one warn per poll).
        if let Err(e) = ctx.plane.save_shadow() {
            tracing::warn!(error = %e, "shadow registry save failed");
        }
    }
}

/// One shadow-scan pass: read the head and pin, walk the unscanned blocks,
/// record the walk's metrics, and publish the fact. The loop above is a sleep
/// plus this call.
async fn scan_once(ctx: &Ctx) -> Result<()> {
    let t_head = ctx.anvil_t.block_number().await?;
    // Never scan at or below T's fork base: after a re-pin those blocks
    // are the source's own (anvil forwards pre-pin ranges upstream), and
    // marking their writes devnet-owned would shadow mainnet's real state
    // (measured: 311 real txs scanned as devnet writes right after a
    // re-pin). The venue driver clamps its scan the same way.
    let pin = ctx.anvil_t.fork_block().await?.unwrap_or(0);
    // A fresh mark starts at the head: do not scan pre-existing history
    // (the registry was persisted; T does not advance while the engine is
    // down). The applier's inline scan shares this high-water mark, so its
    // blocks are not re-traced.
    let walk = mirror_devnet::scan::scan_to(&ctx.plane, &ctx.anvil_t, pin, t_head, t_head).await?;
    ctx.record_scan(&walk);
    mirror_devnet::scan::publish(&ctx.plane, ctx.mirror.label(), &walk);
    Ok(())
}

/// Run the configured probes now: T at the applied cursor vs reth at the
/// cursor — the same assertion the applier makes per block, on demand. The
/// per-block path owns the `mirror_t_convergence_*` counters; this run reports
/// its own outcome and leaves them alone. The venue's `/admin/probes` is the
/// same verb over its at-pin probe set.
pub(crate) async fn probes_now(ctx: &Ctx) -> Result<mirror_devnet::probes::ProbeOutcome> {
    let Some(cursor) = ctx.plane.state.cursor()? else {
        tracing::warn!("on-demand probes skipped: no cursor yet");
        return Ok(mirror_devnet::probes::ProbeOutcome {
            skipped: ctx.engine.verifier.probes.len(),
            ..Default::default()
        });
    };
    let probes = mirror_devnet::probes::ProbeRunner::devnet(
        &ctx.engine.verifier.probes,
        &ctx.plane.shadow,
        ctx.plane.time_offset.load(Ordering::Relaxed) != 0,
    )
    .parity(&ctx.reth, cursor.block, &ctx.anvil_t)
    .await?;
    mirror_devnet::probes::report(&ctx.plane, ctx.mirror.label(), cursor.block, &probes);
    Ok(probes)
}
