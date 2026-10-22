//! The venue engine's HTTP surface: health, status, metrics, and the shared
//! admin routes (deal / shadow / pause / resume).
//!
//! The admin *semantics* live in `mirror-devnet`'s [`AdminSurface`] (shared
//! with the Mainnet Mirror); this module owns the routes and the HTTP mapping
//! of the error taxonomy: request refusals (the devnet's `NoCursor` /
//! `InvalidRewind` / `InvalidShadow`) are 4xx; a poisoned registry is 503;
//! everything else is a server-side fault, logged with its typed cause.
//!
//! `/admin/rewind` is deliberately absent: the venue's journal is empty until
//! the devnet's own writes are journaled (or snapshots are wired), and a
//! rewind that silently restores nothing while reporting success is the exact
//! no-op the doctrine forbids. The route lands with that journaling.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use mirror_devnet::admin::ShadowRequest;
use mirror_devnet::deal::DealRequest;
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
            Error::Devnet(
                mirror_devnet::Error::NoCursor
                | mirror_devnet::Error::InvalidRewind { .. }
                | mirror_devnet::Error::InvalidShadow { .. },
            ) => StatusCode::BAD_REQUEST,
            Error::BadRequest { .. } => StatusCode::BAD_REQUEST,
            Error::NotFound { .. } | Error::NoMarket { .. } => StatusCode::NOT_FOUND,
            Error::NotConfigured { .. } => StatusCode::CONFLICT,
            Error::ApiStatus { .. } | Error::Http(_) => StatusCode::BAD_GATEWAY,
            Error::Warming | Error::Devnet(mirror_devnet::Error::Poisoned { .. }) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "request failed");
        }
        (status, self.to_string()).into_response()
    }
}

pub async fn serve(ctx: Arc<Ctx>) -> Result<()> {
    let addr = format!("{}:{}", ctx.venue.http.addr, ctx.venue.http.port);
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/admin/resume", post(admin_resume))
        .route("/admin/pause", post(admin_pause))
        .route("/admin/shadow", post(admin_shadow))
        .route("/admin/deal", post(admin_deal))
        .route("/admin/epoch", post(admin_epoch))
        .route("/admin/probes", post(admin_probes))
        .with_state(ctx);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| Error::Bind {
            addr: addr.clone(),
            source,
        })?;
    tracing::info!(addr, "venue http api listening");
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

async fn status(State(ctx): State<Arc<Ctx>>) -> Result<Json<StatusResponse>> {
    // Head gauges: an endpoint that cannot answer degrades the report to 0,
    // never fails it - /status is a diagnostic, not a fact source.
    let head = ctx.v.block_number().await.unwrap_or(0);
    let cursor = ctx.cursor_v()?;
    let shadow = ctx.shadow.lock()?;
    let mut shadowed: Vec<String> = shadow
        .slots
        .keys()
        .chain(shadow.accounts.iter())
        .map(|a| format!("{a:?}").to_lowercase())
        .collect();
    shadowed.sort();
    shadowed.dedup();
    Ok(Json(StatusResponse {
        head,
        cursor: cursor.as_ref().map(|c| c.block),
        fork_pin: match ctx.pin.load(Ordering::Relaxed) {
            0 => None,
            p => Some(p),
        },
        epoch_pin: ctx.state.lock()?.data.epoch_pin,
        halted: ctx.is_halted(),
        shadowed_slots: shadow.slot_count(),
        shadowed_accounts: shadow.accounts.len(),
        shadowed,
        probes: ctx.venue.probes.len(),
        source: ctx.chain.source_url.clone(),
    }))
}

async fn metrics(State(ctx): State<Arc<Ctx>>) -> String {
    // The venue's own set beside the admin surface's shared gauges (the
    // ethereum engine's head/replay gauges are not this profile's facts).
    let mut out = ctx.venue_metrics.render();
    out.push_str(&ctx.metrics.render_admin());
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

#[derive(Debug, Serialize)]
struct ResumeResponse {
    resumed: bool,
}

#[derive(Debug, Serialize)]
struct PauseResponse {
    paused: bool,
}

async fn admin_shadow(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<ShadowRequest>,
) -> Result<Json<mirror_devnet::admin::ShadowOutcome>> {
    if req.clear {
        // The scanner is a ~1 s poll: a devnet write mined moments ago may not
        // be marked yet. Clearing before it is marked would let the scanner
        // re-mark the key *with the devnet's value* right after this re-sync -
        // the poisoned-source race. Force the scan up to the current head
        // first; a scan that cannot complete fails the request (marks intact,
        // so the caller can retry).
        let head = ctx.v.block_number().await?;
        let from = ctx.shadow_scanned_upto.load(Ordering::Relaxed);
        if from == 0 {
            ctx.shadow_scanned_upto.store(head, Ordering::Relaxed);
        } else {
            mirror_devnet::scan::scan_range(
                &ctx.v,
                &ctx.shadow,
                &ctx.shadow_scanned_upto,
                from,
                head,
            )
            .await?;
        }
    }
    Ok(Json(ctx.admin().shadow(&req).await?))
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
    // The reset rides a public RPC and can outlive the client's timeout: on
    // a transport fault, check whether the fork base actually moved — an
    // applied-but-unanswered reset is a success, not a failure (the
    // retry-discipline rule: never report a fault for a completed effect).
    let before = ctx.v.fork_config().await?.map(|(_, block)| block);
    if let Err(e) = ctx.v.anvil_reset_fork_latest(&ctx.chain.source_url).await {
        let after = ctx.v.fork_config().await?.map(|(_, block)| block);
        if after == before {
            return Err(e.into());
        }
        tracing::warn!(
            error = %e,
            before = ?before,
            after = ?after,
            "anvil_reset answered with a transport fault but the fork base moved; treating it as applied"
        );
    }
    tracing::warn!(
        source = %ctx.chain.source_url,
        "V was re-pinned to the source's latest block; the driver reconciles on its next pass"
    );
    Ok(Json(EpochResponse {
        reset: true,
        source: ctx.chain.source_url.clone(),
    }))
}

#[derive(Debug, Serialize)]
struct EpochResponse {
    reset: bool,
    source: String,
}

/// `/admin/probes`: run the at-pin parity probes now and report the counters
/// (boot and re-pin run them automatically; the on-demand trigger matters
/// after a hand-back, when the probes should go green again).
async fn admin_probes(State(ctx): State<Arc<Ctx>>) -> Result<Json<ProbeReport>> {
    crate::driver::probes_now(&ctx).await?;
    Ok(Json(ProbeReport {
        ok: ctx.venue_metrics.probe_ok.load(Ordering::Relaxed),
        failed: ctx.venue_metrics.probe_failed.load(Ordering::Relaxed),
        skipped: ctx.venue_metrics.probe_skipped.load(Ordering::Relaxed),
        last_failed: ctx.venue_metrics.probe_last_failed.load(Ordering::Relaxed),
    }))
}

#[derive(Debug, Serialize)]
struct ProbeReport {
    ok: u64,
    failed: u64,
    skipped: u64,
    last_failed: u64,
}
