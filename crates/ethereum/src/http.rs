//! Engine HTTP API: health, status, Prometheus metrics, and the admin surface
//! (rewind / resume / pause / time / shadow / deal) — the fork controls that
//! belong to the mirror itself rather than to Anvil.
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
use mirror_devnet::admin::{
    EpochResponse, PauseResponse, ResumeResponse, RewindRequest, RewindResponse,
    ShadowRequest, TimeRequest,
};
use mirror_devnet::deal::DealRequest;
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
    m_head: u64,
    t_head: u64,
    cursor: Option<u64>,
    cursor_t: Option<u64>,
    lag: u64,
    frozen: bool,
    halted: bool,
    watchlist: usize,
    shadowed_slots: usize,
    shadowed_accounts: usize,
    /// The shadowed account list (not just counts): the devnet's own writes
    /// are copy-on-write, so these accounts are expected to differ from
    /// mainnet — parity checks (scenario probes) must skip them instead of
    /// alarming.
    shadowed: Vec<String>,
    blob_form: Option<&'static str>,
    time_offset_seconds: u64,
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

pub async fn serve(ctx: Arc<Ctx>, bridge: Option<Arc<FeedBridge>>) -> Result<()> {
    let addr = format!("{}:{}", ctx.engine.http.addr, ctx.engine.http.port);
    let feed_routes = Router::new()
        .route("/admin/feed", get(feed_list))
        .route("/admin/feed/pause", post(feed_pause))
        .route("/admin/feed/resume", post(feed_resume))
        .route("/admin/feed/override", post(feed_override))
        .with_state(FeedState {
            ctx: ctx.clone(),
            bridge,
        });
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/livez", get(livez))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/admin/rewind", post(admin_rewind))
        .route("/admin/resume", post(admin_resume))
        .route("/admin/pause", post(admin_pause))
        .route("/admin/time", post(admin_time))
        .route("/admin/shadow", post(admin_shadow))
        .route("/admin/deal", post(admin_deal))
        .route("/admin/epoch", post(admin_epoch))
        .route("/admin/probes", post(admin_probes))
        .with_state(ctx)
        .merge(feed_routes);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| Error::Bind {
            addr: addr.clone(),
            source,
        })?;
    tracing::info!(addr, "engine http api listening");
    axum::serve(listener, app).await.map_err(|source| Error::Io {
        what: "http api",
        path: addr.into(),
        source,
    })?;
    Ok(())
}

async fn healthz(State(ctx): State<Arc<Ctx>>) -> impl IntoResponse {
    if ctx.is_halted() {
        (StatusCode::SERVICE_UNAVAILABLE, "halted")
    } else {
        (StatusCode::OK, "ok")
    }
}

/// `/livez` — the orchestrator's liveness signal: the process and its HTTP
/// runtime are answering. Deliberately *not* `/healthz`: that reports the
/// engine's own state (503 while paused), and probing it would take the engine
/// out of the Service (its NodePort included) and eventually restart the pod
/// in the middle of the documented pause drill — while the contract says a
/// paused engine keeps serving reads and writes.
async fn livez() -> &'static str {
    "ok"
}

async fn status(State(ctx): State<Arc<Ctx>>) -> Result<Json<StatusResponse>> {
    // Head gauges: an endpoint that cannot answer degrades the report to 0,
    // never fails it - /status is a diagnostic, not a fact source.
    let reth_head = ctx.reth.block_number().await.unwrap_or(0);
    let follow = ctx.follow_head().await.ok().flatten().unwrap_or(0);
    let m_head = ctx.anvil_m.block_number().await.unwrap_or(0);
    let t_head = ctx.anvil_t.block_number().await.unwrap_or(0);
    let cm = ctx.cursor_m()?;
    let ct = ctx.cursor_t()?;
    let shadow = ctx.shadow.lock()?;
    Ok(Json(StatusResponse {
        reth_head,
        follow_head: follow,
        follow: ctx.follow_tag(),
        m_head,
        t_head,
        cursor: cm.as_ref().map(|c| c.block),
        cursor_t: ct.as_ref().map(|c| c.block),
        lag: follow.saturating_sub(cm.as_ref().map(|c| c.block).unwrap_or(follow)),
        frozen: ctx.frozen.load(Ordering::Relaxed),
        halted: ctx.is_halted(),
        watchlist: ctx.watchlist.len(),
        shadowed_slots: shadow.slot_count(),
        shadowed_accounts: shadow.accounts.len(),
        shadowed: shadow.shadowed_addresses(),
        blob_form: ctx.blob_form()?.map(crate::blob::BlobForm::as_str),
        time_offset_seconds: ctx.time_offset.load(Ordering::Relaxed),
    }))
}

async fn metrics(State(ctx): State<Arc<Ctx>>) -> String {
    let mut out = ctx.metrics.render();
    out.push_str(&ctx.feed_bridge_metrics.render());
    out
}

async fn admin_rewind(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<RewindRequest>,
) -> Result<Json<RewindResponse>> {
    Ok(Json(ctx.admin().rewind_response(req.blocks).await?))
}

async fn admin_resume(State(ctx): State<Arc<Ctx>>) -> Json<ResumeResponse> {
    ctx.admin().resume();
    Json(ResumeResponse { resumed: true })
}

async fn admin_pause(State(ctx): State<Arc<Ctx>>) -> Json<PauseResponse> {
    ctx.admin().pause();
    Json(PauseResponse { paused: true })
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

async fn admin_shadow(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<ShadowRequest>,
) -> Result<Json<mirror_devnet::admin::ShadowOutcome>> {
    // The hand-back carries its own race guard (AdminSurface::shadow_hand_back).
    Ok(Json(ctx.admin().shadow_hand_back(&req).await?))
}

async fn admin_deal(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<DealRequest>,
) -> Result<Json<mirror_devnet::deal::DealOutcome>> {
    Ok(Json(ctx.admin().deal(&req).await?))
}

/// `/admin/epoch` — one-call revert-to-blank (plan §7.1): re-pin T to the
/// source's latest block. The engine's cursor check detects the reset on its
/// next pass and runs the epoch rule (new base = clear marks and values;
/// same base = re-apply the recorded writes), so this route never replicates
/// that logic.
async fn admin_epoch(State(ctx): State<Arc<Ctx>>) -> Result<Json<EpochResponse>> {
    Ok(Json(
        mirror_devnet::admin::AdminSurface::epoch_repin(&ctx.anvil_t, &ctx.l1.source_url).await?,
    ))
}

/// `/admin/probes`: run the configured probes now (M at the cursor vs reth)
/// and report the run. The venue's route is the same verb over its at-pin
/// probe set; the per-block convergence counters stay the loop's.
async fn admin_probes(
    State(ctx): State<Arc<Ctx>>,
) -> Result<Json<mirror_devnet::probes::ProbeOutcome>> {
    Ok(Json(crate::driver::probes_now(&ctx).await?))
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
    bridge.override_answer(&state.ctx, &req.name, req.answer).await?;
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
