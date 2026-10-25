//! The follow step: the venue's counterpart of the mainnet applier.
//!
//! One window per pass: compose the source's window BAL (the spine's generic
//! composer — the HL public API has no native BAL, so it synthesizes one from
//! the envelopes and the declared scope), COW it against the devnet's own
//! writes, land the rest on V, and record the live parity of what was
//! applied. The gap guard pauses a window that is too wide (a re-pin
//! decision, never a silent partial poll); the continuity check names a
//! source reorg.
//!
//! The parts mirror the applier's `apply/`: this module is the step and its
//! guards, [`parity`] is the measurement (the applier's drift-lens
//! counterpart), [`audit`] is the uncovered-log trail (venue-only: reth's
//! native BALs carry every write).
mod audit;
mod parity;

use crate::ctx::Ctx;
use crate::error::Result;
use mirror_devnet::facts::Fact;
use mirror_devnet::metrics::Registry as _;
use std::time::Instant;


/// One follow window's outcome (what the fact and the gauges report) — the
/// venue's counterpart of the applier's `ApplyOutcome`, returned by [`step`]
/// so a caller that reports per-window lines (the selftest) reads the same
/// numbers the fact carries. The field names are the applier's fact
/// vocabulary (`total`/`applied`/`filtered`), so the two mirrors' facts and
/// logs read the same way.
pub(crate) struct FollowOutcome {
    /// The compose rung's name (`plan.kind.as_str()`: `poll` for the public
    /// API's touches; `bal`/`trace` if a future source serves them).
    pub(crate) source: &'static str,
    /// The composed change count, before COW (the fact's `writes`).
    pub(crate) total: usize,
    pub(crate) applied: usize,
    pub(crate) filtered: usize,
    pub(crate) skipped_shadowed: usize,
    pub(crate) sampled: usize,
    pub(crate) sampled_ok: usize,
    pub(crate) duration_ms: u64,
}

/// The gap guard's drift line: the source head ran past the follow cursor by
/// more than the policy allows, so the window is paused (a re-pin decision).
/// The log's schema is per producer (`kind` names the anomaly, like the
/// applier's `source` names its diff rung).
#[derive(serde::Serialize)]
struct GapPauseEntry {
    chain: &'static str,
    kind: &'static str,
    followed: u64,
    source_head: u64,
    gap: u64,
    limit: u64,
}

/// A source continuity mismatch's drift line: the block after the last
/// composed one carried a different parent hash (a reorg under the follow).
#[derive(serde::Serialize)]
struct SourceReorgEntry {
    chain: &'static str,
    kind: &'static str,
    block: u64,
    expected: String,
    found: String,
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
pub(crate) async fn step(ctx: &Ctx) -> Result<Option<FollowOutcome>> {
    let source_head = ctx.source_rpc.block_number().await?;
    ctx.venue_metrics
        .set(&ctx.venue_metrics.source_head, source_head);
    let Some(followed) = ctx.plane.state.source_last_block()? else {
        // Not anchored yet (reconcile anchors at the pin); nothing to do.
        return Ok(None);
    };
    ctx.venue_metrics
        .set(&ctx.venue_metrics.source_followed, followed);
    if source_head <= followed {
        return Ok(None);
    }
    if !gap_ok(ctx, followed, source_head)? {
        return Ok(None);
    }

    let started = Instant::now();
    continuity_check(ctx, followed).await?;
    let mut stats = compose_window(ctx, followed, source_head).await?;
    advance_cursor(ctx, source_head).await?;
    stats.duration_ms = started.elapsed().as_millis() as u64;
    report_applied(ctx, followed, source_head, &stats);
    Ok(Some(stats))
}

/// One place for the window's gauges, fact, and log.
fn report_applied(ctx: &Ctx, from: u64, to: u64, window: &FollowOutcome) {
    ctx.venue_metrics
        .inc(&ctx.venue_metrics.follow_windows_total);
    ctx.venue_metrics
        .add(&ctx.venue_metrics.follow_applied_total, window.applied as u64);
    ctx.venue_metrics
        .add(&ctx.venue_metrics.follow_sampled_total, window.sampled as u64);
    ctx.venue_metrics
        .add(&ctx.venue_metrics.follow_sampled_ok_total, window.sampled_ok as u64);
    ctx.venue_metrics.add(
        &ctx.venue_metrics.follow_skipped_shadowed_total,
        window.skipped_shadowed as u64,
    );
    ctx.plane.facts.publish(Fact::BlockApplied {
        chain: ctx.mirror.label(),
        block: to,
        source: window.source,
        writes: window.total,
        filtered: window.filtered,
        skipped_shadowed: window.skipped_shadowed,
        sampled: window.sampled,
        sampled_ok: window.sampled_ok,
        ms: window.duration_ms,
    });
    tracing::info!(
        from,
        to,
        writes = window.total,
        applied = window.applied,
        skipped_shadowed = window.skipped_shadowed,
        sampled_ok = window.sampled_ok,
        sampled = window.sampled,
        ms = window.duration_ms,
        "source window composed onto V"
    );
}

/// The gap guard: a window wider than the profile's limit pauses the follow
/// (the same regime as the mainnet applier's `max_gap_blocks`).
/// The gap guard's predicate: a gap at or below the limit follows (the
/// mainnet's `gap_within_limit` adds the rebase bound; the venue's windows
/// have none).
fn gap_within_limit(gap: u64, limit: u64) -> bool {
    gap <= limit
}

fn gap_ok(ctx: &Ctx, followed: u64, source_head: u64) -> Result<bool> {
    let gap = source_head - followed;
    let Some(limit) = ctx.mirror.policies().gap else {
        return Ok(true);
    };
    if !gap_within_limit(gap, limit) {
        ctx.venue_metrics
            .inc(&ctx.venue_metrics.follow_gap_pauses_total);
        ctx.drift.append(&GapPauseEntry {
            chain: ctx.mirror.label(),
            kind: "gap_pause",
            followed,
            source_head,
            gap,
            limit,
        })?;
        tracing::error!(
            followed,
            source_head,
            gap,
            max = limit,
            "follow gap too large - paused; run the re-pin drill (make repin)"
        );
        return Ok(false);
    }
    Ok(true)
}

/// Continuity: the block after the last composed one must carry the recorded
/// hash as its parent. A mismatch is a source reorg — the window composes from
/// latest-vs-latest reads, so the applied keys are canonical, but keys only
/// the orphaned blocks touched may be stale until a later window touches them:
/// count it, say so, and let the operator re-pin (HL finality is BFT; a
/// visible alarm is the honest guard).
async fn continuity_check(ctx: &Ctx, followed: u64) -> Result<()> {
    let Some(last_hash) = ctx.plane.state.source_last_hash()? else {
        return Ok(());
    };
    let next = ctx.source_rpc.block_by_number(followed + 1, false).await?;
    if next.header.parent_hash != last_hash {
        ctx.venue_metrics
            .inc(&ctx.venue_metrics.source_reorg_total);
        ctx.drift.append(&SourceReorgEntry {
            chain: ctx.mirror.label(),
            kind: "source_reorg",
            block: followed + 1,
            expected: last_hash.to_string(),
            found: next.header.parent_hash.to_string(),
        })?;
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
async fn compose_window(ctx: &Ctx, from: u64, to: u64) -> Result<FollowOutcome> {
    // The shared pipeline half (compose → COW → flatten): the same call the
    // applier makes for one parent block, here for the whole window.
    let plan = mirror_devnet::bal::Composer {
        source: ctx.mirror.follow_source().as_ref(),
        target: ctx.target.as_ref(),
        scope: ctx.mirror.scope(),
    }
    .plan(&ctx.plane.shadow, mirror_devnet::bal::Window::new(from, to))
    .await?;
    record_window_metrics(ctx, &plan.compose);
    audit::record(ctx, &plan.compose.uncovered)?;
    let applied = if plan.writes.is_empty() {
        0
    } else {
        mirror_anvil::apply_writes(&ctx.anvil_v, &plan.writes, ctx.engine.loops.chunk_size).await?
    };
    let (sampled, sampled_ok) = parity::parity_step(ctx, &plan.bal).await?;
    Ok(FollowOutcome {
        source: plan.kind.as_str(),
        total: plan.total,
        applied,
        filtered: plan.writes.len(),
        skipped_shadowed: plan.filter.skipped_shadowed,
        sampled,
        sampled_ok,
        duration_ms: 0, // set by `step` once the window's work is timed
    })
}

/// The poll's gauges: what the window's composition touched, planned and
/// changed (the uncovered-log trail is [`audit`]'s).
fn record_window_metrics(ctx: &Ctx, stats: &mirror_devnet::bal::ComposeStats) {
    let metrics = &ctx.venue_metrics;
    metrics.set(&metrics.poll_touched, stats.touched as u64);
    metrics.set(&metrics.poll_planned_keys, stats.planned as u64);
    metrics.set(&metrics.poll_changed_keys, stats.changed as u64);
}

/// Advance the follow cursor to `source_head` (recording the source's own
/// block hash — the continuity check's anchor) and persist the state.
async fn advance_cursor(ctx: &Ctx, source_head: u64) -> Result<()> {
    let head_block = ctx.source_rpc.block_by_number(source_head, false).await?;
    ctx.plane.state.set_source_last(source_head, head_block.header.hash)?;
    ctx.plane.save_state()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::gap_within_limit;

    /// The gap guard's predicate: at or below the limit follows, beyond it
    /// pauses (the mainnet's test pins the same boundary with its rebase
    /// bound).
    #[test]
    fn the_gap_guard_honors_its_limit() {
        assert!(gap_within_limit(0, 32));
        assert!(gap_within_limit(32, 32));
        assert!(!gap_within_limit(33, 32));
    }
}
