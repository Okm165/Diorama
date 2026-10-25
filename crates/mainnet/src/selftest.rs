//! End-to-end self test: diff-applies real mainnet blocks onto a scratch
//! Anvil forked from a public RPC. Needs no reth, so it can validate the
//! whole apply pipeline before the local fork source is synced.
use crate::ctx::Ctx;
use crate::apply::step;
use crate::error::{Error, Result};
use mirror_config::Instance;
use std::sync::Arc;

/// The selftest's scratch targets: the source RPC and the anvil endpoint.
pub struct Targets {
    pub rpc: String,
    pub anvil_t: String,
}

pub async fn run(cfg: Instance, targets: Targets, blocks: u64) -> Result<bool> {
    let Targets { rpc, anvil_t } = targets;
    let ctx = Arc::new(Ctx::new(cfg, &rpc, &anvil_t)?);

    // Wait for the scratch anvil (same pod, it starts in parallel).
    let boot = std::time::Duration::from_secs(120);
    ctx.anvil_t.wait_until_up("anvil-t", boot).await?;
    ctx.anvil_t.anvil_set_automine(false).await?;

    // The pin comes from the scratch anvil itself (forkConfig.forkBlockNumber).
    let pin = mirror_devnet::pin::fork_pin(&ctx.anvil_t, "anvil-t").await?;

    println!("== mirror engine selftest ==");
    println!("rpc:      {rpc}");
    println!("anvil-t:  {anvil_t}");
    println!("pin:      {pin}");
    println!("blocks:   {blocks}");
    println!();

    let mut all_ok = true;
    for n in (pin + 1)..=(pin + blocks) {
        println!("-- block {n}");
        if !run_one_block(&ctx, n).await? {
            all_ok = false;
        }
        println!();
    }

    println!("== selftest {} ==", if all_ok { "PASSED" } else { "FAILED" });
    Ok(all_ok)
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
