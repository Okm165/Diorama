//! The engine's loop machinery: the parent-stream drivers for M (replay +
//! verify) and T (BAL diffs), the shadow scanner, the source-sync supervisor,
//! and the on-demand probe run.
//!
//! Extracted from the crate root so both projects read the same way: this
//! module is the ethereum profile's counterpart of `mirror-hyperliquid`'s
//! `driver` (the fork driver over anvil V). The loops themselves are
//! source-specific — replay and BAL application have no venue counterpart —
//! and the substrate's half (shadow, journal, cursor, probes) lives in
//! `mirror-devnet`, shared.
use crate::ctx::Ctx;
use crate::diff::apply_block;
use crate::error::Result;
use crate::replay::replay_block;
use crate::source_sync;
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::state::{self, ChainCursor};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
enum Chain {
    M,
    T,
}

/// Decide the next parent block to process for one chain, applying the cursor,
/// rebase, gap, and reorg rules. `None` means "nothing to do right now".
async fn next_block_for(ctx: &Ctx, chain: Chain) -> Result<Option<u64>> {
    let Some(follow) = ctx.follow_head().await? else {
        tracing::debug!("follow head unavailable");
        return Ok(None);
    };
    ctx.eth_metrics.set(&ctx.eth_metrics.reth_follow_head, follow);
    // Schedule a small margin behind the head: the beacon cannot serve blob
    // sidecars for the newest block yet (measured: >15 s of retries on ~every
    // blob block with `follow: latest`, holding M each time). ~2 blocks of
    // extra lag is invisible; the metric keeps the raw head so
    // `mirror_lag_blocks` still measures the true distance.
    let follow = follow.saturating_sub(ctx.l1.follow_margin_blocks);
    // Metrics-only read: a reth blip must not fail the poll (the gauge reads 0
    // until reth answers again; the next pass overwrites it).
    let reth_head = ctx.reth.block_number().await.unwrap_or(0);
    ctx.eth_metrics.set(&ctx.eth_metrics.reth_head, reth_head);

    let (head, cursor, rpc) = chain_state(ctx, chain).await?;
    let cursor = validate_cursor(ctx, chain, rpc, head, cursor).await?;
    let (start, rebased) = resume_point(chain, rpc, cursor.as_ref(), head).await?;

    let forced = matches!(chain, Chain::M) && ctx.force_rebase.swap(false, Ordering::Relaxed);
    if matches!(chain, Chain::M) && (head > follow || forced) {
        rebase_m(ctx, chain, head, follow, forced).await?;
        return Ok(None);
    }
    if rebased {
        on_rebase(chain, rpc, head, follow, start).await?;
    }
    if start > follow {
        tracing::debug!(chain = ?chain, head, follow, start, "chain is at the follow head");
        return Ok(None);
    }
    if !gap_within_limit(ctx, chain, follow.saturating_sub(start), rebased) {
        return Ok(None);
    }
    if !reorg_continuity(ctx, chain, start).await? {
        return Ok(None);
    }
    Ok(Some(start))
}

/// The chain's head, cursor and RPC endpoint (the per-chain head metrics
/// included).
async fn chain_state(
    ctx: &Ctx,
    chain: Chain,
) -> Result<(u64, Option<ChainCursor>, &mirror_anvil::Rpc)> {
    let (head, cursor, rpc) = match chain {
        Chain::M => (
            ctx.anvil_m.block_number().await?,
            ctx.cursor_m()?,
            &ctx.anvil_m,
        ),
        Chain::T => (
            ctx.anvil_t.block_number().await?,
            ctx.cursor_t()?,
            &ctx.anvil_t,
        ),
    };
    match chain {
        Chain::M => ctx.eth_metrics.set(&ctx.eth_metrics.m_head, head),
        Chain::T => {
            ctx.eth_metrics.set(&ctx.eth_metrics.t_head, head);
            if let Ok(info) = rpc.anvil_node_info().await {
                if let Some(fork) = info.fork_config.fork_block_number {
                    ctx.plane.metrics
                        .set(&ctx.eth_metrics.t_fork_age_blocks, head.saturating_sub(fork));
                }
            }
        }
    }
    Ok((head, cursor, rpc))
}

/// Validate the cursor against the chain; a mismatch means the chain was reset
/// (re-pin / wiped state) and the cursor must rebase. Returns the cursor when
/// it is still valid, `None` after a rebase.
async fn validate_cursor(
    ctx: &Ctx,
    chain: Chain,
    rpc: &mirror_anvil::Rpc,
    head: u64,
    cursor: Option<ChainCursor>,
) -> Result<Option<ChainCursor>> {
    let Some(c) = cursor else {
        return Ok(None);
    };
    // The base check comes first: a cursor *below* the chain's fork base is
    // definitionally stale (a re-pin moved the base past it), and the base
    // itself proves it — while the hash read would forward a pre-pin read to
    // the fork source, which the public RPCs rate-limit or time out on
    // (measured: a re-pin while the engine ran left the rebase unfired until
    // the next restart). `cursor == base` is the legitimate rest state right
    // after a rebase.
    let below_base = below_fork_base(c.block, rpc.fork_block().await?);
    let valid = if below_base {
        false
    } else {
        mirror_devnet::scan::block_hash(rpc, c.block).await?.as_deref() == Some(c.hash.as_str())
    };
    if valid {
        return Ok(Some(c));
    }
    tracing::warn!(
        block = c.block,
        below_base,
        "cursor not found on chain (reset or re-pin) - rebasing"
    );
    if matches!(chain, Chain::T) {
        // The epoch rule (shared with the venue engine, `mirror-devnet::epoch`):
        // a fresh pin drops marks and values, the same pin re-applies the
        // recorded writes; the journal goes either way. `t_resets_total`
        // counts resets *found* (this site), so it moves even when the
        // registry was empty.
        let pin = rpc.anvil_node_info().await?.fork_config.fork_block_number;
        ctx.eth_metrics.inc(&ctx.eth_metrics.t_resets_total);
        mirror_devnet::epoch::reconcile(&ctx.plane, rpc, ctx.engine.loops.chunk_size, pin).await?;
    }
    // An M re-base skipped this range: those blocks are mainnet's own (the
    // fresh fork serves them and the state comes from the fork base, so it is
    // exact by construction), but the engine did not re-execute them - their
    // tx-level verification is skipped. Bounded by `fork_offset`; counted and
    // logged so the gap is never invisible.
    if matches!(chain, Chain::M) {
        account_m_rebase_skip(ctx, c.block, head);
    }
    // Drop it from the persisted state too: `first_block` is an epoch marker,
    // and the next processed block must set a fresh one (otherwise
    // `mirror_pin_age_blocks` counts across epochs).
    {
        let mut state = ctx.plane.state.lock()?;
        match chain {
            Chain::M => state.data.m = None,
            Chain::T => state.data.t = None,
        }
    }
    ctx.plane.save_state()?;
    Ok(None)
}

/// Is the cursor below the chain's fork base? A re-pin moves the base past the
/// cursor, so the cursor points at blocks the current chain never carried (the
/// source's own, at best) — a stale derivation by position, no read needed.
fn below_fork_base(cursor_block: u64, base: Option<u64>) -> bool {
    base.is_some_and(|base| cursor_block < base)
}

/// An M re-base skipped a range (see [`validate_cursor`]).
fn account_m_rebase_skip(ctx: &Ctx, from: u64, head: u64) {
    let skipped = head.saturating_sub(from);
    if skipped > 0 {
        ctx.eth_metrics.add(&ctx.eth_metrics.m_rebase_skipped, skipped);
        tracing::warn!(
            from,
            to = head,
            skipped,
            "M re-base skipped a range (fork base: state exact, tx verification skipped)"
        );
    }
}

/// The block to resume from, plus whether this is a rebase. A reset chain
/// (re-pin / wiped state): T's state is exactly the fork pin's state, so the
/// applier resumes from the pin. Using T's own height + 1 would be wrong - T
/// mines one block per applied parent block (plus one per journal rewind), so
/// its height is not a parent height and `head + 1` can land past the follow
/// head, stalling T forever (measured). Fail-closed on the pin read: a fault
/// here must not silently pick that stalled path.
async fn resume_point(
    chain: Chain,
    rpc: &mirror_anvil::Rpc,
    cursor: Option<&ChainCursor>,
    head: u64,
) -> Result<(u64, bool)> {
    let reset_pin = if cursor.is_none() {
        rpc.anvil_node_info()
            .await?
            .fork_config
            .fork_block_number
    } else {
        None
    };
    Ok(match (chain, reset_pin) {
        (Chain::T, Some(pin)) => (pin + 1, true),
        _ => state::next_block(cursor, head),
    })
}

/// Re-base M at a fresh pin behind the follow head.
///
/// M's replay is irreversible, but the follow head can move *backward*: the
/// CL's forkchoice may report a lower safe block (reorg / failed
/// justification), which strands M until it catches up (measured: a 42-block
/// recede stalling ~10 min, with a warning every poll). Re-base M at a fresh
/// pin behind the follow head instead; the cursor hash check on the next pass
/// rebases the replay. The blocks between the old cursor and the new pin are
/// mainnet's own (the fork serves them and the state comes from the fork base,
/// so it is exact by construction), but they are not re-executed: their
/// tx-level verification is skipped. Bounded by `fork_offset` per re-base and
/// counted in `mirror_m_rebase_skipped_blocks_total` with a WARN naming the
/// range, so the gap is never invisible. T is unaffected: the epoch/COW state
/// is keyed on T's own fork block, not M's - and T is exempt from this check
/// entirely (its heights advance independently of mainnet's).
async fn rebase_m(ctx: &Ctx, chain: Chain, head: u64, follow: u64, forced: bool) -> Result<()> {
    {
        let mut last = ctx.last_stall.lock()?;
        if *last != (head, follow) {
            *last = (head, follow);
            tracing::warn!(
                chain = ?chain,
                head,
                follow,
                forced = forced,
                "re-basing M: ahead of the follow head (reorg) or a block is unservable"
            );
        }
    }
    // At most one attempt per minute: a failed re-base retries on later polls.
    let attempt = {
        let mut t = ctx.last_rebase.lock()?;
        let due = t
            .map(|i: std::time::Instant| i.elapsed() > std::time::Duration::from_secs(60))
            .unwrap_or(true);
        if due {
            *t = Some(std::time::Instant::now());
        }
        due
    };
    if !attempt {
        return Ok(());
    }
    match ctx.anvil_m.fork_config().await {
        Ok(Some((url, _))) => {
            let pin = follow.saturating_sub(ctx.l1.fork_offset);
            match ctx.anvil_m.anvil_reset_fork(&url, pin).await {
                Ok(()) => tracing::warn!(
                    head,
                    follow,
                    pin,
                    "M re-based at a fresh pin behind the follow head"
                ),
                Err(e) => tracing::warn!(error = %e, "M re-base failed; will retry"),
            }
        }
        Ok(None) => tracing::warn!("M re-base skipped: chain has no fork config"),
        Err(e) => tracing::warn!(error = %e, "M re-base: anvil_nodeInfo failed"),
    }
    Ok(())
}

/// A rebase means an anvil restart (re-pin, OOM) happened, which resets
/// automine to its default (on). Re-assert it: otherwise each raw tx mines its
/// own block and the engine's mine(1) produces empty blocks. (The epoch
/// decision - clear vs restore - lives with the reset handling above, where
/// the fork pin is compared before anything is written.)
async fn on_rebase(
    chain: Chain,
    rpc: &mirror_anvil::Rpc,
    head: u64,
    follow: u64,
    start: u64,
) -> Result<()> {
    if let Err(e) = rpc.anvil_set_automine(false).await {
        tracing::warn!(chain = ?chain, error = %e, "could not re-assert automine=false");
    }
    tracing::info!(chain = ?chain, head, follow, start, "starting chain at block");
    Ok(())
}

/// Two regimes (plan §3 ops): normal operation pauses at `max_gap_blocks`
/// (300 = the plan's "> 300 -> pause and re-sync"), while a *rebased* cursor
/// (a fresh pin after a restart/re-fork) is allowed to replay its whole pin
/// window - bounded by reth's state window (10,064), the plan's own limit.
fn gap_within_limit(ctx: &Ctx, chain: Chain, gap: u64, rebased: bool) -> bool {
    let limit = if rebased {
        10_064
    } else {
        ctx.engine.loops.max_gap_blocks
    };
    if gap > limit {
        tracing::error!(
            chain = ?chain,
            gap,
            max = limit,
            rebased,
            "sync gap too large - paused; run the re-pin drill (make repin)"
        );
        return false;
    }
    true
}

/// Upstream reorg continuity: only meaningful when we processed start-1, and
/// it belongs to the *M* loop alone - it owns the parent-stream integrity
/// (`reth_last_*` is written by the M replay). T consumes the same blocks
/// through the diff applier, whose absolute-value writes self-heal across a
/// reorg, and its own cursor check rebases on a reset. Running the check in
/// both loops double-counted every reorg. Returns `false` to stop this poll.
async fn reorg_continuity(ctx: &Ctx, chain: Chain, start: u64) -> Result<bool> {
    if !matches!(chain, Chain::M) {
        return Ok(true);
    }
    let (Some(last_hash), Some(last_block)) = (ctx.reth_last_hash()?, ctx.reth_last_block()?)
    else {
        return Ok(true);
    };
    if start > 1 && start - 1 == last_block {
        if let Some(prev) = mirror_devnet::scan::block_hash(&ctx.reth, start - 1).await? {
            if prev != last_hash {
                ctx.eth_metrics.inc(&ctx.eth_metrics.reorgs_detected);
                ctx.on_divergence(&format!("reth reorg at {}", start - 1));
                return Ok(false);
            }
        }
    }
    Ok(true)
}

pub(crate) async fn replay_loop(ctx: Arc<Ctx>) {
    let poll = Duration::from_millis(ctx.engine.loops.poll_interval_ms.max(200));
    let mut failing_block = 0u64;
    let mut attempts = 0u64;
    loop {
        if ctx.plane.is_halted() || ctx.plane.frozen.load(Ordering::Relaxed) {
            tokio::time::sleep(poll).await;
            continue;
        }
        match next_block_for(&ctx, Chain::M).await {
            Ok(Some(n)) => {
                if n != failing_block {
                    failing_block = n;
                    attempts = 0;
                }
                match replay_block(&ctx, n).await {
                    Ok(_) => attempts = 0,
                    Err(e) => {
                        attempts += 1;
                        tracing::error!(
                            block = n,
                            attempt = attempts,
                            error = %e,
                            "replay failed (will retry)"
                        );
                        // A block whose raws could not be built (missing blob
                        // sidecars) must NOT be mined: mining "what the pool
                        // holds" produces an *empty* block, breaks the 1:1
                        // block mapping and records a fatal tx-count divergence
                        // (measured: 261 txs lost per block while the beacon
                        // ingested the sidecars seconds behind the head). Hold
                        // M at n-1 and retry - `mirror_lag_blocks` is the alarm.
                        if attempts == 18 {
                            // ~90 s of the same block failing: not transient.
                            // Ask `next_block_for` to re-base M at a fresh pin
                            // (blob sidecars older than the CL's prune window
                            // can never be served, so waiting is forever).
                            tracing::error!(
                                block = n,
                                "block unservable for ~90 s; asking for an M re-base"
                            );
                            ctx.force_rebase.store(true, Ordering::Relaxed);
                        }
                        if attempts == 1 || attempts.is_multiple_of(20) {
                            tracing::error!(
                                block = n,
                                attempt = attempts,
                                error = %e,
                                "replay failed; holding M (never mining an empty or partial block)"
                            );
                        } else {
                            tracing::debug!(block = n, attempt = attempts, error = %e, "replay retry");
                        }
                        tokio::time::sleep(poll * 5).await;
                    }
                }
            }
            Ok(None) => tokio::time::sleep(poll).await,
            Err(e) => {
                tracing::warn!(error = %e, "replay loop poll failed");
                tokio::time::sleep(poll * 5).await;
            }
        }
    }
}

pub(crate) async fn apply_loop(ctx: Arc<Ctx>) {
    let poll = Duration::from_millis(ctx.engine.loops.poll_interval_ms.max(200));
    loop {
        if ctx.plane.is_halted() || ctx.plane.frozen.load(Ordering::Relaxed) {
            tokio::time::sleep(poll).await;
            continue;
        }
        match next_block_for(&ctx, Chain::T).await {
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
/// paused mirror must not let the source's head fall behind the replay.
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
        let t_head = match ctx.anvil_t.block_number().await {
            Ok(h) => h,
            Err(_) => continue,
        };
        // Never scan at or below T's fork base: after a re-pin those blocks
        // are the source's own (anvil forwards pre-pin ranges upstream), and
        // marking their writes devnet-owned would shadow mainnet's real state
        // (measured: 311 real txs scanned as devnet writes right after a
        // re-pin). The venue driver clamps its scan the same way.
        let pin = match ctx.anvil_t.fork_block().await {
            Ok(pin) => pin.unwrap_or(0),
            Err(_) => continue,
        };
        // A fresh mark starts at the head: do not scan pre-existing history
        // (the registry was persisted; T does not advance while the engine is
        // down). The applier's inline scan shares this high-water mark, so its
        // blocks are not re-traced.
        match mirror_devnet::scan::scan_to(&ctx.plane, &ctx.anvil_t, pin, t_head, t_head).await {
            Ok(walked) => ctx
                .eth_metrics
                .add(&ctx.eth_metrics.shadow_scanned_blocks, walked),
            Err(e) => tracing::warn!(error = %e, "shadow scan failed; will retry from the last clean block"),
        }
        // A save fault is logged, not swallowed: the marks are in memory and
        // the next pass retries the write (bounded: one warn per poll).
        if let Err(e) = ctx.plane.save_shadow() {
            tracing::warn!(error = %e, "shadow registry save failed");
        }
    }
}


/// Run the configured probes now: M at the cursor vs reth at the cursor — the
/// same assertion the replay loop makes per block (`convergence_probes`), on
/// demand. The per-block loop owns the `mirror_m_convergence_*` counters;
/// this run reports its own outcome and leaves them alone (the "last block"
/// gauge must stay per-block). The venue's `/admin/probes` is the same verb
/// over its at-pin probe set.
pub(crate) async fn probes_now(ctx: &Ctx) -> Result<mirror_devnet::probes::ProbeOutcome> {
    let Some(cursor) = ctx.cursor_m()? else {
        tracing::warn!("on-demand probes skipped: no M cursor yet");
        return Ok(mirror_devnet::probes::ProbeOutcome {
            skipped: ctx.engine.verifier.probes.len(),
            ..Default::default()
        });
    };
    let probes = mirror_devnet::probes::ProbeRunner::replay(&ctx.engine.verifier.probes)
        .parity(&ctx.reth, cursor.block, &ctx.anvil_m)
        .await?;
    tracing::info!(
        block = cursor.block,
        ok = probes.ok,
        failed = probes.failed,
        skipped = probes.skipped,
        "on-demand probes complete"
    );
    Ok(probes)
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
