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
//! - **the source follow** ([`crate::follow`]): compose the window since the
//!   last composed source block and land the changed keys on V under the COW
//!   filter — the venue's counterpart of the mainnet applier's parent stream.
use crate::ctx::Ctx;
use crate::error::Result;
use crate::reset::{read_canary, write_canary};
use mirror_devnet::engine::Engine as _;
use mirror_devnet::facts::Fact;
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::probes::ProbeOutcome;
use mirror_devnet::scan;
use mirror_devnet::spine::{MinePolicy, ResetSignal};
use mirror_devnet::state::{cursor_from, ChainCursor};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// HyperEVM's chain id on mainnet (998 on testnet). A code fact.
const VENUE_CHAIN_ID: u64 = 999;

/// V's name in every fault it raises (one spelling; the mainnet's is
/// `anvil-t`).
const ANVIL: &str = "anvil-v";

/// The boot wait (see `boot`): a fork-mode anvil comes up in seconds; the
/// only normal delay is a coordinated restart, and a crash-loop is worse
/// than a bounded wait.
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);

/// Boot: wait for V, assert the chain, reconcile the registry, run the at-pin
/// probes. A boot fault is fatal (the bin exits): a driver over the wrong
/// chain or a broken registry must not serve.
pub(crate) async fn boot(ctx: &Ctx) -> Result<()> {
    mirror_devnet::engine::boot_prefix(ctx, ANVIL, VENUE_CHAIN_ID, BOOT_TIMEOUT, "hl public api").await?;
    let pin = reconcile(ctx).await?;
    probes_at_pin(ctx, pin).await?;
    Ok(())
}

/// V's block production, its own cadence: the pool is drained on a tick
/// independent of the follow's window work. A slow window against the public
/// source must not delay an external tx (measured: a 143-block compose ran
/// 3m49s and the bridge's 30 s receipt window missed while its tx sat
/// pending) — T's producer is the applier's per-parent mine, V's is this
/// loop (the `MinePolicy` split).
pub(crate) async fn mine_loop(ctx: Arc<Ctx>) {
    let poll = ctx.engine.loops.poll();
    loop {
        if mirror_devnet::engine::halt_gate(&*ctx, poll).await {
            continue;
        }
        mine_pending_best_effort(&ctx).await;
        tokio::time::sleep(poll).await;
    }
}

/// The steady-state loop (boot has already run) — the venue's counterpart
/// of the applier's `apply_loop`.
pub(crate) async fn follow_loop(ctx: Arc<Ctx>) {
    let poll = ctx.engine.loops.poll();
    let mut last_status = 0u64;
    // The pass's own backoff: a failed pass — the public source throttling,
    // a transient transport fault — doubles the pause up to the cap, and a
    // pass that serves resets it. The fixed cadence alone hammered a
    // throttled endpoint (measured live: a burst tripped the public RPC's
    // limiter and the retry loop kept it hot). The cap stays at a minute:
    // the refusals measured live are transient bursts, and a longer pause
    // only wastes the good windows between them.
    let mut backoff = mirror_anvil::Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
    loop {
        // The follow's heartbeat (a minute apart, the same shape on both
        // mirrors): the steady DEBUG line only speaks for an idle chain, and
        // the live source is never at the followed height.
        mirror_devnet::engine::FollowStatus {
            source_head: ctx.venue_metrics.source_head.load(std::sync::atomic::Ordering::Relaxed),
            followed: ctx
                .venue_metrics
                .source_followed
                .load(std::sync::atomic::Ordering::Relaxed),
            applied: ctx
                .venue_metrics
                .follow_windows_total
                .load(std::sync::atomic::Ordering::Relaxed),
            paused: ctx.plane.gap_paused.load(std::sync::atomic::Ordering::Relaxed),
        }
        .log_if_due(ctx.mirror.label(), &mut last_status, 60_000);
        if mirror_devnet::engine::halt_gate(&*ctx, poll).await {
            continue;
        }
        let progress = match pass(&ctx).await {
            Ok(Pass::Steady) => mirror_anvil::Progress::Progressed,
            Ok(Pass::Reset) => {
                recover_from_reset(&ctx).await;
                mirror_anvil::Progress::Progressed
            }
            Err(e) => {
                tracing::warn!(error = %e, "venue driver pass failed; will retry");
                mirror_anvil::Progress::Stalled
            }
        };
        backoff.record(progress);
        // A pass that served sleeps the plain cadence; a failed one sleeps
        // the grown delay.
        let pause = match progress {
            mirror_anvil::Progress::Progressed => poll,
            mirror_anvil::Progress::Stalled => poll.max(backoff.delay()),
        };
        tokio::time::sleep(pause).await;
    }
}

/// A reset under the driver: re-assert the chain's identity, run the epoch
/// rule, and re-run the at-pin probes. Failures are logged, not fatal — the
/// next pass retries (the canary keeps reading "moved" until the reconcile
/// succeeds).
async fn recover_from_reset(ctx: &Ctx) {
    tracing::warn!("V was reset under the driver - reconciling (epoch rule)");
    if let Err(e) =
        mirror_devnet::engine::assert_chain(ctx, ANVIL, VENUE_CHAIN_ID, BOOT_TIMEOUT).await
    {
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
    let head = ctx.anvil_v.block_number().await?;
    ctx.venue_metrics.set(&ctx.venue_metrics.head, head);

    // Reset detection: the bundle's detector (the pin canary — see
    // `crate::reset`): anything but Steady runs the epoch rule.
    if !matches!(
        ctx.mirror.reset().detect(&ctx.anvil_v).await?,
        ResetSignal::Steady
    ) {
        return Ok(Pass::Reset);
    }

    let cursor = ctx.plane.state.cursor()?;
    record_cursor_metrics(&ctx.venue_metrics, cursor.as_ref(), head);
    scan_and_advance(ctx, cursor.as_ref(), head).await?;

    // Follow the source: compose the window since the last composed source
    // block and land the changed keys on V (COW). After the scan, so the
    // devnet's own writes are marked before the filter reads them. The
    // window's stats are the fact's and the selftest's to read; the steady
    // state ignores them.
    let _ = crate::follow::step(ctx).await?;

    let reapply = *ctx.plane.reapply_from.lock()?;
    if let Some((_, until)) = ctx.plane.clear_reapply_from(head, reapply)? {
        tracing::info!(block = head, until, "rewound range passed");
    }
    Ok(Pass::Steady)
}

/// V's block production (T's is the applier's), best-effort: the pool is
/// mined once per pass, so the bridge's boot txs and relay mints land without
/// waiting for a harness mine; an idle chain stays static. A fault is a warn —
/// the next pass retries.
async fn mine_pending_best_effort(ctx: &Ctx) {
    if let Err(e) = mine_pending(ctx).await {
        tracing::warn!(error = %e, "pending-tx mine failed; the next pass retries");
    }
}

/// The cursor's gauges: the cursor height and the pin-age reading (blocks
/// since the epoch's first).
fn record_cursor_metrics(metrics: &crate::metrics::VenueMetrics, cursor: Option<&ChainCursor>, head: u64) {
    let Some(c) = cursor else { return };
    metrics.set(&metrics.cursor, c.block);
    metrics.set(
        &metrics.pin_age_blocks,
        head.saturating_sub(c.first_block) + 1,
    );
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
        &ctx.anvil_v,
        mirror_devnet::pin::resolved_pin(&ctx.pin).unwrap_or(0),
        head,
        first,
    )
    .await?;
    if walk.walked == 0 {
        return Ok(());
    }
    ctx.record_scan(&walk);
    scan::publish(&ctx.plane, ctx.mirror.label(), &walk);
    ctx.plane.save_shadow()?;
    // Ring the journal like the applier does on T: the rewind window is the
    // same 1024 blocks on both mirrors (files older than head - keep are
    // pruned; the venue's journal holds its own writes — the deal).
    ctx.plane.journal.prune(head);
    let block = ctx.anvil_v.block_by_number(head, mirror_anvil::Txs::Hashes).await?;
    mirror_devnet::state::advance_cursor(&ctx.plane.state, &block, head)?;
    ctx.venue_metrics.set(&ctx.venue_metrics.cursor, head);
    ctx.plane.facts.publish(Fact::CursorAdvanced {
        chain: ctx.mirror.label(),
        block: head,
    });
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
    if ctx.mirror.policies().mine != MinePolicy::PoolDrain {
        return Ok(());
    }
    let Some(pending) = ctx.anvil_v.mine_pending().await? else {
        return Ok(());
    };
    ctx.venue_metrics.inc(&ctx.venue_metrics.mined_blocks_total);
    tracing::info!(pending = %pending, "mined V's pending transactions");
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
    if read_canary(&ctx.anvil_v).await? == pin {
        return Ok(pin);
    }
    // The witness comes first (the cause, then the effect), through the same
    // mapping the mainnet's detector path uses. A fresh state dir is not a
    // transition: there is no instance the registry recorded, so the boot is
    // an anchor and stays silent — the mainnet's boot anchor behaves the
    // same way (the epoch rule's fresh case is the negation of `holds_state`).
    if mirror_devnet::epoch::holds_state(&ctx.plane)? {
        let signal = ResetSignal::PinMismatch {
            expected: ctx.plane.state.epoch_pin()?,
            found: pin,
        };
        ctx.plane.witness_reset(ctx.mirror.label(), signal);
    }
    run_epoch_rule(ctx, pin).await?;
    anchor_cursor(ctx, pin).await?;
    Ok(pin)
}

/// The pin read, its gauge, and the unshadow anchor. Fail-closed on the read:
/// without it the epoch decision would default to "same pin" and re-apply
/// writes across a pin change.
async fn pin_read(ctx: &Ctx) -> Result<Option<u64>> {
    let pin = ctx.anvil_v.fork_block().await?;
    ctx.venue_metrics
        .set(&ctx.venue_metrics.fork_pin, pin.unwrap_or(0));
    if let Some(p) = pin {
        // The unshadow source's anchor: V's base state is the source's state
        // at the pin.
        ctx.pin.store(p, Ordering::Relaxed);
    }
    Ok(pin)
}

/// The epoch rule (shared with the mainnet engine, `mirror-devnet::epoch`):
/// a fresh pin drops marks and values, the same pin re-applies the recorded
/// writes; the journal goes either way. The transition's logs are the shared
/// module's; the counters are V's (epoch transitions, not resets found).
async fn run_epoch_rule(ctx: &Ctx, pin: Option<u64>) -> Result<()> {
    match ctx.epoch_rule(pin).await? {
        mirror_devnet::epoch::Epoch::Steady => {}
        mirror_devnet::epoch::Epoch::Cleared => {
            ctx.venue_metrics.inc(&ctx.venue_metrics.epochs_total);
            ctx.plane.publish_epoch_changed(ctx.mirror.label(), pin);
        }
        mirror_devnet::epoch::Epoch::Restored { restored } => {
            ctx.venue_metrics.inc(&ctx.venue_metrics.epochs_total);
            ctx.venue_metrics
                .add(&ctx.venue_metrics.restore_writes_total, restored as u64);
            ctx.plane.publish_epoch_changed(ctx.mirror.label(), pin);
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
        let block = ctx.anvil_v.block_by_number(p, mirror_anvil::Txs::Hashes).await?;
        // Not the shared advance step: a new epoch's `first_block` *is* the
        // pin (the shared step keeps the current cursor's first block).
        ctx.plane.state.set_cursor(cursor_from(&block, p))?;
        // The follow cursor starts at the pin: V's state *is* the source's
        // state at the pin, so the next window composes pin+1 onward. The
        // hash is the source's own block hash (the continuity check compares
        // it to the next block's parent).
        let source_block = ctx.source_rpc.block_by_number(p, mirror_anvil::Txs::Hashes).await?;
        ctx.plane.state.set_source_last(p, source_block.header.hash)?;
        p
    } else {
        tracing::warn!("anvil-v has no fork pin: epoch decisions are pin-based and cannot run");
        ctx.anvil_v.block_number().await?
    };
    ctx.plane
        .shadow_scanned_upto
        .store(watermark, Ordering::Relaxed);
    ctx.plane.save_state()?;
    if let Some(p) = pin {
        write_canary(&ctx.anvil_v, p).await?;
    }
    Ok(())
}

/// Run the at-pin probes now (the admin route's entry: boot and re-pin run
/// them automatically; this is the on-demand trigger, e.g. after a hand-back).
pub(crate) async fn probes_now(ctx: &Ctx) -> Result<ProbeOutcome> {
    let pin = ctx.anvil_v.fork_block().await?;
    probes_at_pin(ctx, pin).await
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
///
/// The run's outcome is returned (the cumulative counters are updated here —
/// an at-pin run is the only probe source on the venue side). The route
/// reports the run; `/metrics` carries the totals.
pub(crate) async fn probes_at_pin(ctx: &Ctx, pin: Option<u64>) -> Result<ProbeOutcome> {
    let skipped_all = || ProbeOutcome::skipped(ctx.engine.verifier.probes.len());
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
    let probes = mirror_devnet::probes::run_at(ctx, pin).await?;
    let out = count_probes(ctx, probes, pin, source_head).await?;
    mirror_devnet::probes::report(&ctx.plane, ctx.mirror.label(), pin, &out);
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

