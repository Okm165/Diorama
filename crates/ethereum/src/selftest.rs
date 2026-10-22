//! End-to-end self test: replays and diff-applies real mainnet blocks against
//! two scratch Anvil instances forked from a public RPC. Needs no reth, so it
//! can validate the whole pipeline before the local fork source is synced.
use mirror_config::Instance;
use crate::ctx::Ctx;
use crate::diff::apply_block;
use crate::error::{Error, Result};
use crate::replay::replay_block;
use std::sync::Arc;

pub async fn run(
    mut cfg: Instance,
    rpc: String,
    anvil_m: String,
    anvil_t: String,
    beacon: Option<String>,
    blocks: u64,
) -> Result<bool> {
    if let Some(b) = beacon {
        let engine = cfg.services.engine.as_mut().ok_or_else(|| Error::Config(
            mirror_config::ConfigError::Invalid {
                issues: vec!["profile \"ethereum\" requires [services.engine]".into()],
            },
        ))?;
        engine.blobs.beacon_urls = vec![b];
    }
    let ctx = Arc::new(Ctx::new(cfg, &rpc, &anvil_m, &anvil_t)?);
    ctx.restore_blob_form()?;

    // Wait for the scratch anvils (same pod, they start in parallel).
    let mut last: Option<mirror_anvil::Error>;
    for attempt in 0..120 {
        match (ctx.anvil_m.block_number().await, ctx.anvil_t.block_number().await) {
            (Ok(_), Ok(_)) => break,
            (m, t) => last = m.err().or(t.err()),
        }
        if attempt == 119 {
            return Err(Error::NotReady {
                name: "scratch anvils",
                timeout_secs: 120,
                source: last,
            });
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    ctx.anvil_m.anvil_set_automine(false).await?;

    // The pin comes from the scratch anvil itself (forkConfig.forkBlockNumber).
    let info = ctx.anvil_m.anvil_node_info().await?;
    let pin = info
        .fork_config
        .fork_block_number
        .ok_or(Error::NoForkPin { chain: "anvil-m" })?;

    println!("== mirror engine selftest ==");
    println!("rpc:      {rpc}");
    println!("anvil-m:  {anvil_m}");
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

/// Replay + verify + apply one block, printing the per-check lines; returns
/// whether the block passed every check.
async fn run_one_block(ctx: &Ctx, n: u64) -> Result<bool> {
    let r = replay_block(ctx, n).await.map_err(|source| Error::Step {
        step: "replay",
        block: n,
        source: Box::new(source),
    })?;
    let mut ok = true;
    if let Some(v) = &r.verification {
        for (check, passed, detail) in &v.checks {
            if !*passed {
                ok = false;
            }
            println!("   M {} {:<24} {}", if *passed { "✔" } else { "✘" }, check, detail);
        }
        if !v.ok {
            ok = false;
        }
    }
    println!(
        "   M replay: txs={} replayed={} skipped_blobs={} ms={} {}",
        r.txs,
        r.replayed,
        r.skipped_blobs,
        r.duration_ms,
        if ok { "" } else { "(DIVERGED)" }
    );

    let a = apply_block(ctx, n).await.map_err(|source| Error::Step {
        step: "apply",
        block: n,
        source: Box::new(source),
    })?;
    println!(
        "   T apply:  source={} writes={} filtered={} skipped_shadowed={} sampled={}/{} ms={}",
        a.source, a.writes, a.filtered, a.skipped_shadowed, a.sampled_ok, a.sampled, a.duration_ms
    );
    if a.sampled != a.sampled_ok {
        ok = false;
        println!("   T ✘ sampled storage parity failed");
    }
    Ok(ok)
}
