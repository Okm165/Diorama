//! End-to-end self test: replays and diff-applies real mainnet blocks against
//! two scratch Anvil instances forked from a public RPC. Needs no reth, so it
//! can validate the whole pipeline before the local fork source is synced.
use crate::config::Config;
use crate::ctx::Ctx;
use crate::diff::apply_block;
use crate::replay::replay_block;
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;

pub async fn run(
    mut cfg: Config,
    rpc: String,
    anvil_m: String,
    anvil_t: String,
    beacon: Option<String>,
    blocks: u64,
) -> Result<bool> {
    if let Some(b) = beacon {
        cfg.blobs.beacon_urls = vec![b];
    }
    let ctx = Arc::new(Ctx::new(cfg, &rpc, &anvil_m, &anvil_t));
    ctx.restore_blob_form();

    // Wait for the scratch anvils (same pod, they start in parallel).
    for attempt in 0..120 {
        if ctx.anvil_m.block_number().await.is_ok() && ctx.anvil_t.block_number().await.is_ok() {
            break;
        }
        if attempt == 119 {
            anyhow::bail!("scratch anvils did not become ready");
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    ctx.anvil_m.anvil_set_automine(false).await?;

    // The pin comes from the scratch anvil itself (forkConfig.forkBlockNumber).
    let info = ctx.anvil_m.anvil_node_info().await?;
    let pin = info
        .pointer("/forkConfig/forkBlockNumber")
        .and_then(|v| {
            v.as_u64().or_else(|| {
                v.as_str()
                    .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            })
        })
        .ok_or_else(|| {
            anyhow!("anvil-m has no fork pin - start it with --fork-url and --fork-block-number")
        })?;

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

        let r = replay_block(&ctx, n)
            .await
            .with_context(|| format!("replay {n}"))?;
        let mut replayed_ok = true;
        if let Some(v) = &r.verification {
            for (check, ok, detail) in &v.checks {
                if !*ok {
                    replayed_ok = false;
                }
                println!(
                    "   M {} {:<24} {}",
                    if *ok { "✔" } else { "✘" },
                    check,
                    detail
                );
            }
            if !v.ok {
                all_ok = false;
            }
        }
        println!(
            "   M replay: txs={} replayed={} skipped_blobs={} ms={} {}",
            r.txs,
            r.replayed,
            r.skipped_blobs,
            r.duration_ms,
            if replayed_ok { "" } else { "(DIVERGED)" }
        );

        let a = apply_block(&ctx, n)
            .await
            .with_context(|| format!("apply {n}"))?;
        println!(
            "   T apply:  source={} writes={} filtered={} skipped_shadowed={} sampled={}/{} ms={}",
            a.source, a.writes, a.filtered, a.skipped_shadowed, a.sampled_ok, a.sampled, a.duration_ms
        );
        if a.sampled != a.sampled_ok {
            all_ok = false;
            println!("   T ✘ sampled storage parity failed");
        }
        println!();
    }

    println!("== selftest {} ==", if all_ok { "PASSED" } else { "FAILED" });
    Ok(all_ok)
}
