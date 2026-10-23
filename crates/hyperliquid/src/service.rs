//! The simulator service's composition root (the `venue-sim` bin is the
//! shell over [`sim_entry`] — the same shell as the engines').
//!
//! Boot order (plan §7.2): load the instance, build the market plane (REST
//! poller + WS client) and the account plane (persisted registry), then serve
//! the daloc-facing info API (8090) and the control API (8091) beside the
//! self-advance tick (1 Hz: accrue funding, check liquidations, persist when
//! something moved).
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


