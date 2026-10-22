//! mirror-engine: replay mainnet blocks onto M, keep T's watch-list state fresh
//! from EIP-7928 BALs, verify every block, and expose a small admin surface.
mod bal;
mod blob;
mod ctx;
mod diff;
mod drift;
mod engine_api;
mod error;
mod eth_source;
pub mod feed_bridge;
mod http;
mod replay;
mod selftest;
mod source_sync;
mod verify;

// R1/R2 extraction shims: the frozen `crate::rpc::*`, `crate::bal::*` and
// `crate::shadow|journal|state|metrics` paths keep resolving; the code lives
// in `mirror-anvil` / `mirror-devnet` (zero call-site churn).
pub use mirror_anvil as rpc;
pub use mirror_devnet::{journal, metrics, shadow, state};

pub use error::{Error, Result};

use mirror_config::Instance;
use crate::ctx::Ctx;
use crate::diff::apply_block;
use crate::replay::replay_block;
use clap::{Parser, Subcommand};
use mirror_devnet::state::ChainCursor;
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
        #[arg(long, default_value = "/etc/mirror/instance.toml")]
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
        #[arg(long, default_value = "/etc/mirror/instance.toml")]
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
        #[arg(long, default_value = "/etc/mirror/instance.toml")]
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
            let mut cfg = Instance::load(&config)?;
            cfg.services
                .engine
                .as_mut()
                .ok_or_else(|| {
                    Error::Config(mirror_config::ConfigError::Invalid {
                        issues: vec!["profile \"ethereum\" requires [services.engine]".into()],
                    })
                })?
                .reth
                .request_timeout_secs = timeout_secs;
            let ok = selftest::run(cfg, rpc, anvil_m, anvil_t, beacon, blocks).await?;
            std::process::exit(if ok { 0 } else { 1 });
        }
        Cmd::Rewind { config, blocks } => {
            let cfg = Instance::load(&config)?;
            let (reth, m, t) = cfg.ethereum_urls()?;
            let ctx = Ctx::new(cfg, &reth, &m, &t)?;
            let out = ctx.admin().rewind(blocks).await?;
            println!(
                "rewound {blocks} block(s): {} values restored, cursor_t now {}",
                out.restored, out.cursor
            );
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
    let cfg = Instance::load(&config)?;
    let (reth_url, m_url, t_url) = cfg.ethereum_urls()?;
    let ctx = Arc::new(Ctx::new(cfg, &reth_url, &m_url, &t_url)?);
    ctx.restore_blob_form()?;

    // M is engine-owned: transactions are batched and mined explicitly.
    // A coordinated restart (anvil, reth or lighthouse rolling) must not turn
    // into CrashLoopBackOff: wait for both anvils to answer before proceeding,
    // then fail fast only if they stay down.
    wait_for_anvil(&ctx.anvil_m, "anvil-m").await?;
    ctx.anvil_m.anvil_set_automine(false).await?;
    wait_for_anvil(&ctx.anvil_t, "anvil-t").await?;
    ctx.anvil_t.anvil_set_automine(false).await?;

    tracing::info!(
        reth = %ctx.reth.url(),
        anvil_m = %ctx.anvil_m.url(),
        anvil_t = %ctx.anvil_t.url(),
        follow = ctx.follow_tag(),
        watchlist = ctx.watchlist.len(),
        replay_into_m = ctx.engine.loops.replay_into_m,
        apply_into_t = ctx.engine.loops.apply_into_t,
        "mirror engine starting"
    );

    let mut tasks = Vec::new();
    if ctx.engine.loops.replay_into_m {
        tasks.push(tokio::spawn(replay_loop(ctx.clone())));
    }
    if ctx.engine_api.is_some() {
        tasks.push(tokio::spawn(source_sync_supervisor(ctx.clone())));
    }
    if ctx.engine.loops.apply_into_t {
        tasks.push(tokio::spawn(apply_loop(ctx.clone())));
        tasks.push(tokio::spawn(shadow_loop(ctx.clone())));
    }
    // The feed bridge: only when configured; a boot fault here is fatal
    // (a mis-scaled price would poison every scenario).
    let bridge = match ctx.engine.feed_bridge.is_some() {
        true => {
            let bridge = Arc::new(feed_bridge::FeedBridge::new(&ctx).await?);
            tracing::info!(feeds = bridge.list().len(), "feed bridge active");
            tasks.push(tokio::spawn(bridge.clone().run(ctx.clone())));
            Some(bridge)
        }
        false => None,
    };
    {
        let ctx = ctx.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = http::serve(ctx, bridge).await {
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
fn shadow_has_recorded_state(ctx: &Ctx) -> Result<bool> {
    let s = ctx.shadow.lock()?;
    Ok(s.slot_count() > 0
        || !s.accounts.is_empty()
        || !s.values.is_empty()
        || !s.account_values.is_empty())
}

/// Decide the next parent block to process for one chain, applying the cursor,
/// rebase, gap, and reorg rules. `None` means "nothing to do right now".
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
    let follow = follow.saturating_sub(ctx.l1.follow_margin_blocks);
    // Metrics-only read: a reth blip must not fail the poll (the gauge reads 0
    // until reth answers again; the next pass overwrites it).
    let reth_head = ctx.reth.block_number().await.unwrap_or(0);
    ctx.metrics.set(&ctx.metrics.reth_head, reth_head);

    let (head, cursor, rpc) = chain_state(ctx, chain).await?;
    let cursor = validate_cursor(ctx, chain, rpc, head, cursor).await?;
    let (start, rebased) = resume_point(chain, rpc, cursor.as_ref(), head).await?;

    let forced = matches!(chain, Chain::M) && ctx.force_rebase.swap(false, Ordering::Relaxed);
    if matches!(chain, Chain::M) && (head > follow || forced) {
        rebase_m(ctx, chain, head, follow, forced).await?;
        return Ok(None);
    }
    if rebased {
        on_rebase(chain, rpc, head, follow, start).await?;
    }
    if start > follow {
        tracing::debug!(chain = ?chain, head, follow, start, "chain is at the follow head");
        return Ok(None);
    }
    if !gap_within_limit(ctx, chain, follow.saturating_sub(start), rebased) {
        return Ok(None);
    }
    if !reorg_continuity(ctx, chain, start).await? {
        return Ok(None);
    }
    Ok(Some(start))
}

/// The chain's head, cursor and RPC endpoint (the per-chain head metrics
/// included).
async fn chain_state(
    ctx: &Ctx,
    chain: Chain,
) -> Result<(u64, Option<ChainCursor>, &mirror_anvil::Rpc)> {
    let (head, cursor, rpc) = match chain {
        Chain::M => (
            ctx.anvil_m.block_number().await?,
            ctx.cursor_m()?,
            &ctx.anvil_m,
        ),
        Chain::T => (
            ctx.anvil_t.block_number().await?,
            ctx.cursor_t()?,
            &ctx.anvil_t,
        ),
    };
    match chain {
        Chain::M => ctx.metrics.set(&ctx.metrics.m_head, head),
        Chain::T => {
            ctx.metrics.set(&ctx.metrics.t_head, head);
            if let Ok(info) = rpc.anvil_node_info().await {
                if let Some(fork) = info.fork_config.fork_block_number {
                    ctx.metrics
                        .set(&ctx.metrics.t_fork_age_blocks, head.saturating_sub(fork));
                }
            }
        }
    }
    Ok((head, cursor, rpc))
}

/// Validate the cursor against the chain; a mismatch means the chain was reset
/// (re-pin / wiped state) and the cursor must rebase. Returns the cursor when
/// it is still valid, `None` after a rebase.
async fn validate_cursor(
    ctx: &Ctx,
    chain: Chain,
    rpc: &mirror_anvil::Rpc,
    head: u64,
    cursor: Option<ChainCursor>,
) -> Result<Option<ChainCursor>> {
    let Some(c) = cursor else {
        return Ok(None);
    };
    let hash = mirror_devnet::scan::block_hash(rpc, c.block).await?;
    if hash.as_deref() == Some(c.hash.as_str()) {
        return Ok(Some(c));
    }
    tracing::warn!(
        block = c.block,
        "cursor not found on chain (reset or re-pin) - rebasing"
    );
    if matches!(chain, Chain::T) {
        handle_t_reset(ctx, rpc).await?;
    }
    // An M re-base skipped this range: those blocks are mainnet's own (the
    // fresh fork serves them and the state comes from the fork base, so it is
    // exact by construction), but the engine did not re-execute them - their
    // tx-level verification is skipped. Bounded by `fork_offset`; counted and
    // logged so the gap is never invisible.
    if matches!(chain, Chain::M) {
        account_m_rebase_skip(ctx, c.block, head);
    }
    // Drop it from the persisted state too: `first_block` is an epoch marker,
    // and the next processed block must set a fresh one (otherwise
    // `mirror_pin_age_blocks` counts across epochs).
    {
        let mut state = ctx.state.lock()?;
        match chain {
            Chain::M => state.data.m = None,
            Chain::T => state.data.t = None,
        }
    }
    ctx.save_state()?;
    Ok(None)
}

/// T was re-forked (its OOM restart or a re-pin). Which epoch is this?
///
/// - The fork pin *changed*: the base moved, so the recorded values are stale
///   derivations of the old base (plan 2.2/6.3: "derived values go stale when
///   the base advances"). Start a *new epoch*: drop marks and values - the
///   product re-creates its positions from its own journal (plan 3.6).
///   Measured: restoring across a pin change re-applied a +30d time-travel
///   write as a future `lastUpdateTimestamp` and every Aave supply panicked
///   with an arithmetic underflow.
/// - The pin is *unchanged* (an out-of-band reset at the same base): the
///   recorded writes are still coherent - re-apply them and keep the marks.
///
/// Fail-closed on the pin read: without it the epoch decision would default to
/// "same pin" and re-apply writes across a pin change - the measured
/// Aave-underflow hazard.
async fn handle_t_reset(ctx: &Ctx, rpc: &mirror_anvil::Rpc) -> Result<()> {
    let pin = rpc.anvil_node_info().await?.fork_config.fork_block_number;
    let known = ctx.state.lock()?.data.epoch_pin;
    let new_epoch = matches!((known, pin), (Some(k), Some(p)) if k != p);
    ctx.metrics.inc(&ctx.metrics.t_resets_total);
    if new_epoch {
        let (slots, accounts) = ctx.shadow.lock()?.clear_all();
        ctx.save_shadow()?;
        tracing::warn!(
            pin = ?pin,
            previous = ?known,
            cleared_slots = slots,
            cleared_accounts = accounts,
            "new epoch: the fork base moved; the devnet's writes are gone with it (re-create positions from your journal)"
        );
    } else if shadow_has_recorded_state(ctx)? {
        restore_shadow(ctx).await?;
    }
    if pin.is_some() && known != pin {
        ctx.state.lock()?.data.epoch_pin = pin;
        ctx.save_state()?;
    }
    // The journal holds before-values of the *old* chain: a rewind against the
    // re-forked chain would restore values that never existed there. Drop it.
    if let Err(e) = ctx.journal.clear() {
        tracing::warn!(error = %e, "clearing the journal after a T reset failed");
    }
    Ok(())
}

/// Same-pin restore: prune orphaned values, re-apply the recorded writes, and
/// report marked-but-unrecorded keys (the shared implementation lives in
/// `mirror-devnet::shadow`).
async fn restore_shadow(ctx: &Ctx) -> Result<()> {
    let stats = mirror_devnet::shadow::restore_recorded(
        &ctx.anvil_t,
        &ctx.shadow,
        ctx.engine.loops.chunk_size,
    )
    .await?;
    ctx.save_shadow()?;
    tracing::warn!(
        slots = stats.slots,
        accounts = stats.accounts,
        storage_accounts = stats.storage_accounts,
        account_values = stats.account_values,
        restored = stats.restored,
        orphan_slots = stats.orphan_slots,
        orphan_accounts = stats.orphan_accounts,
        unknown_slots = stats.unknown_slots,
        unknown_accounts = stats.unknown_accounts,
        "T was reset at the same pin: re-applied the devnet's writes"
    );
    Ok(())
}

/// An M re-base skipped a range (see [`validate_cursor`]).
fn account_m_rebase_skip(ctx: &Ctx, from: u64, head: u64) {
    let skipped = head.saturating_sub(from);
    if skipped > 0 {
        ctx.metrics.add(&ctx.metrics.m_rebase_skipped, skipped);
        tracing::warn!(
            from,
            to = head,
            skipped,
            "M re-base skipped a range (fork base: state exact, tx verification skipped)"
        );
    }
}

/// The block to resume from, plus whether this is a rebase. A reset chain
/// (re-pin / wiped state): T's state is exactly the fork pin's state, so the
/// applier resumes from the pin. Using T's own height + 1 would be wrong - T
/// mines one block per applied parent block (plus one per journal rewind), so
/// its height is not a parent height and `head + 1` can land past the follow
/// head, stalling T forever (measured). Fail-closed on the pin read: a fault
/// here must not silently pick that stalled path.
async fn resume_point(
    chain: Chain,
    rpc: &mirror_anvil::Rpc,
    cursor: Option<&ChainCursor>,
    head: u64,
) -> Result<(u64, bool)> {
    let reset_pin = if cursor.is_none() {
        rpc.anvil_node_info()
            .await?
            .fork_config
            .fork_block_number
    } else {
        None
    };
    Ok(match (chain, reset_pin) {
        (Chain::T, Some(pin)) => (pin + 1, true),
        _ => state::next_block(cursor, head),
    })
}

/// Re-base M at a fresh pin behind the follow head.
///
/// M's replay is irreversible, but the follow head can move *backward*: the
/// CL's forkchoice may report a lower safe block (reorg / failed
/// justification), which strands M until it catches up (measured: a 42-block
/// recede stalling ~10 min, with a warning every poll). Re-base M at a fresh
/// pin behind the follow head instead; the cursor hash check on the next pass
/// rebases the replay. The blocks between the old cursor and the new pin are
/// mainnet's own (the fork serves them and the state comes from the fork base,
/// so it is exact by construction), but they are not re-executed: their
/// tx-level verification is skipped. Bounded by `fork_offset` per re-base and
/// counted in `mirror_m_rebase_skipped_blocks_total` with a WARN naming the
/// range, so the gap is never invisible. T is unaffected: the epoch/COW state
/// is keyed on T's own fork block, not M's - and T is exempt from this check
/// entirely (its heights advance independently of mainnet's).
async fn rebase_m(ctx: &Ctx, chain: Chain, head: u64, follow: u64, forced: bool) -> Result<()> {
    {
        let mut last = ctx.last_stall.lock()?;
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
    // At most one attempt per minute: a failed re-base retries on later polls.
    let attempt = {
        let mut t = ctx.last_rebase.lock()?;
        let due = t
            .map(|i: std::time::Instant| i.elapsed() > std::time::Duration::from_secs(60))
            .unwrap_or(true);
        if due {
            *t = Some(std::time::Instant::now());
        }
        due
    };
    if !attempt {
        return Ok(());
    }
    match ctx.anvil_m.fork_config().await {
        Ok(Some((url, _))) => {
            let pin = follow.saturating_sub(ctx.l1.fork_offset);
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
    Ok(())
}

/// A rebase means an anvil restart (re-pin, OOM) happened, which resets
/// automine to its default (on). Re-assert it: otherwise each raw tx mines its
/// own block and the engine's mine(1) produces empty blocks. (The epoch
/// decision - clear vs restore - lives with the reset handling above, where
/// the fork pin is compared before anything is written.)
async fn on_rebase(
    chain: Chain,
    rpc: &mirror_anvil::Rpc,
    head: u64,
    follow: u64,
    start: u64,
) -> Result<()> {
    if let Err(e) = rpc.anvil_set_automine(false).await {
        tracing::warn!(chain = ?chain, error = %e, "could not re-assert automine=false");
    }
    tracing::info!(chain = ?chain, head, follow, start, "starting chain at block");
    Ok(())
}

/// Two regimes (plan §3 ops): normal operation pauses at `max_gap_blocks`
/// (300 = the plan's "> 300 -> pause and re-sync"), while a *rebased* cursor
/// (a fresh pin after a restart/re-fork) is allowed to replay its whole pin
/// window - bounded by reth's state window (10,064), the plan's own limit.
fn gap_within_limit(ctx: &Ctx, chain: Chain, gap: u64, rebased: bool) -> bool {
    let limit = if rebased {
        10_064
    } else {
        ctx.engine.loops.max_gap_blocks
    };
    if gap > limit {
        tracing::error!(
            chain = ?chain,
            gap,
            max = limit,
            rebased,
            "sync gap too large - paused; run the re-pin drill (make repin)"
        );
        return false;
    }
    true
}

/// Upstream reorg continuity: only meaningful when we processed start-1, and
/// it belongs to the *M* loop alone - it owns the parent-stream integrity
/// (`reth_last_*` is written by the M replay). T consumes the same blocks
/// through the diff applier, whose absolute-value writes self-heal across a
/// reorg, and its own cursor check rebases on a reset. Running the check in
/// both loops double-counted every reorg. Returns `false` to stop this poll.
async fn reorg_continuity(ctx: &Ctx, chain: Chain, start: u64) -> Result<bool> {
    if !matches!(chain, Chain::M) {
        return Ok(true);
    }
    let (Some(last_hash), Some(last_block)) = (ctx.reth_last_hash()?, ctx.reth_last_block()?)
    else {
        return Ok(true);
    };
    if start > 1 && start - 1 == last_block {
        if let Some(prev) = mirror_devnet::scan::block_hash(&ctx.reth, start - 1).await? {
            if prev != last_hash {
                ctx.metrics.inc(&ctx.metrics.reorgs_detected);
                ctx.on_divergence(&format!("reth reorg at {}", start - 1));
                return Ok(false);
            }
        }
    }
    Ok(true)
}

async fn replay_loop(ctx: Arc<Ctx>) {
    let poll = Duration::from_millis(ctx.engine.loops.poll_interval_ms.max(200));
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
    let poll = Duration::from_millis(ctx.engine.loops.poll_interval_ms.max(200));
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
async fn wait_for_anvil(anvil: &crate::rpc::Rpc, name: &'static str) -> Result<()> {
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
                    return Err(Error::NotReady {
                        name,
                        timeout_secs: 900,
                        source: Some(e),
                    });
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
async fn shadow_loop(ctx: Arc<Ctx>) {
    let poll = Duration::from_millis(ctx.engine.loops.poll_interval_ms.max(200));
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
        if let Err(e) =
            mirror_devnet::scan::scan_range(&ctx.anvil_t, &ctx.shadow, &ctx.shadow_scanned_upto, last, t_head)
                .await
        {
            tracing::warn!(error = %e, "shadow scan failed; will retry from the last clean block");
        }
        // A save fault is logged, not swallowed: the marks are in memory and
        // the next pass retries the write (bounded: one warn per poll).
        if let Err(e) = ctx.save_shadow() {
            tracing::warn!(error = %e, "shadow registry save failed");
        }
    }
}

#[cfg(test)]
mod design_assertions {
    /// Public types stay Send + Sync + Sized + Unpin (Rust-for-Rustaceans);
    /// the engine hands `Ctx` to every loop and the HTTP surface.
    #[test]
    fn design_assertions() {
        fn is_normal<T: Send + Sync + Sized + Unpin>() {}
        is_normal::<crate::ctx::Ctx>();
    }
}
