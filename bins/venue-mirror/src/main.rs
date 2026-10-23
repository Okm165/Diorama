//! The Venue Mirror engine binary — the `venue` profile's entrypoint.
//!
//! Boot order (plan §7.2): wait for anvil V, assert the chain, reconcile the
//! registry (reset detection + the epoch rule), run the at-pin probes, then
//! serve the HTTP surface and the driver loop. A boot fault is fatal: a driver
//! over the wrong chain or a broken registry must not serve.
//!
//! Thin shell by design: the runtime is built here, the story lives in
//! `mirror_hyperliquid`.
use mirror_config::boot;
use std::process::ExitCode;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(boot::WORKER_STACK_SIZE)
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            boot::report_error(&e);
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        match run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                boot::report_error(&e);
                ExitCode::FAILURE
            }
        }
    })
}

async fn run() -> mirror_hyperliquid::Result<()> {
    boot::init_logging();
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


