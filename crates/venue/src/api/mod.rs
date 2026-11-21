//! The simulator's HTTP surface: the consumer-facing info API (8090) and the
//! control API (8091).
//!
//! Two routers, one state ([`ApiState`]): the info router answers the venue's
//! info types (simulated for touched accounts, passed through to the real
//! venue for untouched ones), the control router drives scenarios. The error
//! mapping keeps the taxonomy's meaning on the wire: control refusals are
//! 4xx, a warming market plane is 503, a venue fault is 502.
pub(crate) mod candles;
pub(crate) mod control;
pub(crate) mod cors;
pub(crate) mod exchange;
pub(crate) mod info;
#[cfg(test)]
mod liquidation_parity;
#[cfg(test)]
mod parity;
pub(crate) mod shape;
pub(crate) mod signing;
mod user_actions;
pub(crate) mod ws;
pub(crate) mod ws_codec;

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
    /// The market websocket lane's shared pool, when the service runs one.
    /// Kept optional so the API can be built without a live plane in tests;
    /// `/metrics` renders it when present.
    pub ws_pool: Option<diorama_anvil::EndpointPool>,
    /// The venue websocket's push cadence (the account automaton's configured
    /// tick, not a second hardcoded second).
    pub push_interval: std::time::Duration,
    /// The relayed candle streams (the venue serves candles; the sim has no
    /// trade tape to build them from).
    pub candles: Arc<candles::CandleHub>,
}

impl ApiState {
    #[expect(
        clippy::too_many_arguments,
        reason = "the state's inputs are the deployment's own boundaries; grouping them would \
                  hide which caller supplied each"
    )]
    pub fn new(
        feed: Arc<MarketFeed>,
        sim: Arc<Sim>,
        api: RestClient,
        engine_admin: Option<String>,
        bridge: Option<Arc<crate::bridge::metrics::BridgeMetrics>>,
        admin_timeout: std::time::Duration,
        push_interval: std::time::Duration,
    ) -> Result<Self> {
        Ok(Self {
            feed,
            sim,
            api,
            engine_admin,
            // The engine's re-pin forks from a public RPC: the deployment
            // sets this call's own budget.
            http: reqwest::Client::builder().timeout(admin_timeout).build()?,
            bridge,
            ws_pool: None,
            push_interval,
            candles: Arc::new(candles::CandleHub::default()),
        })
    }

    /// Attach the market websocket lane's pool so `/metrics` can expose its
    /// per-endpoint health. One pool per lane, not a second policy; the
    /// candle relays use the same venue endpoints.
    #[must_use]
    pub fn with_ws_pool(mut self, pool: diorama_anvil::EndpointPool) -> Self {
        self.candles.set_pool(pool.clone());
        self.ws_pool = Some(pool);
        self
    }

    /// Readiness: the market plane has a meta and every configured market's
    /// context is fresher than `max_stale_ms` (the plan's freshness gate).
    pub fn ready(&self, max_stale_ms: u64) -> bool {
        if self.feed.meta().is_none() {
            return false;
        }
        // A configured HIP-3 market (`xyz:*`) is addressable only through the
        // upstream `perpDexs` list: the asset index encodes that list's
        // position. Before the list lands — or if the venue does not quote a
        // configured dex — the order lane cannot reach the market, so
        // readiness would be a lie.
        for market in &self.feed.markets {
            let Some((dex, _)) = market.split_once(':') else {
                continue;
            };
            if self.feed.dex_index(dex).is_none() {
                return false;
            }
        }
        // The spot token registry is part of the served contract (spot deals,
        // `spotClearinghouseState`'s token mapping). The poller fetches it in
        // the same pass as the meta, one HEAVY call later — readiness that
        // passed between the two left the first spot deal hitting the gap
        // (found by the local run).
        if !self.feed.spot_registry_loaded() {
            return false;
        }
        let now = crate::now_ms();
        // **A cached book that froze is a fault; a book nobody asked for is
        // not.** The book cache is demand-driven: after the relay's grace
        // window an unread book is evicted and the next ask (an order, an
        // `l2Book` read, a position) rebuilds it from the live socket or REST.
        // Requiring every configured market's cached book to be fresh made
        // readiness flap with a perfectly healthy plane (measured
        // 2026-11-21: `ws_connected 1`, `rest_errors 0`, ctx 150 ms, and
        // still `market plane warming up` every time a quiet market's book
        // aged out — the upstream services then block writes on it). Readiness
        // now fails a book that exists and stopped updating; a missing book is
        // acceptable while the live plane is healthy, and the order lane still
        // refuses a stale book it actually reads.
        let plane_healthy = !self.feed.stalled() && self.feed.metrics.ws_up();
        let book_ready = |coin: &str| match self.feed.book_age(coin, now, max_stale_ms) {
            crate::feed::BookAge::Scripted | crate::feed::BookAge::Fresh => true,
            crate::feed::BookAge::Warming => plane_healthy,
            crate::feed::BookAge::Stale(_) => false,
        };
        let perps_ready = self.feed.markets.iter().all(|coin| {
            let fresh_mark = self
                .feed
                .stale_ms(coin, now)
                .map(|stale| stale <= max_stale_ms)
                .unwrap_or(false);
            fresh_mark && book_ready(coin)
        });
        // A mirrored spot pair has a book and an `allMids` entry, but no
        // `activeAssetCtx` (there is no spot funding). Requiring a ctx would
        // keep readiness false forever.
        let spot_ready = self
            .feed
            .spot_markets
            .iter()
            .all(|coin| self.feed.mark(coin).is_some() && book_ready(coin));
        perps_ready && spot_ready
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
// The market channels ride this step's tick + dedup (the fork's relay is a
// separate lane); the candle hub is attached and its `candle` channel
// subscribes it.
pub fn info_router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route(
            diorama_devnet::handlers::HEALTHZ,
            axum::routing::get(info::healthz),
        )
        // The info port's liveness alias. The shared inter-service health probe
        // (`the HTTP client::health_verdict`) reads a JSON `status`; the control
        // port's text body is for humans and orchestrators.
        .route("/health", axum::routing::get(info::health))
        .route("/readyz", axum::routing::get(info::readyz))
        .route("/info", axum::routing::post(info::info))
        // The matching lane's named refusal: their client posts
        // actions here; the envelope is HL's own err shape, never a 404.
        .route("/exchange", axum::routing::post(exchange::exchange))
        // The venue's websocket lane ("info + WS" on this port).
        .route("/ws", axum::routing::get(ws::ws))
        // Browsers call this port straight from the page (a JSON POST, so a
        // preflight first): the info surface answers any origin, like the
        // venue it mirrors. The control router stays closed to browsers.
        .layer(axum::middleware::from_fn(cors::cors))
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
            "/control/clear_account/{address}",
            post(control::clear_account),
        )
        .route(
            "/control/clear_fixture/{address}",
            post(control::clear_fixture),
        )
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

#[cfg(test)]
mod readiness_tests {
    //! Readiness is a liveness signal, not a completeness claim: the book
    //! cache is demand-driven, so an idle (evicted) book is not a fault while
    //! the live plane is healthy — but a cached book that froze is.

    use super::*;
    use crate::feed::{MarketFeed, rest::RestClient};
    use rust_decimal::Decimal;
    use std::time::Duration;

    fn feed_with(coin: &str) -> MarketFeed {
        let feed = MarketFeed::memory(vec![coin.into()]);
        feed.put_meta(crate::sim::meta_with(coin, 10), 1);
        feed.put_ctx(coin.into(), crate::wire::funding_ctx("101", "101"), crate::now_ms());
        feed.put_mids(
            std::collections::BTreeMap::from([(coin.to_string(), Decimal::from(101))]),
            crate::now_ms(),
        );
        feed
    }

    fn state(feed: MarketFeed) -> Arc<ApiState> {
        Arc::new(
            ApiState::new(
                Arc::new(feed),
                Arc::new(crate::sim::Sim::memory()),
                RestClient::new(&["http://127.0.0.1:1".to_string()]).unwrap(),
                None,
                None,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .unwrap(),
        )
    }

    #[test]
    fn an_idle_book_is_ready_while_the_plane_is_healthy() {
        let feed = feed_with("ETH");
        feed.put_spot_meta(crate::wire::SpotMeta { tokens: vec![], universe: vec![] });
        feed.metrics.mark_ws_up();
        // No book at all (evicted / never asked): the order lane rebuilds it.
        assert!(state(feed).ready(MarketFeed::MAX_STALE_MS), "an idle cache is not a fault");
    }

    #[test]
    fn a_frozen_cached_book_is_not_ready() {
        let feed = feed_with("ETH");
        feed.put_spot_meta(crate::wire::SpotMeta { tokens: vec![], universe: vec![] });
        feed.metrics.mark_ws_up();
        // A book stamped 10 minutes ago and never confirmed since: cached but
        // frozen — exactly the failure the stamp exists to catch.
        feed.put_book(
            crate::wire::L2Book {
                coin: "ETH".into(),
                time: 1,
                spread: None,
                levels: [vec![], vec![]],
            },
            crate::now_ms() - 600_000,
        );
        assert!(!state(feed).ready(MarketFeed::MAX_STALE_MS), "a frozen book refuses readiness");
    }

    #[test]
    fn a_missing_book_is_not_ready_with_the_plane_down() {
        let feed = feed_with("ETH");
        feed.put_spot_meta(crate::wire::SpotMeta { tokens: vec![], universe: vec![] });
        // The socket never came up: an idle cache cannot be rebuilt on demand.
        assert!(!state(feed).ready(MarketFeed::MAX_STALE_MS), "a dead plane is not ready");
    }
}
