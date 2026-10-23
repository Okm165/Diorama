//! The Venue Mirror engine binary — the `venue` profile's entrypoint.
//!
//! Boot order (plan §7.2): wait for anvil V, assert the chain, reconcile the
//! registry (reset detection + the epoch rule; the reconcile also anchors the
//! epoch pin before any writes exist — the counterpart of the ethereum
//! engine's explicit boot anchor), run the at-pin probes, then serve the HTTP
//! surface and the driver loop. A boot fault is fatal: a driver over the wrong
//! chain or a broken registry must not serve.
//!
//! Thin shell by design: the runtime is built here, the story lives in
//! `mirror_hyperliquid`. The shutdown path mirrors the ethereum engine's: the
//! signal aborts the tasks; every mutation persists its state as it happens
//! (shadow registry, cursor), so there is nothing to drain.
use mirror_config::boot;
use std::process::ExitCode;

fn main() -> ExitCode {
    boot::run_main(run())
}

async fn run() -> mirror_hyperliquid::Result<()> {
    boot::init_logging();
    let config = boot::instance_path();
    // One instance file, many readers: this pod provides only the venue
    // engine's wiring. `inspect` reports the other services' unresolved
    // references instead of failing on them; the engine's own sections are
    // then required here, by name (the cctp bridge's pattern).
    let (cfg, unresolved) = mirror_config::Instance::inspect(std::path::Path::new(&config))?;
    if let (Some(chain), Some(venue)) = (cfg.chains.venue.as_ref(), cfg.services.venue.as_ref()) {
        let state_dir = venue.state_dir.to_string_lossy().to_string();
        mirror_config::require_resolved_logged(
            "venue engine wiring",
            &[&chain.source_url, &venue.anvil_url, &state_dir],
            &unresolved,
        )?;
    }
    let (source_url, v_url) = cfg.venue_urls()?;
    let ctx = std::sync::Arc::new(mirror_hyperliquid::Ctx::new(cfg, &source_url, &v_url)?);

    mirror_hyperliquid::driver::boot(&ctx).await?;
    let mut tasks = Vec::new();
    {
        let ctx = ctx.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = mirror_hyperliquid::http::serve(ctx).await {
                tracing::error!(error = %e, "http server exited");
            }
        }));
    }
    {
        let ctx = ctx.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = mirror_hyperliquid::driver::run(ctx).await {
                tracing::error!(error = %e, "venue driver exited");
            }
        }));
    }

    boot::shutdown_signal().await;
    tracing::info!("shutdown signal received");
    for t in tasks {
        t.abort();
    }
    Ok(())
}


