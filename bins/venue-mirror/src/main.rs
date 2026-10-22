//! The Venue Mirror engine binary — the `venue` profile's entrypoint.
//!
//! Boot order (plan §7.2): wait for anvil V, assert the chain, reconcile the
//! registry (reset detection + the epoch rule), run the at-pin probes, then
//! serve the HTTP surface and the driver loop. A boot fault is fatal: a driver
//! over the wrong chain or a broken registry must not serve.
//!
//! Thin shell by design: the runtime is built here, the story lives in
//! `mirror_hyperliquid`.
use std::process::ExitCode;

/// Worker stack budget: the same measured convention as the ethereum engine
/// (debug builds give large by-value buffers their own stack slots; the
/// driver's blob-free paths are far smaller, but one budget for both binaries
/// is one thing to know).
const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_SIZE)
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            report(&e);
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        match run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                report(&e);
                ExitCode::FAILURE
            }
        }
    })
}

async fn run() -> mirror_hyperliquid::Result<()> {
    init_tracing();
    let config = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/etc/mirror/instance.toml".to_string());
    let cfg = mirror_config::Instance::load(std::path::Path::new(&config))?;
    let (source_url, v_url) = cfg.venue_urls()?;
    let ctx = std::sync::Arc::new(mirror_hyperliquid::Ctx::new(cfg, &source_url, &v_url)?);

    mirror_hyperliquid::driver::boot(&ctx).await?;
    let http = tokio::spawn(mirror_hyperliquid::http::serve(ctx.clone()));
    mirror_hyperliquid::driver::run(ctx).await?;
    http.await.map_err(|e| {
        mirror_hyperliquid::Error::Io {
            what: "http task",
            path: "http".into(),
            source: std::io::Error::other(e),
        }
    })?
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("MIRROR_LOG_JSON").map(|v| v != "0").unwrap_or(false);
    if json {
        fmt().with_env_filter(filter).json().init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}

/// Print the error and its full cause chain: the typed sources are the point
/// (the operator sees the boundary that failed, not a flattened string).
fn report(e: &dyn std::error::Error) {
    eprintln!("Error: {e}");
    let mut source = e.source();
    while let Some(s) = source {
        eprintln!("  caused by: {s}");
        source = s.source();
    }
}
