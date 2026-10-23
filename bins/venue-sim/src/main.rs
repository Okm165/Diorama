//! The Venue Simulator binary — the `venue` profile's market + account plane.
//!
//! Thin shell by design, the same shell as `venue-engine`: the runtime and
//! the boot order live in `mirror_hyperliquid::sim_entry` (the venue crate's
//! second service entry). The engine binaries, the simulator and the CCTP
//! bridge share the common crates and never each other (R3).
use mirror_config::boot;
use std::process::ExitCode;

fn main() -> ExitCode {
    boot::run_main(mirror_hyperliquid::sim_entry(std::env::args().skip(1).collect()))
}
