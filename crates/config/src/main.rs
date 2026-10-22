//! `mirror-config`: check | get | export-env (plan §19.5).
//!
//! The single owner of config semantics: the layered loader lives in this
//! package's library; this binary is the scripting surface for it.
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mirror-config", version, about = "The mirrors' configuration CLI")]
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
        /// Section name (known: anvil).
        #[arg(long, default_value = "anvil")]
        section: String,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Check { instance } => {
            let (inst, unresolved) = mirror_config::Instance::inspect(&instance)?;
            println!("{}: ok", inst.instance.name);
            if !unresolved.is_empty() {
                println!("deploy-time wiring (resolved from the pod environment):");
                for var in unresolved {
                    println!("  ${{{var}}}");
                }
            }
        }
        Cmd::Get { instance, path } => {
            let (inst, _) = mirror_config::Instance::inspect(&instance)?;
            println!("{}", render(&inst.get(&path)?));
        }
        Cmd::ExportEnv { instance, section } => {
            let (inst, _) = mirror_config::Instance::inspect(&instance)?;
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
