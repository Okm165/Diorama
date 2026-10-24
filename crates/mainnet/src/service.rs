//! The engine service's entry: the CLI (`run` / `selftest` / `rewind`), the
//! boot asserts, and the loop spawner — the story the `mainnet-engine` bin
//! shells over (`main_entry`). One home per service: the venue crate's engine
//! and simulator entries and the bridge's entry live the same way.

use crate::error::{Error, Result};
use crate::{driver, feed_bridge, http, selftest};
use crate::ctx::Ctx;
use clap::{Parser, Subcommand};
use mirror_config::Instance;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "mainnet-engine", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the engine loops and the HTTP API.
    Run {
        #[arg(long, default_value = mirror_config::boot::DEFAULT_INSTANCE_PATH)]
        config: PathBuf,
    },
    /// End-to-end self test against a public RPC + one scratch Anvil fork.
    Selftest {
        #[arg(long)]
        rpc: String,
        #[arg(long)]
        anvil_t: String,
        #[arg(long, default_value = mirror_config::boot::DEFAULT_INSTANCE_PATH)]
        config: PathBuf,
        /// Number of consecutive blocks to test starting at the anvil pin + 1.
        #[arg(long, default_value_t = 3)]
        blocks: u64,
        /// RPC timeout for the scratch run (remote-fork mining is 55-256 s/block).
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
    },
    /// Rewind the last N applied parent blocks on T (journal-based).
    Rewind {
        #[arg(long, default_value = mirror_config::boot::DEFAULT_INSTANCE_PATH)]
        config: PathBuf,
        #[arg(long)]
        blocks: u64,
    },
}

pub async fn main_entry(args: Vec<String>) -> Result<()> {
    mirror_config::boot::init_logging();
    let cli = Cli::parse_from(std::iter::once("mainnet-engine".to_string()).chain(args));
    match cli.cmd {
        Cmd::Run { config } => run(config).await,
        Cmd::Selftest {
            rpc,
            anvil_t,
            config,
            blocks,
            timeout_secs,
        } => {
            let targets = selftest::Targets { rpc, anvil_t };
            selftest_cmd(config, targets, blocks, timeout_secs).await
        }
        Cmd::Rewind { config, blocks } => rewind_cmd(config, blocks).await,
    }
}

/// The `selftest` subcommand: load the instance, apply the CLI's request
/// timeout, run the scratch-anvil pipeline, and exit with its verdict.
async fn selftest_cmd(
    config: std::path::PathBuf,
    targets: selftest::Targets,
    blocks: u64,
    timeout_secs: u64,
) -> Result<()> {
    let mut cfg = Instance::load(&config)?;
    cfg.services
        .engine
        .as_mut()
        .ok_or_else(|| {
            Error::Config(mirror_config::ConfigError::Invalid {
                issues: vec!["profile \"mainnet\" requires [services.engine]".into()],
            })
        })?
        .reth
        .request_timeout_secs = timeout_secs;
    let ok = selftest::run(cfg, targets, blocks).await?;
    std::process::exit(if ok { 0 } else { 1 });
}

/// The `rewind` subcommand: undo the last N applied parent blocks on T.
async fn rewind_cmd(config: std::path::PathBuf, blocks: u64) -> Result<()> {
    let cfg = Instance::load(&config)?;
    let (reth, t) = cfg.mainnet_urls()?;
    let ctx = Ctx::new(cfg, &reth, &t)?;
    let out = ctx.admin().rewind(blocks).await?;
    println!(
        "rewound {blocks} block(s): {} values restored, cursor_t now {}",
        out.restored, out.cursor
    );
    Ok(())
}

/// 15 minutes: reth unwinding and the anvils re-forking both take minutes on
/// a bad source day, and a crash-loop is worse than a slow boot.
const ANVIL_BOOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// Ethereum mainnet: what both anvils must fork (a code fact, plan §19.6).
const MAINNET_CHAIN_ID: u64 = 1;

async fn run(config: PathBuf) -> Result<()> {
    let cfg = Instance::load(&config)?;
    let (reth_url, t_url) = cfg.mainnet_urls()?;
    let ctx = Arc::new(Ctx::new(cfg, &reth_url, &t_url)?);
    boot_asserts(&ctx).await?;

    tracing::info!(
        reth = %ctx.reth.url(),
        anvil_t = %ctx.anvil_t.url(),
        follow = ctx.follow_tag(),
        watchlist = ctx.mirror.scope().watch().accounts.len(),
        "mirror engine starting"
    );

    let tasks = spawn_loops(&ctx).await?;
    mirror_config::boot::shutdown_signal().await;
    tracing::info!("shutdown signal received");
    for t in tasks {
        t.abort();
    }
    Ok(())
}

/// The boot asserts, in order: the anvil answers (a coordinated restart —
/// anvil, reth or lighthouse rolling — must not turn into CrashLoopBackOff),
/// automine is off (T is engine-owned: transactions are batched and mined
/// explicitly), it forks Ethereum mainnet (a wrong source — a testnet reth, a
/// misdirected URL — would mirror the wrong chain silently: state, addresses
/// and every measured fact follow from the id; the venue asserts V's identity
/// the same way at boot), and the epoch pin is anchored before any writes
/// exist (a fresh registry's epoch *is* the current fork base; without the
/// record a later same-pin restart would be an unprovable epoch and the rule
/// would have to fail closed, dropping the devnet's writes — the venue's
/// boot-time reconcile anchors the same way).
async fn boot_asserts(ctx: &Ctx) -> Result<()> {
    ctx.anvil_t.wait_until_up("anvil-t", ANVIL_BOOT_TIMEOUT).await?;
    ctx.anvil_t.anvil_set_automine(false).await?;
    let got = ctx.anvil_t.chain_id().await?;
    if got != MAINNET_CHAIN_ID {
        return Err(Error::WrongChain {
            anvil: "anvil-t",
            got,
            want: MAINNET_CHAIN_ID,
        });
    }
    let t_pin = ctx.anvil_t.anvil_node_info().await?.fork_config.fork_block_number;
    mirror_devnet::epoch::anchor(&ctx.plane, t_pin)?;
    // The declared capabilities are audited once, against the live endpoint
    // (a declaration that disagrees is named here, never discovered
    // mid-window).
    mirror_devnet::source::audit_capabilities(
        "reth",
        ctx.mirror.follow_source().capabilities(),
        &ctx.reth,
    )
    .await?;
    Ok(())
}

/// Spawn the profile's loops (the applier, the top-up supervisor, the shadow
/// scan, the feed bridge, the HTTP surface) and return their handles for the
/// shutdown tail. The feed bridge is built only when configured; a boot fault
/// there is fatal (a mis-scaled price would poison every scenario).
async fn spawn_loops(ctx: &Arc<Ctx>) -> Result<Vec<tokio::task::JoinHandle<()>>> {
    let mut tasks = Vec::new();
    if ctx.engine_api.is_some() {
        tasks.push(tokio::spawn(driver::source_sync_supervisor(ctx.clone())));
    }
    tasks.push(tokio::spawn(driver::apply_loop(ctx.clone())));
    tasks.push(tokio::spawn(driver::shadow_loop(ctx.clone())));
    let bridge = match ctx.engine.feed_bridge.is_some() {
        true => {
            let bridge = Arc::new(feed_bridge::FeedBridge::new(ctx).await?);
            tracing::info!(feeds = bridge.list().len(), "feed bridge active");
            tasks.push(tokio::spawn(bridge.clone().run(ctx.clone())));
            Some(bridge)
        }
        false => None,
    };
    let http_ctx = ctx.clone();
    tasks.push(tokio::spawn(async move {
        if let Err(e) = http::serve(http_ctx, bridge).await {
            tracing::error!(error = %e, "http server exited");
        }
    }));
    Ok(tasks)
}
