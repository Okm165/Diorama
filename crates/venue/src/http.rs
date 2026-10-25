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
//! shared rewind undoes the last N scanned blocks newest-first. The follow's
//! source windows are not journaled (a rewind restores devnet writes, never
//! source history), so the devnet re-creates its own state; the
//! driver's range marker clears once the head passes the rewound range.
//! `/admin/time` stays absent by design: V's clock is native anvil
//! (`setNextBlockTimestamp`/`evm_mine`, measured), which the harness calls
//! directly — an engine offset nobody re-aligns would be a no-op route.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use axum::{
    http::StatusCode,
    Router,
};
use mirror_devnet::engine::SharedStatus;
use serde::Serialize;
use std::sync::Arc;

/// `/status` — the venue's diagnostic report: the shared fields
/// (`SharedStatus`) flattened beside this profile's own. A typed shape:
/// scripts read it by field name.
#[derive(Debug, Serialize)]
pub struct StatusResponse {
    #[serde(flatten)]
    shared: SharedStatus,
    fork_pin: Option<u64>,
    epoch_pin: Option<u64>,
    probes: usize,
    /// The source URL the pin derives from (the unshadow truth's home).
    source: String,
}

/// The document's gathering: the shared fields from the shell, the rest from
/// this engine's own state.
pub(crate) async fn status_doc(ctx: &Ctx) -> Result<StatusResponse> {
    let shared =
        SharedStatus::gather(ctx.anvil_v.as_ref(), &ctx.plane, ctx.mirror.as_ref()).await?;
    Ok(StatusResponse {
        fork_pin: mirror_devnet::pin::resolved_pin(&ctx.pin),
        epoch_pin: ctx.plane.state.epoch_pin()?,
        probes: ctx.engine.verifier.probes.len(),
        source: ctx.chain.source_url.clone(),
        shared,
    })
}

// The HTTP mapping of the error taxonomy (the engine and the simulator
// share it): request refusals are 4xx, a warming/poisoned state is 503, the
// venue's own refusals are 502, everything else is a server-side fault.
mirror_devnet::http_status_map!(Error, "request failed" {
    Error::BadRequest { .. } => StatusCode::BAD_REQUEST,
    Error::NotFound { .. } | Error::NoMarket { .. } => StatusCode::NOT_FOUND,
    Error::NotConfigured { .. } => StatusCode::CONFLICT,
    // The upstream boundaries (anvil, the venue API): a transport fault is a
    // bad gateway, not this service's fault.
    Error::Rpc(_) | Error::ApiStatus { .. } | Error::Http(_) => StatusCode::BAD_GATEWAY,
    Error::Warming => StatusCode::SERVICE_UNAVAILABLE,
});

/// The engine's control surface: the shared route table, this profile's own
/// routes added beside it (the exact routes this process serves — the table
/// the control-plane docs list).
pub(crate) fn app(ctx: Arc<Ctx>) -> Router {
    mirror_devnet::handlers::shared_routes::<Ctx>().with_state(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    /// The shared taxonomy, pinned per service (the same test name lives in
    /// the mainnet engine and the CCTP bridge): a request refusal is 400, a
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
            status(Error::bad_request("no")),
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
