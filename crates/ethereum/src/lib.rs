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

pub async fn main_entry(args: Vec<String>) -> Result<()> {
    init_tracing();
    let cli = Cli::parse_from(std::iter::once("mirror-engine".to_string()).chain(args));
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
            let ctx = Ctx::new(cfg, &reth, &m, &t);
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
    let ctx = Arc::new(Ctx::new(cfg, &reth_url, &m_url, &t_url));
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

/// Whether the shadow registry holds marks or recorded values (decides the
/// same-pin restore path after a restart).
fn shadow_has_recorded_state(ctx: &Ctx) -> bool {
    let s = ctx.shadow.lock().unwrap();
    s.slot_count() > 0
        || !s.accounts.is_empty()
        || !s.values.is_empty()
        || !s.account_values.is_empty()
}

/// Decide the next parent block to process for one chain, applying the cursor,
/// rebase, gap, and reorg rules. `None` means "nothing to do right now".
#[allow(
    clippy::too_many_lines,
    reason = "R0 freeze: verbatim; the split is a deferred follow-up refactor"
)]
async fn next_block_for(ctx: &Ctx, chain: Chain) -> Result<Option<u64>> {
    let Some(follow) = ctx.follow_head().await? else {
        tracing::debug!("follow head unavailable");
        return Ok(None);
    };
    ctx.metrics.set(&ctx.metrics.reth_follow_head, follow);
    // Schedule a small margin behind the head: the beacon cannot serve blob
    // sidecars for the newest block yet (measured: >15 s of retries on ~every
    // blob block with `follow: latest`, holding M each time). ~2 blocks of
    // extra lag is invisible; the metric keeps the raw head so
    // `mirror_lag_blocks` still measures the true distance.
    let follow = follow.saturating_sub(ctx.cfg.reth.follow_margin_blocks);
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
        Chain::T => {
            ctx.metrics.set(&ctx.metrics.t_head, head);
            if let Ok(info) = rpc.anvil_node_info().await {
                if let Some(fork) = info
                    .pointer("/forkConfig/forkBlockNumber")
                    .and_then(serde_json::Value::as_u64)
                {
                    ctx.metrics
                        .set(&ctx.metrics.t_fork_age_blocks, head.saturating_sub(fork));
                }
            }
        }
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
                if matches!(chain, Chain::T) {
                    // T was re-forked (its OOM restart or a re-pin). Which
                    // epoch is this?
                    // - The fork pin *changed*: the base moved, so the
                    //   recorded values are stale derivations of the old base
                    //   (plan 2.2/6.3: "derived values go stale when the base
                    //   advances"). Start a *new epoch*: drop marks and values
                    //   - the product re-creates its positions from its own
                    //   journal (plan 3.6). Measured: restoring across a pin
                    //   change re-applied a +30d time-travel write as a future
                    //   `lastUpdateTimestamp` and every Aave supply panicked
                    //   with an arithmetic underflow.
                    // - The pin is *unchanged* (an out-of-band reset at the
                    //   same base): the recorded writes are still coherent -
                    //   re-apply them and keep the marks.
                    let pin = rpc
                        .anvil_node_info()
                        .await
                        .ok()
                        .and_then(|i| {
                            i.pointer("/forkConfig/forkBlockNumber")
                                .and_then(serde_json::Value::as_u64)
                        });
                    let known = ctx.state.lock().unwrap().data.epoch_pin;
                    let new_epoch = matches!((known, pin), (Some(k), Some(p)) if k != p);
                    ctx.metrics.inc(&ctx.metrics.t_resets_total);
                    if new_epoch {
                        let (slots, accounts) = ctx.shadow.lock().unwrap().clear_all();
                        let _ = ctx.save_shadow();
                        tracing::warn!(
                            pin = ?pin,
                            previous = ?known,
                            cleared_slots = slots,
                            cleared_accounts = accounts,
                            "new epoch: the fork base moved; the devnet's writes are gone with it (re-create positions from your journal)"
                        );
                    } else if shadow_has_recorded_state(ctx) {
                        // Values whose keys are not marked are orphans from an
                        // older engine (unmark used to keep them): drop them
                        // before restoring, or the re-apply would resurrect a
                        // value the devnet handed back (measured: a crashed
                        // oracle source). The marks themselves are kept.
                        let (orphan_slots, orphan_accounts) =
                            ctx.shadow.lock().unwrap().prune_orphans();
                        let (slots, accounts, values, accounts_values) = {
                            let s = ctx.shadow.lock().unwrap();
                            (
                                s.slot_count(),
                                s.accounts.len(),
                                s.values.len(),
                                s.account_values.len(),
                            )
                        };
                        // Clone out: the re-apply awaits, and a MutexGuard must
                        // not be held across it (the loop must stay Send).
                        let snapshot = ctx.shadow.lock().unwrap().clone();
                        let restored = snapshot.reapply(&ctx.anvil_t).await;
                        // Marked keys with no recorded value would hold pin-era
                        // mainnet values under a devnet mark - hand them back.
                        let (unknown_slots, unknown_accounts) =
                            ctx.shadow.lock().unwrap().unrecorded();
                        let _ = ctx.save_shadow();
                        tracing::warn!(
                            slots,
                            accounts,
                            storage_accounts = values,
                            account_values = accounts_values,
                            restored,
                            orphan_slots,
                            orphan_accounts,
                            unknown_slots,
                            unknown_accounts,
                            "T was reset at the same pin: re-applied the devnet's writes"
                        );
                    }
                    if pin.is_some() && known != pin {
                        ctx.state.lock().unwrap().data.epoch_pin = pin;
                        let _ = ctx.save_state();
                    }
                    // The journal holds before-values of the *old* chain: a
                    // rewind against the re-forked chain would restore values
                    // that never existed there. Drop it.
                    if let Err(e) = ctx.journal.clear() {
                        tracing::warn!(error = %e, "clearing the journal after a T reset failed");
                    }
                }
                // Drop it from the persisted state too: `first_block` is an
                // epoch marker, and the next processed block must set a fresh
                // one (otherwise `mirror_pin_age_blocks` counts across epochs).
                if matches!(chain, Chain::M) {
                    // An M re-base skipped this range: those blocks are
                    // mainnet's own (the fresh fork serves them and the state
                    // comes from the fork base, so it is exact by construction),
                    // but the engine did not re-execute them - their tx-level
                    // verification is skipped. Bounded by `fork_offset`; counted
                    // and logged so the gap is never invisible.
                    let skipped = head.saturating_sub(c.block);
                    if skipped > 0 {
                        ctx.metrics.add(&ctx.metrics.m_rebase_skipped, skipped);
                        tracing::warn!(
                            from = c.block,
                            to = head,
                            skipped,
                            "M re-base skipped a range (fork base: state exact, tx verification skipped)"
                        );
                    }
                }
                {
                    let mut state = ctx.state.lock().unwrap();
                    match chain {
                        Chain::M => state.data.m = None,
                        Chain::T => state.data.t = None,
                    }
                }
                let _ = ctx.save_state();
                None
            }
        }
        None => None,
    };

    // A reset chain (re-pin / wiped state): T's state is exactly the fork pin's
    // state, so the applier resumes from the pin. Using T's own height + 1 would
    // be wrong - T mines one block per applied parent block (plus one per
    // journal rewind), so its height is not a parent height and `head + 1` can
    // land past the follow head, stalling T forever (measured).
    let reset_pin = if cursor.is_none() {
        rpc.anvil_node_info()
            .await
            .ok()
            .and_then(|info| {
                info.pointer("/forkConfig/forkBlockNumber")
                    .and_then(serde_json::Value::as_u64)
            })
    } else {
        None
    };
    let (start, rebased) = match (chain, reset_pin) {
        (Chain::T, Some(pin)) => (pin + 1, true),
        _ => state::next_block(cursor.as_ref(), head),
    };
    let forced = matches!(chain, Chain::M) && ctx.force_rebase.swap(false, Ordering::Relaxed);
    if matches!(chain, Chain::M) && (head > follow || forced) {
        // M's replay is irreversible, but the follow head can move *backward*:
        // the CL's forkchoice may report a lower safe block (reorg / failed
        // justification), which strands M until it catches up (measured: a
        // 42-block recede stalling ~10 min, with a warning every poll).
        // Re-base M at a fresh pin behind the follow head instead; the cursor
        // hash check on the next pass rebases the replay. The blocks between
        // the old cursor and the new pin are mainnet's own (the fork serves
        // them and the state comes from the fork base, so it is exact by
        // construction), but they are not re-executed: their tx-level
        // verification is skipped. Bounded by `fork_offset` per re-base and
        // counted in `mirror_m_rebase_skipped_blocks_total` with a WARN naming
        // the range, so the gap is never invisible. T is unaffected: the
        // epoch/COW state is keyed on T's own fork block, not M's.
        //
        // T is exempt from this check entirely: its heights advance
        // independently of mainnet's (one T block per applied parent block,
        // plus one per journal rewind).
        {
            let mut last = ctx.last_stall.lock().unwrap();
            if *last != (head, follow) {
                *last = (head, follow);
                tracing::warn!(
                    chain = ?chain,
                    head,
                    follow,
                    forced = forced,
                    "re-basing M: ahead of the follow head (reorg) or a block is unservable"
                );
            }
        }
        let attempt = {
            let mut t = ctx.last_rebase.lock().unwrap();
            let due = t
                .map(|i: std::time::Instant| i.elapsed() > std::time::Duration::from_secs(60))
                .unwrap_or(true);
            if due {
                *t = Some(std::time::Instant::now());
            }
            due
        };
        if attempt {
            match ctx.anvil_m.fork_config().await {
                Ok(Some((url, _))) => {
                    let pin = follow.saturating_sub(ctx.cfg.loops.fork_offset);
                    match ctx.anvil_m.anvil_reset_fork(&url, pin).await {
                        Ok(()) => tracing::warn!(
                            head,
                            follow,
                            pin,
                            "M re-based at a fresh pin behind the follow head"
                        ),
                        Err(e) => tracing::warn!(error = %e, "M re-base failed; will retry"),
                    }
                }
                Ok(None) => tracing::warn!("M re-base skipped: chain has no fork config"),
                Err(e) => tracing::warn!(error = %e, "M re-base: anvil_nodeInfo failed"),
            }
        }
        return Ok(None);
    }
    if rebased {
        // An anvil restart (re-pin, OOM) resets automine to its default (on).
        // Re-assert it on every rebase: otherwise each raw tx mines its own
        // block and the engine's mine(1) produces empty blocks.
        if let Err(e) = rpc.anvil_set_automine(false).await {
            tracing::warn!(chain = ?chain, error = %e, "could not re-assert automine=false");
        }
        // The epoch decision (clear vs restore) lives with the reset handling
        // above, where the fork pin is compared before anything is written.
        tracing::info!(chain = ?chain, head, follow, start, "starting chain at block");
    }
    if start > follow {
        tracing::debug!(chain = ?chain, head, follow, start, "chain is at the follow head");
        return Ok(None);
    }
    let gap = follow.saturating_sub(start);
    // Two regimes (plan §3 ops): normal operation pauses at `max_gap_blocks`
    // (300 = the plan's "> 300 -> pause and re-sync"), while a *rebased* cursor
    // (a fresh pin after a restart/re-fork) is allowed to replay its whole pin
    // window - bounded by reth's state window (10,064), the plan's own limit.
    let limit = if rebased { 10_064 } else { ctx.cfg.loops.max_gap_blocks };
    if gap > limit {
        tracing::error!(
            chain = ?chain,
            gap,
            max = limit,
            rebased,
            "sync gap too large - paused; run the re-pin drill (make repin)"
        );
        return Ok(None);
    }

    // Upstream reorg continuity: only meaningful when we processed start-1, and
    // it belongs to the *M* loop alone - it owns the parent-stream integrity
    // (`reth_last_*` is written by the M replay). T consumes the same blocks
    // through the diff applier, whose absolute-value writes self-heal across a
    // reorg, and its own cursor check rebases on a reset. Running the check in
    // both loops double-counted every reorg.
    if matches!(chain, Chain::M) {
        if let (Some(last_hash), Some(last_block)) =
            (ctx.reth_last_hash(), ctx.reth_last_block())
        {
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
                        // A block whose raws could not be built (missing blob
                        // sidecars) must NOT be mined: mining "what the pool
                        // holds" produces an *empty* block, breaks the 1:1
                        // block mapping and records a fatal tx-count divergence
                        // (measured: 261 txs lost per block while the beacon
                        // ingested the sidecars seconds behind the head). Hold
                        // M at n-1 and retry - `mirror_lag_blocks` is the alarm.
                        if attempts == 18 {
                            // ~90 s of the same block failing: not transient.
                            // Ask `next_block_for` to re-base M at a fresh pin
                            // (blob sidecars older than the CL's prune window
                            // can never be served, so waiting is forever).
                            tracing::error!(
                                block = n,
                                "block unservable for ~90 s; asking for an M re-base"
                            );
                            ctx.force_rebase.store(true, Ordering::Relaxed);
                        }
                        if attempts == 1 || attempts.is_multiple_of(20) {
                            tracing::error!(
                                block = n,
                                attempt = attempts,
                                error = %e,
                                "replay failed; holding M (never mining an empty or partial block)"
                            );
                        } else {
                            tracing::debug!(block = n, attempt = attempts, error = %e, "replay retry");
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
        if ctx.halted.load(Ordering::Relaxed) || ctx.frozen.load(Ordering::Relaxed) {
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
    loop {
        tokio::time::sleep(poll * 2).await;
        let t_head = match ctx.anvil_t.block_number().await {
            Ok(h) => h,
            Err(_) => continue,
        };
        let last = ctx.shadow_scanned_upto.load(Ordering::Relaxed);
        if last == 0 {
            // First pass: do not scan pre-existing history (the registry was
            // persisted); start from the current head. The applier's inline
            // scan shares this high-water mark, so its blocks are not re-traced.
            ctx.shadow_scanned_upto.store(t_head, Ordering::Relaxed);
            continue;
        }
        for n in (last + 1)..=t_head {
            if let Err(e) = scan_t_block(&ctx, n).await {
                tracing::warn!(block = n, error = %e, "shadow scan failed");
            }
            ctx.shadow_scanned_upto.fetch_max(n, Ordering::Relaxed);
        }
        let _ = ctx.save_shadow();
    }
}

/// Watch T's own blocks: every devnet transaction's writes become shadowed
/// (copy-on-write) so parent diffs never overwrite them.
pub(crate) async fn scan_t_block(ctx: &Ctx, n: u64) -> Result<()> {
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
