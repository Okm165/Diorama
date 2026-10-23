//! The Venue Mirror engine binary — the `venue` profile's entrypoint.
//!
//! Thin shell by design, the same shell as every other service binary: the
//! runtime and the boot order live in `mirror_venue::main_entry`. The services share the
//! common crates and never each other (R3).
use mirror_config::boot;
use std::process::ExitCode;

fn main() -> ExitCode {
    boot::run_main(mirror_venue::main_entry(std::env::args().skip(1).collect()))
}
