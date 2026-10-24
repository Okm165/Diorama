//! Engine HTTP API: health, status, Prometheus metrics, and the admin surface
//! (rewind / resume / pause / time / shadow / deal / epoch / probes) — the
//! fork controls that belong to the mirror itself rather than to Anvil.
//!
//! The routes are wired here (axum binds [`Ctx`]); the admin *semantics* live
//! in `mirror-devnet`'s admin surface, which both projects share.
//!
//! This module owns the HTTP mapping of the error taxonomy: request refusals
//! (the devnet's `NoCursor` / `InvalidRewind`) are 4xx; a poisoned registry
//! is 503 (the mirror cannot serve correct answers until it is restarted);
//! everything else is a server-side fault, logged with its typed cause.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::feed_bridge::FeedBridge;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use mirror_devnet::admin::TimeRequest;
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// `/status` — the engine's diagnostic report. A typed shape: the scripts and
/// dashboards read it by field name, so a rename must be a compile error here,
/// not a silent `null`.
#[derive(Debug, Serialize)]
struct StatusResponse {
    reth_head: u64,
    follow_head: u64,
    follow: &'static str,
    head: u64,
    /// The devnet chain's cursor (T-height; the shared field's meaning on both
    /// engines; the plan's clock-labels rule).
    cursor: Option<u64>,
    /// T's cursor by its profile name (the scripts' established spelling).
    cursor_t: Option<u64>,
    lag: u64,
    halted: bool,
    watchlist: usize,
    shadowed_slots: usize,
    shadowed_accounts: usize,
    /// The shadowed account list (not just counts): the devnet's own writes
    /// are copy-on-write, so these accounts are expected to differ from
    /// mainnet — parity checks (scenario probes) must skip them instead of
    /// alarming.
    shadowed: Vec<String>,
    time_offset_seconds: u64,
    /// What the source declares it can serve (the follow source's audited
    /// capability matrix: native BALs, traces, historical state, system txs).
    capabilities: mirror_devnet::source::Capabilities,
}

/// `/admin/time` — the devnet clock offset is set (and persisted).
#[derive(Debug, Serialize)]
struct TimeResponse {
    offset_seconds: u64,
    t_head: u64,
    note: &'static str,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            // The shared devnet taxonomy (mirror-devnet's typed predicates):
            // both engines classify the same refusals identically.
            Error::Devnet(e) if e.is_request_refusal() => StatusCode::BAD_REQUEST,
            Error::Devnet(e) if e.is_unavailable() => StatusCode::SERVICE_UNAVAILABLE,
            // The upstream anvil boundary: a transport fault is a bad
            // gateway, not this service's fault.
            Error::Rpc(_) => StatusCode::BAD_GATEWAY,
            // The feed bridge's named refusals (the venue maps its own
            // NotConfigured / NotFound the same way).
            Error::FeedBridgeUnconfigured => StatusCode::CONFLICT,
            Error::FeedBridgeUnknown { .. } => StatusCode::NOT_FOUND,
            Error::FeedBridgeWaiting { .. } => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "admin request failed");
        }
        (status, self.to_string()).into_response()
    }
}

/// The feed bridge's routes need both the context (override publishes a tx)
/// and the bridge itself; a separate state keeps the main router untouched.
#[derive(Clone)]
struct FeedState {
    ctx: Arc<Ctx>,
    bridge: Option<Arc<FeedBridge>>,
}

/// The engine's control surface: the exact routes this process serves (the
/// table the control-plane docs list; the mirror's twin keeps the same shape).
fn app(ctx: Arc<Ctx>, bridge: Option<Arc<FeedBridge>>) -> Router {
    let feed_routes = Router::new()
        .route("/admin/feed", get(feed_list))
        .route("/admin/feed/pause", post(feed_pause))
        .route("/admin/feed/resume", post(feed_resume))
        .route("/admin/feed/override", post(feed_override))
        .with_state(FeedState {
            ctx: ctx.clone(),
            bridge,
        });
    Router::new()
        .route("/healthz", get(healthz))
        .route("/livez", get(livez))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/events", get(admin_events))
        .route("/admin/rewind", post(admin_rewind))
        .route("/admin/resume", post(admin_resume))
        .route("/admin/pause", post(admin_pause))
        .route("/admin/time", post(admin_time))
        .route("/admin/shadow", post(admin_shadow))
        .route("/admin/deal", post(admin_deal))
        .route("/admin/epoch", post(admin_epoch))
        .route("/admin/probes", post(admin_probes))
        .with_state(ctx)
        .merge(feed_routes)
}

/// Bind the configured address and serve the control surface until the
/// process is torn down.
pub async fn serve(ctx: Arc<Ctx>, bridge: Option<Arc<FeedBridge>>) -> Result<()> {
    let addr = format!("{}:{}", ctx.engine.http.addr, ctx.engine.http.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| Error::Bind {
            addr: addr.clone(),
            source,
        })?;
    tracing::info!(addr, "engine http api listening");
    axum::serve(listener, app(ctx, bridge))
        .await
        .map_err(|source| Error::Io {
            what: "http api",
            path: addr.into(),
            source,
        })?;
    Ok(())
}

// The health and admin handlers both mirrors must keep byte-identical: one
// macro, two invocations (crates/devnet/src/handlers.rs).
mirror_devnet::mirror_handlers!(Ctx, crate::error::Error);

async fn status(State(ctx): State<Arc<Ctx>>) -> Result<Json<StatusResponse>> {
    // Head gauges: an endpoint that cannot answer degrades the report to 0,
    // never fails it - /status is a diagnostic, not a fact source.
    let reth_head = ctx.reth.block_number().await.unwrap_or(0);
    let follow = ctx.follow_head().await.ok().flatten().unwrap_or(0);
    let head = ctx.anvil_t.block_number().await.unwrap_or(0);
    let ct = ctx.plane.state.cursor()?;
    let shadow = ctx.plane.shadow.lock()?;
    Ok(Json(StatusResponse {
        reth_head,
        follow_head: follow,
        follow: ctx.follow_tag(),
        head,
        cursor: ct.as_ref().map(|c| c.block),
        cursor_t: ct.as_ref().map(|c| c.block),
        lag: follow.saturating_sub(ct.as_ref().map(|c| c.block).unwrap_or(follow)),
        halted: ctx.plane.is_halted(),
        watchlist: ctx.mirror.scope().watch().accounts.len(),
        shadowed_slots: shadow.slot_count(),
        shadowed_accounts: shadow.accounts.len(),
        shadowed: shadow.shadowed_addresses(),
        time_offset_seconds: ctx.plane.time_offset.load(Ordering::Relaxed),
        capabilities: ctx.mirror.follow_source().capabilities(),
    }))
}

async fn metrics(State(ctx): State<Arc<Ctx>>) -> String {
    // The same composition as the venue engine: this profile's own set, then
    // the shared admin gauges (one definition — `Metrics::render`), then the
    // feed bridge's counters.
    let mut out = ctx.eth_metrics.render();
    out.push_str(&ctx.plane.metrics.render());
    out.push_str(&ctx.feed_bridge_metrics.render());
    out
}

async fn admin_time(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<TimeRequest>,
) -> Result<Json<TimeResponse>> {
    let out = ctx.admin().time(req.offset_seconds).await?;
    Ok(Json(TimeResponse {
        offset_seconds: out.offset_seconds,
        t_head: out.head,
        note: "T's timestamps are monotonic; lowering the offset lets the clock converge, it cannot jump backwards",
    }))
}

/// `/admin/feed` — the bridged feeds and their state.
async fn feed_list(State(state): State<FeedState>) -> Result<Json<Vec<crate::feed_bridge::FeedStatus>>> {
    let bridge = state.bridge.as_ref().ok_or(Error::FeedBridgeUnconfigured)?;
    Ok(Json(bridge.list()))
}

#[derive(Debug, serde::Deserialize)]
struct FeedNameRequest {
    name: String,
}

#[derive(Debug, Serialize)]
struct FeedAck {
    name: String,
    paused: bool,
}

/// `/admin/feed/pause` — the scenario takes the value (the bridge stops
/// writing this feed; divergence detection does the same automatically).
async fn feed_pause(
    State(state): State<FeedState>,
    Json(req): Json<FeedNameRequest>,
) -> Result<Json<FeedAck>> {
    let bridge = state.bridge.as_ref().ok_or(Error::FeedBridgeUnconfigured)?;
    bridge.pause(&req.name)?;
    Ok(Json(FeedAck {
        name: req.name,
        paused: true,
    }))
}

/// `/admin/feed/resume` — hand the feed back: the bridge forgets what it last
/// wrote, so the next pass re-establishes the parent chain's value.
async fn feed_resume(
    State(state): State<FeedState>,
    Json(req): Json<FeedNameRequest>,
) -> Result<Json<FeedAck>> {
    let bridge = state.bridge.as_ref().ok_or(Error::FeedBridgeUnconfigured)?;
    bridge.resume(&req.name)?;
    Ok(Json(FeedAck {
        name: req.name,
        paused: false,
    }))
}

#[derive(Debug, serde::Deserialize)]
struct FeedOverrideRequest {
    name: String,
    /// The raw aggregator answer (the feed's own decimals).
    answer: i128,
}

/// `/admin/feed/override` — publish a scripted value now and pause the feed
/// (the same net effect as a scenario's crash write, driven explicitly).
async fn feed_override(
    State(state): State<FeedState>,
    Json(req): Json<FeedOverrideRequest>,
) -> Result<Json<FeedAck>> {
    let bridge = state.bridge.as_ref().ok_or(Error::FeedBridgeUnconfigured)?;
    bridge
        .override_answer(&state.ctx.anvil_t, &req.name, req.answer)
        .await?;
    Ok(Json(FeedAck {
        name: req.name,
        paused: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared taxonomy, pinned per service (the same test name lives in
    /// the venue engine and the CCTP bridge): a request refusal is 400, a
    /// transport fault at a chain boundary is 502, an unavailable registry is
    /// 503, a named absence/conflict keeps its 4xx, everything else is 500.
    /// The live symmetry suite exercises the classes it can reach without
    /// breaking a chain; this pins the rest.
    #[test]
    fn the_http_status_mapping_follows_the_shared_convention() {
        let status = |e: Error| e.into_response().status();
        let anvil = || mirror_anvil::Error::Status {
            endpoint: "anvil-t".into(),
            method: "eth_call".into(),
            status: 503,
            body: String::new(),
        };
        assert_eq!(
            status(Error::Devnet(mirror_devnet::Error::NoCursor)),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(Error::Devnet(mirror_devnet::Error::Poisoned {
                what: "shadow registry"
            })),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(status(Error::Rpc(anvil())), StatusCode::BAD_GATEWAY);
        assert_eq!(status(Error::FeedBridgeUnconfigured), StatusCode::CONFLICT);
        assert_eq!(
            status(Error::FeedBridgeUnknown {
                feed: "ETH/USD".into()
            }),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(Error::FeedBridgeWaiting {
                feed: "ETH/USD".into()
            }),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(Error::WrongChain {
                anvil: "anvil-t",
                got: 5,
                want: 1
            }),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
