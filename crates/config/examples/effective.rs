//! `effective` — render one instance's merged effective config as TOML.
//!
//! The proof instrument for the config model: what the pods would see, the
//! whole layered pipeline (defaults <- specialization <- instance, `${VAR}`
//! expansion, `DIORAMA__*` overrides) applied, serialized. Config identity
//! before/after a delivery change is a diff of this output, never a diff of
//! the source files.
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "effective", about = "Render the merged effective config")]
struct Cli {
    /// Path to config/instances/<name>/instance.toml.
    instance: PathBuf,
}

fn main() -> Result<(), diorama_config::ConfigError> {
    let cli = Cli::parse();
    let (instance, unresolved) = diorama_config::Instance::inspect(&cli.instance)?;
    if !unresolved.is_empty() {
        eprintln!("unresolved deploy-time wiring: {}", unresolved.join(", "));
    }
    print!("{}", toml::to_string_pretty(&instance.to_value()?).expect("serialize"));
    Ok(())
}
