//! The fork driver: V's lifecycle from boot to epoch to steady state.
//!
//! V is a fork-mode anvil (chain id 999): static between re-pins, so the
//! ethereum profile's parent stream machinery has no counterpart here. The
//! driver's jobs (plan §7.2):
//!
//! - **boot asserts**: V answers, is the venue chain, and automine is off;
//! - **reset detection**: V's chain-instance canary no longer matches the pin
//!   (a re-pin moved the base, an OOM restart lost the word) — checked on
//!   every pass; V's cursor hashes cannot tell (below the pin, heights resolve
//!   to the source's own blocks, ADR 0002);
//! - **the epoch rule**: a fresh pin means the base moved (drop marks and
//!   values: a new epoch); the same pin means the recorded writes are still
//!   coherent (re-apply them and keep the marks);
//! - **probes-at-pin**: V vs the fork source at the pin, devnet-owned targets
//!   skipped and counted;
//! - **the steady-state scan**: every devnet tx's writes become shadowed
//!   (COW), so a hand-back can restore the source's truth and probes can skip
//!   what the devnet owns.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::probes::{ProbeOutcome, ProbeRunner};
use mirror_devnet::scan;
use mirror_devnet::state::cursor_from;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// HyperEVM's chain id on mainnet (998 on testnet). A code fact.
const VENUE_CHAIN_ID: u64 = 999;

/// The chain-instance canary: one storage word on a scratch address, written
/// by `reconcile` with the current pin as its value.
///
/// V is a fork-mode anvil, and a fresh fork cannot carry the canary — so
/// "the canary reads back the recorded pin" means "this is still the chain
/// instance the registry was built against". It is the only reliable reset
/// signal: heights below the fork resolve to the *source's* real blocks (a
/// re-pin to a later base keeps every cursor hash valid — measured live:
/// the driver kept scanning a stale range), and a same-pin restart leaves no
/// other trace. One storage read per pass; one write per reconcile.
const CANARY_ADDRESS: alloy_primitives::Address =
    alloy_primitives::address!("00000000000000000000000000000000ca11ca11");
const CANARY_SLOT: alloy_primitives::U256 = alloy_primitives::U256::ZERO;

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
    assert_venue_chain(ctx).await?;
    let pin = reconcile(ctx).await?;
    probes_at_pin(ctx, pin).await?;
    Ok(())
}

/// The steady-state loop (boot has already run).
pub async fn run(ctx: Arc<Ctx>) -> Result<()> {
    let poll = Duration::from_millis(ctx.venue.poll_interval_ms.max(200));
    loop {
        if ctx.plane.is_halted() {
            tokio::time::sleep(poll).await;
            continue;
        }
        match pass(&ctx).await {
            Ok(Pass::Steady) => {}
            Ok(Pass::Reset) => {
                tracing::warn!("V was reset under the driver - reconciling (epoch rule)");
                if let Err(e) = assert_venue_chain(&ctx).await {
                    tracing::warn!(error = %e, "venue chain assert failed after a reset");
                }
                match reconcile(&ctx).await {
                    Ok(pin) => {
                        if let Err(e) = probes_at_pin(&ctx, pin).await {
                            tracing::warn!(error = %e, "at-pin probes failed after a reset");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "reconcile failed; will retry"),
                }
            }
            Err(e) => tracing::warn!(error = %e, "venue driver pass failed; will retry"),
        }
        tokio::time::sleep(poll).await;
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

    // Reset detection: the canary must read back the recorded pin (see
    // CANARY_ADDRESS). Anything else — a re-pin, a same-pin restart, a wiped
    // state — means this is no longer the chain instance the registry was
    // built against, and the epoch rule must run.
    let expected = ctx.plane.state.lock()?.data.epoch_pin;
    let canary = read_canary(ctx).await?;
    if canary != expected {
        return Ok(Pass::Reset);
    }

    let cursor = ctx.cursor_v()?;
    if let Some(c) = &cursor {
        ctx.venue_metrics.set(&ctx.venue_metrics.cursor, c.block);
        ctx.venue_metrics
            .set(&ctx.venue_metrics.pin_age_blocks, head.saturating_sub(c.first_block) + 1);
    }

    // Scan the devnet's own blocks: every tx's writes become shadowed (COW).
    // The watermark starts from the persisted cursor, so blocks the devnet
    // mined while the driver was down are scanned too (V's head advances when
    // someone mines: this loop mines a non-empty pool, the tests mine
    // explicitly — the chain runs with automine off). Never at or below the
    // fork pin (the bridge anchors the same way): those blocks are the
    // source's, their txs are not in this chain, and tracing one fails —
    // anvil answers an unknown tx with `-32601 Method not found` (measured),
    // which would stall the watermark below the pin and livelock the driver
    // after a rewind or a re-pin.
    let first = cursor.as_ref().map(|c| c.block).unwrap_or(head);
    let walked = scan::scan_to(
        &ctx.plane,
        &ctx.v,
        ctx.pin.load(Ordering::Relaxed),
        head,
        first,
    )
    .await?;
    if walked > 0 {
        ctx.venue_metrics
            .add(&ctx.venue_metrics.scanned_blocks_total, walked);
        ctx.plane.save_shadow()?;
        // Ring the journal like the applier does on T: the rewind window is
        // the same 1024 blocks on both mirrors (files older than head - keep
        // are pruned; the venue's journal holds its own writes — the deal).
        ctx.plane.journal.prune(head);
        let block = ctx.v.block_by_number(head, false).await?;
        let first_block = cursor.as_ref().map(|c| c.first_block).unwrap_or(head);
        ctx.set_cursor_v(cursor_from(&block, first_block))?;
        ctx.plane.save_state()?;
        ctx.venue_metrics.set(&ctx.venue_metrics.cursor, head);
    }

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

/// V's block production: T has the applier (one block per applied parent
/// block), V has no parent stream — so the engine mines a non-empty pool,
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

/// The canary word: `Some(pin)` when the recorded chain instance wrote it,
/// `None` when the storage is empty (a fresh fork) — and a read fault is the
/// caller's error.
// TRUST: the fork pin is the trusted root (A1/A2 — the base's state is the
// source's, unverifiable by construction). The guards are this canary and the
// epoch rule: a moved base clears marks *and* values; a same-pin reset
// restores the recorded writes; a fresh chain cannot carry the canary word.
async fn read_canary(ctx: &Ctx) -> Result<Option<u64>> {
    let word = ctx.v.storage_at_latest(CANARY_ADDRESS, CANARY_SLOT).await?;
    let value = alloy_primitives::U256::from_be_bytes(word.0);
    if value.is_zero() {
        return Ok(None);
    }
    Ok(Some(value.to::<u64>()))
}

/// Mark this chain instance (see [`CANARY_ADDRESS`]).
async fn write_canary(ctx: &Ctx, pin: u64) -> Result<()> {
    let value = alloy_primitives::B256::from(alloy_primitives::U256::from(pin));
    ctx.v
        .anvil_set_storage_at(CANARY_ADDRESS, CANARY_SLOT, value)
        .await?;
    Ok(())
}

/// Wait for V to answer (a coordinated restart must not become
/// CrashLoopBackOff).
/// V must be the venue chain with automine off. An anvil restart resets
/// automine to its default (on) — measured — so the discipline is re-asserted
/// on every boot and after every reset.
async fn assert_venue_chain(ctx: &Ctx) -> Result<()> {
    let got = ctx.v.chain_id().await?;
    if got != VENUE_CHAIN_ID {
        return Err(Error::WrongChain {
            got,
            want: VENUE_CHAIN_ID,
        });
    }
    ctx.v.anvil_set_automine(false).await?;
    Ok(())
}

/// Reconcile the registry with V's base: check the chain-instance canary, run
/// the epoch rule when the instance changed, and anchor the cursor. Returns
/// the fork pin.
async fn reconcile(ctx: &Ctx) -> Result<Option<u64>> {
    // Fail-closed on the pin read: without it the epoch decision would default
    // to "same pin" and re-apply writes across a pin change.
    let pin = ctx.v.anvil_node_info().await?.fork_config.fork_block_number;
    ctx.venue_metrics
        .set(&ctx.venue_metrics.fork_pin, pin.unwrap_or(0));
    if let Some(p) = pin {
        // The unshadow source's anchor: V's base state is the source's state
        // at the pin.
        ctx.pin.store(p, Ordering::Relaxed);
    }

    // The canary tells whether this is still the chain instance the registry
    // was built against (see CANARY_ADDRESS): present and equal to the pin
    // means steady, nothing to reconcile.
    if read_canary(ctx).await? == pin {
        return Ok(pin);
    }

    // The epoch rule (shared with the ethereum engine, `mirror-devnet::epoch`):
    // a fresh pin drops marks and values, the same pin re-applies the recorded
    // writes; the journal goes either way. The transition's logs are the shared
    // module's; the counters are V's (epoch transitions, not resets found).
    match mirror_devnet::epoch::reconcile(&ctx.plane, &ctx.v, ctx.venue.chunk_size, pin).await? {
        mirror_devnet::epoch::Epoch::Steady => {}
        mirror_devnet::epoch::Epoch::Cleared => {
            ctx.venue_metrics.inc(&ctx.venue_metrics.epochs_total);
        }
        mirror_devnet::epoch::Epoch::Restored { restored } => {
            ctx.venue_metrics.inc(&ctx.venue_metrics.epochs_total);
            ctx.venue_metrics
                .add(&ctx.venue_metrics.restore_writes_total, restored as u64);
        }
    }

    // The registry now reflects V's base: anchor the cursor at the pin (V's
    // state *is* the source's state at the pin), and let the steady-state scan
    // advance it as the devnet mines.
    let watermark = if let Some(p) = pin {
        let block = ctx.v.block_by_number(p, false).await?;
        ctx.set_cursor_v(cursor_from(&block, p))?;
        p
    } else {
        tracing::warn!("anvil-v has no fork pin: epoch decisions are pin-based and cannot run");
        ctx.v.block_number().await?
    };
    ctx.plane.shadow_scanned_upto.store(watermark, Ordering::Relaxed);
    ctx.plane.save_state()?;
    if let Some(p) = pin {
        write_canary(ctx, p).await?;
    }
    Ok(pin)
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
pub async fn probes_at_pin(ctx: &Ctx, pin: Option<u64>) -> Result<ProbeOutcome> {
    let skipped_all = || ProbeOutcome {
        skipped: ctx.venue.probes.len(),
        ..Default::default()
    };
    let Some(pin) = pin else {
        tracing::warn!("at-pin probes skipped: V has no fork pin");
        return Ok(skipped_all());
    };
    let source_head = ctx.source_rpc.block_number().await?;
    if pin != source_head {
        ctx.venue_metrics
            .add(&ctx.venue_metrics.probe_skipped, ctx.venue.probes.len() as u64);
        tracing::info!(
            pin,
            source_head,
            skipped = ctx.venue.probes.len(),
            "at-pin probes skipped: the source serves its latest state (measured); the pin is not the source head"
        );
        return Ok(skipped_all());
    }
    // The venue has no clock offset (the plane's field is shape-only: nothing
    // re-aligns V's clock), so the probes never take the offset path.
    let probes = ProbeRunner::devnet(&ctx.venue.probes, &ctx.plane.shadow, false)
        .parity(&ctx.source_rpc, pin, &ctx.v)
        .await?;
    count_probes(ctx, probes, pin, source_head).await
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
