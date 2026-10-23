//! The CCTP bridge binary — the third element's service (plan §15.4).
//!
//! Thin shell by design, the same shell as the engines: the runtime and the
//! boot order (instance, wiring asserts, the attester install, the watchers,
//! the relayer, the two listeners) live in `mirror_cctp::main_entry`. The
//! other services share the common crates with it and never each other (R3).
use mirror_config::boot;
use std::process::ExitCode;

fn main() -> ExitCode {
    boot::run_main(mirror_cctp::main_entry(std::env::args().skip(1).collect()))
}
