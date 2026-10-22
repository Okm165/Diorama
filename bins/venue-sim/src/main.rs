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
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

/// Worker stack budget: the same measured convention as the other binaries.
const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_SIZE)
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            report(&e);
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        match run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                report(&e);
                ExitCode::FAILURE
            }
        }
    })
}

async fn run() -> mirror_hyperliquid::Result<()> {
    init_tracing();
    let config = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/etc/mirror/instance.toml".to_string());
    let mut cfg = mirror_config::Instance::load(std::path::Path::new(&config))?;
    let (chain, sim_section) = cfg.take_sim()?;

    let feed = Arc::new(mirror_hyperliquid::feed::MarketFeed::load(
        sim_section.state_dir.join("feed.json"),
        sim_section.markets.clone(),
    )?);
    let sim = Arc::new(mirror_hyperliquid::sim::Sim::load(
        sim_section.state_dir.join("sim.json"),
    )?);

    let api_client = mirror_hyperliquid::feed::rest::RestClient::new(&chain.api_url)?;
    let state = Arc::new(mirror_hyperliquid::api::ApiState::new(
        feed.clone(),
        sim.clone(),
        api_client.clone(),
        sim_section.engine_admin_url.clone(),
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

    tokio::signal::ctrl_c().await.map_err(|source| {
        mirror_hyperliquid::Error::Io {
            what: "signal handler",
            path: "ctrl_c".into(),
            source,
        }
    })?;
    info.abort();
    control.abort();
    sim.save()?;
    Ok(())
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

/// Print the error and its full cause chain: the typed sources are the point.
fn report(e: &dyn std::error::Error) {
    eprintln!("Error: {e}");
    let mut source = e.source();
    while let Some(s) = source {
        eprintln!("  caused by: {s}");
        source = s.source();
    }
}
