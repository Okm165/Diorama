//! The Venue Simulator binary — the `venue` profile's market + account plane.
//!
//! Boot order (plan §7.2): load the instance, build the market plane (REST
//! poller + WS client) and the account plane (persisted registry), then serve
//! the daloc-facing info API (8090) and the control API (8091) beside the
//! self-advance tick (1 Hz: accrue funding, check liquidations, persist when
//! something moved).
//!
//! Thin shell by design: the runtime is built here, the story lives in
//! `mirror_hyperliquid`.
use mirror_config::boot;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

fn main() -> ExitCode {
    boot::run_main(run(std::env::args().skip(1).collect()))
}

async fn run(args: Vec<String>) -> mirror_hyperliquid::Result<()> {
    boot::init_logging();
    let config = match boot::service_config(&args) {
        Ok(config) => config,
        Err(usage) => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    };
    let mut cfg = load_wiring(&config)?;
    let (chain, sim_section) = cfg.take_sim()?;

    // The planes: the market feed's cache and the persisted account registry.
    let feed = Arc::new(mirror_hyperliquid::feed::MarketFeed::load(
        sim_section.state_dir.join("feed.json"),
        sim_section.markets.clone(),
    )?);
    let sim = Arc::new(mirror_hyperliquid::sim::Sim::load(
        sim_section.state_dir.join("sim.json"),
    )?);
    let api_client = mirror_hyperliquid::feed::rest::RestClient::new(&chain.api_url)?;
    let bridge_metrics = Arc::new(mirror_hyperliquid::bridge::metrics::BridgeMetrics::default());
    let state = Arc::new(mirror_hyperliquid::api::ApiState::new(
        feed.clone(),
        sim.clone(),
        api_client.clone(),
        sim_section.engine_admin_url.clone(),
        Some(bridge_metrics.clone()),
    )?);

    spawn_market_plane(&feed, api_client, &chain.ws_url, &sim_section);
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
fn load_wiring(config: &std::path::Path) -> mirror_hyperliquid::Result<mirror_config::Instance> {
    let (cfg, unresolved) = mirror_config::Instance::inspect(config)?;
    if let (Some(chain), Some(sim)) = (cfg.chains.venue.as_ref(), cfg.services.sim.as_ref()) {
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
    feed: &Arc<mirror_hyperliquid::feed::MarketFeed>,
    api_client: mirror_hyperliquid::feed::rest::RestClient,
    ws_url: &str,
    sim_section: &mirror_config::SimSection,
) {
    let poller = mirror_hyperliquid::feed::rest::Poller::new(
        feed.clone(),
        api_client,
        Duration::from_millis(sim_section.poll_interval_ms),
    );
    let ws = mirror_hyperliquid::feed::ws::WsClient::new(feed.clone(), ws_url.to_string());
    tokio::spawn(poller.run());
    tokio::spawn(ws.run());
}

/// The Core↔EVM bridge (when V is configured): system-address credits and
/// CoreWriter actions become sim state.
fn spawn_bridge(
    sim_section: &mirror_config::SimSection,
    sim: &Arc<mirror_hyperliquid::sim::Sim>,
    feed: &Arc<mirror_hyperliquid::feed::MarketFeed>,
    metrics: &Arc<mirror_hyperliquid::bridge::metrics::BridgeMetrics>,
) -> mirror_hyperliquid::Result<()> {
    match &sim_section.anvil_url {
        Some(v_url) => {
            let v = mirror_anvil::Rpc::new(v_url, Duration::from_secs(30))?;
            let bridge = mirror_hyperliquid::bridge::Bridge::new(
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
    sim: &Arc<mirror_hyperliquid::sim::Sim>,
    feed: &Arc<mirror_hyperliquid::feed::MarketFeed>,
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
    sim_section: &mirror_config::SimSection,
    state: &Arc<mirror_hyperliquid::api::ApiState>,
) -> (
    tokio::task::JoinHandle<mirror_hyperliquid::Result<()>>,
    tokio::task::JoinHandle<mirror_hyperliquid::Result<()>>,
) {
    let info_addr = format!("{}:{}", sim_section.info.addr, sim_section.info.port);
    let control_addr = format!("{}:{}", sim_section.control.addr, sim_section.control.port);
    let info = tokio::spawn(mirror_hyperliquid::api::serve(
        info_addr,
        mirror_hyperliquid::api::info_router(state.clone()),
    ));
    let control = tokio::spawn(mirror_hyperliquid::api::serve(
        control_addr,
        mirror_hyperliquid::api::control_router(state.clone()),
    ));
    (info, control)
}


