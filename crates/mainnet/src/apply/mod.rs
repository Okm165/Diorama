//! Applier: keeps T's watch-list state fresh by applying parent BALs.
//!
//! Per parent block: prepend devnet txs -> compose the parent's
//! BAL (the spine's generic composer: native on reth, the trace rung as
//! fallback) -> COW it against the devnet's own writes -> read before-values
//! -> batched cheatcode writes -> align block context -> mine -> journal ->
//! sampled parity + probe parity. Each phase is one function; `step` is
//! the sequence. The two phase families with their own state live beside it:
//! [`prepend`] (the devnet's own blocks) and [`parity`] (the drift lens and
//! the convergence probes).
pub(crate) mod parity;
pub(crate) mod prepend;

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use alloy_rpc_types_eth::Header;
use diorama_anvil::Write;
use diorama_devnet::cow::FilterStats;
use diorama_devnet::drift::Drift;
use diorama_devnet::facts::Fact;
use diorama_devnet::journal::JournalEntry;
use diorama_devnet::plane::ReapplyRange;
use diorama_devnet::metrics::Registry as _;
use diorama_devnet::plane::Plane;
use diorama_devnet::spine::{ClockPolicy, MinePolicy};
use parity::parity_step;
use prepend::{prepend_and_mark, scan_own_blocks};
use std::sync::atomic::Ordering;
use std::time::Instant;

pub(crate) struct ApplyOutcome {
    pub source: &'static str,
    pub writes: usize,
    pub filtered: usize,
    pub skipped_shadowed: usize,
    pub sampled: usize,
    pub sampled_ok: usize,
    pub duration_ms: u64,
}

/// One T apply anomaly line in drift.jsonl (the re-apply of a rewound range,
/// or a sampled-parity mismatch). The `source` names which diff source the
/// anomaly belongs to (`bal`, `trace`, `late`, `reapply`).
#[derive(serde::Serialize)]
struct TDriftEntry<'a> {
    chain: &'static str,
    block: u64,
    source: &'a str,
    bal_writes: usize,
    applied: usize,
    filtered: usize,
    not_watched: usize,
    skipped_shadowed: usize,
    sampled: usize,
    sampled_ok: usize,
    duration_ms: u64,
}

/// Write the filtered diffs to T (batched; padding is mandatory — measured)
/// and mine the aligned parent block. Returns the writes applied.
async fn write_and_mine(ctx: &Ctx, header: &Header, filtered: &[Write]) -> Result<usize> {
    let applied =
        diorama_anvil::apply_writes(&ctx.anvil_t, filtered, ctx.engine.loops.chunk_size).await?;
    let policies = ctx.mirror.policies();
    // The profile's clock rule: the applied block's context is the parent's
    // (aligned to timestamp + the plane's live offset) or the devnet's own.
    if policies.clock == ClockPolicy::AlignToSource {
        align_context(ctx, header).await?;
    }
    // The profile's production rule: one T block per applied parent block
    // (T's cadence is the devnet's); a pool-drain profile mines elsewhere.
    if policies.mine == MinePolicy::PerAppliedBlock {
        ctx.anvil_t.anvil_mine(1).await?;
    }
    Ok(applied)
}

/// Journal the before-values and scan the apply's own devnet blocks.
///
/// The scan runs *before* the next parity assertion: the prepend loop may have
/// mined devnet txs whose writes are not marked shadowed yet (the shadow loop
/// polls every ~2 s), and the probe parity must skip devnet-owned accounts
/// (copy-on-write) instead of alarming on the devnet's own writes. Measured:
/// spurious aave_weth_price / univ3_weth_usdc_slot0 mismatches right after a
/// venue interaction. A scan that cannot complete keeps the marks already in
/// memory and the walk retries from the last clean block: a named, bounded
/// degradation.
async fn journal_and_scan(
    ctx: &Ctx,
    n: u64,
    entries: &[JournalEntry],
    t_head: u64,
) -> Result<()> {
    ctx.plane.journal.record(n, entries)?;
    ctx.plane.journal.prune(n);
    if let Err(e) = scan_own_blocks(ctx, t_head).await {
        tracing::warn!(error = %e, "inline shadow scan incomplete");
    }
    Ok(())
}

/// Whether this block's writes were rolled back by a journal rewind and must
/// be re-applied even though `t_head >= n`: the late-completion skip would
/// otherwise leave the rolled-back values stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reapply {
    /// The rewind covers this block: re-apply its writes.
    RolledBack,
    /// The normal forward case.
    Forward,
}

/// What the apply's entry checks decided.
enum ApplyEntry {
    /// The block's writes are already in (a timed-out apply that finished):
    /// the outcome is the late-completion one.
    Late(ApplyOutcome),
    /// Proceed, with the re-apply state the applier must carry.
    Proceed {
        reapply: Reapply,
        reapply_from: Option<ReapplyRange>,
    },
}

/// The apply's entry checks, in the order that keeps their measured lessons:
///
/// - A journal rewind rolled T's values back without moving its head: those
///   blocks must be re-applied even though `t_head >= n`, otherwise the
///   late-completion skip below leaves the rolled-back values stale.
/// - The journal is the discriminator between "this block's writes are already
///   in" and "T's head is ahead because devnet txs were mined in their own
///   prepend block". A journal fault is an error, never "no journal": guessing
///   would re-journal already-applied values as before-values.
/// - T's head may be *ahead* of the cursor (devnet prepend blocks) — normal.
///   Behind by more than one block is not: that means someone reset T outside
///   the engine (anvil_reset), which the incremental diff applier cannot
///   recover from — alarm and keep retrying instead of corrupting state.
async fn apply_entry(ctx: &Ctx, n: u64, t_head: u64, started: Instant) -> Result<ApplyEntry> {
    let reapply_from = *ctx.plane.reapply_from.lock()?;
    let reapply = if matches!(reapply_from, Some(range) if n >= range.from) {
        Reapply::RolledBack
    } else {
        Reapply::Forward
    };
    let journaled = ctx.plane.journal.has_entry(n)?;
    if t_head >= n && journaled && reapply == Reapply::Forward {
        return Ok(ApplyEntry::Late(late_completion(ctx, n, t_head, started).await?));
    }
    if t_head + 1 < n {
        return Err(Error::ApplierBehind { t_head, n });
    }
    Ok(ApplyEntry::Proceed {
        reapply,
        reapply_from,
    })
}

pub(crate) async fn step(ctx: &Ctx, n: u64) -> Result<ApplyOutcome> {
    let started = Instant::now();
    let header = ctx.mirror.source().header(n).await?;

    let t_head = ctx.anvil_t.block_number().await?;
    // The base this apply started under: `plan_and_check_base` aborts if a
    // re-pin races the (slow) compose, before any write; the next pass
    // reconciles (the cursor sits below the base → the epoch rule).
    let base = ctx.anvil_t.fork_block().await?;
    let (reapply, reapply_from) = match apply_entry(ctx, n, t_head, started).await? {
        ApplyEntry::Late(outcome) => return Ok(outcome),
        ApplyEntry::Proceed {
            reapply,
            reapply_from,
        } => (reapply, reapply_from),
    };

    prepend_and_mark(ctx, n).await?;

    let plan = plan_and_check_base(ctx, n, base).await?;
    let entries =
        diorama_devnet::journal::read_before_values(&ctx.anvil_t, &plan.writes, t_head).await?;

    let applied = write_and_mine(ctx, &header, &plan.writes).await?;
    journal_and_scan(ctx, n, &entries, t_head).await?;
    // The parent-stream continuity marker (reorg detection reads it).
    ctx.plane.state.set_source_last(n, header.hash)?;

    let t_latest = ctx.anvil_t.block_number().await?;
    let (sampled, sampled_ok) = parity_step(ctx, n, &plan.writes, t_latest).await?;

    finalize(
        ctx,
        n,
        ApplyFacts {
            chain: ctx.mirror.label(),
            source: if reapply == Reapply::RolledBack {
                "reapply"
            } else {
                plan.kind.as_str()
            },
            total: plan.total,
            applied,
            filtered: plan.writes.len(),
            stats: plan.filter,
            sampled,
            sampled_ok,
            reapply,
            reapply_from,
            t_latest,
            started,
        },
    )
    .await
}

/// Compose the window's diffs and apply the watch/COW filter. The pre-write
/// base check lives here: a re-pin racing the (slow) compose must abort
/// before any write. Returns `(filtered writes, total changes, stats, kind)`.
async fn plan_and_check_base(
    ctx: &Ctx,
    n: u64,
    base: Option<u64>,
) -> Result<diorama_devnet::bal::WindowPlan> {
    // The shared pipeline half (compose → COW → flatten), then the check: the
    // compose awaits for seconds, and a re-pin racing it must abort before any
    // write (the next pass reconciles).
    let plan = diorama_devnet::bal::Composer {
        source: ctx.mirror.follow_source().as_ref(),
        target: ctx.target.as_ref(),
        scope: ctx.mirror.scope(),
    }
    .plan(&ctx.plane.shadow, diorama_devnet::bal::Window::single(n))
    .await?;
    if ctx.anvil_t.fork_block().await? != base {
        return Err(Error::ResetDuringApply { n });
    }
    Ok(plan)
}

/// The per-block facts the finalize step records (cursor, metrics, drift,
/// outcome).
struct ApplyFacts {
    /// The mirror's chain tag (the bundle's label).
    chain: &'static str,
    source: &'static str,
    total: usize,
    applied: usize,
    filtered: usize,
    stats: FilterStats,
    sampled: usize,
    sampled_ok: usize,
    reapply: Reapply,
    reapply_from: Option<ReapplyRange>,
    t_latest: u64,
    started: Instant,
}

/// Cursor + metrics + drift + log for one applied block.
async fn finalize(ctx: &Ctx, n: u64, facts: ApplyFacts) -> Result<ApplyOutcome> {
    advance_cursor(ctx, n).await?;
    let outcome = ApplyOutcome {
        source: facts.source,
        writes: facts.total,
        filtered: facts.filtered,
        skipped_shadowed: facts.stats.skipped_shadowed,
        sampled: facts.sampled,
        sampled_ok: facts.sampled_ok,
        duration_ms: facts.started.elapsed().as_millis() as u64,
    };
    record_apply_metrics(ctx, n, &facts, outcome.duration_ms);
    if let Some(range) = ctx.plane.clear_reapply_from(n, facts.reapply_from)? {
        tracing::info!(block = n, until = range.upto, "rewound range re-applied");
    }
    log_drift_if_anomalous(&ctx.drift, n, &facts, &outcome)?;
    report_applied(&ctx.plane, n, &facts, &outcome);
    Ok(outcome)
}

/// The applied block's announcements: the `block_applied` fact, the cursor
/// fact, and the log line — one place, both engines' vocabulary.
fn report_applied(plane: &Plane, n: u64, facts: &ApplyFacts, outcome: &ApplyOutcome) {
    plane.facts.publish(Fact::BlockApplied {
        chain: facts.chain,
        block: n,
        source: facts.source,
        writes: facts.total,
        filtered: facts.filtered,
        skipped_shadowed: facts.stats.skipped_shadowed,
        sampled: facts.sampled,
        sampled_ok: facts.sampled_ok,
        ms: outcome.duration_ms,
    });
    plane.facts.publish(Fact::CursorAdvanced {
        chain: facts.chain,
        block: n,
    });
    tracing::info!(
        block = n,
        source = facts.source,
        bal_writes = facts.total,
        applied = facts.applied,
        filtered = outcome.filtered,
        skipped_shadowed = outcome.skipped_shadowed,
        sampled_ok = outcome.sampled_ok,
        sampled = outcome.sampled,
        ms = outcome.duration_ms,
        "parent diffs applied to T"
    );
}

/// Advance the devnet cursor to `n` — the applier's and the late-completion
/// path's shared step: the block's hash is read from the chain and the
/// cursor written through the spine's one cursor step.
async fn advance_cursor(ctx: &Ctx, n: u64) -> Result<()> {
    let block = ctx.anvil_t.block_by_number(n, diorama_anvil::Txs::Hashes).await?;
    diorama_devnet::state::advance_cursor(&ctx.plane.state, &block, n)?;
    Ok(())
}

/// The applier's per-block gauges (one place; the late-completion path sets
/// its own subset).
fn record_apply_metrics(ctx: &Ctx, n: u64, facts: &ApplyFacts, duration_ms: u64) {
    ctx.mainnet_metrics.set(&ctx.mainnet_metrics.cursor, n);
    ctx.mainnet_metrics.inc(&ctx.mainnet_metrics.blocks_applied);
    ctx.mainnet_metrics.set(&ctx.mainnet_metrics.bal_writes, facts.applied as u64);
    ctx.plane
        .metrics
        .set(&ctx.mainnet_metrics.bal_filtered_out, facts.stats.not_watched as u64);
    ctx.plane
        .metrics
        .set(&ctx.mainnet_metrics.bal_skipped_shadowed, facts.stats.skipped_shadowed as u64);
    ctx.mainnet_metrics.set(&ctx.mainnet_metrics.last_apply_ms, duration_ms);
    ctx.mainnet_metrics.set(&ctx.mainnet_metrics.head, facts.t_latest);
}

/// Whether the apply is a drift-log candidate: a re-apply of a rewound
/// range, or a sampled-parity mismatch (the venue's `fresh_emitters` bound
/// pins the other side of the same "only anomalies land in drift.jsonl"
/// rule).
fn is_anomalous(facts: &ApplyFacts, outcome: &ApplyOutcome) -> bool {
    facts.reapply == Reapply::RolledBack || outcome.sampled_ok < outcome.sampled
}

/// Divergence log, not a flight recorder: only anomalies land here (the
/// re-apply of a rewound range, or a sampled-parity mismatch). Counts live in
/// the metrics.
fn log_drift_if_anomalous(
    drift: &Drift,
    n: u64,
    facts: &ApplyFacts,
    outcome: &ApplyOutcome,
) -> Result<()> {
    if !is_anomalous(facts, outcome) {
        return Ok(());
    }
    drift.append(&TDriftEntry {
        chain: facts.chain,
        block: n,
        source: facts.source,
        bal_writes: facts.total,
        applied: facts.applied,
        filtered: outcome.filtered,
        not_watched: facts.stats.not_watched,
        skipped_shadowed: facts.stats.skipped_shadowed,
        sampled: outcome.sampled,
        sampled_ok: outcome.sampled_ok,
        duration_ms: outcome.duration_ms,
    })?;
    Ok(())
}

/// Late completion after a timeout: the writes are already in (the journal
/// has the block). Cursor + metrics only.
async fn late_completion(ctx: &Ctx, n: u64, t_head: u64, started: Instant) -> Result<ApplyOutcome> {
    tracing::warn!(block = n, head = t_head, "diffs already applied; finalizing late completion");
    advance_cursor(ctx, n).await?;
    ctx.mainnet_metrics.inc(&ctx.mainnet_metrics.blocks_applied);
    ctx.mainnet_metrics.set(&ctx.mainnet_metrics.head, t_head);
    ctx.mainnet_metrics.set(&ctx.mainnet_metrics.cursor, n);
    ctx.plane.facts.publish(Fact::CursorAdvanced {
        chain: ctx.mirror.label(),
        block: n,
    });
    Ok(ApplyOutcome {
        source: "late",
        writes: 0,
        filtered: 0,
        skipped_shadowed: 0,
        sampled: 0,
        sampled_ok: 0,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Align block context + mine, so `latest` views (Aave-style time
/// extrapolation) match the parent at N. Always mined: the parent's effects
/// (diffs + context) must land in their own block, and the cursor advances
/// once per parent regardless of how many devnet blocks T mined.
///
/// Align to the parent, plus the devnet's time offset. Clamped to last+1 so a
/// deliberate `evm_increaseTime` (or a lowered offset) can never make the
/// timestamp non-monotonic and fail the alignment - anvil would then mine with
/// its own default and the context would silently drift (measured).
/// The applied block's context: the parent's timestamp + the plane's live
/// offset, clamped to last+1 so a deliberate jump can never go non-monotonic
/// (`ClockPolicy::AlignToSource`; the venue's clock is native and nothing
/// re-aligns it).
async fn align_context(ctx: &Ctx, header: &Header) -> Result<()> {
    let offset = ctx.plane.time_offset.load(Ordering::Relaxed);
    let last_ts = ctx
        .anvil_t
        .block_by_number(ctx.anvil_t.block_number().await?, diorama_anvil::Txs::Hashes)
        .await?
        .header
        .timestamp;
    let ts = (header.timestamp + offset).max(last_ts.saturating_add(1));
    if let Err(e) = ctx.anvil_t.set_next_block_context(ts, header).await {
        tracing::warn!(error = %e, "context alignment failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(reapply: Reapply) -> ApplyFacts {
        ApplyFacts {
            chain: "t",
            source: "bal",
            total: 1,
            applied: 1,
            filtered: 1,
            stats: FilterStats::default(),
            sampled: 2,
            sampled_ok: 2,
            reapply,
            reapply_from: None,
            t_latest: 1,
            started: Instant::now(),
        }
    }

    fn outcome(sampled: usize, sampled_ok: usize) -> ApplyOutcome {
        ApplyOutcome {
            source: "bal",
            writes: 1,
            filtered: 1,
            skipped_shadowed: 0,
            sampled,
            sampled_ok,
            duration_ms: 1,
        }
    }

    /// Only anomalies are drift-log candidates: a clean apply is not, a
    /// re-apply of a rewound range is, and a sampled-parity mismatch is.
    #[test]
    fn only_anomalies_are_logged() {
        assert!(!is_anomalous(&facts(Reapply::Forward), &outcome(2, 2)));
        assert!(is_anomalous(&facts(Reapply::RolledBack), &outcome(2, 2)));
        assert!(is_anomalous(&facts(Reapply::Forward), &outcome(2, 1)));
    }
}
