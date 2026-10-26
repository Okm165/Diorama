//! The follow step: the venue's counterpart of the mainnet applier.
//!
//! One bounded window per pass: compose the source's window BAL (the spine's
//! generic composer — the HL public API has no native BAL, so it synthesizes
//! one from the envelopes and the declared scope), COW it against the
//! devnet's own writes, land the rest on V, and record the live parity of
//! what was applied. The gap guard *bounds one window's width* — a gap past
//! the policy composes the oldest `limit` blocks and continues next pass, so
//! the follow converges on the live source instead of latching paused
//! (measured: the old latching pause froze V's chain-side state ~9.9k blocks
//! behind while the public HyperEVM moved ~60 blocks/min — only a manual
//! re-pin recovered it; that is not live following). The continuity check
//! names a source reorg.
//!
//! The parts mirror the applier's `apply/`: this module is the step and its
//! guards, [`parity`] is the measurement (the applier's drift-lens
//! counterpart), [`audit`] is the uncovered-log trail (venue-only: reth's
//! native BALs carry every write).
pub(crate) mod audit;
pub(crate) mod parity;

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
    pub(crate) writes: usize,
    pub(crate) applied: usize,
    pub(crate) filtered: usize,
    pub(crate) skipped_shadowed: usize,
    pub(crate) sampled: usize,
    pub(crate) sampled_ok: usize,
    pub(crate) duration_ms: u64,
}

/// The catch-up state's drift line: the source head ran past the follow
/// cursor by more than one window's policy width, so the follow composes
/// bounded windows until it is current. The log's schema is per producer
/// (`kind` names the anomaly, like the applier's `source` names its diff
/// rung).
#[derive(serde::Serialize)]
struct GapCatchupEntry {
    chain: &'static str,
    kind: &'static str,
    followed: u64,
    source_head: u64,
    window_to: u64,
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
        // The symmetric steady line: the mainnet driver logs the same
        // message per pass at its follow head.
        tracing::debug!(
            head = source_head,
            follow = followed,
            "chain is at the follow head"
        );
        return Ok(None);
    }
    // One bounded window per pass: the policy's limit caps the window's
    // *width*, never the following itself — a gap past it composes the
    // oldest chunk and continues next pass (the chunked catch-up). Sequential
    // chunks land the same final state a single wide window would, each
    // bounded and logged.
    let limit = ctx.mirror.policies().gap;
    let to = catchup_window(followed, source_head, limit);
    if to < source_head {
        if ctx.plane.gap_pause_started() {
            ctx.venue_metrics
                .inc(&ctx.venue_metrics.follow_gap_catchups_total);
            ctx.drift.append(&GapCatchupEntry {
                chain: ctx.mirror.label(),
                kind: "gap_catchup",
                followed,
                source_head,
                window_to: to,
                gap: source_head - followed,
                limit: limit.unwrap_or(0),
            })?;
            tracing::warn!(
                followed,
                source_head,
                to,
                gap = source_head - followed,
                "follow catching up: the source ran ahead; composing bounded windows"
            );
        }
    } else if ctx.plane.gap_pause_cleared() {
        tracing::info!(followed, source_head, "follow caught up; the source is live");
    }

    let started = Instant::now();
    continuity_check(ctx, followed).await?;
    let plan = plan_window(ctx, followed, to).await?;
    let applied = apply_window(ctx, &plan).await?;
    let (sampled, sampled_ok) = parity::parity_step(ctx, &plan.bal).await?;
    advance_source_cursor(ctx, to).await?;
    let stats = FollowOutcome {
        source: plan.kind.as_str(),
        writes: plan.total,
        applied,
        filtered: plan.writes.len(),
        skipped_shadowed: plan.filter.skipped_shadowed,
        sampled,
        sampled_ok,
        duration_ms: started.elapsed().as_millis() as u64,
    };
    report_applied(ctx, followed, to, &stats);
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
        writes: window.writes,
        filtered: window.filtered,
        skipped_shadowed: window.skipped_shadowed,
        sampled: window.sampled,
        sampled_ok: window.sampled_ok,
        ms: window.duration_ms,
    });
    tracing::info!(
        from,
        to,
        writes = window.writes,
        applied = window.applied,
        skipped_shadowed = window.skipped_shadowed,
        sampled_ok = window.sampled_ok,
        sampled = window.sampled,
        ms = window.duration_ms,
        "source window composed onto V"
    );
}

/// The catch-up window's end: one bounded window per pass (the policy's
/// limit caps the *width*, never the following itself). `None` = unbounded.
fn catchup_window(followed: u64, source_head: u64, limit: Option<u64>) -> u64 {
    match limit {
        Some(limit) => source_head.min(followed.saturating_add(limit)),
        None => source_head,
    }
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
/// The window's first phase, the applier's `plan_and_check_base` slot: the
/// shared pipeline half (compose → COW → flatten) for the whole window, its
/// gauges recorded and the uncovered log audited.
async fn plan_window(ctx: &Ctx, from: u64, to: u64) -> Result<mirror_devnet::bal::WindowPlan> {
    let plan = mirror_devnet::bal::Composer {
        source: ctx.mirror.follow_source().as_ref(),
        target: ctx.target.as_ref(),
        scope: ctx.mirror.scope(),
    }
    .plan(&ctx.plane.shadow, mirror_devnet::bal::Window::new(from, to))
    .await?;
    record_window_metrics(&ctx.venue_metrics, &plan.compose);
    audit::record(ctx, &plan.compose.uncovered)?;
    Ok(plan)
}

/// The window's second phase, the applier's `write_and_mine` slot: land the
/// planned writes on V (chunked), counted.
async fn apply_window(ctx: &Ctx, plan: &mirror_devnet::bal::WindowPlan) -> Result<usize> {
    if plan.writes.is_empty() {
        return Ok(0);
    }
    Ok(mirror_anvil::apply_writes(&ctx.anvil_v, &plan.writes, ctx.engine.loops.chunk_size).await?)
}

/// The poll's gauges: what the window's composition touched, planned and
/// changed (the uncovered-log trail is [`audit`]'s).
fn record_window_metrics(metrics: &crate::metrics::VenueMetrics, stats: &mirror_devnet::bal::ComposeStats) {
    metrics.set(&metrics.poll_touched, stats.touched as u64);
    metrics.set(&metrics.poll_planned_keys, stats.planned as u64);
    metrics.set(&metrics.poll_changed_keys, stats.changed as u64);
}

/// Advance the source cursor to `source_head` (recording the source's own
/// block hash — the continuity check's anchor) and persist the state. The
/// name says which cursor: the devnet cursor is the spine's `advance_cursor`.
async fn advance_source_cursor(ctx: &Ctx, source_head: u64) -> Result<()> {
    let head_block = ctx.source_rpc.block_by_number(source_head, false).await?;
    ctx.plane.state.set_source_last(source_head, head_block.header.hash)?;
    ctx.plane.save_state()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::catchup_window;

    /// The catch-up window: one bounded window per pass — the policy's limit
    /// caps the width, never the following itself (the latching pause it
    /// replaced froze V's chain-side state for hours; measured live).
    #[test]
    fn the_catchup_window_bounds_one_pass() {
        assert_eq!(catchup_window(0, 100, Some(32)), 32);
        assert_eq!(catchup_window(0, 32, Some(32)), 32);
        assert_eq!(catchup_window(0, 31, Some(32)), 31);
        assert_eq!(catchup_window(100, 132, Some(32)), 132);
        assert_eq!(catchup_window(100, 10_000, Some(300)), 400);
        assert_eq!(catchup_window(100, 10_000, None), 10_000);
    }
}
