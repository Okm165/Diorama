//! The two mirrors' identical HTTP handlers — one macro, both crates.
//!
//! The profiles differ in their state type, their error type and a few
//! `Ctx` accessors; the shapes and the shared `mirror_devnet` payload types do
//! not. The handlers the orchestrator and the drills depend on (probes hit
//! `/livez` on both, the pause/resume/deal/rewind/shadow/epoch drills run
//! against both) must stay byte-identical, so they are generated once and
//! invoked twice — the doctrine's one repeated small-code macro, never a
//! single-use one. The profile-specific facts the handlers need are `Ctx`
//! methods (`admin`, `record_scan`, `epoch_repin`).

/// Generate the shared health and admin handlers inside a mirror's `http`
/// module (its `Ctx` and its error type in scope):
///
/// ```ignore
/// mirror_devnet::mirror_handlers!(Ctx, crate::error::Error);
/// ```
#[macro_export]
macro_rules! mirror_handlers {
    ($ctx:ty, $error:ty) => {
        async fn healthz(
            ::axum::extract::State(ctx): ::axum::extract::State<::std::sync::Arc<$ctx>>,
        ) -> impl ::axum::response::IntoResponse {
            if ctx.plane.is_halted() {
                (::axum::http::StatusCode::SERVICE_UNAVAILABLE, "halted")
            } else {
                (::axum::http::StatusCode::OK, "ok")
            }
        }

        /// `/livez` — the orchestrator's liveness signal: the process and its
        /// HTTP runtime are answering. Deliberately *not* `/healthz`: that
        /// reports the engine's own state (503 while paused), and probing it
        /// would take the engine out of the Service (its NodePort included)
        /// and eventually restart the pod in the middle of the documented
        /// pause drill — while the contract says a paused engine keeps serving
        /// reads and writes.
        async fn livez() -> &'static str {
            "ok"
        }

        async fn admin_resume(
            ::axum::extract::State(ctx): ::axum::extract::State<::std::sync::Arc<$ctx>>,
        ) -> ::axum::Json<$crate::admin::ResumeResponse> {
            ctx.admin().resume();
            ::axum::Json($crate::admin::ResumeResponse { resumed: true })
        }

        async fn admin_pause(
            ::axum::extract::State(ctx): ::axum::extract::State<::std::sync::Arc<$ctx>>,
        ) -> ::axum::Json<$crate::admin::PauseResponse> {
            ctx.admin().pause();
            ::axum::Json($crate::admin::PauseResponse { paused: true })
        }

        async fn admin_deal(
            ::axum::extract::State(ctx): ::axum::extract::State<::std::sync::Arc<$ctx>>,
            ::axum::Json(req): ::axum::Json<$crate::deal::DealRequest>,
        ) -> ::std::result::Result<::axum::Json<$crate::deal::DealOutcome>, $error> {
            let outcome = ctx.admin().deal(&req).await?;
            // The event model's one fact per state change: the faucet's write
            // is visible to `/events` consumers. The sim service has no bus
            // (it is plane-less by design), so the engine's door is the
            // fact's home.
            ctx.plane.facts.publish($crate::facts::Fact::DealApplied {
                chain: ctx.mirror.label(),
                address: req.to,
            });
            Ok(::axum::Json(outcome))
        }

        /// `/admin/rewind` — undo the engine's journaled writes over the last
        /// `blocks` scanned blocks, newest first.
        async fn admin_rewind(
            ::axum::extract::State(ctx): ::axum::extract::State<::std::sync::Arc<$ctx>>,
            ::axum::Json(req): ::axum::Json<$crate::admin::RewindRequest>,
        ) -> ::std::result::Result<::axum::Json<$crate::admin::RewindResponse>, $error> {
            Ok(::axum::Json(ctx.admin().rewind_response(req.blocks).await?))
        }

        /// `/admin/shadow` — the COW hand-back: clear the named keys' marks
        /// (or all of an account's) and re-sync them from the source. The
        /// hand-back carries its own race guard; the scan its pre-step paid
        /// for lands in the profile's counters (`Ctx::record_scan`).
        async fn admin_shadow(
            ::axum::extract::State(ctx): ::axum::extract::State<::std::sync::Arc<$ctx>>,
            ::axum::Json(req): ::axum::Json<$crate::admin::ShadowRequest>,
        ) -> ::std::result::Result<::axum::Json<$crate::admin::ShadowOutcome>, $error> {
            let (out, walk) = ctx.admin().shadow_hand_back(&req).await?;
            ctx.record_scan(&walk);
            Ok(::axum::Json(out))
        }

        /// `/admin/epoch` — one-call revert-to-blank: re-pin the chain to the
        /// source's latest block. The engine's reset detection notices on its
        /// next pass and runs the epoch rule (new base = clear marks and
        /// values; same base = re-apply the recorded writes), so this route
        /// never replicates that logic.
        async fn admin_epoch(
            ::axum::extract::State(ctx): ::axum::extract::State<::std::sync::Arc<$ctx>>,
        ) -> ::std::result::Result<::axum::Json<$crate::admin::EpochResponse>, $error> {
            Ok(::axum::Json(ctx.epoch_repin().await?))
        }

        /// `/admin/probes` — run the configured probes now and report the
        /// run (boot and re-pin run them automatically; the on-demand trigger
        /// matters after a hand-back, when they should go green again). The
        /// per-block counters stay the loops'.
        async fn admin_probes(
            ::axum::extract::State(ctx): ::axum::extract::State<::std::sync::Arc<$ctx>>,
        ) -> ::std::result::Result<::axum::Json<$crate::probes::ProbeOutcome>, $error> {
            Ok(::axum::Json(ctx.probes_now().await?))
        }

        /// `/events` — the fact stream, SSE. One JSON notice per event
        /// (`id:` = the bus sequence number; `event: fact`). Notifications,
        /// not state: the server replays nothing on reconnect — a consumer
        /// that missed events re-reads `/status` and the state files. A
        /// lagged subscriber resumes from the oldest retained fact (tokio
        /// broadcast semantics), which is the notification contract.
        async fn admin_events(
            ::axum::extract::State(ctx): ::axum::extract::State<::std::sync::Arc<$ctx>>,
        ) -> impl ::axum::response::IntoResponse {
            let rx = ctx.plane.facts.subscribe();
            let stream = ::futures_util::stream::unfold(rx, |mut rx| async move {
                loop {
                    match rx.recv().await {
                        Ok(notice) => {
                            let event = ::axum::response::sse::Event::default()
                                .id(notice.seq.to_string())
                                .event("fact")
                                .json_data(notice)
                                .unwrap_or_else(|_| {
                                    ::axum::response::sse::Event::default().event("encode_error")
                                });
                            return Some((Ok::<_, ::std::convert::Infallible>(event), rx));
                        }
                        Err(::tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(::tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    }
                }
            });
            ::axum::response::sse::Sse::new(stream)
                .keep_alive(::axum::response::sse::KeepAlive::default())
        }
    };
}
