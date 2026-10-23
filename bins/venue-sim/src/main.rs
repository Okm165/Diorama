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
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(boot::WORKER_STACK_SIZE)
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            boot::report_error(&e);
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        match run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                boot::report_error(&e);
                ExitCode::FAILURE
            }
        }
    })
}

async fn run() -> mirror_hyperliquid::Result<()> {
    boot::init_logging();
    let config = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/etc/mirror/instance.toml".to_string());
    // One instance file, many readers: this pod provides only the simulator's
    // wiring (the venue engine's and the bridge's references stay unresolved
    // here, by design).
    let (cfg, unresolved) = mirror_config::Instance::inspect(std::path::Path::new(&config))?;
    if let (Some(chain), Some(sim)) = (cfg.chains.venue.as_ref(), cfg.services.sim.as_ref()) {
        let state_dir = sim.state_dir.to_string_lossy().to_string();
        mirror_config::require_resolved(
            "venue simulator wiring",
            &[
                &chain.source_url,
                &state_dir,
                sim.engine_admin_url.as_deref().unwrap_or_default(),
                sim.anvil_url.as_deref().unwrap_or_default(),
            ],
        )
        .inspect_err(|e| {
            tracing::error!(
                unresolved = %unresolved.join(", "),
                error = %e,
                "venue simulator wiring is incomplete"
            );
        })?;
    }
    let mut cfg = cfg;
    let (chain, sim_section) = cfg.take_sim()?;

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

    // The market plane: REST snapshots + WS pushes (both write the same cache).
    let poller = mirror_hyperliquid::feed::rest::Poller::new(
        feed.clone(),
        api_client,
        Duration::from_millis(sim_section.poll_interval_ms),
    );
    let ws = mirror_hyperliquid::feed::ws::WsClient::new(feed.clone(), chain.ws_url.clone());
    tokio::spawn(poller.run());
    tokio::spawn(ws.run());

    // The Core↔EVM bridge (when V is configured): system-address credits and
    // CoreWriter actions become sim state.
    if let Some(v_url) = &sim_section.anvil_url {
        let v = mirror_anvil::Rpc::new(v_url, Duration::from_secs(30))?;
        let bridge = mirror_hyperliquid::bridge::Bridge::new(v, sim.clone(), feed.clone(), bridge_metrics.clone());
        tokio::spawn(bridge.run());
    } else {
        tracing::warn!("services.sim.anvil_url is not set: the Core<->EVM bridge is disabled");
    }

    // The account plane's self-advance tick: funding + liquidations + persist.
    let tick_feed = feed.clone();
    let tick_sim = sim.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if let Err(e) = tick_sim.tick(&tick_feed) {
                tracing::warn!(error = %e, "sim tick failed; will retry");
            }
        }
    });

    // Two listeners: the info API daloc reads, the control API the harness drives.
    let info_addr = format!("{}:{}", sim_section.info.addr, sim_section.info.port);
    let control_addr = format!("{}:{}", sim_section.control.addr, sim_section.control.port);
    let info = tokio::spawn(mirror_hyperliquid::api::serve(
        info_addr.clone(),
        mirror_hyperliquid::api::info_router(state.clone()),
    ));
    let control = tokio::spawn(mirror_hyperliquid::api::serve(
        control_addr.clone(),
        mirror_hyperliquid::api::control_router(state.clone()),
    ));
    tracing::info!(
        info = %info_addr,
        control = %control_addr,
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


