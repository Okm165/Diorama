//! Thin shell: selects the mirror profile and hands the remaining arguments to it.
//!
//! `--profile ethereum` runs the Mainnet Mirror engine (today's behavior);
//! `--profile hyperliquid` lands in R3.
use clap::{Parser, ValueEnum};

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.profile {
        Profile::Ethereum => mirror_ethereum::main_entry(cli.args).await,
        Profile::Hyperliquid => anyhow::bail!("the hyperliquid profile lands in R3"),
    }
}
