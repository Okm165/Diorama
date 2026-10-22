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
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use mirror_devnet::admin::{RewindRequest, ShadowRequest, TimeRequest};
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

/// `/admin/rewind` — the journal rewind's report.
#[derive(Debug, Serialize)]
struct RewindResponse {
    rewound_blocks: u64,
    restored_values: usize,
    cursor_t: u64,
}

/// `/admin/pause` — the loops are held (plan §4.4 stop-sync).
#[derive(Debug, Serialize)]
struct PauseResponse {
    paused: bool,
}

/// `/admin/resume` — the halt and the divergence freeze are both cleared.
#[derive(Debug, Serialize)]
struct ResumeResponse {
    resumed: bool,
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
            Error::Devnet(
                mirror_devnet::Error::NoCursor
                | mirror_devnet::Error::InvalidRewind { .. }
                | mirror_devnet::Error::InvalidShadow { .. },
            ) => StatusCode::BAD_REQUEST,
            Error::Devnet(mirror_devnet::Error::Poisoned { .. }) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "admin request failed");
        }
        (status, self.to_string()).into_response()
    }
}

pub async fn serve(ctx: Arc<Ctx>) -> Result<()> {
    let addr = format!("{}:{}", ctx.cfg.services.engine.http.addr, ctx.cfg.services.engine.http.port);
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/admin/rewind", post(admin_rewind))
        .route("/admin/resume", post(admin_resume))
        .route("/admin/pause", post(admin_pause))
        .route("/admin/time", post(admin_time))
        .route("/admin/shadow", post(admin_shadow))
        .route("/admin/deal", post(admin_deal))
        .with_state(ctx);
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
    if ctx.halted.load(Ordering::Relaxed) {
        (StatusCode::SERVICE_UNAVAILABLE, "halted")
    } else {
        (StatusCode::OK, "ok")
    }
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
    let mut shadowed: Vec<String> = shadow
        .slots
        .keys()
        .chain(shadow.accounts.iter())
        .map(|a| format!("{a:?}").to_lowercase())
        .collect();
    shadowed.sort();
    shadowed.dedup();
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
        halted: ctx.halted.load(Ordering::Relaxed),
        watchlist: ctx.watchlist.len(),
        shadowed_slots: shadow.slot_count(),
        shadowed_accounts: shadow.accounts.len(),
        shadowed,
        blob_form: ctx.blob_form()?.map(crate::blob::BlobForm::as_str),
        time_offset_seconds: ctx.time_offset.load(Ordering::Relaxed),
    }))
}

async fn metrics(State(ctx): State<Arc<Ctx>>) -> String {
    ctx.metrics.render()
}

async fn admin_rewind(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<RewindRequest>,
) -> Result<Json<RewindResponse>> {
    let out = ctx.admin().rewind(req.blocks).await?;
    Ok(Json(RewindResponse {
        rewound_blocks: req.blocks,
        restored_values: out.restored,
        cursor_t: out.cursor,
    }))
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
    if req.clear {
        // The shadow scanner is a ~2 s poll: a devnet write mined moments ago
        // may not be marked yet. Clearing before it is marked would let the
        // scanner re-mark the key *with the devnet's value* right after this
        // re-sync - the "poisoned source" race (measured: an oracle crash tx
        // mined <2 s before the unshadow, re-marked with the crashed source,
        // and the borrow that followed reverted on the crashed price). Force
        // the scan up to the current head first, so the registry reflects every
        // mined write and `all` really collects them all. A scan that cannot
        // complete fails the request: a partial registry would let the race
        // through (marks intact, so the caller can retry).
        let t_head = ctx.anvil_t.block_number().await?;
        let from = ctx.shadow_scanned_upto.load(Ordering::Relaxed);
        if from == 0 {
            ctx.shadow_scanned_upto.store(t_head, Ordering::Relaxed);
        } else {
            crate::scan_range(&ctx, from, t_head).await?;
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
