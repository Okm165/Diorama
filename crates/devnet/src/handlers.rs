//! The two mirrors' identical HTTP handlers — one macro, both crates.
//!
//! The profiles differ in their state type and their error type; the shapes
//! and the shared `mirror_devnet` payload types do not. The five handlers the
//! orchestrator and the drills depend on (probes hit `/livez` on both, the
//! pause/resume/deal drills run against both) must stay byte-identical, so
//! they are generated once and invoked twice — the doctrine's one repeated
//! small-code macro, never a single-use one.

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
            Ok(::axum::Json(ctx.admin().deal(&req).await?))
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
