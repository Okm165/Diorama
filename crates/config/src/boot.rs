//! The binaries' shared boot conventions: the logging init (the workspace's
//! one env read besides the loader's), the measured worker stack budget, and
//! the fatal-error report.
//!
//! One home so the four binaries (`mirror-engine`, `venue-mirror`,
//! `venue-sim`, `cctp-bridge`) cannot drift: the measured facts below are the
//! engine's, and the others inherit them deliberately — one budget for every
//! binary is one thing to know.
use std::error::Error as StdError;

/// Worker stack budget for every binary's tokio runtime.
///
/// Debug builds give every by-value 128 KiB `Blob` temporary its own stack
/// slot: the blob/KZG chain alone needs ~2.1 MiB (measured: `repair_proofs`
/// 1.28 MiB + `validate_sidecars` 0.64 MiB, `encode_blob_tx` 1.29 MiB), which
/// is exactly tokio's 2 MiB worker default — the first blob block overflowed
/// it. The binaries own an explicit budget with headroom; release builds use
/// a fraction of it, and the cost is address space, not resident memory.
pub const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

/// The budget must keep headroom over the measured ~2.1 MiB blob/KZG chain
/// (debug builds) — a compile-time fact, not a runtime test.
const _: () = assert!(WORKER_STACK_SIZE >= 8 * 1024 * 1024);

/// Install the tracing subscriber. `MIRROR_LOG_JSON` selects the JSON form
/// (any value but `0`); `RUST_LOG` filters (default `info`).
///
/// This is the one env read outside the config loader — a *logging* concern,
/// not configuration — and it lives here because every binary must read it
/// the same way (the loader's rule, applied to the boot step).
pub fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("MIRROR_LOG_JSON").map(|v| v != "0").unwrap_or(false);
    if json {
        fmt().with_env_filter(filter).json().init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}

/// Print a fatal error and its full cause chain: the typed sources are the
/// point (the operator sees the boundary that failed, not a flattened
/// string).
pub fn report_error(error: &dyn StdError) {
    eprintln!("Error: {error}");
    let mut source = error.source();
    while let Some(s) = source {
        eprintln!("  caused by: {s}");
        source = s.source();
    }
}

/// The default instance path every service boots from when no argument is
/// given (`--config` for the engine; the first positional for the others).
pub const DEFAULT_INSTANCE_PATH: &str = "/etc/mirror/instance.toml";

/// The shared `main()` shell: build the runtime with the measured worker-stack
/// budget, run the service future, report a fatal error with its cause chain.
/// The bins keep their own `run()` — the planes, boot orders and final saves
/// are theirs; this is only the runtime shell they all share.
pub fn run_main<F, E>(service: F) -> std::process::ExitCode
where
    F: std::future::Future<Output = Result<(), E>>,
    E: StdError,
{
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_SIZE)
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            report_error(&e);
            return std::process::ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        match service.await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                report_error(&e);
                std::process::ExitCode::FAILURE
            }
        }
    })
}

/// Wait for a shutdown signal: SIGINT (Ctrl-C) **or SIGTERM**.
///
/// Kubernetes sends SIGTERM on pod termination (then SIGKILL after the grace
/// period), so a `tokio::signal::ctrl_c`-only wait — which listens for SIGINT
/// — would never fire under a rollout and the graceful path would be dead
/// code in production. Measured 2026-10-23: no service log had ever shown its
/// "shutdown signal received" line, because every `kill` is SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler unavailable; SIGINT still shuts down");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Wall-clock milliseconds since the Unix epoch — the shared clock. One
/// definition for the sim's served time, the feed bridge's publish deadlines
/// and the CCTP store's timestamps; its callers compare deltas and ordering,
/// never the absolute value.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_error_walks_the_whole_chain() {
        #[derive(Debug)]
        struct Layer(Option<Box<dyn StdError + Send + Sync>>);
        impl std::fmt::Display for Layer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "layer")
            }
        }
        impl StdError for Layer {
            fn source(&self) -> Option<&(dyn StdError + 'static)> {
                self.0.as_ref().map(|b| b.as_ref() as &(dyn StdError + 'static))
            }
        }
        // The chain walks without panicking; the print goes to stderr.
        let root = Layer(None);
        let outer = Layer(Some(Box::new(root)));
        report_error(&outer);
    }
}
