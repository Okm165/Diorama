//! The binaries' shared boot conventions: the logging init (the workspace's
//! one env read besides the loader's), the measured worker stack budget, and
//! the fatal-error report.
//!
//! One home so the four binaries (`diorama-mainnet-engine`,
//! `diorama-venue-engine`, `diorama-venue-sim`, `diorama-cctp-bridge`) cannot drift: the measured facts below are the
//! engine's, and the others inherit them deliberately — one budget for every
//! binary is one thing to know.
use std::error::Error as StdError;

/// Worker stack budget for every binary's tokio runtime.
///
/// Debug builds give every by-value temporary its own stack slot: the removed
/// blob/KZG pipeline (M's replay) needed ~2.1 MiB there (measured:
/// `repair_proofs` 1.28 MiB + `validate_sidecars` 0.64 MiB, `encode_blob_tx`
/// 1.29 MiB) — exactly tokio's 2 MiB worker default, so the first blob block
/// overflowed it. The pipeline is gone (the BAL carries every effect), but
/// the budget stays: it costs address space, not resident memory, and one
/// by-value block type in a future path would overflow the default again in
/// debug builds.
pub(crate) const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

/// The budget must keep headroom over the measured ~2.1 MiB debug-build chain
/// (a compile-time fact, not a runtime test).
const _: () = assert!(WORKER_STACK_SIZE >= 8 * 1024 * 1024);

/// Install the tracing subscriber. `DIORAMA_LOG_JSON` selects the JSON form
/// (any value but `0`); `RUST_LOG` filters (default `info`).
///
/// This is the one env read outside the config loader — a *logging* concern,
/// not configuration — and it lives here because every binary must read it
/// the same way (the loader's rule, applied to the boot step).
pub fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("DIORAMA_LOG_JSON").map(|v| v != "0").unwrap_or(false);
    if json {
        fmt().with_env_filter(filter).json().init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}

/// Print a fatal error and its full cause chain: the typed sources are the
/// point (the operator sees the boundary that failed, not a flattened
/// string).
pub(crate) fn report_error(error: &dyn StdError) {
    eprintln!("Error: {error}");
    let mut source = error.source();
    while let Some(s) = source {
        eprintln!("  caused by: {s}");
        source = s.source();
    }
}

/// The default instance path every service boots from when `--config` is not
/// given.
pub const DEFAULT_INSTANCE_PATH: &str = "/etc/mirror/instance.toml";

/// The shared service CLI: `run [--config <path>]`. Every service binary has
/// exactly one mode today and the instance file is its one flag, so the shape
/// is parsed here — one definition, and the four bins cannot drift apart in
/// usage. (The two engines' richer CLIs — `run` / `selftest`, plus the
/// mainnet's `rewind` — keep their own clap subcommands on top of the same
/// `run --config` shape, parsed through [`parse_cli`].)
pub(crate) fn service_config(args: &[String]) -> Result<std::path::PathBuf, String> {
    let mut rest = args.iter().map(String::as_str);
    match rest.next() {
        Some("run") => {}
        other => return Err(format!("usage: run [--config <path>] (got {other:?})")),
    }
    let mut config = std::path::PathBuf::from(DEFAULT_INSTANCE_PATH);
    while let Some(arg) = rest.next() {
        match arg {
            "--config" => match rest.next() {
                Some(path) => config = std::path::PathBuf::from(path),
                None => return Err("usage: run [--config <path>] (--config needs a path)".into()),
            },
            other => return Err(format!("usage: run [--config <path>] (unexpected {other:?})")),
        }
    }
    Ok(config)
}

/// The plain services' entry: [`service_config`], with a malformed invocation
/// printing the usage on stderr and exiting 2 — the one place the exit code
/// and the stream are decided, so the bins cannot drift apart on it.
pub fn service_config_or_exit(args: &[String]) -> std::path::PathBuf {
    match service_config(args) {
        Ok(config) => config,
        Err(usage) => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}

/// Parse a subcommand CLI from the raw args (the engines' shell): the binary
/// name is restored so `--help` and clap's errors name the service, never the
/// argv[0] path. The subcommands themselves are the owning crate's (the
/// engines' `run`/`selftest`, the mainnet's `rewind`); this is only the parse
/// shell every subcommand CLI shares.
pub fn parse_cli<T: clap::Parser>(name: &str, args: Vec<String>) -> T {
    T::parse_from(std::iter::once(name.to_string()).chain(args))
}

/// The selftest verdict is the process's exit code (the k8s Jobs read it):
/// one place, so the engines' selftest commands cannot drift on it.
pub fn exit_verdict(ok: bool) -> ! {
    std::process::exit(if ok { 0 } else { 1 })
}

/// The shared `main()` shell: build the runtime with the measured worker-stack
/// budget, run the service future, report a fatal error with its cause chain.
/// The service future is the owning crate's entry (`main_entry`, or the
/// second service's `<service>_entry`) — the planes, boot orders and final
/// saves are the crates'; this is only the runtime shell every bin shares.
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
