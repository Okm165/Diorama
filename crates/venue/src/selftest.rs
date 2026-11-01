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
use diorama_config::{ConfigError, Endpoints, Instance};
use std::sync::Arc;
use std::time::Duration;

/// The selftest's scratch targets: the spine's one `ScratchTargets` (the
/// mainnet selftest reads the same struct).
pub use diorama_devnet::engine::ScratchTargets as Targets;

use diorama_devnet::engine::Verdict;

pub async fn run(
    cfg: Instance,
    targets: Targets,
    windows: u64,
    move_wait: Duration,
    max_windows: u64,
) -> Result<Verdict> {
    let Targets { rpc, anvil } = targets;
    let rpc = Endpoints::parse([rpc]).map_err(|detail| ConfigError::Invalid {
        issues: vec![detail],
    })?;
    let ctx = Arc::new(Ctx::new(cfg, &rpc, &anvil)?);

    // The engine's own boot: the chain assert, the capability audit, the
    // reconcile (which anchors the cursor and the follow at V's pin), and the
    // at-pin probes — a boot fault is the selftest's first failure.
    crate::driver::boot(&ctx).await?;
    let pin = diorama_devnet::pin::fork_pin(&ctx.anvil_v, "anvil-v").await?;

    diorama_devnet::engine::selftest_banner(
        "venue engine selftest",
        rpc.primary(),
        &anvil,
        pin,
        &format!(
            "windows:  {windows} at least; the run continues until a window composes a change{}",
            if max_windows == 0 {
                String::new()
            } else {
                format!(", bounded at {max_windows}")
            }
        ),
    );

    let (done, writes, mut verdict) = windows_loop(&ctx, windows, move_wait, max_windows).await?;
    println!();
    if writes == 0 {
        println!("   ✘ no window composed a change (the watchlist saw no activity)");
        verdict = diorama_devnet::engine::Verdict::Failed;
    }
    println!("   windows={done} composed_keys={writes}");
    diorama_devnet::engine::selftest_verdict(verdict);
    Ok(verdict)
}

/// The window loop's bound: the operator's `max_windows` when supplied, and
/// no code-side cap when it is zero. The minimum requested window count still
/// applies, so a max below it cannot cut the run short.
fn window_cap(windows: u64, max_windows: u64) -> u64 {
    if max_windows == 0 {
        u64::MAX
    } else {
        max_windows.max(windows)
    }
}

/// The window loop: `windows` windows at least, continuing while nothing has
/// composed. `max_windows = 0` means no code-side cap: the operator's
/// process deadline (the Job's) is the only bound. Returns
/// `(windows, composed keys, ok)`.
async fn windows_loop(
    ctx: &Ctx,
    windows: u64,
    move_wait: Duration,
    max_windows: u64,
) -> Result<(u64, usize, Verdict)> {
    let cap = window_cap(windows, max_windows);
    let mut done = 0u64;
    let mut writes = 0usize;
    let mut verdict = Verdict::Passed;
    while done < cap {
        if !wait_for_move(ctx, move_wait).await? {
            println!(
                "   ✘ the source did not move within {}s; stopping",
                move_wait.as_secs()
            );
            verdict = Verdict::Failed;
            break;
        }
        let Some(stats) = follow::step(ctx).await? else {
            continue;
        };
        done += 1;
        writes += stats.writes;
        println!(
            "-- window {done}: source={} writes={} applied={} filtered={} skipped_shadowed={} sampled={}/{}",
            stats.source,
            stats.writes,
            stats.applied,
            stats.filtered,
            stats.skipped_shadowed,
            stats.sampled_ok,
            stats.sampled
        );
        if stats.sampled != stats.sampled_ok {
            verdict = Verdict::Failed;
            println!("   ✘ live parity failed");
        }
        if done >= windows && writes > 0 {
            break;
        }
    }
    Ok((done, writes, verdict))
}

/// Wait (bounded) for the source head to move past the follow cursor: a
/// window needs a block to compose, and the follow is a no-op otherwise.
async fn wait_for_move(ctx: &Ctx, move_wait: Duration) -> Result<bool> {
    let followed = ctx.plane.state.source_last_block()?.unwrap_or(0);
    let deadline = std::time::Instant::now() + move_wait;
    loop {
        if ctx.source_rpc.block_number().await? > followed {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        // The engine's configured cadence, not a second poll interval.
        tokio::time::sleep(ctx.engine.loops.poll()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::window_cap;

    /// The loop's bound is the operator's, never an invented multiplier: zero
    /// means the process deadline is the only cap, and a max below the
    /// requested windows cannot cut the run short.
    #[test]
    fn the_window_cap_is_the_operators() {
        assert_eq!(window_cap(5, 0), u64::MAX);
        assert_eq!(window_cap(5, 30), 30);
        assert_eq!(window_cap(5, 3), 5);
        assert_eq!(window_cap(5, 5), 5);
    }
}
