//! The Venue Mirror engine binary — the `venue` profile's entrypoint.
//!
//! Thin shell by design, the same shell as `mainnet-engine`: the runtime and
//! the boot order live in `mirror_hyperliquid::main_entry`. The two engine
//! binaries share the common crates and never each other (R3); the simulator
//! and the CCTP bridge are the venue stack's own services.
use mirror_config::boot;
use std::process::ExitCode;

fn main() -> ExitCode {
    boot::run_main(mirror_hyperliquid::main_entry(std::env::args().skip(1).collect()))
}
