//! The Venue Mirror engine binary — the `venue` profile's entrypoint.
//!
//! Thin shell by design, the same shell as every other service binary: the
//! runtime and the boot order live in `diorama_venue::main_entry`. The services share the
//! common crates and never each other (R3).
use diorama_config::boot;
use std::process::ExitCode;

fn main() -> ExitCode {
    boot::run_main(diorama_venue::main_entry(std::env::args().skip(1).collect()))
}
