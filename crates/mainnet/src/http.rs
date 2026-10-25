//! The mainnet engine's HTTP surface: the shared route table
//! (`mirror_devnet::handlers`) plus this profile's own route — `/admin/time`
//! (T's clock offset). The feed bridge's routes live with the bridge
//! (`feed/http.rs`, the venue's simulator-API pattern). The admin
//! *semantics* live in `mirror-devnet`'s admin surface; the shared handlers
//! are one generic definition there.
//!
//! This module owns the HTTP mapping of the error taxonomy: request refusals
//! (the devnet's `NoCursor` / `InvalidRewind`) are 4xx; a poisoned registry
//! is 503 (the mirror cannot serve correct answers until it is restarted);
//! everything else is a server-side fault, logged with its typed cause.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::feed::FeedBridge;
use axum::{
    extract::State,
    http::StatusCode,
    routing::post,
    Json, Router,
};
use mirror_devnet::admin::TimeRequest;
use mirror_devnet::engine::{Engine as _, SharedStatus};
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// `/status` — the engine's diagnostic report: the shared fields
/// (`SharedStatus`) flattened beside this profile's own. A typed shape: the
/// scripts and dashboards read it by field name, so a rename must be a
/// compile error here, not a silent `null`.
#[derive(Debug, Serialize)]
pub struct StatusResponse {
    #[serde(flatten)]
    shared: SharedStatus,
    reth_head: u64,
    follow_head: u64,
    follow: &'static str,
    /// T's cursor by its profile name (the scripts' established spelling).
    cursor_t: Option<u64>,
    lag: u64,
    watchlist: usize,
    time_offset_seconds: u64,
}

/// The document's gathering: the shared fields from the shell, the rest from
/// this engine's own endpoints. A read that cannot answer degrades to 0 —
/// `/status` is a diagnostic, not a fact source.
pub(crate) async fn status_doc(ctx: &Ctx) -> Result<StatusResponse> {
    let shared =
        SharedStatus::gather(ctx.anvil_t.as_ref(), &ctx.plane, ctx.mirror.as_ref()).await?;
    let reth_head = ctx.reth.block_number().await.unwrap_or(0);
    let follow = ctx.follow_head().await.ok().flatten().unwrap_or(0);
    Ok(StatusResponse {
        reth_head,
        follow_head: follow,
        follow: ctx.follow_tag(),
        cursor_t: shared.cursor,
        lag: follow.saturating_sub(shared.cursor.unwrap_or(follow)),
        watchlist: ctx.mirror.scope().watch().accounts.len(),
        time_offset_seconds: ctx.plane.time_offset.load(Ordering::Relaxed),
        shared,
    })
}

/// `/admin/time` — the devnet clock offset is set (and persisted).
#[derive(Debug, Serialize)]
struct TimeResponse {
    offset_seconds: u64,
    t_head: u64,
    note: &'static str,
}

mirror_devnet::http_status_map!(Error, "admin request failed" {
    // The upstream anvil boundary: a transport fault is a bad gateway, not
    // this service's fault.
    Error::Rpc(_) => StatusCode::BAD_GATEWAY,
    // The feed bridge's named refusals (the venue maps its own
    // NotConfigured / NotFound the same way).
    Error::FeedBridgeUnconfigured => StatusCode::CONFLICT,
    Error::FeedBridgeUnknown { .. } => StatusCode::NOT_FOUND,
    Error::FeedBridgeWaiting { .. } => StatusCode::SERVICE_UNAVAILABLE,
});

/// The engine's control surface: the shared route table plus this profile's
/// own routes (the exact routes this process serves — the table the
/// control-plane docs list).
pub(crate) fn app(ctx: Arc<Ctx>, bridge: Option<Arc<FeedBridge>>) -> Router {
    mirror_devnet::handlers::shared_routes::<Ctx>()
        .route("/admin/time", post(admin_time))
        .with_state(ctx.clone())
        .merge(crate::feed::admin_routes(ctx, bridge))
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

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
