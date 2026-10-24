//! Applier: keeps T's watch-list state fresh by applying parent BALs.
//!
//! Per parent block (plan §4.2): prepend devnet txs -> compose the parent's
//! BAL (the spine's generic composer: native on reth, the trace rung as
//! fallback) -> COW it against the devnet's own writes -> read before-values
//! -> batched cheatcode writes -> align block context -> mine -> journal ->
//! sampled parity + probe parity. Each phase is one function; `apply_block` is
//! the sequence.
use crate::ctx::Ctx;
use crate::drift::Drift;
use crate::error::{Error, Result};
use alloy_primitives::U256;
use alloy_rpc_types_eth::Header;
use mirror_anvil::{Rpc, Write};
use mirror_devnet::cow::FilterStats;
use mirror_devnet::facts::Fact;
use mirror_devnet::journal::{entry_for_write, BeforeValue, JournalEntry};
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::plane::Plane;
use mirror_devnet::probes::ProbeRunner;
use mirror_devnet::spine::{ClockPolicy, MinePolicy, ProbeSchedule};
use mirror_devnet::state::cursor_from;
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
async fn write_and_mine(ctx: &Ctx, header: &Header, filtered: &[mirror_anvil::Write]) -> Result<usize> {
    let applied =
        mirror_anvil::apply_writes(&ctx.anvil_t, filtered, ctx.engine.loops.chunk_size).await?;
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
    entries: &[mirror_devnet::journal::JournalEntry],
    t_head: u64,
) -> Result<()> {
    ctx.plane.journal.record(n, entries)?;
    ctx.plane.journal.prune(n);
    if let Err(e) = scan_own_blocks(ctx, t_head).await {
        tracing::warn!(error = %e, "inline shadow scan incomplete");
    }
    Ok(())
}

/// What the apply's entry checks decided.
enum ApplyEntry {
    /// The block's writes are already in (a timed-out apply that finished):
    /// the outcome is the late-completion one.
    Late(ApplyOutcome),
    /// Proceed, with the re-apply state the applier must carry.
    Proceed {
        needs_reapply: bool,
        reapply: Option<(u64, u64)>,
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
    let reapply = *ctx.plane.reapply_from.lock()?;
    let needs_reapply = matches!(reapply, Some((from, _)) if n >= from);
    let journaled = ctx.plane.journal.has_entry(n)?;
    if t_head >= n && journaled && !needs_reapply {
        return Ok(ApplyEntry::Late(late_completion(ctx, n, t_head, started).await?));
    }
    if t_head + 1 < n {
        return Err(Error::ApplierBehind { t_head, n });
    }
    Ok(ApplyEntry::Proceed {
        needs_reapply,
        reapply,
    })
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
    let watermark = ctx
        .plane
        .shadow_scanned_upto
        .load(std::sync::atomic::Ordering::Relaxed);
    scan_own_blocks(ctx, watermark).await
}

pub(crate) async fn apply_block(ctx: &Ctx, n: u64) -> Result<ApplyOutcome> {
    let started = Instant::now();
    let header = ctx.mirror.source().header(n).await?;

    let t_head = ctx.anvil_t.block_number().await?;
    // The base this apply started under: a re-pin racing the (slow) fetch
    // below would land parent n's writes on the *new* base — watched state
    // regressed to an old height and a block mined at the wrong height
    // (measured: the applier then sat silent on the resetting anvil).
    // `plan_and_check_base` aborts on the move; the next pass reconciles (the
    // cursor sits below the base → the epoch rule).
    let base = ctx.anvil_t.fork_block().await?;
    let (needs_reapply, reapply) = match apply_entry(ctx, n, t_head, started).await? {
        ApplyEntry::Late(outcome) => return Ok(outcome),
        ApplyEntry::Proceed {
            needs_reapply,
            reapply,
        } => (needs_reapply, reapply),
    };

    prepend_devnet_txs(&ctx.anvil_t, n, ctx.mirror.policies().prepend_rounds).await?;
    mark_own_writes(ctx).await?;

    let plan = plan_and_check_base(ctx, n, base).await?;
    let entries = read_before_values(&ctx.anvil_t, &plan.writes, t_head).await?;

    let applied = write_and_mine(ctx, &header, &plan.writes).await?;
    journal_and_scan(ctx, n, &entries, t_head).await?;
    // The parent-stream continuity marker (reorg detection reads it).
    ctx.set_source_last_hash(header.hash)?;
    ctx.set_source_last_block(n)?;

    let t_latest = ctx.anvil_t.block_number().await?;
    let (sampled, sampled_ok) = sample_parity(ctx, n, &plan.writes, t_latest).await;
    if ctx.mirror.policies().probes == ProbeSchedule::PerBlock {
        t_convergence_probes(ctx, n).await?;
    }

    finalize(
        ctx,
        n,
        ApplyFacts {
            chain: ctx.mirror.label(),
            source: if needs_reapply { "reapply" } else { plan.kind.as_str() },
            total: plan.total,
            applied,
            filtered: plan.writes.len(),
            stats: plan.filter,
            sampled,
            sampled_ok,
            needs_reapply,
            reapply,
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
) -> Result<mirror_devnet::bal::WindowPlan> {
    // The shared pipeline half (compose → COW → flatten), then the check: the
    // compose awaits for seconds, and a re-pin racing it must abort before any
    // write (the next pass reconciles).
    let plan = mirror_devnet::bal::plan(
        ctx.mirror.follow_source().as_ref(),
        ctx.target.as_ref(),
        ctx.mirror.scope(),
        &ctx.plane.shadow,
        mirror_devnet::bal::Window::single(n),
    )
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
    needs_reapply: bool,
    reapply: Option<(u64, u64)>,
    t_latest: u64,
    started: Instant,
}

/// Cursor + metrics + drift + log for one applied block.
async fn finalize(ctx: &Ctx, n: u64, facts: ApplyFacts) -> Result<ApplyOutcome> {
    advance_cursor(ctx, n).await?;
    let duration_ms = facts.started.elapsed().as_millis() as u64;
    record_apply_metrics(ctx, n, &facts, duration_ms);
    clear_reapply(&ctx.plane, n, facts.reapply)?;

    let outcome = ApplyOutcome {
        source: facts.source,
        writes: facts.total,
        filtered: facts.filtered,
        skipped_shadowed: facts.stats.skipped_shadowed,
        sampled: facts.sampled,
        sampled_ok: facts.sampled_ok,
        duration_ms,
    };
    log_drift_if_anomalous(&ctx.drift, n, &facts, &outcome)?;
    ctx.plane.facts.publish(Fact::BlockApplied {
        chain: facts.chain,
        block: n,
        source: facts.source,
        writes: facts.total,
        filtered: facts.filtered,
        skipped_shadowed: facts.stats.skipped_shadowed,
        sampled: facts.sampled,
        sampled_ok: facts.sampled_ok,
        ms: duration_ms,
    });
    ctx.plane.facts.publish(Fact::CursorAdvanced {
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
        ms = duration_ms,
        "parent diffs applied to T"
    );
    Ok(outcome)
}

/// Advance the devnet cursor to `n` — the applier's and the late-completion
/// path's shared step: the cursor block's hash is read from the chain and the
/// state persisted.
async fn advance_cursor(ctx: &Ctx, n: u64) -> Result<()> {
    let t_block = ctx.anvil_t.block_by_number(n, false).await?;
    let first_block = ctx.cursor_t()?.map(|c| c.first_block).unwrap_or(n);
    ctx.set_cursor_t(cursor_from(&t_block, first_block))?;
    ctx.plane.save_state()?;
    Ok(())
}

/// The applier's per-block gauges (one place; the late-completion path sets
/// its own subset).
fn record_apply_metrics(ctx: &Ctx, n: u64, facts: &ApplyFacts, duration_ms: u64) {
    ctx.eth_metrics.set(&ctx.eth_metrics.cursor, n);
    ctx.eth_metrics.inc(&ctx.eth_metrics.blocks_applied);
    ctx.eth_metrics.set(&ctx.eth_metrics.bal_writes, facts.applied as u64);
    ctx.plane
        .metrics
        .set(&ctx.eth_metrics.bal_filtered_out, facts.stats.not_watched as u64);
    ctx.plane
        .metrics
        .set(&ctx.eth_metrics.bal_skipped_shadowed, facts.stats.skipped_shadowed as u64);
    ctx.eth_metrics.set(&ctx.eth_metrics.last_apply_ms, duration_ms);
    ctx.eth_metrics.set(&ctx.eth_metrics.head, facts.t_latest);
}

/// The rewound range is fully re-applied: resume normal late-completion skips.
fn clear_reapply(plane: &Plane, n: u64, reapply: Option<(u64, u64)>) -> Result<()> {
    if let Some((_, until)) = reapply {
        if n >= until {
            *plane.reapply_from.lock()? = None;
            tracing::info!(block = n, until, "rewound range re-applied");
        }
    }
    Ok(())
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
    if !(facts.needs_reapply || outcome.sampled_ok < outcome.sampled) {
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
    })
}

/// Late completion after a timeout: the writes are already in (the journal
/// has the block). Cursor + metrics only.
async fn late_completion(ctx: &Ctx, n: u64, t_head: u64, started: Instant) -> Result<ApplyOutcome> {
    tracing::warn!(block = n, head = t_head, "diffs already applied; finalizing late completion");
    advance_cursor(ctx, n).await?;
    ctx.eth_metrics.inc(&ctx.eth_metrics.blocks_applied);
    ctx.eth_metrics.set(&ctx.eth_metrics.head, t_head);
    ctx.eth_metrics.set(&ctx.eth_metrics.cursor, n);
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

/// Before-values for the journal, read at T's current head. A read that fails
/// fails the apply: a journal with a missing before-value would let a later
/// rewind report success while leaving that key un-restored.
async fn read_before_values(
    anvil_t: &Rpc,
    filtered: &[Write],
    t_head: u64,
) -> Result<Vec<JournalEntry>> {
    let mut entries = Vec::with_capacity(filtered.len());
    for w in filtered {
        let before = match w {
            Write::Storage { address, slot, .. } => BeforeValue::Storage(U256::from_be_bytes(
                anvil_t.storage_at(*address, *slot, t_head).await?.0,
            )),
            Write::Balance { address, .. } => {
                BeforeValue::Balance(anvil_t.balance_at(*address, t_head).await?)
            }
            Write::Nonce { address, .. } => {
                BeforeValue::Nonce(anvil_t.nonce_at(*address, t_head).await?)
            }
            Write::Code { address, .. } => {
                BeforeValue::Code(anvil_t.code_at(*address, t_head).await?)
            }
        };
        entries.push(entry_for_write(w, before)?);
    }
    Ok(entries)
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
        .block_by_number(ctx.anvil_t.block_number().await?, false)
        .await?
        .header
        .timestamp;
    let ts = (header.timestamp + offset).max(last_ts.saturating_add(1));
    if let Err(e) = ctx.anvil_t.set_next_block_context(ts, header).await {
        tracing::warn!(error = %e, "context alignment failed");
    }
    Ok(())
}

/// Scan T's blocks from `from` up to the current head (the apply's own devnet
/// blocks included). Anchored at the pin like every scan: a re-pin mid-apply
/// must not walk the source's blocks.
async fn scan_own_blocks(ctx: &Ctx, from: u64) -> Result<()> {
    let now = ctx.anvil_t.block_number().await?;
    let pin = ctx.anvil_t.fork_block().await?.unwrap_or(0);
    let from = mirror_devnet::scan::scan_anchor(from, pin);
    let walk = mirror_devnet::scan::scan_range(&ctx.plane, &ctx.anvil_t, from, now).await?;
    ctx.eth_metrics.record_scan(
        &walk,
        &ctx.eth_metrics.shadow_scanned_blocks,
        &ctx.eth_metrics.shadow_skipped_empty_blocks,
    );
    Ok(())
}

/// One sampled storage-parity reading vs reth at n (drift lens). T's *height*
/// drifts ahead of the parent height (devnet prepend blocks), so compare T's
/// latest block against reth at n - reading T by height compared unrelated
/// blocks and produced ~200 false mismatches per 30 min. With
/// `verifier.full_sweep` every applied storage key is checked, not just the
/// sample. Returns `(sampled, sampled_ok)`.
async fn sample_parity(ctx: &Ctx, n: u64, filtered: &[Write], t_latest: u64) -> (usize, usize) {
    let full = ctx.engine.verifier.full_sweep;
    let mut sample: Vec<&Write> = filtered
        .iter()
        .filter(|w| matches!(w, Write::Storage { .. }))
        .collect();
    if !full {
        sample.truncate(ctx.engine.verifier.drift_sample_keys);
    }
    let mut sampled_ok = 0usize;
    for w in &sample {
        if let Write::Storage { address, slot, .. } = w {
            match (
                ctx.anvil_t.storage_at(*address, *slot, t_latest).await,
                ctx.reth.storage_at(*address, *slot, n).await,
            ) {
                (Ok(a), Ok(b)) if a == b => sampled_ok += 1,
                (a, b) => {
                    if full {
                        ctx.eth_metrics.inc(&ctx.eth_metrics.full_sweep_mismatches);
                    }
                    tracing::warn!(
                        block = n,
                        address = %address,
                        slot = %slot,
                        "drift sample mismatch: t={a:?} reth={b:?}"
                    );
                }
            }
        }
    }
    (sample.len(), sampled_ok)
}

/// Probe parity for T (the plan's invariant: "probe parity on T after context
/// alignment"). Devnet-owned targets are skipped - their state is the
/// devnet's, not mainnet's.
async fn t_convergence_probes(ctx: &Ctx, n: u64) -> Result<()> {
    let probes = ProbeRunner::devnet(
        &ctx.engine.verifier.probes,
        &ctx.plane.shadow,
        ctx.plane.time_offset.load(Ordering::Relaxed) != 0,
    )
    .parity(&ctx.reth, n, &ctx.anvil_t)
    .await?;
    ctx.eth_metrics.add(&ctx.eth_metrics.t_convergence_ok, probes.ok as u64);
    ctx.eth_metrics
        .add(&ctx.eth_metrics.t_convergence_failed, probes.failed as u64);
    ctx.eth_metrics
        .set(&ctx.eth_metrics.t_convergence_last_failed, probes.failed as u64);
    ctx.eth_metrics
        .add(&ctx.eth_metrics.t_probe_skipped, probes.skipped as u64);
    Ok(())
}
