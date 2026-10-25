//! The feed bridge's HTTP surface: `/admin/feed` (list), `/admin/feed/pause`,
//! `/admin/feed/resume`, `/admin/feed/override`. The bridge owns its routes
//! (the venue's simulator API does the same); the engine's `http.rs` merges
//! them beside the shared table.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::feed_bridge::{FeedBridge, FeedStatus};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;

/// The feed bridge's routes need both the context (override publishes a tx)
/// and the bridge itself; a separate state keeps the main router untouched.
#[derive(Clone)]
struct FeedState {
    ctx: Arc<Ctx>,
    bridge: Option<Arc<FeedBridge>>,
}

/// The feed bridge's routes, with their own state (the engine merges them).
pub(crate) fn admin_routes(ctx: Arc<Ctx>, bridge: Option<Arc<FeedBridge>>) -> Router {
    Router::new()
        .route("/admin/feed", get(feed_list))
        .route("/admin/feed/pause", post(feed_pause))
        .route("/admin/feed/resume", post(feed_resume))
        .route("/admin/feed/override", post(feed_override))
        .with_state(FeedState { ctx, bridge })
}

/// `/admin/feed` — the bridged feeds and their state.
async fn feed_list(State(state): State<FeedState>) -> Result<Json<Vec<FeedStatus>>> {
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
