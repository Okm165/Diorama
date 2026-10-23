//! The CCTP bridge binary — the cross-mirror bridge's service (plan §15.4).
//!
//! Thin shell by design, the same shell as every other service binary: the
//! runtime and the boot order live in `mirror_cctp::main_entry`. The services share the
//! common crates and never each other (R3).
use mirror_config::boot;
use std::process::ExitCode;

fn main() -> ExitCode {
    boot::run_main(mirror_cctp::main_entry(std::env::args().skip(1).collect()))
}
