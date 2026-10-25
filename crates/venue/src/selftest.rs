//! End-to-end self test: follows real HyperEVM windows onto a scratch Anvil
//! forked from the public RPC. Needs no cluster, so it can validate the
//! follow pipeline — the envelopes, the synthesized BAL, the COW filter, the
//! live parity — against the *real* read model, including the system-tx
//! envelope a local anvil cannot serve (a chain without
//! `eth_getSystemTxsByBlockNumber` answers an empty list by design). The
//! mainnet counterpart is `crates/mainnet/src/selftest.rs` (apply real parent
//! blocks onto a scratch T).
use crate::ctx::Ctx;
use crate::error::Result;
use crate::follow;
use mirror_config::Instance;
use std::sync::Arc;
use std::time::Duration;

/// The selftest's scratch targets: the source RPC and the anvil endpoint.
pub struct Targets {
    pub rpc: String,
    pub anvil_v: String,
}

/// The wait bound for one window's source movement: the public chain produces
/// a block about every second, so the bound only fires on a stalled source
/// (which must fail the run, not hang it).
const MOVE_WAIT: Duration = Duration::from_secs(120);

pub async fn run(cfg: Instance, targets: Targets, windows: u64) -> Result<bool> {
    let Targets { rpc, anvil_v } = targets;
    let ctx = Arc::new(Ctx::new(cfg, &rpc, &anvil_v)?);

    // The engine's own boot: the chain assert, the capability audit, the
    // reconcile (which anchors the cursor and the follow at V's pin), and the
    // at-pin probes — a boot fault is the selftest's first failure.
    crate::driver::boot(&ctx).await?;
    let pin = mirror_devnet::pin::fork_pin(&ctx.anvil_v, "anvil-v").await?;

    println!("== venue engine selftest ==");
    println!("rpc:      {rpc}");
    println!("anvil-v:  {anvil_v}");
    println!("pin:      {pin}");
    println!("windows:  {windows} at least; the run continues, bounded, until a window composes a change");
    println!();

    let (done, writes, ok) = windows_loop(&ctx, windows).await?;
    println!();
    let mut all_ok = ok;
    if writes == 0 {
        println!("   ✘ no window composed a change (the watchlist saw no activity)");
        all_ok = false;
    }
    println!("   windows={done} composed_keys={writes}");
    println!("== selftest {} ==", if all_ok { "PASSED" } else { "FAILED" });
    Ok(all_ok)
}

/// The window loop: `windows` windows at least, and up to 6x (never below 12)
/// while nothing has composed — a run whose windows were all quiet proves
/// nothing about the pipeline, and the cap keeps a quiet watchlist from
/// running forever. Returns `(windows, composed keys, ok)`.
async fn windows_loop(ctx: &Ctx, windows: u64) -> Result<(u64, usize, bool)> {
    let cap = windows.saturating_mul(6).max(12);
    let mut done = 0u64;
    let mut writes = 0usize;
    let mut ok = true;
    while done < cap {
        if !wait_for_move(ctx).await? {
            println!(
                "   ✘ the source did not move within {}s; stopping",
                MOVE_WAIT.as_secs()
            );
            ok = false;
            break;
        }
        let Some(stats) = follow::step(ctx).await? else {
            continue;
        };
        done += 1;
        writes += stats.total;
        println!(
            "-- window {done}: source={} total={} applied={} filtered={} skipped_shadowed={} sampled={}/{}",
            stats.source,
            stats.total,
            stats.applied,
            stats.filtered,
            stats.skipped_shadowed,
            stats.sampled_ok,
            stats.sampled
        );
        if stats.sampled != stats.sampled_ok {
            ok = false;
            println!("   ✘ live parity failed");
        }
        if done >= windows && writes > 0 {
            break;
        }
    }
    Ok((done, writes, ok))
}

/// Wait (bounded) for the source head to move past the follow cursor: a
/// window needs a block to compose, and the follow is a no-op otherwise.
async fn wait_for_move(ctx: &Ctx) -> Result<bool> {
    let followed = ctx.plane.state.source_last_block()?.unwrap_or(0);
    let deadline = std::time::Instant::now() + MOVE_WAIT;
    loop {
        if ctx.source_rpc.block_number().await? > followed {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
