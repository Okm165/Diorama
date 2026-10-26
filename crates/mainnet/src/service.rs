//! The engine service's composition root: the CLI (`run` / `selftest` /
//! `rewind`), the boot call, and the loop spawner — the story the
//! `diorama-mainnet-engine` bin shells over (`main_entry`). One home per service: the
//! venue crate's engine and simulator entries and the bridge's entry live the
//! same way.

use crate::error::{Error, Result};
use crate::{driver, feed, http, selftest};
use crate::ctx::Ctx;
use clap::{Parser, Subcommand};
use diorama_config::Instance;
use diorama_devnet::engine::Engine as _;
use std::path::PathBuf;
use std::sync::Arc;

/// The Mainnet Mirror engine's CLI.
#[derive(Parser)]
#[command(name = "diorama-mainnet-engine", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// The subcommands: `run` | `selftest` | `rewind`.
#[derive(Subcommand)]
enum Cmd {
    /// Run the engine loops and the HTTP API.
    Run {
        #[arg(long, default_value = diorama_config::boot::DEFAULT_INSTANCE_PATH)]
        config: PathBuf,
    },
    /// End-to-end self test against a public RPC + one scratch Anvil fork.
    Selftest {
        #[arg(long)]
        rpc: String,
        #[arg(long)]
        anvil_t: String,
        #[arg(long, default_value = diorama_config::boot::DEFAULT_INSTANCE_PATH)]
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
        #[arg(long, default_value = diorama_config::boot::DEFAULT_INSTANCE_PATH)]
        config: PathBuf,
        #[arg(long)]
        blocks: u64,
    },
}

/// The Mainnet Mirror engine's entrypoint (the bin is the shell over this; the
/// same shell as `diorama-venue-engine`, R3).
///
/// Boot order: wait for T, assert the chain, anchor the epoch pin
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
    diorama_config::boot::init_logging();
    let cli: Cli = diorama_config::boot::parse_cli("diorama-mainnet-engine", args);
    match cli.cmd {
        Cmd::Run { config } => run(config).await,
        Cmd::Selftest {
            rpc,
            anvil_t,
            config,
            blocks,
            timeout_secs,
        } => {
            let targets = selftest::Targets { rpc, anvil: anvil_t };
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
    diorama_devnet::engine::selftest_entry(
        &config,
        |cfg, unresolved| {
            // The scratch run's endpoints come from the CLI: only the
            // boundaries `Ctx::new` builds regardless are this command's to
            // require.
            require_wiring(cfg, unresolved, "mainnet selftest wiring", &[]).map_err(Error::from)
        },
        move |mut cfg| {
            Box::pin(async move {
                // The CLI's request timeout wins for the scratch run.
                cfg.services
                    .engine
                    .as_mut()
                    .ok_or_else(|| {
                        Error::Config(diorama_config::ConfigError::Invalid {
                            issues: vec![
                                "profile \"mainnet\" requires [services.engine]".into(),
                            ],
                        })
                    })?
                    .reth
                    .request_timeout_secs = timeout_secs;
                selftest::run(cfg, targets, blocks).await
            })
        },
    )
    .await
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

async fn run(config: PathBuf) -> Result<()> {
    diorama_devnet::engine::run_engine::<Ctx>(config).await
}

/// The shell's service hooks (`diorama_devnet::engine::EngineService`): the
/// sequence is the spine's; these are T's own pieces.
impl diorama_devnet::engine::EngineService for Ctx {
    fn wiring_label() -> &'static str {
        "mainnet engine wiring"
    }

    fn wiring_extra(cfg: &Instance) -> Vec<String> {
        self::wiring_extra(cfg)
    }

    fn urls(cfg: &Instance) -> Result<(String, String)> {
        Ok(cfg.mainnet_urls()?)
    }

    fn context(cfg: Instance, source_url: &str, target_url: &str) -> Result<Self> {
        Ctx::new(cfg, source_url, target_url)
    }

    fn boot(ctx: &Self) -> futures_util::future::BoxFuture<'_, Result<()>> {
        Box::pin(driver::boot(ctx))
    }

    fn log_start(ctx: &Self) {
        tracing::info!(
            reth = %ctx.reth.url(),
            anvil_t = %ctx.anvil_t.url(),
            follow = ctx.follow_tag(),
            watchlist = ctx.mirror.scope().watch().accounts.len(),
            "mirror engine starting"
        );
    }

    fn spawn_loops(
        ctx: &Arc<Ctx>,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<tokio::task::JoinHandle<()>>>> {
        Box::pin(async move {
            let bridge = build_bridge(ctx).await?;
            let mut tasks = Vec::new();
            tasks.push(diorama_devnet::engine::spawn_http(
                ctx,
                http::app(ctx.clone(), bridge.clone()),
            ));
            if ctx.engine_api.is_some() {
                tasks.push(tokio::spawn(driver::source_sync_supervisor(ctx.clone())));
            }
            tasks.push(tokio::spawn(driver::apply_loop(ctx.clone())));
            tasks.push(tokio::spawn(driver::shadow_loop(ctx.clone())));
            if let Some(bridge) = &bridge {
                tasks.push(tokio::spawn(bridge.clone().run(ctx.clone())));
            }
            Ok(tasks)
        })
    }
}

/// This command's wiring, by name: the endpoints plus the boundaries only
/// this profile builds (`wiring_extra`). Unresolved here is a boot error
/// naming the variable, never a literal used as an endpoint.
fn require_wiring(
    cfg: &Instance,
    unresolved: &[String],
    what: &'static str,
    endpoints: &[&str],
) -> std::result::Result<(), diorama_config::ConfigError> {
    let extra = wiring_extra(cfg);
    let refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    diorama_devnet::engine::require_engine_wiring(cfg, unresolved, what, endpoints, &refs)
}

/// The profile-only boundary values this instance must resolve: the journal's
/// ring, the beacon sources, the engine API's URL and JWT — the endpoints and
/// the state dir are the spine's to check.
fn wiring_extra(cfg: &Instance) -> Vec<String> {
    let mut extra: Vec<String> = Vec::new();
    let Some(engine) = cfg.services.engine.as_ref() else {
        return extra;
    };
    extra.push(engine.journal.dir.to_string_lossy().into_owned());
    extra.extend(engine.beacon.urls.iter().cloned());
    if let Some(url) = &engine.source_sync.engine_url {
        extra.push(url.clone());
    }
    if let Some(jwt) = &engine.source_sync.jwt_path {
        extra.push(jwt.to_string_lossy().into_owned());
    }
    extra
}

/// The feed bridge, when configured. Built before any loop serves: a fault
/// here is fatal (a mis-scaled price would poison every scenario).
async fn build_bridge(ctx: &Arc<Ctx>) -> Result<Option<Arc<feed::FeedBridge>>> {
    if ctx.engine.feed_bridge.is_none() {
        return Ok(None);
    }
    let bridge = Arc::new(feed::FeedBridge::new(ctx).await?);
    tracing::info!(feeds = bridge.list().len(), "feed bridge active");
    Ok(Some(bridge))
}

