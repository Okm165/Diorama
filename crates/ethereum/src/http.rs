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
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/admin/rewind", post(admin_rewind))
        .route("/admin/resume", post(admin_resume))
        .route("/admin/pause", post(admin_pause))
        .route("/admin/time", post(admin_time))
        .route("/admin/shadow", post(admin_shadow))
        .route("/admin/deal", post(admin_deal))
        .route("/admin/epoch", post(admin_epoch))
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
    let mut out = ctx.metrics.render();
    out.push_str(&ctx.feed_bridge_metrics.render());
    out
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
            mirror_devnet::scan::scan_range(
                &ctx.anvil_t,
                &ctx.shadow,
                &ctx.shadow_scanned_upto,
                from,
                t_head,
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

/// `/admin/epoch` — one-call revert-to-blank (plan §7.1): re-pin T to the
/// source's latest block. The engine's cursor check detects the reset on its
/// next pass and runs the epoch rule (new base = clear marks and values;
/// same base = re-apply the recorded writes), so this route never replicates
/// that logic.
async fn admin_epoch(State(ctx): State<Arc<Ctx>>) -> Result<Json<EpochResponse>> {
    // The reset rides a public RPC and can outlive the client's timeout: on
    // a transport fault, check whether the fork base actually moved — an
    // applied-but-unanswered reset is a success, not a failure (the
    // retry-discipline rule: never report a fault for a completed effect).
    let before = ctx.anvil_t.fork_config().await?.map(|(_, block)| block);
    if let Err(e) = ctx.anvil_t.anvil_reset_fork_latest(&ctx.l1.source_url).await {
        let after = ctx.anvil_t.fork_config().await?.map(|(_, block)| block);
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
        source = %ctx.l1.source_url,
        "T was re-pinned to the source's latest block; the engine reconciles on its next pass"
    );
    Ok(Json(EpochResponse {
        reset: true,
        source: ctx.l1.source_url.clone(),
    }))
}

#[derive(Debug, Serialize)]
struct EpochResponse {
    reset: bool,
    source: String,
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
