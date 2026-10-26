//! The simulator's HTTP surface: the daloc-facing info API (8090) and the
//! control API (8091).
//!
//! Two routers, one state ([`ApiState`]): the info router answers the venue's
//! info types (simulated for touched accounts, passed through to the real
//! venue for untouched ones), the control router drives scenarios. The error
//! mapping keeps the taxonomy's meaning on the wire: control refusals are
//! 4xx, a warming market plane is 503, a venue fault is 502.
pub(crate) mod control;
pub(crate) mod exchange;
pub(crate) mod info;
pub(crate) mod ws;

use crate::error::Result;
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
        })
    }

    /// Readiness: the market plane has a meta and every configured market's
    /// context is fresher than `max_stale_ms` (the plan's freshness gate).
    pub fn ready(&self, max_stale_ms: u64) -> bool {
        if self.feed.meta().is_none() {
            return false;
        }
        // The spot token registry is part of the served contract (spot deals,
        // `spotClearinghouseState`'s token mapping). The poller fetches it in
        // the same pass as the meta, one HEAVY call later — readiness that
        // passed between the two left the first spot deal hitting the gap
        // (found by the local daloc run).
        if !self.feed.spot_registry_loaded() {
            return false;
        }
        let now = crate::now_ms();
        self.feed.markets.iter().all(|coin| {
            let fresh_mark = self
                .feed
                .stale_ms(coin, now)
                .map(|stale| stale <= max_stale_ms)
                .unwrap_or(false);
            // The served book is part of the same contract (daloc's l2Book
            // read): the poller fetches it a call after the meta, so
            // readiness that passed between the two left the book read
            // hitting the gap (the same local-run finding, one call later).
            fresh_mark && self.feed.book(coin, now).is_some()
        })
    }
}

/// The account plane's poison answer (503): one spelling for the three
/// health routes that raise it.
pub(crate) const SIM_POISONED: &str = "sim state poisoned";

/// The sim-poisoned gate's refusal: the 503 every health surface returns
/// while the account plane is poisoned (the reason is the shared constant).
pub(crate) struct SimPoisoned;

impl axum::response::IntoResponse for SimPoisoned {
    fn into_response(self) -> axum::response::Response {
        (axum::http::StatusCode::SERVICE_UNAVAILABLE, SIM_POISONED).into_response()
    }
}

/// The sim-poisoned gate every health surface shares: a poisoned account
/// plane refuses every read, so a 200 would hide an outage from the
/// orchestrator (the verifier's readiness-gap find). `Err` = the refusal.
pub(crate) fn sim_gate(state: &ApiState) -> Result<(), SimPoisoned> {
    if state.sim.is_poisoned() {
        Err(SimPoisoned)
    } else {
        Ok(())
    }
}

/// The info router (8090): the venue's `POST /info`, liveness, readiness.
pub fn info_router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route(diorama_devnet::handlers::HEALTHZ, axum::routing::get(info::healthz))
        .route("/readyz", axum::routing::get(info::readyz))
        .route("/info", axum::routing::post(info::info))
        // The matching lane's named refusal: their client posts
        // actions here; the envelope is HL's own err shape, never a 404.
        .route("/exchange", axum::routing::post(exchange::exchange))
        // The venue's websocket lane ("info + WS" on this port).
        .route("/ws", axum::routing::get(ws::ws))
        .with_state(state)
}

/// The control router (8091): scenario mutations, the state read, metrics.
pub fn control_router(state: Arc<ApiState>) -> Router {
    use axum::routing::{get, post};
    Router::new()
        .route("/health", get(control::health))
        .route(diorama_devnet::handlers::METRICS, get(control::metrics))
        .route("/control/deal", post(control::deal))
        .route("/control/order", post(control::order))
        .route("/control/advance", post(control::advance))
        .route("/control/crash", post(control::crash))
        .route("/control/rally", post(control::rally))
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

/// Serve one router on one listener (the bin's two ports): the spine's
/// shared boundary — one bind fault, one listening line, one serve fault.
pub async fn serve(addr: String, router: Router) -> Result<()> {
    diorama_devnet::engine::serve_router("venue sim api", addr, router).await
}

// The HTTP mapping of the error taxonomy lives in the engine's `http` module
// (one impl for both surfaces: the engine's admin routes and the simulator's
// info/control routers).
