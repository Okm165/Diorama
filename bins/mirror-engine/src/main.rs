//! The Mainnet Mirror engine binary — the `ethereum` profile's entrypoint.
//!
//! Thin shell by design: the runtime is built with the measured worker stack
//! budget, then `mirror_ethereum::main_entry` owns the subcommands (`run`,
//! `selftest`, `rewind`). The Venue Mirror is a sibling binary over its own
//! project crate (R3); the two share the common crates and never each other.
use std::process::ExitCode;

/// Worker stack budget for the engine runtime.
///
/// Debug builds give every by-value 128 KiB `Blob` temporary its own stack
/// slot: the blob/KZG chain alone needs ~2.1 MiB (measured: `repair_proofs`
/// 1.28 MiB + `validate_sidecars` 0.64 MiB, `encode_blob_tx` 1.29 MiB), which
/// is exactly tokio's 2 MiB worker default — the first blob block overflowed
/// it. The engine owns an explicit budget with headroom; release builds use a
/// fraction of it, and the cost is address space, not resident memory.
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
        match mirror_ethereum::main_entry(std::env::args().skip(1).collect()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                report(&e);
                ExitCode::FAILURE
            }
        }
    })
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
