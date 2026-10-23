//! The venue engine's HTTP surface: health, status, metrics, and the shared
//! admin routes (deal / shadow / rewind / pause / resume / epoch / probes).
//!
//! The admin *semantics* live in `mirror-devnet`'s [`AdminSurface`] (shared
//! with the Mainnet Mirror); this module owns the routes and the HTTP mapping
//! of the error taxonomy: request refusals (the devnet's `NoCursor` /
//! `InvalidRewind` / `InvalidShadow`) are 4xx; a poisoned registry is 503;
//! everything else is a server-side fault, logged with its typed cause.
//!
//! `/admin/rewind` is the venue's own-write journal: the engine records the
//! before-values of the writes it performs on V (the deal faucet's silent
//! storage writes) under the devnet's coordinate — the scan cursor — and the
//! shared rewind undoes the last N scanned blocks newest-first. There is no
//! parent stream to re-apply, so the devnet re-creates its own state; the
//! driver's range marker clears once the head passes the rewound range.
//! `/admin/time` stays absent by design: V's clock is native anvil
//! (`setNextBlockTimestamp`/`evm_mine`, measured), which the harness calls
//! directly — an engine offset nobody re-aligns would be a no-op route.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use mirror_devnet::admin::{
    EpochResponse, PauseResponse, ResumeResponse, RewindRequest, RewindResponse,
    ShadowRequest,
};
use mirror_devnet::deal::DealRequest;
use mirror_devnet::metrics::Registry as _;
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// `/status` — the venue's diagnostic report (a typed shape: scripts read it
/// by field name).
#[derive(Debug, Serialize)]
struct StatusResponse {
    head: u64,
    cursor: Option<u64>,
    fork_pin: Option<u64>,
    epoch_pin: Option<u64>,
    halted: bool,
    shadowed_slots: usize,
    shadowed_accounts: usize,
    /// The shadowed account list (not just counts): these accounts are the
    /// devnet's own state (COW), so parity checks must skip them.
    shadowed: Vec<String>,
    probes: usize,
    /// The source URL the pin derives from (the unshadow truth's home).
    source: String,
}

/// The HTTP mapping of the error taxonomy (the engine and the simulator
/// share it): request refusals are 4xx, a warming/poisoned state is 503, the
/// venue's own refusals are 502, everything else is a server-side fault.
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            // The shared devnet taxonomy (mirror-devnet's typed predicates):
            // both engines classify the same refusals identically.
            Error::Devnet(e) if e.is_request_refusal() => StatusCode::BAD_REQUEST,
            Error::BadRequest { .. } => StatusCode::BAD_REQUEST,
            Error::NotFound { .. } | Error::NoMarket { .. } => StatusCode::NOT_FOUND,
            Error::NotConfigured { .. } => StatusCode::CONFLICT,
            // The upstream boundaries (anvil, the venue API): a transport
            // fault is a bad gateway, not this service's fault.
            Error::Rpc(_) | Error::ApiStatus { .. } | Error::Http(_) => StatusCode::BAD_GATEWAY,
            Error::Warming => StatusCode::SERVICE_UNAVAILABLE,
            Error::Devnet(e) if e.is_unavailable() => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "request failed");
        }
        (status, self.to_string()).into_response()
    }
}

/// The engine's control surface: the exact routes this process serves (the
/// table the control-plane docs list; the mirror's twin keeps the same shape).
fn app(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/livez", get(livez))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/admin/resume", post(admin_resume))
        .route("/admin/pause", post(admin_pause))
        .route("/admin/rewind", post(admin_rewind))
        .route("/admin/shadow", post(admin_shadow))
        .route("/admin/deal", post(admin_deal))
        .route("/admin/epoch", post(admin_epoch))
        .route("/admin/probes", post(admin_probes))
        .with_state(ctx)
}

/// Bind the configured address and serve the control surface until the
/// process is torn down.
pub async fn serve(ctx: Arc<Ctx>) -> Result<()> {
    let addr = format!("{}:{}", ctx.engine.http.addr, ctx.engine.http.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| Error::Bind {
            addr: addr.clone(),
            source,
        })?;
    tracing::info!(addr, "venue http api listening");
    axum::serve(listener, app(ctx))
        .await
        .map_err(|source| Error::Io {
            what: "http api",
            path: addr.into(),
            source,
        })?;
    Ok(())
}

async fn healthz(State(ctx): State<Arc<Ctx>>) -> impl IntoResponse {
    if ctx.plane.is_halted() {
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
    let head = ctx.v.block_number().await.unwrap_or(0);
    let cursor = ctx.cursor_v()?;
    let shadow = ctx.plane.shadow.lock()?;
    Ok(Json(StatusResponse {
        head,
        cursor: cursor.as_ref().map(|c| c.block),
        fork_pin: match ctx.pin.load(Ordering::Relaxed) {
            0 => None,
            p => Some(p),
        },
        epoch_pin: ctx.plane.state.lock()?.data.epoch_pin,
        halted: ctx.plane.is_halted(),
        shadowed_slots: shadow.slot_count(),
        shadowed_accounts: shadow.accounts.len(),
        shadowed: shadow.shadowed_addresses(),
        probes: ctx.engine.verifier.probes.len(),
        source: ctx.chain.source_url.clone(),
    }))
}

async fn metrics(State(ctx): State<Arc<Ctx>>) -> String {
    // The same composition as the ethereum engine: this profile's own set,
    // then the shared admin gauges (one definition — `Metrics::render`).
    let mut out = ctx.venue_metrics.render();
    out.push_str(&ctx.plane.metrics.render());
    out
}

async fn admin_resume(State(ctx): State<Arc<Ctx>>) -> Json<ResumeResponse> {
    ctx.admin().resume();
    Json(ResumeResponse { resumed: true })
}

async fn admin_pause(State(ctx): State<Arc<Ctx>>) -> Json<PauseResponse> {
    ctx.admin().pause();
    Json(PauseResponse { paused: true })
}

/// `/admin/rewind`: undo the engine's journaled writes (the deal faucet's
/// silent storage writes) over the last `blocks` scanned blocks, newest first.
async fn admin_rewind(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<RewindRequest>,
) -> Result<Json<RewindResponse>> {
    Ok(Json(ctx.admin().rewind_response(req.blocks).await?))
}
async fn admin_shadow(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<ShadowRequest>,
) -> Result<Json<mirror_devnet::admin::ShadowOutcome>> {
    // The hand-back carries its own race guard (AdminSurface::shadow_hand_back);
    // the scan cost its pre-step paid lands in this profile's counter.
    let (out, walk) = ctx.admin().shadow_hand_back(&req).await?;
    ctx.venue_metrics.record_scan(
        &walk,
        &ctx.venue_metrics.scanned_blocks_total,
        &ctx.venue_metrics.skipped_empty_blocks_total,
    );
    Ok(Json(out))
}

async fn admin_deal(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<DealRequest>,
) -> Result<Json<mirror_devnet::deal::DealOutcome>> {
    Ok(Json(ctx.admin().deal(&req).await?))
}

/// `/admin/epoch`: re-pin V to the fork source's latest block. One anvil
/// call — the driver's canary notices the reset on its next pass and runs the
/// epoch rule (new base = new epoch, same base = restore), so this route
/// never needs to replicate that logic.
async fn admin_epoch(State(ctx): State<Arc<Ctx>>) -> Result<Json<EpochResponse>> {
    Ok(Json(
        mirror_devnet::admin::AdminSurface::epoch_repin(&ctx.v, &ctx.chain.source_url).await?,
    ))
}

/// `/admin/probes`: run the at-pin parity probes now and report the run (boot
/// and re-pin run them automatically; the on-demand trigger matters after a
/// hand-back, when the probes should go green again). The ethereum engine's
/// route is the same verb over its M-vs-reth probe set; the cumulative
/// counters live in `/metrics` on both.
async fn admin_probes(
    State(ctx): State<Arc<Ctx>>,
) -> Result<Json<mirror_devnet::probes::ProbeOutcome>> {
    Ok(Json(crate::driver::probes_now(&ctx).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared taxonomy, pinned per service (the same test name lives in
    /// the ethereum engine and the CCTP bridge): a request refusal is 400, a
    /// transport fault at a boundary (anvil, the venue API) is 502, an
    /// unavailable/warming state is 503, a named absence/conflict keeps its
    /// 4xx. The sim serves the same mapping (this crate's `Error`).
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the status-mapping table: one row per error variant, all in one place"
    )]
    fn the_http_status_mapping_follows_the_shared_convention() {
        let status = |e: Error| e.into_response().status();
        let anvil = || mirror_anvil::Error::Status {
            endpoint: "anvil-v".into(),
            method: "eth_call".into(),
            status: 503,
            body: String::new(),
        };
        assert_eq!(
            status(Error::Devnet(mirror_devnet::Error::NoCursor)),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(Error::BadRequest {
                reason: "no".into()
            }),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(Error::NotFound {
                what: "coin".into()
            }),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(Error::NoMarket {
                coin: "BTC".into()
            }),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(Error::NotConfigured { what: "sim" }),
            StatusCode::CONFLICT
        );
        assert_eq!(status(Error::Rpc(anvil())), StatusCode::BAD_GATEWAY);
        assert_eq!(
            status(Error::ApiStatus {
                status: 429,
                body: "slow down".into()
            }),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(status(Error::Warming), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            status(Error::Devnet(mirror_devnet::Error::Poisoned {
                what: "shadow registry"
            })),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(Error::Decode {
                what: "meta",
                source: serde_json::from_str::<i32>("nope").unwrap_err()
            }),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
