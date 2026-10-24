//! The follow step: the venue's counterpart of the ethereum applier.
//!
//! One window per pass: compose the source's window BAL (the spine's generic
//! composer — the HL public API has no native BAL, so it synthesizes one from
//! the envelopes and the declared scope), COW it against the devnet's own
//! writes, land the rest on V, and record the live parity of what was
//! applied. The gap guard pauses a window that is too wide (a re-pin
//! decision, never a silent partial poll); the continuity check names a
//! source reorg.
use crate::ctx::Ctx;
use crate::error::Result;
use mirror_devnet::facts::Fact;
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::spine::ProbeSchedule;
use std::time::Instant;


/// One follow window's outcome (what the fact and the gauges report).
struct Window {
    total: usize,
    applied: usize,
    filtered: usize,
    skipped_shadowed: usize,
    sampled: usize,
    sampled_ok: usize,
}

/// Follow the source: compose the window since the last composed source
/// block, land the changed keys on V (COW: devnet-owned keys are skipped),
/// and record the live parity of the applied keys. One window per pass; the
/// gap guard pauses a window that is too wide (a re-pin decision, never a
/// silent partial poll).
///
/// The window's writes are the *source's* state, not the devnet's: they are
/// not journaled (a rewind restores devnet writes, never source history), and
/// a key the devnet owns is skipped until it is handed back.
pub(crate) async fn step(ctx: &Ctx) -> Result<()> {
    let source_head = ctx.source_rpc.block_number().await?;
    ctx.venue_metrics
        .set(&ctx.venue_metrics.source_head, source_head);
    let Some(followed) = ctx.source_last_block()? else {
        // Not anchored yet (reconcile anchors at the pin); nothing to do.
        return Ok(());
    };
    ctx.venue_metrics
        .set(&ctx.venue_metrics.source_followed, followed);
    if source_head <= followed {
        return Ok(());
    }
    if !gap_ok(ctx, followed, source_head) {
        return Ok(());
    }

    let started = Instant::now();
    continuity_check(ctx, followed).await?;
    let window = compose_window(ctx, followed, source_head).await?;
    advance_follow(ctx, source_head).await?;
    report_window(ctx, followed, source_head, &window, started.elapsed().as_millis() as u64);
    Ok(())
}

/// One place for the window's gauges, fact, and log.
fn report_window(ctx: &Ctx, from: u64, to: u64, window: &Window, ms: u64) {
    ctx.venue_metrics
        .inc(&ctx.venue_metrics.follow_windows_total);
    ctx.venue_metrics
        .add(&ctx.venue_metrics.follow_applied_total, window.applied as u64);
    ctx.venue_metrics.add(
        &ctx.venue_metrics.follow_skipped_shadowed_total,
        window.skipped_shadowed as u64,
    );
    ctx.plane.facts.publish(Fact::BlockApplied {
        chain: ctx.mirror.label(),
        block: to,
        source: "poll",
        writes: window.total,
        filtered: window.filtered,
        skipped_shadowed: window.skipped_shadowed,
        sampled: window.sampled,
        sampled_ok: window.sampled_ok,
        ms,
    });
    tracing::info!(
        from,
        to,
        writes = window.total,
        applied = window.applied,
        skipped_shadowed = window.skipped_shadowed,
        sampled_ok = window.sampled_ok,
        sampled = window.sampled,
        ms,
        "source window composed onto V"
    );
}

/// The gap guard: a window wider than the profile's limit pauses the follow
/// (the same regime as the ethereum applier's `max_gap_blocks`).
fn gap_ok(ctx: &Ctx, followed: u64, source_head: u64) -> bool {
    let gap = source_head - followed;
    let Some(limit) = ctx.mirror.policies().gap else {
        return true;
    };
    if gap > limit {
        ctx.venue_metrics
            .inc(&ctx.venue_metrics.follow_gap_pauses_total);
        tracing::error!(
            followed,
            source_head,
            gap,
            max = limit,
            "follow gap too large - paused; run the re-pin drill (make repin)"
        );
        return false;
    }
    true
}

/// Continuity: the block after the last composed one must carry the recorded
/// hash as its parent. A mismatch is a source reorg — the window composes from
/// latest-vs-latest reads, so the applied keys are canonical, but keys only
/// the orphaned blocks touched may be stale until a later window touches them:
/// count it, say so, and let the operator re-pin (HL finality is BFT; a
/// visible alarm is the honest guard).
async fn continuity_check(ctx: &Ctx, followed: u64) -> Result<()> {
    let Some(last_hash) = ctx.source_last_hash()? else {
        return Ok(());
    };
    let next = ctx.source_rpc.block_by_number(followed + 1, false).await?;
    if next.header.parent_hash != last_hash {
        ctx.venue_metrics
            .inc(&ctx.venue_metrics.source_reorg_total);
        tracing::error!(
            block = followed + 1,
            expected = %last_hash,
            found = %next.header.parent_hash,
            "source continuity mismatch (reorg): the follow window recomposed from latest"
        );
    }
    Ok(())
}

/// Compose the window and land its writes on V under the COW filter, then
/// record the live parity of what was applied. The composer is the spine's
/// generic one: the venue's source has no native BAL, so it synthesizes.
async fn compose_window(ctx: &Ctx, from: u64, to: u64) -> Result<Window> {
    let (window, stats) = mirror_devnet::bal::compose(
        ctx.mirror.follow_source().as_ref(),
        ctx.target.as_ref(),
        ctx.mirror.scope(),
        from,
        to,
    )
    .await?;
    record_poll_stats(ctx, &stats);
    let total = mirror_devnet::bal::change_count(&window.bal);
    let (filtered, cow) = {
        let shadow = ctx.plane.shadow.lock()?;
        mirror_devnet::cow::filter_bal(ctx.mirror.scope().watch(), &shadow, window.bal)
    };
    let writes = mirror_anvil::flatten_bal(&filtered);
    let applied = if writes.is_empty() {
        0
    } else {
        mirror_anvil::apply_writes(&ctx.v, &writes, ctx.engine.loops.chunk_size).await?
    };
    let (sampled, sampled_ok) =
        if matches!(ctx.mirror.policies().probes, ProbeSchedule::LiveParity) {
            mirror_devnet::bal::verify(ctx.target.as_ref(), &filtered).await?
        } else {
            (0, 0)
        };
    Ok(Window {
        total,
        applied,
        filtered: writes.len(),
        skipped_shadowed: cow.skipped_shadowed,
        sampled,
        sampled_ok,
    })
}

/// The poll's gauges and the audit warning, one place.
fn record_poll_stats(ctx: &Ctx, stats: &mirror_devnet::bal::ComposeStats) {
    ctx.venue_metrics
        .set(&ctx.venue_metrics.poll_touched, stats.touched as u64);
    ctx.venue_metrics
        .set(&ctx.venue_metrics.poll_planned_keys, stats.planned as u64);
    ctx.venue_metrics
        .set(&ctx.venue_metrics.poll_changed_keys, stats.changed as u64);
    if !stats.uncovered.is_empty() {
        ctx.venue_metrics
            .add(&ctx.venue_metrics.poll_uncovered_logs, stats.uncovered.len() as u64);
        tracing::warn!(
            emitters = ?stats.uncovered,
            "follow: logs from contracts no closure covers (declare a closure to follow them)"
        );
    }
}

/// Advance the follow cursor to `source_head` (recording the source's own
/// block hash — the continuity check's anchor) and persist the state.
async fn advance_follow(ctx: &Ctx, source_head: u64) -> Result<()> {
    let head_block = ctx.source_rpc.block_by_number(source_head, false).await?;
    ctx.set_source_last_block(source_head)?;
    ctx.set_source_last_hash(head_block.header.hash)?;
    ctx.plane.save_state()?;
    Ok(())
}
