//! The engine service's entry: the CLI (`run` / `selftest` / `rewind`), the
//! boot asserts, and the loop spawner — the story the `mainnet-engine` bin
//! shells over (`main_entry`). One home per service: the venue crate's engine
//! and simulator entries and the bridge's entry live the same way.

use crate::error::{Error, Result};
use crate::{driver, feed_bridge, http, selftest};
use crate::ctx::Ctx;
use clap::{Parser, Subcommand};
use mirror_config::Instance;
use mirror_devnet::engine::Engine as _;
use std::path::PathBuf;
use std::sync::Arc;

/// The Mainnet Mirror engine's CLI.
#[derive(Parser)]
#[command(name = "mainnet-engine", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// The subcommands: `run` | `selftest` | `rewind`.
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

/// The Mainnet Mirror engine's entrypoint (the bin is the shell over this; the
/// same shell as `venue-engine`, R3).
///
/// Boot order (plan §7.2): wait for T, assert the chain, anchor the epoch pin
/// (a fresh registry's epoch *is* the current fork base — the counterpart of
/// the venue engine's reconcile, which anchors it the same way), audit the
/// declared capabilities, then serve the HTTP surface and the applier, shadow
/// and top-up loops. A boot fault is fatal: an applier over the wrong chain or
/// an unanchored epoch must not serve.
///
/// Shutdown mirrors the venue engine's: the signal aborts the tasks; every
/// mutation persists its state as it happens (shadow registry, cursor,
/// journal), so there is nothing to drain.
pub async fn main_entry(args: Vec<String>) -> Result<()> {
    mirror_config::boot::init_logging();
    let cli: Cli = mirror_config::boot::parse_cli("mainnet-engine", args);
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
    let (mut cfg, unresolved) = Instance::inspect(&config)?;
    // The scratch run's endpoints come from the CLI: only the boundaries
    // `Ctx::new` builds regardless are this command's to require.
    require_wiring(&cfg, &unresolved, "mainnet selftest wiring", &[])?;
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
    mirror_config::boot::exit_verdict(ok);
}

/// The `rewind` subcommand: undo the last N applied parent blocks on T.
async fn rewind_cmd(config: std::path::PathBuf, blocks: u64) -> Result<()> {
    let (cfg, unresolved) = Instance::inspect(&config)?;
    let (reth, t) = cfg.mainnet_urls()?;
    require_wiring(&cfg, &unresolved, "mainnet engine wiring", &[&reth, &t])?;
    let ctx = Ctx::new(cfg, &reth, &t)?;
    let out = ctx.admin().rewind(blocks).await?;
    println!(
        "rewound {blocks} block(s): {} values restored, cursor_t now {}",
        out.restored, out.cursor
    );
    Ok(())
}

/// This command's wiring, by name: the endpoint values it needs (`endpoints`
/// — empty when the CLI supplies them), the state dirs, the CL endpoints and
/// the top-up boundary, which `Ctx::new` builds unconditionally. Unresolved
/// here is a boot error naming the variable, never a literal used as an
/// endpoint; the other services' references in the same instance file are not
/// this pod's to provide.
fn require_wiring(
    cfg: &Instance,
    unresolved: &[String],
    what: &'static str,
    endpoints: &[&str],
) -> std::result::Result<(), mirror_config::ConfigError> {
    let Some(engine) = cfg.services.engine.as_ref() else {
        return Ok(());
    };
    let state_dir = engine.state_dir.to_string_lossy().into_owned();
    let journal = engine.journal.dir.to_string_lossy().into_owned();
    let jwt = engine
        .source_sync
        .jwt_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let mut values: Vec<&str> = endpoints.to_vec();
    values.push(&state_dir);
    values.push(&journal);
    values.extend(engine.beacon.urls.iter().map(String::as_str));
    if let Some(url) = &engine.source_sync.engine_url {
        values.push(url);
    }
    if let Some(jwt) = &jwt {
        values.push(jwt);
    }
    mirror_config::require_resolved_logged(what, &values, unresolved)
}

async fn run(config: PathBuf) -> Result<()> {
    // One instance file, many readers: this pod provides only the mainnet
    // engine's wiring. `inspect` reports the other services' unresolved
    // references instead of failing on them; the engine's own sections are
    // then required here, by name (the venue engine's, the simulator's and
    // the bridge's pattern).
    let (cfg, unresolved) = Instance::inspect(&config)?;
    let (reth_url, t_url) = cfg.mainnet_urls()?;
    require_wiring(&cfg, &unresolved, "mainnet engine wiring", &[&reth_url, &t_url])?;
    let ctx = Arc::new(Ctx::new(cfg, &reth_url, &t_url)?);
    driver::boot(&ctx).await?;

    tracing::info!(
        reth = %ctx.reth.url(),
        anvil_t = %ctx.anvil_t.url(),
        follow = ctx.follow_tag(),
        watchlist = ctx.mirror.scope().watch().accounts.len(),
        "mirror engine starting"
    );

    let bridge = build_bridge(&ctx).await?;
    let tasks = spawn_loops(&ctx, bridge);
    mirror_devnet::engine::shutdown(tasks).await;
    Ok(())
}

/// The feed bridge, when configured. Built before any loop serves: a fault
/// here is fatal (a mis-scaled price would poison every scenario).
async fn build_bridge(ctx: &Arc<Ctx>) -> Result<Option<Arc<feed_bridge::FeedBridge>>> {
    if ctx.engine.feed_bridge.is_none() {
        return Ok(None);
    }
    let bridge = Arc::new(feed_bridge::FeedBridge::new(ctx).await?);
    tracing::info!(feeds = bridge.list().len(), "feed bridge active");
    Ok(Some(bridge))
}

/// Spawn the profile's loops (the applier, the top-up supervisor, the shadow
/// scan, the feed bridge, the HTTP surface) and return their handles for the
/// shutdown tail.
fn spawn_loops(
    ctx: &Arc<Ctx>,
    bridge: Option<Arc<feed_bridge::FeedBridge>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    if ctx.engine_api.is_some() {
        tasks.push(tokio::spawn(driver::source_sync_supervisor(ctx.clone())));
    }
    tasks.push(tokio::spawn(driver::apply_loop(ctx.clone())));
    tasks.push(tokio::spawn(driver::shadow_loop(ctx.clone())));
    if let Some(bridge) = &bridge {
        tasks.push(tokio::spawn(bridge.clone().run(ctx.clone())));
    }
    tasks.push(mirror_devnet::engine::spawn_http(
        ctx,
        http::app(ctx.clone(), bridge),
    ));
    tasks
}
