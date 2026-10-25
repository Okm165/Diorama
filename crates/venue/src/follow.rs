//! The follow step: the venue's counterpart of the mainnet applier.
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


/// One follow window's outcome (what the fact and the gauges report) — the
/// venue's counterpart of the applier's `ApplyOutcome`, returned by [`step`]
/// so a caller that reports per-window lines (the selftest) reads the same
/// numbers the fact carries.
pub(crate) struct WindowStats {
    /// The compose rung's name (`plan.kind.as_str()`: `poll` for the public
    /// API's touches; `bal`/`trace` if a future source serves them).
    pub(crate) source: &'static str,
    pub(crate) total: usize,
    pub(crate) applied: usize,
    pub(crate) filtered: usize,
    pub(crate) skipped_shadowed: usize,
    pub(crate) sampled: usize,
    pub(crate) sampled_ok: usize,
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

/// The uncovered-log audit's drift line: log emitters no declared closure
/// covers — their state changes cannot be followed until a closure is
/// declared (the metric counts every window; the line carries the emitters
/// new to the log — see [`fresh_emitters`]).
#[derive(serde::Serialize)]
struct UncoveredLogsEntry {
    chain: &'static str,
    kind: &'static str,
    emitters: Vec<String>,
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
pub(crate) async fn step(ctx: &Ctx) -> Result<Option<WindowStats>> {
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
    let stats = compose_window(ctx, followed, source_head).await?;
    advance_follow(ctx, source_head).await?;
    report_window(ctx, followed, source_head, &stats, started.elapsed().as_millis() as u64);
    Ok(Some(stats))
}

/// One place for the window's gauges, fact, and log.
fn report_window(ctx: &Ctx, from: u64, to: u64, window: &WindowStats, ms: u64) {
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
/// (the same regime as the mainnet applier's `max_gap_blocks`).
fn gap_ok(ctx: &Ctx, followed: u64, source_head: u64) -> Result<bool> {
    let gap = source_head - followed;
    let Some(limit) = ctx.mirror.policies().gap else {
        return Ok(true);
    };
    if gap > limit {
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
async fn compose_window(ctx: &Ctx, from: u64, to: u64) -> Result<WindowStats> {
    // The shared pipeline half (compose → COW → flatten): the same call the
    // applier makes for one parent block, here for the whole window.
    let plan = mirror_devnet::bal::Composer {
        source: ctx.mirror.follow_source().as_ref(),
        target: ctx.target.as_ref(),
        scope: ctx.mirror.scope(),
    }
    .plan(&ctx.plane.shadow, mirror_devnet::bal::Window::new(from, to))
    .await?;
    record_poll_stats(ctx, &plan.compose)?;
    let applied = if plan.writes.is_empty() {
        0
    } else {
        mirror_anvil::apply_writes(&ctx.anvil_v, &plan.writes, ctx.engine.loops.chunk_size).await?
    };
    let (sampled, sampled_ok) =
        if matches!(ctx.mirror.policies().probes, ProbeSchedule::LiveParity) {
            // `verify` answers `(ok, failed)`; the fact's contract is
            // `(sample size, passed)` — the applier's naming, so the two
            // mirrors' facts read the same way (found by the venue selftest:
            // the inverted pair reported every clean window as a failure).
            let (ok, failed) = mirror_devnet::bal::verify(ctx.target.as_ref(), &plan.bal).await?;
            (ok + failed, ok)
        } else {
            (0, 0)
        };
    Ok(WindowStats {
        source: plan.kind.as_str(),
        total: plan.total,
        applied,
        filtered: plan.writes.len(),
        skipped_shadowed: plan.filter.skipped_shadowed,
        sampled,
        sampled_ok,
    })
}

/// The poll's gauges and the audit trail, one place. The counter counts every
/// window; the drift line and the warning carry only the emitters *new* to
/// the log — a busy chain touches dozens of contracts per window and the set
/// churns, so a per-window line would grow forever while saying nothing new.
fn record_poll_stats(ctx: &Ctx, stats: &mirror_devnet::bal::ComposeStats) -> Result<()> {
    let metrics = &ctx.venue_metrics;
    metrics.set(&metrics.poll_touched, stats.touched as u64);
    metrics.set(&metrics.poll_planned_keys, stats.planned as u64);
    metrics.set(&metrics.poll_changed_keys, stats.changed as u64);
    if !stats.uncovered.is_empty() {
        metrics.add(&metrics.poll_uncovered_logs, stats.uncovered.len() as u64);
        let fresh = fresh_emitters(&ctx.uncovered_seen, &stats.uncovered);
        if !fresh.is_empty() {
            ctx.drift.append(&UncoveredLogsEntry {
                chain: ctx.mirror.label(),
                kind: "uncovered_logs",
                emitters: fresh.clone(),
            })?;
            tracing::warn!(
                emitters = ?fresh,
                "follow: logs from contracts no closure covers (declare a closure to follow them)"
            );
        }
    }
    Ok(())
}

/// The emitters in `current` that the seen set had not carried yet, adding
/// them as it goes. A poisoned lock is recovered by construction (the set is
/// a cache of what was logged; there is no invariant a panic could break).
fn fresh_emitters(
    seen: &std::sync::Mutex<std::collections::HashSet<String>>,
    current: &[alloy_primitives::Address],
) -> Vec<String> {
    let mut seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    current
        .iter()
        .map(ToString::to_string)
        .filter(|emitter| seen.insert(emitter.clone()))
        .collect()
}

/// Advance the follow cursor to `source_head` (recording the source's own
/// block hash — the continuity check's anchor) and persist the state.
async fn advance_follow(ctx: &Ctx, source_head: u64) -> Result<()> {
    let head_block = ctx.source_rpc.block_by_number(source_head, false).await?;
    ctx.plane.state.set_source_last(source_head, head_block.header.hash)?;
    ctx.plane.save_state()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fresh_emitters;
    use alloy_primitives::Address;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// The audit's bound: the first sighting of an emitter is fresh, its
    /// repeats are not, and a churning set reports only what is new.
    #[test]
    fn fresh_emitters_reports_each_address_once() {
        let seen = Mutex::new(HashSet::new());
        let a = Address::repeat_byte(0xaa);
        let b = Address::repeat_byte(0xbb);
        assert_eq!(fresh_emitters(&seen, &[a, b]).len(), 2);
        assert!(fresh_emitters(&seen, &[a, b]).is_empty());
        assert_eq!(fresh_emitters(&seen, &[b, a]).len(), 0);
        let c = Address::repeat_byte(0xcc);
        let fresh = fresh_emitters(&seen, &[a, c]);
        assert_eq!(fresh, vec![c.to_string()]);
    }
}
