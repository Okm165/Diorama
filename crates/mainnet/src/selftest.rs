//! End-to-end self test: diff-applies real mainnet blocks onto a scratch
//! Anvil forked from a public RPC. Needs no reth, so it can validate the
//! whole apply pipeline before the local fork source is synced.
use crate::ctx::Ctx;
use crate::apply::step;
use crate::error::{Error, Result};
use mirror_config::Instance;
use std::sync::Arc;

/// The selftest's scratch targets: the spine's one `ScratchTargets` (the
/// venue selftest reads the same struct).
pub use mirror_devnet::engine::ScratchTargets as Targets;

pub async fn run(cfg: Instance, targets: Targets, blocks: u64) -> Result<bool> {
    let Targets { rpc, anvil } = targets;
    let ctx = Arc::new(Ctx::new(cfg, &rpc, &anvil)?);

    // Wait for the scratch anvil (same pod, it starts in parallel).
    let boot = std::time::Duration::from_secs(120);
    ctx.anvil_t.wait_until_up("anvil-t", boot).await?;
    ctx.anvil_t.anvil_set_automine(mirror_anvil::Automine::Off).await?;

    // The pin comes from the scratch anvil itself (forkConfig.forkBlockNumber).
    let pin = mirror_devnet::pin::fork_pin(&ctx.anvil_t, "anvil-t").await?;

    mirror_devnet::engine::selftest_banner(
        "mirror engine selftest",
        &rpc,
        &anvil,
        pin,
        &format!("blocks:   {blocks}"),
    );

    let mut verdict = mirror_devnet::engine::Verdict::Passed;
    for n in (pin + 1)..=(pin + blocks) {
        println!("-- block {n}");
        if !run_one_block(&ctx, n).await? {
            verdict = mirror_devnet::engine::Verdict::Failed;
        }
        println!();
    }

    mirror_devnet::engine::selftest_verdict(verdict);
    // The exit-code boundary (the runner's own currency); the decision
    // itself is the named verdict above.
    Ok(matches!(verdict, mirror_devnet::engine::Verdict::Passed))
}

/// Apply one block, printing the per-check lines; returns whether it passed.
async fn run_one_block(ctx: &Ctx, n: u64) -> Result<bool> {
    let a = step(ctx, n).await.map_err(|source| Error::Step {
        step: "apply",
        block: n,
        source: Box::new(source),
    })?;
    let mut ok = true;
    println!(
        "   apply: source={} writes={} filtered={} skipped_shadowed={} sampled={}/{} ms={}",
        a.source, a.writes, a.filtered, a.skipped_shadowed, a.sampled_ok, a.sampled, a.duration_ms
    );
    if a.sampled != a.sampled_ok {
        ok = false;
        println!("   ✘ sampled storage parity failed");
    }
    Ok(ok)
}
