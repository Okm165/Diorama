//! The shared HTTP handlers and route table: one generic definition per
//! route, so the two engines' surfaces cannot drift. The handler bodies were
//! always identical — the per-crate macro that used to expand them is gone
//! with the [`Engine`] trait, which made them nameable once.
//!
//! A mirror adds its own routes beside [`shared_routes`] in its `http`
//! module (one engine adds `/admin/time` and the feed bridge; the other
//! deliberately serves no clock route).
use crate::admin::{
    EpochResponse, PauseResponse, ResumeResponse, RewindRequest, RewindResponse, ShadowOutcome,
    ShadowRequest,
};
use crate::deal::{DealOutcome, DealRequest};
use crate::engine::Engine;
use crate::facts::Fact;
use crate::probes::ProbeOutcome;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt as _;
use std::sync::Arc;

/// The shared route table: health, status, metrics, the fact stream, and the
/// admin surface. The mirror's own routes are added beside it.
/// The shared health path: the engines' table and the venue simulator's info
/// router answer the same route (one spelling; the k8s probes hit it on both).
pub const HEALTHZ: &str = "/healthz";

/// The shared metrics path: the engines' table and the venue simulator's
/// control router answer the same route (the cctp bridge's own router spells
/// its own — it does not depend on the spine).
pub const METRICS: &str = "/metrics";

/// The shared HTTP status of the spine's own taxonomy: a request refusal is
/// 400, an unavailable boundary is 503 (retryable — never a 500, which would
/// read as a bug). `None` means the shell's own variant decides: each surface
/// maps its own refusals beside this, so the same shared refusal can never
/// answer two statuses.
pub fn devnet_status(error: &crate::Error) -> Option<StatusCode> {
    if error.is_request_refusal() {
        return Some(StatusCode::BAD_REQUEST);
    }
    if error.is_unavailable() {
        return Some(StatusCode::SERVICE_UNAVAILABLE);
    }
    None
}

pub fn shared_routes<E: Engine>() -> Router<Arc<E>> {
    Router::new()
        .route(HEALTHZ, get(healthz::<E>))
        .route(METRICS, get(metrics::<E>))
        .route("/livez", get(livez))
        .route("/status", get(status::<E>))
        .route("/events", get(admin_events::<E>))
        .route("/admin/resume", post(admin_resume::<E>))
        .route("/admin/pause", post(admin_pause::<E>))
        .route("/admin/rewind", post(admin_rewind::<E>))
        .route("/admin/shadow", post(admin_shadow::<E>))
        .route("/admin/deal", post(admin_deal::<E>))
        .route("/admin/epoch", post(admin_epoch::<E>))
        .route("/admin/probes", post(admin_probes::<E>))
}

/// `/healthz` — 503 while the engine is paused (the orchestrator probes
/// `/livez`, which is pause-agnostic).
async fn healthz<E: Engine>(State(ctx): State<Arc<E>>) -> impl IntoResponse {
    if ctx.plane().is_halted() {
        (StatusCode::SERVICE_UNAVAILABLE, "halted")
    } else {
        (StatusCode::OK, "ok")
    }
}

/// `/livez` — the orchestrator's liveness signal: the process and its HTTP
/// runtime are answering. Deliberately *not* `/healthz`: that reports the
/// engine's own state (503 while paused), and probing it would take the
/// engine out of the Service (its NodePort included) and eventually restart
/// the pod in the middle of the documented pause drill — while the contract
/// says a paused engine keeps serving reads and writes.
async fn livez() -> &'static str {
    "ok"
}

/// `/status` — the mirror's diagnostic report (its typed document, gathered
/// by the mirror; the shared fields come from [`SharedStatus`]).
async fn status<E: Engine>(State(ctx): State<Arc<E>>) -> crate::Result<Json<E::Status>, E::Error> {
    Ok(Json(ctx.status().await?))
}

/// `/metrics` — the mirror's own exposition, then the shared admin gauges
/// (one definition — `Metrics::render`).
async fn metrics<E: Engine>(State(ctx): State<Arc<E>>) -> String {
    let mut out = ctx.profile_metrics();
    out.push_str(&ctx.plane().metrics.render());
    out
}

async fn admin_resume<E: Engine>(State(ctx): State<Arc<E>>) -> Json<ResumeResponse> {
    ctx.admin().resume();
    Json(ResumeResponse { resumed: true })
}

async fn admin_pause<E: Engine>(State(ctx): State<Arc<E>>) -> Json<PauseResponse> {
    ctx.admin().pause();
    Json(PauseResponse { paused: true })
}

async fn admin_deal<E: Engine>(
    State(ctx): State<Arc<E>>,
    Json(req): Json<DealRequest>,
) -> crate::Result<Json<DealOutcome>, E::Error> {
    let outcome = ctx.admin().deal(&req).await?;
    // The event model's one fact per state change: the faucet's write is
    // visible to `/events` consumers. A no-slot verdict writes nothing, so it
    // publishes nothing — the fact would otherwise claim a write that never
    // happened (found by the local engine proof: a token without the standard
    // layout answered `ok: false` and still emitted `deal_applied`). The sim
    // service has no bus (it is plane-less by design), so the engine's door is
    // the fact's home.
    if outcome.ok {
        ctx.plane().facts.publish(Fact::DealApplied {
            chain: ctx.label(),
            address: req.to,
        });
    }
    Ok(Json(outcome))
}

/// `/admin/rewind` — undo the engine's journaled writes over the last
/// `blocks` scanned blocks, newest first.
async fn admin_rewind<E: Engine>(
    State(ctx): State<Arc<E>>,
    Json(req): Json<RewindRequest>,
) -> crate::Result<Json<RewindResponse>, E::Error> {
    let out = ctx.admin().rewind_response(req.blocks).await?;
    // The event model's one fact per state change: the rewind moved the
    // chain and the cursor backwards together.
    ctx.plane().facts.publish(Fact::RewindApplied {
        chain: ctx.label(),
        blocks: out.rewound_blocks,
        cursor: out.cursor,
    });
    Ok(Json(out))
}

/// `/admin/shadow` — the COW hand-back: clear the named keys' marks (or all
/// of an account's) and re-sync them from the source. The hand-back carries
/// its own race guard; the scan its pre-step paid for lands in the profile's
/// counters.
async fn admin_shadow<E: Engine>(
    State(ctx): State<Arc<E>>,
    Json(req): Json<ShadowRequest>,
) -> crate::Result<Json<ShadowOutcome>, E::Error> {
    let (out, walk) = ctx.admin().shadow_hand_back(&req).await?;
    ctx.record_scan(&walk);
    Ok(Json(out))
}

/// `/admin/epoch` — one-call revert-to-blank: re-pin the chain to the
/// source's latest block. The engine's reset detection notices on its next
/// pass and runs the epoch rule (new base = clear marks and values; same
/// base = re-apply the recorded writes), so this route never replicates
/// that logic.
async fn admin_epoch<E: Engine>(
    State(ctx): State<Arc<E>>,
) -> crate::Result<Json<EpochResponse>, E::Error> {
    Ok(Json(ctx.epoch_repin().await?))
}

/// `/admin/probes` — run the configured probes now and report the run (boot
/// and re-pin run them automatically; the on-demand trigger matters after a
/// hand-back, when they should go green again). The per-block counters stay
/// the loops'.
async fn admin_probes<E: Engine>(
    State(ctx): State<Arc<E>>,
) -> crate::Result<Json<ProbeOutcome>, E::Error> {
    Ok(Json(ctx.probes_now().await?))
}

/// `/events` — the fact stream, SSE. One JSON notice per event (`id:` = the
/// bus sequence number; `event: fact`). Notifications, not state: the server
/// replays nothing on reconnect — a consumer that missed events re-reads
/// `/status` and the state files. A lagged subscriber resumes from the
/// oldest retained fact (tokio broadcast semantics), which is the
/// notification contract. An opening `: connected` comment makes the live
/// subscription observable, so a consumer that must act *after* the stream
/// is live (the local proofs' sequencing) need not race the subscribe — a
/// comment, not a fact, so the no-replay contract is untouched.
async fn admin_events<E: Engine>(State(ctx): State<Arc<E>>) -> impl IntoResponse {
    let rx = ctx.plane().facts.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(notice) => {
                    let event = Event::default()
                        .id(notice.seq.to_string())
                        .event("fact")
                        .json_data(notice)
                        .unwrap_or_else(|_| Event::default().event("encode_error"));
                    return Some((Ok::<_, std::convert::Infallible>(event), rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let opening = futures_util::stream::once(async {
        Ok::<_, std::convert::Infallible>(Event::default().comment("connected"))
    });
    Sse::new(opening.chain(stream)).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared taxonomy's statuses: refusals 400, unavailable 503, and
    /// everything else leaves the decision to the shell (`None`).
    #[test]
    fn the_shared_taxonomy_maps_once() {
        assert_eq!(
            devnet_status(&crate::Error::NoCursor),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            devnet_status(&crate::Error::ShadowScan { block: 7, failed: 1 }),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(
            devnet_status(&crate::Error::KindMismatch {
                write_kind: "storage",
                before_kind: "balance",
            }),
            None
        );
    }
}
