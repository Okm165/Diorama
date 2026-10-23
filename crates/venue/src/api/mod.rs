//! The simulator's HTTP surface: the daloc-facing info API (8090) and the
//! control API (8091).
//!
//! Two routers, one state ([`ApiState`]): the info router answers the venue's
//! info types (simulated for touched accounts, passed through to the real
//! venue for untouched ones), the control router drives scenarios. The error
//! mapping keeps the taxonomy's meaning on the wire: control refusals are
//! 4xx, a warming market plane is 503, a venue fault is 502.
pub mod control;
pub mod info;
pub mod ws;

use crate::error::{Error, Result};
use crate::feed::rest::RestClient;
use crate::feed::MarketFeed;
use crate::sim::Sim;
use axum::Router;
use std::sync::Arc;

/// Everything both routers read. Built once by the bin.
pub struct ApiState {
    pub feed: Arc<MarketFeed>,
    pub sim: Arc<Sim>,
    /// The venue's public API (passthrough reads).
    pub api: RestClient,
    /// The venue engine's admin base (`/admin/epoch`), for `epoch_new`.
    pub engine_admin: Option<String>,
    /// Plain client for the engine admin call.
    pub http: reqwest::Client,
    /// The bridge's counters (rendered with the sim's metrics), when the
    /// bridge is configured.
    pub bridge: Option<Arc<crate::bridge::metrics::BridgeMetrics>>,
    pub started_ms: u64,
}

impl ApiState {
    pub fn new(
        feed: Arc<MarketFeed>,
        sim: Arc<Sim>,
        api: RestClient,
        engine_admin: Option<String>,
        bridge: Option<Arc<crate::bridge::metrics::BridgeMetrics>>,
    ) -> Result<Self> {
        Ok(Self {
            feed,
            sim,
            api,
            engine_admin,
            // The engine's re-pin forks from a public RPC: the reset itself
            // can take a while, so the admin call gets a wider window than a
            // plain read would.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(45))
                .build()?,
            bridge,
            started_ms: crate::now_ms(),
        })
    }

    /// Readiness: the market plane has a meta and every configured market's
    /// context is fresher than `max_stale_ms` (the plan's freshness gate).
    pub fn ready(&self, max_stale_ms: u64) -> bool {
        if self.feed.meta().is_none() {
            return false;
        }
        let now = crate::now_ms();
        self.feed.markets.iter().all(|coin| {
            self.feed
                .stale_ms(coin, now)
                .map(|stale| stale <= max_stale_ms)
                .unwrap_or(false)
        })
    }
}

/// The info router (8090): the venue's `POST /info`, liveness, readiness.
pub fn info_router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(info::healthz))
        .route("/readyz", axum::routing::get(info::readyz))
        .route("/info", axum::routing::post(info::info))
        // The matching lane's named refusal (plan §18.4): their client posts
        // actions here; the envelope is HL's own err shape, never a 404.
        .route("/exchange", axum::routing::post(info::exchange))
        // The venue's websocket lane (plan §7.5: "info + WS" on this port).
        .route("/ws", axum::routing::get(ws::ws))
        .with_state(state)
}

/// The control router (8091): scenario mutations, the state read, metrics.
pub fn control_router(state: Arc<ApiState>) -> Router {
    use axum::routing::{get, post};
    Router::new()
        .route("/health", get(control::health))
        .route("/metrics", get(control::metrics))
        .route("/control/deal", post(control::deal))
        .route("/control/order", post(control::order))
        .route("/control/advance", post(control::advance))
        .route("/control/crash", post(control::crash))
        .route("/control/stall", post(control::stall))
        .route("/control/set_book", post(control::set_book))
        .route("/control/set_funding", post(control::set_funding))
        .route("/control/set_account", post(control::set_account))
        .route(
            "/control/state/{address}",
            get(control::get_state).post(control::post_state),
        )
        .route("/control/reset", post(control::reset))
        .route("/control/epoch_new", post(control::epoch_new))
        .with_state(state)
}

/// Serve one router on one listener (the bin's two ports).
pub async fn serve(addr: String, router: Router) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| Error::Bind {
            addr: addr.clone(),
            source,
        })?;
    axum::serve(listener, router)
        .await
        .map_err(|source| Error::Io {
            what: "http api",
            path: addr.into(),
            source,
        })
}

// The HTTP mapping of the error taxonomy lives in the engine's `http` module
// (one impl for both surfaces: the engine's admin routes and the simulator's
// info/control routers).
