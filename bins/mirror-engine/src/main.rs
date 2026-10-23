//! The Mainnet Mirror engine binary — the `ethereum` profile's entrypoint.
//!
//! Thin shell by design: the runtime is built with the measured worker stack
//! budget, then `mirror_ethereum::main_entry` owns the subcommands (`run`,
//! `selftest`, `rewind`). The Venue Mirror is a sibling binary over its own
//! project crate (R3); the two share the common crates and never each other.
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
        match mirror_ethereum::main_entry(std::env::args().skip(1).collect()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                boot::report_error(&e);
                ExitCode::FAILURE
            }
        }
    })
}
