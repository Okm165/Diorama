//! mirror-engine: replay mainnet blocks onto M, keep T's watch-list state fresh
//! from EIP-7928 BALs, verify every block, and expose a small admin surface.
mod bal;
mod blob;
mod config;
mod ctx;
mod diff;
mod drift;
mod http;
mod journal;
mod metrics;
mod replay;
mod rpc;
mod engine_api;
mod selftest;
mod source_sync;
mod shadow;
mod state;
mod verify;

use crate::config::Config;
use crate::ctx::Ctx;
use crate::diff::apply_block;
use crate::replay::{replay_block, reth_block_hash};
use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "mirror-engine", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the engine loops and the HTTP API.
    Run {
        #[arg(long, default_value = "/etc/mirror/engine.yaml")]
        config: PathBuf,
    },
    /// End-to-end self test against a public RPC + two scratch Anvil forks.
    Selftest {
        #[arg(long)]
        rpc: String,
        #[arg(long)]
        anvil_m: String,
        #[arg(long)]
        anvil_t: String,
        #[arg(long, default_value = "/etc/mirror/engine.yaml")]
        config: PathBuf,
        /// Optional beacon-API base URL override (sidecar source).
        #[arg(long)]
        beacon: Option<String>,
        /// Number of consecutive blocks to test starting at the anvil pin + 1.
        #[arg(long, default_value_t = 3)]
        blocks: u64,
        /// RPC timeout for the scratch run (remote-fork mining is 55-256 s/block).
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
    },
    /// Rewind the last N applied parent blocks on T (journal-based).
    Rewind {
        #[arg(long, default_value = "/etc/mirror/engine.yaml")]
        config: PathBuf,
        #[arg(long)]
        blocks: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run { config } => run(config).await,
        Cmd::Selftest {
            rpc,
            anvil_m,
            anvil_t,
            config,
            beacon,
            blocks,
            timeout_secs,
        } => {
            let mut cfg = Config::load(&config)?;
            cfg.reth.request_timeout_secs = timeout_secs;
            let ok = selftest::run(cfg, rpc, anvil_m, anvil_t, beacon, blocks).await?;
            std::process::exit(if ok { 0 } else { 1 });
        }
        Cmd::Rewind { config, blocks } => {
            let cfg = Config::load(&config)?;
            let reth = cfg.reth.url.clone();
            let m = cfg.anvil.m_url.clone();
            let t = cfg.anvil.t_url.clone();
            let ctx = Ctx::new(cfg, &reth, &m, &t)?;
            let (cursor, restored) = http::do_rewind(&ctx, blocks).await?;
            println!("rewound {blocks} block(s): {restored} values restored, cursor_t now {cursor}");
            Ok(())
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("MIRROR_LOG_JSON").map(|v| v != "0").unwrap_or(false);
    if json {
        fmt().with_env_filter(filter).json().init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}

async fn run(config: PathBuf) -> Result<()> {
    let cfg = Config::load(&config)?;
    let reth_url = cfg.reth.url.clone();
    let m_url = cfg.anvil.m_url.clone();
    let t_url = cfg.anvil.t_url.clone();
    let ctx = Arc::new(Ctx::new(cfg, &reth_url, &m_url, &t_url)?);
    ctx.restore_blob_form();

    // M is engine-owned: transactions are batched and mined explicitly.
    // A coordinated restart (anvil, reth or lighthouse rolling) must not turn
    // into CrashLoopBackOff: wait for both anvils to answer before proceeding,
    // then fail fast only if they stay down.
    wait_for_anvil(&ctx.anvil_m, "anvil-m").await?;
    ctx.anvil_m
        .anvil_set_automine(false)
        .await
        .context("disabling automine on M")?;
    wait_for_anvil(&ctx.anvil_t, "anvil-t").await?;
    ctx.anvil_t
        .anvil_set_automine(false)
        .await
        .context("disabling automine on T")?;

    tracing::info!(
        reth = %ctx.reth.url(),
        anvil_m = %ctx.anvil_m.url(),
        anvil_t = %ctx.anvil_t.url(),
        follow = ctx.follow_tag(),
        watchlist = ctx.watchlist.len(),
        replay_into_m = ctx.cfg.loops.replay_into_m,
        apply_into_t = ctx.cfg.loops.apply_into_t,
        "mirror engine starting"
    );

    let mut tasks = Vec::new();
    if ctx.cfg.loops.replay_into_m {
        tasks.push(tokio::spawn(replay_loop(ctx.clone())));
    }
    if ctx.engine.is_some() {
        tasks.push(tokio::spawn(source_sync_supervisor(ctx.clone())));
    }
    if ctx.cfg.loops.apply_into_t {
        tasks.push(tokio::spawn(apply_loop(ctx.clone())));
        tasks.push(tokio::spawn(shadow_loop(ctx.clone())));
    }
    {
        let ctx = ctx.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = http::serve(ctx).await {
                tracing::error!(error = %e, "http server exited");
            }
        }));
    }

    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutdown signal received");
    for t in tasks {
        t.abort();
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum Chain {
    M,
    T,
}

/// Decide the next parent block to process for one chain, applying the cursor,
/// rebase, gap, and reorg rules. `None` means "nothing to do right now".
async fn next_block_for(ctx: &Ctx, chain: Chain) -> Result<Option<u64>> {
    let Some(follow) = ctx.follow_head().await? else {
        tracing::debug!("follow head unavailable");
        return Ok(None);
    };
    ctx.metrics.set(&ctx.metrics.reth_follow_head, follow);
    let reth_head = ctx.reth.block_number().await.unwrap_or(0);
    ctx.metrics.set(&ctx.metrics.reth_head, reth_head);

    let (head, cursor, rpc) = match chain {
        Chain::M => (
            ctx.anvil_m.block_number().await?,
            ctx.cursor_m(),
            &ctx.anvil_m,
        ),
        Chain::T => (
            ctx.anvil_t.block_number().await?,
            ctx.cursor_t(),
            &ctx.anvil_t,
        ),
    };
    match chain {
        Chain::M => ctx.metrics.set(&ctx.metrics.m_head, head),
        Chain::T => ctx.metrics.set(&ctx.metrics.t_head, head),
    }

    // Validate the cursor against the chain; a mismatch means the chain was
    // reset (re-pin / wiped state) and the cursor must rebase.
    let cursor = match cursor {
        Some(c) => {
            let hash = crate::ctx::block_hash(rpc, c.block).await?;
            if hash.as_deref() == Some(c.hash.as_str()) {
                Some(c)
            } else {
                tracing::warn!(
                    block = c.block,
                    "cursor not found on chain (reset or re-pin) - rebasing"
                );
                None
            }
        }
        None => None,
    };

    let (start, rebased) = state::next_block(cursor.as_ref(), head);
    if matches!(chain, Chain::M) && head > follow {
        // A restored state dump (or a regressed source head) can leave M ahead
        // of the follow head; M replays 1:1 so its height is a parent height and
        // the loop can never advance from here.
        //
        // T is exempt: its heights advance independently of mainnet's (one T
        // block per applied parent block, plus one per journal rewind), so
        // comparing them would wrongly stall T after a rewind drill. T's
        // progress is bounded by `cursor_t` vs the follow head below.
        tracing::warn!(
            chain = ?chain,
            head,
            follow,
            "M head is ahead of the follow head - stale state dump or source regression; re-pin to realign"
        );
        return Ok(None);
    }
    if rebased {
        // An anvil restart (re-pin, OOM) resets automine to its default (on).
        // Re-assert it on every rebase: otherwise each raw tx mines its own
        // block and the engine's mine(1) produces empty blocks.
        if let Err(e) = rpc.anvil_set_automine(false).await {
            tracing::warn!(chain = ?chain, error = %e, "could not re-assert automine=false");
        }
        // A *new epoch* (re-pin) wipes the devnet's writes, so the shadow set -
        // "these keys belong to the devnet" - must go with them; keeping it
        // would silently stop T from syncing keys it no longer owns. A plain
        // anvil restart keeps the same fork pin, so its shadows stay.
        if matches!(chain, Chain::T) {
            if let Some(pin) = rpc
                .anvil_node_info()
                .await
                .ok()
                .and_then(|info| info.pointer("/forkConfig/forkBlockNumber").and_then(|v| v.as_u64()))
            {
                let known = ctx.state.lock().unwrap().data.epoch_pin;
                if known != Some(pin) {
                    if known.is_some() {
                        let cleared = {
                            let mut shadow = ctx.shadow.lock().unwrap();
                            let n = shadow.slot_count();
                            shadow.clear();
                            n
                        };
                        tracing::info!(
                            pin,
                            previous = ?known,
                            cleared_slots = cleared,
                            "new epoch: cleared the shadow set (devnet writes are gone)"
                        );
                        let _ = ctx.save_shadow();
                    }
                    ctx.state.lock().unwrap().data.epoch_pin = Some(pin);
                    let _ = ctx.save_state();
                }
            }
        }
        tracing::info!(chain = ?chain, head, follow, start, "starting chain at block");
    }
    if start > follow {
        tracing::debug!(chain = ?chain, head, follow, start, "chain is at the follow head");
        return Ok(None);
    }
    let gap = follow.saturating_sub(start);
    if gap > ctx.cfg.loops.max_gap_blocks {
        tracing::error!(
            chain = ?chain,
            gap,
            max = ctx.cfg.loops.max_gap_blocks,
            "sync gap too large - paused; run the re-pin drill (make repin)"
        );
        return Ok(None);
    }

    // Upstream reorg continuity: only meaningful when we processed start-1.
    if let (Some(last_hash), Some(last_block)) = (ctx.reth_last_hash(), ctx.reth_last_block()) {
        if start > 1 && start - 1 == last_block {
            if let Some(prev) = reth_block_hash(&ctx.reth, start - 1).await? {
                if prev != last_hash {
                    ctx.metrics.inc(&ctx.metrics.reorgs_detected);
                    ctx.on_divergence(&format!("reth reorg at {}", start - 1));
                    return Ok(None);
                }
            }
        }
    }
    Ok(Some(start))
}

async fn replay_loop(ctx: Arc<Ctx>) {
    let poll = Duration::from_millis(ctx.cfg.loops.poll_interval_ms.max(200));
    let mut failing_block = 0u64;
    let mut attempts = 0u64;
    loop {
        if ctx.halted.load(Ordering::Relaxed) || ctx.frozen.load(Ordering::Relaxed) {
            tokio::time::sleep(poll).await;
            continue;
        }
        match next_block_for(&ctx, Chain::M).await {
            Ok(Some(n)) => {
                if n != failing_block {
                    failing_block = n;
                    attempts = 0;
                }
                match replay_block(&ctx, n).await {
                    Ok(_) => attempts = 0,
                    Err(e) => {
                        attempts += 1;
                        tracing::error!(
                            block = n,
                            attempt = attempts,
                            error = %e,
                            "replay failed (will retry)"
                        );
                        // Transient failures get a few tries. After that the
                        // block is mined with whatever the pool holds and the
                        // divergence is recorded: the parent-BAL repair still
                        // converges M's state, so skipping one block's txs is
                        // strictly better than stalling the whole mirror (e.g.
                        // a beacon source returning a bad blob sidecar).
                        if attempts >= 3 {
                            tracing::error!(
                                block = n,
                                attempts,
                                error = %e,
                                "giving up on this block; mining what the pool holds"
                            );
                            if let Err(e2) = ctx.anvil_m.anvil_mine(1).await {
                                tracing::error!(block = n, error = %e2, "forced mine failed");
                            }
                            attempts = 0;
                        }
                        tokio::time::sleep(poll * 5).await;
                    }
                }
            }
            Ok(None) => tokio::time::sleep(poll).await,
            Err(e) => {
                tracing::warn!(error = %e, "replay loop poll failed");
                tokio::time::sleep(poll * 5).await;
            }
        }
    }
}

async fn apply_loop(ctx: Arc<Ctx>) {
    let poll = Duration::from_millis(ctx.cfg.loops.poll_interval_ms.max(200));
    loop {
        if ctx.halted.load(Ordering::Relaxed) {
            tokio::time::sleep(poll).await;
            continue;
        }
        match next_block_for(&ctx, Chain::T).await {
            Ok(Some(n)) => {
                if let Err(e) = apply_block(&ctx, n).await {
                    tracing::error!(block = n, error = %e, "diff apply failed (will retry)");
                    tokio::time::sleep(poll * 5).await;
                }
            }
            Ok(None) => tokio::time::sleep(poll).await,
            Err(e) => {
                tracing::warn!(error = %e, "apply loop poll failed");
                tokio::time::sleep(poll * 5).await;
            }
        }
    }
}

/// Wait for an anvil to answer RPC, so a coordinated restart (reth unwinding,
/// anvil re-pinning) does not turn the engine into CrashLoopBackOff. Bounded:
/// if the anvil is still down after the deadline, start-up fails loudly.
async fn wait_for_anvil(anvil: &crate::rpc::Rpc, name: &str) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    let mut last_log = std::time::Instant::now();
    loop {
        match anvil.block_number().await {
            Ok(head) => {
                tracing::info!(anvil = name, head, "anvil is up");
                return Ok(());
            }
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("{name} did not answer RPC within 900s: {e}");
                }
                if last_log.elapsed() > std::time::Duration::from_secs(30) {
                    tracing::warn!(anvil = name, error = %e, "waiting for anvil to come up");
                    last_log = std::time::Instant::now();
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

/// Supervises the top-up loop. A panicking top-up task is invisible from the
/// outside (reth simply stops advancing), so restart it and say so loudly.
async fn source_sync_supervisor(ctx: Arc<Ctx>) {
    loop {
        match tokio::spawn(source_sync::source_sync_loop(ctx.clone())).await {
            Ok(()) => tracing::warn!("top-up loop exited; restarting"),
            Err(e) => tracing::error!(error = %e, "top-up loop panicked; restarting"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Watch T's own blocks: every devnet transaction's writes become shadowed
/// (copy-on-write) so parent diffs never overwrite them.
async fn shadow_loop(ctx: Arc<Ctx>) {    let poll = Duration::from_millis(ctx.cfg.loops.poll_interval_ms.max(200));
    let mut last: Option<u64> = None;
    loop {
        tokio::time::sleep(poll * 2).await;
        let t_head = match ctx.anvil_t.block_number().await {
            Ok(h) => h,
            Err(_) => continue,
        };
        let from = match last {
            Some(l) => l + 1,
            None => {
                last = Some(t_head);
                continue;
            }
        };
        for n in from..=t_head {
            if let Err(e) = scan_t_block(&ctx, n).await {
                tracing::warn!(block = n, error = %e, "shadow scan failed");
            }
        }
        last = Some(t_head);
        let _ = ctx.save_shadow();
    }
}

async fn scan_t_block(ctx: &Ctx, n: u64) -> Result<()> {
    let block = ctx.anvil_t.block_by_number(n, true).await?;
    let txs = ctx::block_txs(&block);
    if txs.is_empty() {
        return Ok(());
    }
    let mut shadowed = 0usize;
    for tx in txs {
        use alloy_consensus::Transaction as _;
        // A tx from an account stops its account-field sync (nonce -> balance too).
        use alloy_network_primitives::TransactionResponse as _;
        let from = tx.from();
        ctx.shadow.lock().unwrap().mark_account_fields(from);
        shadowed += 1;
        // Value transfers shadow the recipient's balance.
        let value = tx.inner.value();
        if value > alloy_primitives::U256::ZERO {
            if let Some(to) = tx.inner.to() {
                ctx.shadow.lock().unwrap().mark_account_fields(to);
            }
        }
        // Storage writes come from the tx trace (anvil 1.8.5 supports
        // prestateTracer diffMode - measured).
        match ctx.anvil_t.trace_tx_prestate(tx.tx_hash()).await {
            Ok(result) => {
                for w in crate::bal::flatten_trace_diff(&result) {
                    ctx.shadow.lock().unwrap().mark(&w);
                    shadowed += 1;
                }
            }
            Err(e) => tracing::warn!(tx = %tx.tx_hash(), error = %e, "shadow trace failed"),
        }
    }
    tracing::info!(
        block = n,
        txs = txs.len(),
        shadowed_writes = shadowed,
        "scanned devnet block for shadow writes"
    );
    Ok(())
}
