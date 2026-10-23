//! mirror-engine: replay mainnet blocks onto M, keep T's watch-list state fresh
//! from EIP-7928 BALs, verify every block, and expose a small admin surface.
mod bal;
mod blob;
mod ctx;
mod diff;
mod driver;
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
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

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
    mirror_config::boot::init_logging();
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

/// 15 minutes: reth unwinding and the anvils re-forking both take minutes on
/// a bad source day, and a crash-loop is worse than a slow boot.
const ANVIL_BOOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// Ethereum mainnet: what both anvils must fork (a code fact, plan §19.6).
const MAINNET_CHAIN_ID: u64 = 1;

async fn run(config: PathBuf) -> Result<()> {
    let cfg = Instance::load(&config)?;
    let (reth_url, m_url, t_url) = cfg.ethereum_urls()?;
    let ctx = Arc::new(Ctx::new(cfg, &reth_url, &m_url, &t_url)?);
    ctx.restore_blob_form()?;

    // M is engine-owned: transactions are batched and mined explicitly.
    // A coordinated restart (anvil, reth or lighthouse rolling) must not turn
    // into CrashLoopBackOff: wait for both anvils to answer before proceeding,
    // then fail fast only if they stay down.
    ctx.anvil_m.wait_until_up("anvil-m", ANVIL_BOOT_TIMEOUT).await?;
    ctx.anvil_m.anvil_set_automine(false).await?;
    ctx.anvil_t.wait_until_up("anvil-t", ANVIL_BOOT_TIMEOUT).await?;
    ctx.anvil_t.anvil_set_automine(false).await?;
    // Both anvils fork Ethereum mainnet: a wrong source (a testnet reth, a
    // misdirected URL) would mirror the wrong chain silently - state,
    // addresses and every measured fact follow from the id. The venue asserts
    // V's identity the same way at boot.
    for (anvil, rpc) in [("anvil-m", &ctx.anvil_m), ("anvil-t", &ctx.anvil_t)] {
        let got = rpc.chain_id().await?;
        if got != MAINNET_CHAIN_ID {
            return Err(Error::WrongChain {
                anvil,
                got,
                want: MAINNET_CHAIN_ID,
            });
        }
    }

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
        tasks.push(tokio::spawn(driver::replay_loop(ctx.clone())));
    }
    if ctx.engine_api.is_some() {
        tasks.push(tokio::spawn(driver::source_sync_supervisor(ctx.clone())));
    }
    if ctx.engine.loops.apply_into_t {
        tasks.push(tokio::spawn(driver::apply_loop(ctx.clone())));
        tasks.push(tokio::spawn(driver::shadow_loop(ctx.clone())));
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
