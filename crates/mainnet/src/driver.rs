//! The applier driver: T's lifecycle from boot to the steady state.
//!
//! T is a fork-mode anvil (chain id 1). The driver's jobs: the boot asserts
//! (wait, the chain identity, the epoch anchor, the capability audit), the
//! devnet applier loop (one T block per applied parent block), the shadow
//! scanner, the source-sync supervisor, and the on-demand probe run. The
//! cursor/rebase/gap/reorg rules live here; the epoch rule itself is shared
//! (`diorama_devnet::epoch`).
use crate::ctx::Ctx;
use crate::apply::step;
use crate::error::Result;
use crate::source_sync;
use diorama_anvil::Rpc;
use diorama_devnet::engine::Engine as _;
use diorama_devnet::metrics::Registry as _;
use diorama_devnet::spine::ResetSignal;
use diorama_devnet::state::{self, ChainCursor};
use std::sync::Arc;
use std::time::Duration;

/// Ethereum mainnet: what both anvils must fork (a code fact).
const MAINNET_CHAIN_ID: u64 = 1;

/// T's name in every fault it raises (one spelling; the venue's is `anvil-v`).
const ANVIL: &str = "anvil-t";

/// The boot wait (see `boot`): reth unwinding and the anvils re-forking both
/// take minutes on a bad source day, and a crash-loop is worse than a slow
/// boot.
const BOOT_TIMEOUT: Duration = Duration::from_secs(900);

/// Boot: wait for T, assert the chain, anchor the epoch pin, audit the
/// declared capabilities. A boot fault is fatal (the bin exits): an applier
/// over the wrong chain or an unanchored epoch must not serve.
pub(crate) async fn boot(ctx: &Ctx) -> Result<()> {
    // The shared prefix (the same call the venue's boot reads): the chain
    // assert, then the capability audit — read-only, so the epoch anchor
    // below is still the first *write*.
    diorama_devnet::engine::boot_prefix(ctx, ANVIL, MAINNET_CHAIN_ID, BOOT_TIMEOUT, "reth").await?;
    // The epoch pin is anchored before any writes exist (a fresh registry's
    // epoch *is* the current fork base; without the record a later same-pin
    // restart would be an unprovable epoch and the rule would have to fail
    // closed, dropping the devnet's writes).
    let t_pin = ctx.anvil_t.fork_block().await?;
    diorama_devnet::epoch::anchor(&ctx.plane, t_pin)?;
    Ok(())
}

/// The pass's next move: the parent block to apply, or idle (the reason is
/// logged at its own site: at the follow head, gap-paused, or the head read
/// failing).
enum Next {
    Apply(u64),
    Idle,
}

/// The next parent block to apply, or `None`. Applies the cursor, rebase,
/// gap, and reorg rules.
async fn next_block(ctx: &Ctx) -> Result<Next> {
    let Some(follow) = ctx.follow_head().await? else {
        tracing::debug!("follow head unavailable");
        return Ok(Next::Idle);
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
    // The cursor's gauges, refreshed per pass like the venue's
    // (`record_cursor_metrics`): the apply path sets them too, but a paused
    // applier (gap) would otherwise leave `diorama_cursor_block` at 0 across a
    // restart and the computed `diorama_lag_blocks` would read the whole
    // follow head (measured: 26,264,761 after a restart with the applier
    // paused, with reth's real lag ~3k).
    record_cursor_metrics(&ctx.mainnet_metrics, ctx.plane.state.cursor()?.as_ref(), head);
    if let Ok(Some(fork)) = ctx.anvil_t.fork_block().await {
        ctx.plane
            .metrics
            .set(&ctx.mainnet_metrics.t_fork_age_blocks, head.saturating_sub(fork));
    }

    let cursor = validate_cursor(ctx, head).await?;
    let (start, start_kind) = resume_point(&ctx.anvil_t, cursor.as_ref(), head).await?;
    if start_kind == state::Start::Rebased {
        on_rebase(&ctx.anvil_t, head, follow, start).await?;
    }
    if start > follow {
        tracing::debug!(head, follow, start, "chain is at the follow head");
        return Ok(Next::Idle);
    }
    let gap = follow.saturating_sub(start);
    if !gap_within_limit(gap, ctx.mirror.policies().gap, start_kind) {
        // One record per pause (the venue's guard fires on the same edge).
        if ctx.plane.gap_pause_started() {
            tracing::error!(
                gap,
                start = ?start_kind,
                "sync gap too large - paused; run the re-pin drill (make repin)"
            );
        }
        return Ok(Next::Idle);
    }
    if ctx.plane.gap_pause_cleared() {
        tracing::info!(gap, "sync gap cleared; the applier resumes");
    }
    reorg_continuity(ctx, start).await?;
    Ok(Next::Apply(start))
}

/// The cursor's gauges: the cursor height and the pin-age reading (blocks
/// since the epoch's first). Per pass, so a paused applier still reports the
/// real cursor; the venue's `record_cursor_metrics` is the same shape.
fn record_cursor_metrics(
    metrics: &crate::metrics::MainnetMetrics,
    cursor: Option<&ChainCursor>,
    head: u64,
) {
    let Some(c) = cursor else { return };
    metrics.set(&metrics.cursor, c.block);
    metrics.set(
        &metrics.pin_age_blocks,
        head.saturating_sub(c.first_block) + 1,
    );
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
    if matches!(
        ctx.plane.witness_reset(ctx.mirror.label(), signal),
        diorama_devnet::plane::Witnessed::Steady
    ) {
        return Ok(None);
    }
    rebase(ctx, head).await?;
    Ok(None)
}

/// The invalid cursor's consequences, one step per effect: T's epoch rule (a
/// fresh pin drops marks and values, the same pin re-applies the recorded
/// writes), the reset counter, and the persisted cursor's drop (`first_block`
/// is an epoch marker; the next processed block sets a fresh one, or
/// `diorama_pin_age_blocks` counts across epochs).
async fn rebase(ctx: &Ctx, head: u64) -> Result<()> {
    let pin = ctx.anvil_t.fork_block().await?;
    ctx.mainnet_metrics.inc(&ctx.mainnet_metrics.epochs_total);
    ctx.epoch_rule(pin).await?;
    ctx.plane.state.clear_cursor()?;
    ctx.plane.save_state()?;
    ctx.plane.publish_epoch_changed(ctx.mirror.label(), pin);
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
async fn resume_point(
    anvil_t: &Rpc,
    cursor: Option<&ChainCursor>,
    head: u64,
) -> Result<(u64, state::Start)> {
    let reset_pin = if cursor.is_none() {
        anvil_t.fork_block().await?
    } else {
        None
    };
    Ok(match reset_pin {
        Some(pin) => (pin + 1, state::Start::Rebased),
        None => state::next_block(cursor, head),
    })
}

/// A rebase means an anvil restart (re-pin, OOM) happened, which resets
/// automine to its default (on). Re-assert it: otherwise each raw tx mines its
/// own block and the engine's mine(1) produces empty blocks. (The epoch
/// decision - clear vs restore - lives with the reset handling above, where
/// the fork pin is compared before anything is written.)
async fn on_rebase(rpc: &diorama_anvil::Rpc, head: u64, follow: u64, start: u64) -> Result<()> {
    if let Err(e) = rpc.anvil_set_automine(diorama_anvil::Automine::Off).await {
        tracing::warn!(error = %e, "could not re-assert automine=false");
    }
    tracing::info!(head, follow, start, "starting chain at block");
    Ok(())
}

/// Two regimes: normal operation pauses at the profile's gap
/// (`max_gap_blocks`; 300 = the "> 300 -> pause and re-sync" rule), while a
/// *rebased* cursor (a fresh pin after a restart/re-fork) is allowed to replay
/// its whole pin window - bounded by reth's state window (10,064), the plan's
/// own limit.
fn gap_within_limit(gap: u64, limit: Option<u64>, start_kind: state::Start) -> bool {
    let limit = if start_kind == state::Start::Rebased {
        10_064
    } else {
        match limit {
            Some(limit) => limit,
            None => return true,
        }
    };
    gap <= limit
}

/// Upstream reorg continuity: the parent hash of `start` must follow the last
/// processed block. On a mismatch the counter moves, the continuity marker is
/// cleared (so the next pass proceeds from the new branch), and the applier
/// continues: its writes are absolute post-values, so keys mainnet writes
/// again re-converge; orphan-only keys stay until their next mainnet write
/// (the tradeoff a single-Anvil mirror accepts, and the reason
/// `diorama_reorgs_detected_total` is an alarm).
async fn reorg_continuity(ctx: &Ctx, start: u64) -> Result<()> {
    let (Some(last_hash), Some(last_block)) = (ctx.plane.state.source_last_hash()?, ctx.plane.state.source_last_block()?) else {
        return Ok(());
    };
    if start > 1 && start - 1 == last_block {
        if let Some(prev) = diorama_devnet::scan::block_hash(&ctx.reth, start - 1).await? {
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
    let poll = ctx.engine.loops.poll();
    let mut last_status = 0u64;
    let via = diorama_devnet::engine::source_host(ctx.reth.url());
    loop {
        // The follow's heartbeat (a minute apart, the same shape on both
        // mirrors): the steady DEBUG line only speaks for an idle chain.
        diorama_devnet::engine::FollowStatus {
            via,
            source_head: ctx.mainnet_metrics.reth_head.load(std::sync::atomic::Ordering::Relaxed),
            followed: ctx.mainnet_metrics.cursor.load(std::sync::atomic::Ordering::Relaxed),
            applied: ctx
                .mainnet_metrics
                .blocks_applied
                .load(std::sync::atomic::Ordering::Relaxed),
            guard: diorama_devnet::engine::GapGuard::of(
                ctx.plane.gap_paused.load(std::sync::atomic::Ordering::Relaxed),
            ),
        }
        .log_if_due(ctx.mirror.label(), &mut last_status, 60_000);
        if diorama_devnet::engine::halt_gate(&*ctx, poll).await {
            continue;
        }
        match next_block(&ctx).await {
            Ok(Next::Apply(n)) => {
                if let Err(e) = step(&ctx, n).await {
                    tracing::error!(block = n, error = %e, "diff apply failed (will retry)");
                    tokio::time::sleep(poll * 5).await;
                }
            }
            Ok(Next::Idle) => tokio::time::sleep(poll).await,
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
    let poll = ctx.engine.loops.poll();
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
    let walk = diorama_devnet::scan::scan_to(&ctx.plane, &ctx.anvil_t, pin, t_head, t_head).await?;
    ctx.record_scan(&walk);
    diorama_devnet::scan::publish(&ctx.plane, ctx.mirror.label(), &walk);
    Ok(())
}

/// Run the configured probes now: T at the applied cursor vs reth at the
/// cursor — the same assertion the applier makes per block, on demand. The
/// per-block path owns the `diorama_t_convergence_*` counters; this run reports
/// its own outcome and leaves them alone. The venue's `/admin/probes` is the
/// same verb over its at-pin probe set.
pub(crate) async fn probes_now(ctx: &Ctx) -> Result<diorama_devnet::probes::ProbeOutcome> {
    let Some(cursor) = ctx.plane.state.cursor()? else {
        tracing::warn!("on-demand probes skipped: no cursor yet");
        return Ok(diorama_devnet::probes::ProbeOutcome::skipped(
            ctx.engine.verifier.probes.len(),
        ));
    };
    let probes = diorama_devnet::probes::run_at(ctx, cursor.block).await?;
    diorama_devnet::probes::report(&ctx.plane, ctx.mirror.label(), cursor.block, &probes);
    Ok(probes)
}

#[cfg(test)]
mod tests {
    use super::{gap_within_limit, state};

    /// The gap guard: a configured limit applies, a rebased cursor gets the
    /// plan's 10,064-block replay bound instead, and no limit means no guard
    /// (the venue's `gap_within_limit` test pins its own column).
    #[test]
    fn the_gap_guard_honors_its_limit() {
        assert!(gap_within_limit(0, Some(32), state::Start::Contiguous));
        assert!(gap_within_limit(32, Some(32), state::Start::Contiguous));
        assert!(!gap_within_limit(33, Some(32), state::Start::Contiguous));
        assert!(gap_within_limit(10_064, None, state::Start::Rebased));
        assert!(!gap_within_limit(10_065, None, state::Start::Rebased));
        assert!(gap_within_limit(u64::MAX, None, state::Start::Contiguous));
    }
}
