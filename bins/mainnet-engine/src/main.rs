//! The Mainnet Mirror engine binary — the `ethereum` profile's entrypoint.
//!
//! Thin shell by design: the runtime is built with the measured worker stack
//! budget, then `mirror_ethereum::main_entry` owns the subcommands (`run`,
//! `selftest`, `rewind`). The Venue Mirror is a sibling binary over its own
//! project crate (R3); the two share the common crates and never each other.
use mirror_config::boot;
use std::process::ExitCode;

fn main() -> ExitCode {
    boot::run_main(mirror_ethereum::main_entry(std::env::args().skip(1).collect()))
}
