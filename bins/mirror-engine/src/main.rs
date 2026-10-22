//! Thin shell: selects the mirror profile and hands the remaining arguments to it.
//!
//! `--profile ethereum` runs the Mainnet Mirror engine (today's behavior);
//! `--profile hyperliquid` lands in R3.
use clap::{Parser, ValueEnum};
use std::process::ExitCode;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Profile {
    /// The Mainnet Mirror engine.
    Ethereum,
    /// The Venue Mirror engine (lands in R3).
    Hyperliquid,
}

#[derive(Parser)]
#[command(name = "mirror-engine", version, about)]
struct Cli {
    /// Which mirror's engine to run.
    #[arg(long, value_enum, default_value = "ethereum")]
    profile: Profile,
    /// The profile's own arguments (subcommand and flags).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

/// The shell's own failures (the profile router's vocabulary).
#[derive(Debug, thiserror::Error)]
enum ShellError {
    #[error("the hyperliquid profile lands in R3")]
    HyperliquidNotYet,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.profile {
        Profile::Ethereum => match mirror_ethereum::main_entry(cli.args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                report(&e);
                ExitCode::FAILURE
            }
        },
        Profile::Hyperliquid => {
            report(&ShellError::HyperliquidNotYet);
            ExitCode::FAILURE
        }
    }
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
