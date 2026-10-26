//! `diorama-config`: check | get | export-env (`--section client`
//! renders the client-facing endpoint block).
//!
//! The single owner of config semantics: the layered loader lives in this
//! package's library; this binary is the scripting surface for it.
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "diorama-config", version, about = "The mirrors' configuration CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Validate an instance file (structural checks + a deploy-time wiring report).
    Check {
        /// Path to config/instances/<name>.toml.
        instance: PathBuf,
    },
    /// Print one value by dotted path (e.g. `chains.l1.fork_offset`).
    Get {
        /// Path to config/instances/<name>.toml.
        instance: PathBuf,
        /// Dotted path into the instance.
        path: String,
    },
    /// Print shell-ready KEY=VALUE lines for one section.
    ExportEnv {
        /// Path to config/instances/<name>.toml.
        instance: PathBuf,
        /// Section name (known: anvil, daloc).
        #[arg(long, alias = "for", default_value = "anvil")]
        section: String,
    },
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // The config error's Display already renders the issue list; the
            // cause chain is appended when a lower boundary named one.
            eprintln!("Error: {e}");
            let mut source = std::error::Error::source(&e);
            while let Some(s) = source {
                eprintln!("  caused by: {s}");
                source = s.source();
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), diorama_config::ConfigError> {
    match Cli::parse().cmd {
        Cmd::Check { instance } => {
            let (inst, unresolved) = diorama_config::Instance::inspect(&instance)?;
            println!("{}: ok", inst.instance.name);
            if !unresolved.is_empty() {
                println!("deploy-time wiring (resolved from the pod environment):");
                for var in unresolved {
                    println!("  ${{{var}}}");
                }
            }
        }
        Cmd::Get { instance, path } => {
            let (inst, _) = diorama_config::Instance::inspect(&instance)?;
            println!("{}", render(&inst.get(&path)?));
        }
        Cmd::ExportEnv { instance, section } => {
            let (inst, _) = diorama_config::Instance::inspect(&instance)?;
            for line in inst.export_env(&section)? {
                println!("{line}");
            }
        }
    }
    Ok(())
}

fn render(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => toml::to_string_pretty(other).unwrap_or_else(|_| format!("{other:?}")),
    }
}
