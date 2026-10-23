//! The Venue Simulator binary — the `venue` profile's market + account plane.
//!
//! Thin shell by design, the same shell as every other service binary: the
//! runtime and the boot order live in `mirror_venue::sim_entry`. The services share the
//! common crates and never each other (R3).
use mirror_config::boot;
use std::process::ExitCode;

fn main() -> ExitCode {
    boot::run_main(mirror_venue::sim_entry(std::env::args().skip(1).collect()))
}
