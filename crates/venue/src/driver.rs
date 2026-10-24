//! The fork driver: V's lifecycle from boot to epoch to steady state.
//!
//! V is a fork-mode anvil (chain id 999). The driver's jobs (plan §7.2):
//!
//! - **boot asserts**: V answers, is the venue chain, and automine is off;
//! - **reset detection**: the mirror bundle's detector — V's chain-instance
//!   canary no longer matches the pin (a re-pin moved the base, an OOM restart
//!   lost the word) — checked on every pass; V's cursor hashes cannot tell
//!   (below the pin, heights resolve to the source's own blocks, ADR 0002);
//! - **the epoch rule**: a fresh pin means the base moved (drop marks and
//!   values: a new epoch); the same pin means the recorded writes are still
//!   coherent (re-apply them and keep the marks);
//! - **probes-at-pin**: V vs the fork source at the pin, devnet-owned targets
//!   skipped and counted;
//! - **the steady-state scan**: every devnet tx's writes become shadowed
//!   (COW), so a hand-back can restore the source's truth and probes can skip
//!   what the devnet owns;
//! - **the source follow**: compose the window since the last composed source
//!   block and land the changed keys on V under the COW filter — the venue's
//!   counterpart of the ethereum applier's parent stream.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::reset::{read_canary, write_canary};
use mirror_anvil::Rpc;
use mirror_devnet::diff::filter_watched;
use mirror_devnet::facts::{Fact, ResetWitness};
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::probes::{ProbeOutcome, ProbeRunner};
use mirror_devnet::scan;
use mirror_devnet::spine::ResetSignal;
use mirror_devnet::state::{cursor_from, ChainCursor};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// HyperEVM's chain id on mainnet (998 on testnet). A code fact.
const VENUE_CHAIN_ID: u64 = 999;

/// Boot: wait for V, assert the chain, reconcile the registry, run the at-pin
/// probes. A boot fault is fatal (the bin exits): a driver over the wrong
/// chain or a broken registry must not serve.
pub async fn boot(ctx: &Ctx) -> Result<()> {
    // 2 minutes: a fork-mode anvil comes up in seconds; a coordinated restart
    // (the init container re-resolving the pin, the fork source slow) is the
    // only normal delay — and a crash-loop is worse than a bounded wait.
    ctx.v
        .wait_until_up("anvil-v", Duration::from_secs(120))
        .await?;
    assert_venue_chain(&ctx.v).await?;
    let pin = reconcile(ctx).await?;
    probes_at_pin(ctx, pin).await?;
    Ok(())
}

/// The steady-state loop (boot has already run).
pub async fn run(ctx: Arc<Ctx>) -> Result<()> {
    let poll = Duration::from_millis(ctx.engine.loops.poll_interval_ms.max(200));
    loop {
        if ctx.plane.is_halted() {
            tokio::time::sleep(poll).await;
            continue;
        }
        match pass(&ctx).await {
            Ok(Pass::Steady) => {}
            Ok(Pass::Reset) => recover_from_reset(&ctx).await,
            Err(e) => tracing::warn!(error = %e, "venue driver pass failed; will retry"),
        }
        tokio::time::sleep(poll).await;
    }
}

/// A reset under the driver: re-assert the chain's identity, run the epoch
/// rule, and re-run the at-pin probes. Failures are logged, not fatal — the
/// next pass retries (the canary keeps reading "moved" until the reconcile
/// succeeds).
async fn recover_from_reset(ctx: &Ctx) {
    tracing::warn!("V was reset under the driver - reconciling (epoch rule)");
    if let Err(e) = assert_venue_chain(&ctx.v).await {
        tracing::warn!(error = %e, "venue chain assert failed after a reset");
    }
    match reconcile(ctx).await {
        Ok(pin) => {
            if let Err(e) = probes_at_pin(ctx, pin).await {
                tracing::warn!(error = %e, "at-pin probes failed after a reset");
            }
        }
        Err(e) => tracing::warn!(error = %e, "reconcile failed; will retry"),
    }
}

/// One steady-state pass. Returns [`Pass::Reset`] when V's chain-instance
/// canary no longer matches the pin (the epoch rule must run).
enum Pass {
    Steady,
    Reset,
}

async fn pass(ctx: &Ctx) -> Result<Pass> {
    // V's block production (T's is the applier's): the pool is mined once per
    // pass, so the bridge's boot txs and relay mints land without waiting for
    // a harness mine; an idle chain stays static.
    if let Err(e) = mine_pending(ctx).await {
        tracing::warn!(error = %e, "pending-tx mine failed; the next pass retries");
    }
    let head = ctx.v.block_number().await?;
    ctx.venue_metrics.set(&ctx.venue_metrics.head, head);

    // Reset detection: the bundle's detector (the pin canary — see
    // `crate::reset`): anything but Steady runs the epoch rule.
    if !matches!(
        ctx.mirror.reset().detect(&ctx.v).await?,
        ResetSignal::Steady
    ) {
        return Ok(Pass::Reset);
    }

    let cursor = ctx.cursor_v()?;
    if let Some(c) = &cursor {
        ctx.venue_metrics.set(&ctx.venue_metrics.cursor, c.block);
        ctx.venue_metrics
            .set(&ctx.venue_metrics.pin_age_blocks, head.saturating_sub(c.first_block) + 1);
    }

    scan_and_advance(ctx, cursor.as_ref(), head).await?;

    // Follow the source: compose the window since the last composed source
    // block and land the changed keys on V (COW). After the scan, so the
    // devnet's own writes are marked before the filter reads them.
    follow(ctx).await?;

    // A journal rewind on the venue restores before-values without moving the
    // head; the range marker clears once the head is past it (there is no
    // parent diff to re-apply - the devnet re-creates its writes). The value
    // is copied out first: a guard held by the `if let` scrutinee across the
    // body's second `lock()` would self-deadlock (std mutexes are not
    // reentrant) - the mainnet's applier copies the same way.
    let reapply = *ctx.plane.reapply_from.lock()?;
    if let Some((_, until)) = reapply {
        if head >= until {
            *ctx.plane.reapply_from.lock()? = None;
            tracing::info!(block = head, until, "rewound range passed");
        }
    }
    Ok(Pass::Steady)
}

/// Scan V's own blocks since `cursor` and persist the walk: the shadow
/// registry, the journal prune and the cursor advance land together (a crash
/// mid-way re-scans; the scan is idempotent). The watermark starts from the
/// persisted cursor, so blocks the devnet mined while the driver was down
/// are scanned too, and it never reaches at or below the fork pin (the
/// bridge anchors the same way): those blocks are the source's, their txs
/// are not in this chain, and tracing one fails — anvil answers an unknown
/// tx with `-32601 Method not found` (measured), which would stall the
/// watermark below the pin and livelock the driver after a rewind or a
/// re-pin.
async fn scan_and_advance(ctx: &Ctx, cursor: Option<&ChainCursor>, head: u64) -> Result<()> {
    let first = cursor.map(|c| c.block).unwrap_or(head);
    let walk = scan::scan_to(
        &ctx.plane,
        &ctx.v,
        ctx.pin.load(Ordering::Relaxed),
        head,
        first,
    )
    .await?;
    if walk.walked == 0 {
        return Ok(());
    }
    ctx.venue_metrics.record_scan(
        &walk,
        &ctx.venue_metrics.scanned_blocks_total,
        &ctx.venue_metrics.skipped_empty_blocks_total,
    );
    ctx.plane.facts.publish(Fact::ShadowScanned {
        chain: ctx.mirror.label(),
        blocks: walk.walked,
        empty: walk.empty,
        shadowed: walk.shadowed,
    });
    ctx.plane.save_shadow()?;
    // Ring the journal like the applier does on T: the rewind window is the
    // same 1024 blocks on both mirrors (files older than head - keep are
    // pruned; the venue's journal holds its own writes — the deal).
    ctx.plane.journal.prune(head);
    let block = ctx.v.block_by_number(head, false).await?;
    let first_block = cursor.map(|c| c.first_block).unwrap_or(head);
    ctx.set_cursor_v(cursor_from(&block, first_block))?;
    ctx.plane.save_state()?;
    ctx.venue_metrics.set(&ctx.venue_metrics.cursor, head);
    ctx.plane.facts.publish(Fact::CursorAdvanced {
        chain: ctx.mirror.label(),
        block: head,
    });
    Ok(())
}

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
async fn follow(ctx: &Ctx) -> Result<()> {
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
/// record the live parity of what was applied.
async fn compose_window(ctx: &Ctx, from: u64, to: u64) -> Result<Window> {
    let diff = ctx.mirror.composer().compose(from, to).await?;
    let total = diff.writes.len();
    let (writes, stats) = {
        let shadow = ctx.plane.shadow.lock()?;
        filter_watched(&ctx.watch, &shadow, diff.writes)
    };
    let applied = if writes.is_empty() {
        0
    } else {
        mirror_anvil::apply_writes(&ctx.v, &writes, ctx.engine.loops.chunk_size).await?
    };
    let (sampled, sampled_ok) = match ctx.mirror.composer().verify(&writes).await? {
        Some((ok, failed)) => (ok + failed, ok),
        None => (0, 0),
    };
    Ok(Window {
        total,
        applied,
        filtered: writes.len(),
        skipped_shadowed: stats.skipped_shadowed,
        sampled,
        sampled_ok,
    })
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

/// V's block production: T's producer is the applier (one block per applied
/// parent block), while V's source windows land as cheatcode writes (no block
/// per window) — so the engine mines a non-empty pool,
/// once per pass. Without it the bridge's boot txs (the attester install, the
/// wrapper deploy) and its relay mints sit in the pool forever on a chain
/// nobody else mines (measured: `attester installation failed: … was not
/// mined within 120s`); with it an idle chain stays static and the harness's
/// own explicit mines are just extra (empty) blocks.
async fn mine_pending(ctx: &Ctx) -> Result<()> {
    let status = ctx.v.txpool_status().await?;
    if status.pending == 0 {
        return Ok(());
    }
    ctx.v.anvil_mine(1).await?;
    ctx.venue_metrics.inc(&ctx.venue_metrics.mined_blocks_total);
    tracing::debug!(pending = %status.pending, "mined V's pending transactions");
    Ok(())
}

/// Wait for V to answer (a coordinated restart must not become
/// CrashLoopBackOff).
/// V must be the venue chain with automine off. An anvil restart resets
/// automine to its default (on) — measured — so the discipline is re-asserted
/// on every boot and after every reset.
async fn assert_venue_chain(v: &Rpc) -> Result<()> {
    let got = v.chain_id().await?;
    if got != VENUE_CHAIN_ID {
        return Err(Error::WrongChain {
            got,
            want: VENUE_CHAIN_ID,
        });
    }
    v.anvil_set_automine(false).await?;
    Ok(())
}

/// Reconcile the registry with V's base: check the chain-instance canary, run
/// the epoch rule when the instance changed, and anchor the cursor. Returns
/// the fork pin.
async fn reconcile(ctx: &Ctx) -> Result<Option<u64>> {
    let pin = pin_read(ctx).await?;

    // The canary tells whether this is still the chain instance the registry
    // was built against (see CANARY_ADDRESS): present and equal to the pin
    // means steady, nothing to reconcile.
    if read_canary(&ctx.v).await? == pin {
        return Ok(pin);
    }
    ctx.plane.facts.publish(Fact::ResetDetected {
        chain: ctx.mirror.label(),
        witness: ResetWitness::Pin {
            expected: ctx.plane.state.lock()?.data.epoch_pin,
            found: pin,
        },
    });
    run_epoch_rule(ctx, pin).await?;
    anchor_cursor(ctx, pin).await?;
    Ok(pin)
}

/// The pin read, its gauge, and the unshadow anchor. Fail-closed on the read:
/// without it the epoch decision would default to "same pin" and re-apply
/// writes across a pin change.
async fn pin_read(ctx: &Ctx) -> Result<Option<u64>> {
    let pin = ctx.v.anvil_node_info().await?.fork_config.fork_block_number;
    ctx.venue_metrics
        .set(&ctx.venue_metrics.fork_pin, pin.unwrap_or(0));
    if let Some(p) = pin {
        // The unshadow source's anchor: V's base state is the source's state
        // at the pin.
        ctx.pin.store(p, Ordering::Relaxed);
    }
    Ok(pin)
}

/// The epoch rule (shared with the ethereum engine, `mirror-devnet::epoch`):
/// a fresh pin drops marks and values, the same pin re-applies the recorded
/// writes; the journal goes either way. The transition's logs are the shared
/// module's; the counters are V's (epoch transitions, not resets found).
async fn run_epoch_rule(ctx: &Ctx, pin: Option<u64>) -> Result<()> {
    match mirror_devnet::epoch::reconcile(&ctx.plane, &ctx.v, ctx.engine.loops.chunk_size, pin)
        .await?
    {
        mirror_devnet::epoch::Epoch::Steady => {}
        mirror_devnet::epoch::Epoch::Cleared => {
            ctx.venue_metrics.inc(&ctx.venue_metrics.epochs_total);
            ctx.plane
                .facts
                .publish(Fact::EpochChanged { chain: ctx.mirror.label(), pin });
        }
        mirror_devnet::epoch::Epoch::Restored { restored } => {
            ctx.venue_metrics.inc(&ctx.venue_metrics.epochs_total);
            ctx.venue_metrics
                .add(&ctx.venue_metrics.restore_writes_total, restored as u64);
            ctx.plane
                .facts
                .publish(Fact::EpochChanged { chain: ctx.mirror.label(), pin });
        }
    }
    Ok(())
}

/// The registry now reflects V's base: anchor the cursor at the pin (V's
/// state *is* the source's state at the pin), and let the steady-state scan
/// advance it as the devnet mines. The canary is stamped last — it witnesses
/// the base the registry was just rebuilt against.
async fn anchor_cursor(ctx: &Ctx, pin: Option<u64>) -> Result<()> {
    let watermark = if let Some(p) = pin {
        let block = ctx.v.block_by_number(p, false).await?;
        ctx.set_cursor_v(cursor_from(&block, p))?;
        // The follow cursor starts at the pin: V's state *is* the source's
        // state at the pin, so the next window composes pin+1 onward. The
        // hash is the source's own block hash (the continuity check compares
        // it to the next block's parent).
        let source_block = ctx.source_rpc.block_by_number(p, false).await?;
        ctx.set_source_last_block(p)?;
        ctx.set_source_last_hash(source_block.header.hash)?;
        p
    } else {
        tracing::warn!("anvil-v has no fork pin: epoch decisions are pin-based and cannot run");
        ctx.v.block_number().await?
    };
    ctx.plane
        .shadow_scanned_upto
        .store(watermark, Ordering::Relaxed);
    ctx.plane.save_state()?;
    if let Some(p) = pin {
        write_canary(&ctx.v, p).await?;
    }
    Ok(())
}

/// Run the at-pin probes: V vs the fork source at the pin, devnet-owned
/// targets skipped and counted (COW means that state is the devnet's).
///
/// The public source serves its *latest* state for `eth_call` (measured: V's
/// fork at block N differs from the source's answer at N, while V's state IS
/// the source's state at N — the block parameter is ignored, or the retention
/// window is shorter than a re-pin interval). The comparison is therefore only
/// meaningful while the pin *is* the source's head — i.e. right after a
/// `latest` pin (boot, re-pin). An older pin is skipped and counted, never
/// alarmed: the source cannot serve it, and a mismatch would be a lie.
/// Run the at-pin probes now (the admin route's entry: boot and re-pin run
/// them automatically; this is the on-demand trigger, e.g. after a hand-back).
pub async fn probes_now(ctx: &Ctx) -> Result<ProbeOutcome> {
    let pin = ctx.v.anvil_node_info().await?.fork_config.fork_block_number;
    probes_at_pin(ctx, pin).await
}

/// Run the at-pin probe set and return the run's outcome (the cumulative
/// counters are updated here — an at-pin run is the only probe source on the
/// venue side). The route reports the run; `/metrics` carries the totals.
pub(crate) async fn probes_at_pin(ctx: &Ctx, pin: Option<u64>) -> Result<ProbeOutcome> {
    let skipped_all = || ProbeOutcome {
        skipped: ctx.engine.verifier.probes.len(),
        ..Default::default()
    };
    let Some(pin) = pin else {
        tracing::warn!("at-pin probes skipped: V has no fork pin");
        return Ok(skipped_all());
    };
    let source_head = ctx.source_rpc.block_number().await?;
    if pin != source_head {
        ctx.venue_metrics
            .add(&ctx.venue_metrics.probe_skipped, ctx.engine.verifier.probes.len() as u64);
        tracing::info!(
            pin,
            source_head,
            skipped = ctx.engine.verifier.probes.len(),
            "at-pin probes skipped: the source serves its latest state (measured); the pin is not the source head"
        );
        return Ok(skipped_all());
    }
    // The venue has no clock offset (the plane's field is shape-only: nothing
    // re-aligns V's clock), so the probes never take the offset path.
    let probes = ProbeRunner::devnet(&ctx.engine.verifier.probes, &ctx.plane.shadow, false)
        .parity(&ctx.source_rpc, pin, &ctx.v)
        .await?;
    let out = count_probes(ctx, probes, pin, source_head).await?;
    ctx.plane.facts.publish(Fact::ProbesRun {
        chain: ctx.mirror.label(),
        at: pin,
        ok: out.ok,
        failed: out.failed,
        skipped: out.skipped,
    });
    Ok(out)
}

/// Fold one run's outcome into the counters and report it. A run that raced a
/// source advance (the chain moved mid-run, so the source's state belongs to a
/// later block than the pin) is counted skipped — a race, not a divergence.
async fn count_probes(
    ctx: &Ctx,
    probes: ProbeOutcome,
    pin: u64,
    source_head: u64,
) -> Result<ProbeOutcome> {
    if probes.failed > 0 {
        let head_now = ctx.source_rpc.block_number().await?;
        if head_now != source_head {
            ctx.venue_metrics
                .add(&ctx.venue_metrics.probe_ok, probes.ok as u64);
            ctx.venue_metrics
                .add(&ctx.venue_metrics.probe_skipped, (probes.failed + probes.skipped) as u64);
            tracing::info!(
                pin,
                source_head,
                head_now,
                "at-pin probes raced a source advance; the run is counted skipped"
            );
            return Ok(ProbeOutcome {
                ok: probes.ok,
                skipped: probes.failed + probes.skipped,
                ..Default::default()
            });
        }
    }
    ctx.venue_metrics
        .add(&ctx.venue_metrics.probe_ok, probes.ok as u64);
    ctx.venue_metrics
        .add(&ctx.venue_metrics.probe_failed, probes.failed as u64);
    ctx.venue_metrics
        .add(&ctx.venue_metrics.probe_skipped, probes.skipped as u64);
    ctx.venue_metrics
        .set(&ctx.venue_metrics.probe_last_failed, probes.failed as u64);
    tracing::info!(
        pin,
        ok = probes.ok,
        failed = probes.failed,
        skipped = probes.skipped,
        "at-pin probes complete"
    );
    Ok(probes)
}
