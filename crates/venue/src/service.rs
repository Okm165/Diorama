//! The simulator service's composition root (the `venue-sim` bin is the
//! shell over [`sim_entry`] — the same shell as the engines').
//!
//! Boot order (plan §7.2): load the instance, build the market plane (REST
//! poller + WS client) and the account plane (persisted registry), then serve
//! the daloc-facing info API (8090) and the control API (8091) beside the
//! self-advance tick (1 Hz: accrue funding, check liquidations, persist when
//! something moved).
use crate::Result;
use crate::{driver, http};
use crate::ctx::Ctx;
use clap::{Parser, Subcommand};
use mirror_config::boot;
use std::sync::Arc;
use std::time::Duration;

pub async fn sim_entry(args: Vec<String>) -> crate::Result<()> {
    boot::init_logging();
    let config = boot::service_config_or_exit(&args);
    let mut cfg = load_wiring(&config)?;
    let (chain, sim_section) = cfg.take_venue_sim()?;

    // The planes: the market feed's cache and the persisted account registry.
    let feed = Arc::new(crate::feed::MarketFeed::load(
        sim_section.state_dir.join("feed.json"),
        sim_section.markets.clone(),
    )?);
    let sim = Arc::new(crate::sim::Sim::load(
        sim_section.state_dir.join("sim.json"),
    )?);
    let api_client = crate::feed::rest::RestClient::new(&chain.api_url)?;
    let bridge_metrics = Arc::new(crate::bridge::metrics::BridgeMetrics::default());
    let state = Arc::new(crate::api::ApiState::new(
        feed.clone(),
        sim.clone(),
        api_client.clone(),
        sim_section.engine_admin_url.clone(),
        Some(bridge_metrics.clone()),
    )?);

    spawn_market_plane(&feed, api_client, &chain.ws_url, &sim_section);
    spawn_plane_heartbeat(&feed, &sim, &chain.api_url);
    spawn_bridge(&sim_section, &sim, &feed, &bridge_metrics)?;
    spawn_tick(&sim, &feed);
    let (info, control) = serve(&sim_section, &state);
    tracing::info!(
        info = %format!("{}:{}", sim_section.info.addr, sim_section.info.port),
        control = %format!("{}:{}", sim_section.control.addr, sim_section.control.port),
        markets = sim_section.markets.len(),
        api = %chain.api_url,
        "venue simulator listening"
    );

    boot::shutdown_signal().await;
    tracing::info!("shutdown signal received");
    info.abort();
    control.abort();
    sim.save()?;
    Ok(())
}

/// Load the instance and require this pod's wiring to be resolved: one
/// instance file, many readers — the venue engine's and the bridge's
/// references stay unresolved here, by design.
fn load_wiring(config: &std::path::Path) -> crate::Result<mirror_config::Instance> {
    let (cfg, unresolved) = mirror_config::Instance::inspect(config)?;
    if let (Some(chain), Some(sim)) = (cfg.chains.venue.as_ref(), cfg.services.venue_sim.as_ref()) {
        let state_dir = sim.state_dir.to_string_lossy().to_string();
        mirror_config::require_resolved_logged(
            "venue simulator wiring",
            &[
                &chain.source_url,
                &state_dir,
                sim.engine_admin_url.as_deref().unwrap_or_default(),
                sim.anvil_url.as_deref().unwrap_or_default(),
            ],
            &unresolved,
        )?;
    }
    Ok(cfg)
}

/// The market plane: REST snapshots + WS pushes (both write the same cache).
fn spawn_market_plane(
    feed: &Arc<crate::feed::MarketFeed>,
    api_client: crate::feed::rest::RestClient,
    ws_url: &str,
    sim_section: &mirror_config::VenueSimSection,
) {
    let poller = crate::feed::rest::Poller::new(
        feed.clone(),
        api_client,
        Duration::from_millis(sim_section.poll_interval_ms),
    );
    let ws = crate::feed::ws::WsClient::new(feed.clone(), ws_url.to_string());
    tokio::spawn(poller.run());
    tokio::spawn(ws.run());
}

/// The market plane's heartbeat: one INFO line a minute naming the live
/// market — each coin's mark and its age (the live-following visibility the
/// WS connect line alone never gives; a scripted coin's mark reads
/// `scripted`).
fn spawn_plane_heartbeat(
    feed: &Arc<crate::feed::MarketFeed>,
    sim: &Arc<crate::sim::Sim>,
    api_url: &str,
) {
    let feed = feed.clone();
    let sim = sim.clone();
    let via = mirror_devnet::engine::source_host(api_url).to_string();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let now = crate::now_ms();
            let coins: Vec<String> = feed
                .markets
                .iter()
                .map(|c| match (feed.mark(c), feed.stale_ms(c, now)) {
                    (Some(m), age) => format!(
                        "{c}={m}@{}",
                        age.map(|a| format!("{a}ms"))
                            .unwrap_or_else(|| "scripted".to_string())
                    ),
                    _ => format!("{c}=-"),
                })
                .collect();
            // The account plane beside the market plane: the registered
            // accounts, the scenario-owned book levels, and the identity
            // violations (any nonzero value is a construction regression).
            tracing::info!(
                coins = %coins.join(" "),
                via = %via,
                accounts = sim.metrics.accounts.load(std::sync::atomic::Ordering::Relaxed),
                owned_levels = feed.owned_levels(),
                identity_violations = sim
                    .metrics
                    .identity_violations
                    .load(std::sync::atomic::Ordering::Relaxed),
                "market plane live"
            );
        }
    });
}

/// The Core↔EVM bridge (when V is configured): system-address credits and
/// CoreWriter actions become sim state.
fn spawn_bridge(
    sim_section: &mirror_config::VenueSimSection,
    sim: &Arc<crate::sim::Sim>,
    feed: &Arc<crate::feed::MarketFeed>,
    metrics: &Arc<crate::bridge::metrics::BridgeMetrics>,
) -> crate::Result<()> {
    match &sim_section.anvil_url {
        Some(v_url) => {
            let v = mirror_anvil::Rpc::new(v_url, Duration::from_secs(30))?;
            let bridge = crate::bridge::Bridge::new(
                v,
                sim.clone(),
                feed.clone(),
                metrics.clone(),
            );
            tokio::spawn(bridge.run());
        }
        None => {
            tracing::warn!("services.sim.anvil_url is not set: the Core<->EVM bridge is disabled");
        }
    }
    Ok(())
}

/// The account plane's self-advance tick: funding + liquidations + persist.
fn spawn_tick(
    sim: &Arc<crate::sim::Sim>,
    feed: &Arc<crate::feed::MarketFeed>,
) {
    let tick_feed = feed.clone();
    let tick_sim = sim.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if let Err(e) = tick_sim.evaluate(&tick_feed) {
                tracing::warn!(error = %e, "sim tick failed; will retry");
            }
        }
    });
}

/// Serve the two listeners: the info API daloc reads, the control API the
/// harness drives. The handles are the shutdown tail's.
fn serve(
    sim_section: &mirror_config::VenueSimSection,
    state: &Arc<crate::api::ApiState>,
) -> (
    tokio::task::JoinHandle<crate::Result<()>>,
    tokio::task::JoinHandle<crate::Result<()>>,
) {
    let info_addr = format!("{}:{}", sim_section.info.addr, sim_section.info.port);
    let control_addr = format!("{}:{}", sim_section.control.addr, sim_section.control.port);
    let info = tokio::spawn(crate::api::serve(
        info_addr,
        crate::api::info_router(state.clone()),
    ));
    let control = tokio::spawn(crate::api::serve(
        control_addr,
        crate::api::control_router(state.clone()),
    ));
    (info, control)
}

// ---- the engine entry -----------------------------------------------------

/// The Venue Mirror engine's CLI.
#[derive(Parser)]
#[command(name = "venue-engine", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// The subcommands: `run` | `selftest` (the same shape as the mainnet
/// engine's, minus `rewind` — the venue's journal holds only the engine's own
/// writes, so a parent-diff rewind is not a venue operation).
#[derive(Subcommand)]
enum Cmd {
    /// Run the engine loops and the HTTP API.
    Run {
        #[arg(long, default_value = mirror_config::boot::DEFAULT_INSTANCE_PATH)]
        config: std::path::PathBuf,
    },
    /// End-to-end self test against a public RPC + one scratch Anvil fork.
    Selftest {
        #[arg(long)]
        rpc: String,
        #[arg(long)]
        anvil_v: String,
        #[arg(long, default_value = mirror_config::boot::DEFAULT_INSTANCE_PATH)]
        config: std::path::PathBuf,
        /// Minimum windows to compose (the run continues, bounded, until a
        /// window composes a change).
        #[arg(long, default_value_t = 5)]
        windows: u64,
    },
}

/// The Venue Mirror engine's entrypoint (the bin is the shell over this; the
/// same shell as `mainnet-engine`, R3).
///
/// Boot order (plan §7.2): wait for anvil V, assert the chain, reconcile the
/// registry (reset detection + the epoch rule; the reconcile also anchors the
/// epoch pin before any writes exist — the counterpart of the mainnet
/// engine's explicit boot anchor), run the at-pin probes, then serve the HTTP
/// surface and the driver loop. A boot fault is fatal: a driver over the wrong
/// chain or a broken registry must not serve.
///
/// Shutdown mirrors the mainnet engine's: the signal aborts the tasks; every
/// mutation persists its state as it happens (shadow registry, cursor), so
/// there is nothing to drain.
pub async fn main_entry(args: Vec<String>) -> Result<()> {
    mirror_config::boot::init_logging();
    let cli: Cli = boot::parse_cli("venue-engine", args);
    match cli.cmd {
        Cmd::Run { config } => run(config).await,
        Cmd::Selftest {
            rpc,
            anvil_v,
            config,
            windows,
        } => selftest_cmd(
                config,
                crate::selftest::Targets {
                    rpc,
                    anvil: anvil_v,
                },
                windows,
            )
            .await,
    }
}

/// The `selftest` subcommand: load the instance, run the scratch-anvil
/// pipeline against the real source, and exit with its verdict.
async fn selftest_cmd(
    config: std::path::PathBuf,
    targets: crate::selftest::Targets,
    windows: u64,
) -> Result<()> {
    mirror_devnet::engine::selftest_entry(
        &config,
        |cfg, unresolved| {
            // The scratch run's endpoints come from the CLI: only the
            // boundaries `Ctx::new` builds regardless are this command's to
            // require.
            require_wiring(cfg, unresolved, "venue selftest wiring", &[])
                .map_err(crate::error::Error::from)
        },
        move |cfg| Box::pin(crate::selftest::run(cfg, targets, windows)),
    )
    .await
}

async fn run(config: std::path::PathBuf) -> Result<()> {
    mirror_devnet::engine::run_engine::<Ctx>(config).await
}

/// This command's wiring, by name: the endpoint values it needs (`endpoints`
/// — empty when the CLI supplies them) plus the state dir, which `Ctx::new`
/// builds unconditionally. Unresolved here is a boot error naming the
/// variable, never a literal used as an endpoint; the other services'
/// references in the same instance file are not this pod's to provide.
fn require_wiring(
    cfg: &mirror_config::Instance,
    unresolved: &[String],
    what: &'static str,
    endpoints: &[&str],
) -> std::result::Result<(), mirror_config::ConfigError> {
    mirror_devnet::engine::require_engine_wiring(cfg, unresolved, what, endpoints, &[])
}

/// The shell's service hooks (`mirror_devnet::engine::EngineService`): the
/// sequence is the spine's; these are V's own pieces.
impl mirror_devnet::engine::EngineService for Ctx {
    fn wiring_label() -> &'static str {
        "venue engine wiring"
    }

    fn urls(cfg: &mirror_config::Instance) -> Result<(String, String)> {
        Ok(cfg.venue_urls()?)
    }

    fn context(cfg: mirror_config::Instance, source_url: &str, target_url: &str) -> Result<Self> {
        Ctx::new(cfg, source_url, target_url)
    }

    fn boot(ctx: &Self) -> futures_util::future::BoxFuture<'_, Result<()>> {
        Box::pin(driver::boot(ctx))
    }

    fn log_start(ctx: &Self) {
        tracing::info!(
            source = %ctx.source_rpc.url(),
            anvil_v = %ctx.anvil_v.url(),
            pin = ?mirror_devnet::pin::resolved_pin(&ctx.pin),
            watchlist = ctx.mirror.scope().watch().accounts.len(),
            "mirror engine starting"
        );
    }

    fn spawn_loops(
        ctx: &std::sync::Arc<Ctx>,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<tokio::task::JoinHandle<()>>>> {
        Box::pin(async move {
            let tasks = vec![
                mirror_devnet::engine::spawn_http(ctx, http::app(ctx.clone())),
                tokio::spawn(driver::follow_loop(ctx.clone())),
                tokio::spawn(driver::mine_loop(ctx.clone())),
            ];
            Ok(tasks)
        })
    }
}

