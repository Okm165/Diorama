//! The mirrors' configuration: one grammar, three layers (plan §19).
//!
//! `config/defaults.toml` (global, embedded at build time) <-
//! `config/instances/<name>.toml` (one deployment, mounted) <-
//! `MIRROR__*` environment overrides (operator debug tooling, logged loudly).
//! The merge is a deep merge; arrays replace, never concatenate; unknown keys
//! are errors at every level.
//!
//! `${VAR}` references in string values resolve from the environment at load
//! time (deploy-time wiring, §19.4). [`Instance::load`] is the service entry
//! and fails on an unresolved reference, naming the variable;
//! [`Instance::inspect`] is the tooling entry (`mirror-config`) and reports
//! unresolved wiring instead of failing on it.
//!
//! The loader is the only environment reader: services never call
//! `std::env::var` themselves ([`boot::init_logging`] is the one exception,
//! and it lives here for exactly that reason).

pub mod boot;
mod env;
mod instance;
mod sections;

pub use instance::{
    ChainKind, ChainL1, ChainVenue, Chains, FollowHead, Instance, InstanceMeta, Profile, WatchEntry,
};
pub use sections::{
    BlobsSection, CctpSection, EngineAnvil, EngineReth, EngineSection, FeedBridgeSection,
    FeedSection, HttpSection, JournalSection, LoopsSection, OnDivergence, Probe, RelayerMode,
    Services, SimSection, SourceSyncSection, VenueSection, VerifierSection,
};

use std::path::Path;

use serde::Deserialize;

/// The workspace defaults: the single home of every shared number. Embedded
/// so services and the CLI read the same file without a second mount.
pub const DEFAULTS: &str = include_str!("../../../config/defaults.toml");

/// Rejected at the load point (no partial config ever escapes).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {what}")]
    Parse {
        what: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid configuration:\n  - {}", issues.join("\n  - "))]
    Invalid { issues: Vec<String> },
}

impl Instance {
    /// Service entry: strict env expansion + cross-validation. A missing
    /// `${VAR}` or a failed check is a boot error naming the key.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let vars: Vec<(String, String)> = std::env::vars().collect();
        let (instance, unresolved) = load_merged(path, &vars)?;
        if !unresolved.is_empty() {
            return Err(ConfigError::Invalid {
                issues: unresolved
                    .iter()
                    .map(|v| format!("unresolved ${{{v}}} - deploy-time wiring must set it"))
                    .collect(),
            });
        }
        instance.check()?;
        Ok(instance)
    }

    /// Tooling entry (`mirror-config check|get|export-env`): the instance is
    /// returned even when `${VAR}` references are unresolved (the deployment
    /// environment is not present here); the names are reported for the
    /// caller to surface. Structural checks still run.
    pub fn inspect(path: &Path) -> Result<(Self, Vec<String>), ConfigError> {
        let vars: Vec<(String, String)> = std::env::vars().collect();
        let (instance, unresolved) = load_merged(path, &vars)?;
        instance.check()?;
        Ok((instance, unresolved))
    }
}

/// The layered pipeline: parse both files, deep-merge, expand `${VAR}`,
/// apply `MIRROR__*` overrides, deserialize into the typed instance.
fn load_merged(
    path: &Path,
    vars: &[(String, String)],
) -> Result<(Instance, Vec<String>), ConfigError> {
    let src = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut merged: toml::Value = toml::from_str(DEFAULTS).map_err(|source| ConfigError::Parse {
        what: "config/defaults.toml".into(),
        source,
    })?;
    let over: toml::Value = toml::from_str(&src).map_err(|source| ConfigError::Parse {
        what: path.display().to_string(),
        source,
    })?;
    // The defaults carry every profile's section tree (the workspace
    // conventions of each service). A load sees only the sections its own
    // profile owns: without this, a venue instance would inherit the
    // ethereum engine's stub and fail deserialization on the missing fields
    // (and vice versa).
    prune_other_profile(&mut merged, profile_of(&over));
    // Optional services: the defaults hold their shared numbers, but a
    // deployment that does not run the service must not inherit a
    // half-filled stub (the section would deserialize as present-but-broken).
    prune_absent_services(&mut merged, &over);
    merge(&mut merged, over);
    let unresolved = env::expand(&mut merged, vars);
    env::apply_overrides(&mut merged, vars)?;
    let instance = Instance::deserialize(merged).map_err(|source| ConfigError::Parse {
        what: format!("{} (merged with config/defaults.toml)", path.display()),
        source,
    })?;
    Ok((instance, unresolved))
}

/// The profile an instance file declares, if it declares one (a missing
/// profile is left to the deserializer, which names it).
fn profile_of(instance_value: &toml::Value) -> Option<&str> {
    instance_value
        .get("instance")?
        .get("profile")?
        .as_str()
}

/// Drop the sections the profile does not own from the defaults layer.
fn prune_other_profile(defaults: &mut toml::Value, profile: Option<&str>) {
    let (chain_section, service_sections): (&str, &[&str]) = match profile {
        Some("ethereum") => ("venue", &["venue", "sim"]),
        Some("venue") => ("l1", &["engine"]),
        _ => return,
    };
    let Some(root) = defaults.as_table_mut() else {
        return;
    };
    if let Some(chains) = root.get_mut("chains").and_then(|v| v.as_table_mut()) {
        chains.remove(chain_section);
    }
    if let Some(services) = root.get_mut("services").and_then(|v| v.as_table_mut()) {
        for section in service_sections {
            services.remove(*section);
        }
    }
}

/// Optional services (not pinned by the profile): the defaults' stub is kept
/// only when the instance file carries the section. The simulator is optional
/// (a venue deployment may run only the engine); the CCTP bridge is optional
/// under either profile (the third element is its own deployment, §15).
fn prune_absent_services(defaults: &mut toml::Value, over: &toml::Value) {
    for section in ["sim", "cctp"] {
        let present = over
            .get("services")
            .and_then(|s| s.get(section))
            .is_some();
        if present {
            continue;
        }
        if let Some(services) = defaults.get_mut("services").and_then(|v| v.as_table_mut()) {
            services.remove(section);
        }
    }
}

/// Deep merge: tables merge key by key; everything else (scalars, arrays)
/// replaces. Arrays never concatenate - an override is the whole list.
fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(base), toml::Value::Table(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(slot) => merge(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, over) => *slot = over,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The wiring the mainnet-mirror instance references (its deploy-time env).
    fn wiring() -> Vec<(String, String)> {
        vars(&[
            ("RETH_URL", "http://reth:8545"),
            ("ANVIL_M_URL", "http://anvil-m:8545"),
            ("ANVIL_T_URL", "http://anvil-t:8545"),
            ("BEACON_URL", "http://lighthouse:5052"),
            ("ENGINE_API_URL", "http://reth:8551"),
            ("JWT_PATH", "/secrets/jwt.hex"),
            ("STATE_DIR", "/state"),
        ])
    }

    fn minimal_instance() -> &'static str {
        r#"
watchlist = []

[instance]
name = "t"
namespace = "t"
profile = "ethereum"

[chains.l1]
kind = "ethereum"
source_url = "http://reth:8545"
follow = "latest"
fork_offset = 128

[services.engine]
state_dir = "/state"

[services.engine.anvil]
m_url = "http://anvil-m:8545"
t_url = "http://anvil-t:8545"

[services.engine.source_sync]
engine_url = "http://reth:8551"
jwt_path = "/secrets/jwt.hex"

[services.engine.journal]
dir = "/state/journal"

[services.engine.blobs]
beacon_urls = ["http://lighthouse:5052"]

[services.engine.verifier]
probes = []

[services.engine.http]
port = 8080
"#
    }

    fn load_str(src: &str, vars: &[(String, String)]) -> Result<(Instance, Vec<String>), ConfigError> {
        let over: toml::Value = toml::from_str(src).unwrap();
        let mut merged: toml::Value = toml::from_str(DEFAULTS).unwrap();
        // Same pipeline as the real loader, pruning included.
        prune_other_profile(&mut merged, profile_of(&over));
        prune_absent_services(&mut merged, &over);
        merge(&mut merged, over);
        let unresolved = env::expand(&mut merged, vars);
        env::apply_overrides(&mut merged, vars)?;
        let instance = Instance::deserialize(merged).map_err(|source| ConfigError::Parse {
            what: "test".into(),
            source,
        })?;
        Ok((instance, unresolved))
    }

    /// The ethereum sections, for the tests' direct field reads.
    fn l1(inst: &Instance) -> &ChainL1 {
        inst.chains.l1.as_ref().expect("[chains.l1]")
    }

    fn engine(inst: &Instance) -> &EngineSection {
        inst.services.engine.as_ref().expect("[services.engine]")
    }

    fn minimal_venue_instance() -> &'static str {
        r#"
watchlist = []

[instance]
name = "v"
namespace = "v"
profile = "venue"

[chains.venue]
kind = "hyperliquid"
source_url = "http://hl-rpc:8545"
api_url = "https://api.hyperliquid.xyz"
ws_url = "wss://api.hyperliquid.xyz/ws"

[services.venue]
state_dir = "/state"
anvil_url = "http://anvil-v:8545"
probes = []

[services.venue.http]
port = 8080
"#
    }

    #[test]
    fn defaults_and_instance_deep_merge() {
        let (inst, unresolved) = load_str(minimal_instance(), &wiring()).unwrap();
        assert!(unresolved.is_empty());
        // From defaults.toml:
        assert_eq!(engine(&inst).loops.poll_interval_ms, 1000);
        assert_eq!(l1(&inst).follow_margin_blocks, 2);
        assert_eq!(engine(&inst).journal.blocks, 1024);
        // From the instance:
        assert_eq!(l1(&inst).fork_offset, 128);
        assert_eq!(engine(&inst).http.port, 8080);
    }

    #[test]
    fn arrays_replace_never_concatenate() {
        let src = minimal_instance().replace(
            r#"beacon_urls = ["http://lighthouse:5052"]"#,
            r#"beacon_urls = ["http://a:1", "http://b:2"]"#,
        );
        let (inst, _) = load_str(&src, &wiring()).unwrap();
        assert_eq!(engine(&inst).blobs.beacon_urls.len(), 2);
    }

    #[test]
    fn cctp_relayer_mode_defaults_to_auto_and_the_instance_can_drive_it_manually() {
        let src = minimal_venue_instance().replace(
            "[services.venue.http]",
            r#"[services.cctp]
state_dir = "/state/cctp"
t_url = "http://t:8545"
v_url = "http://v:8545"
relay_key = "0x11"
attester_seed = "seed"

[services.cctp.iris]
port = 8095

[services.cctp.control]
port = 8096

[services.venue.http]"#,
        );
        let (inst, _) = load_str(&src, &wiring()).unwrap();
        let cctp = inst.services.cctp.as_ref().expect("[services.cctp]");
        assert_eq!(cctp.relayer, RelayerMode::Auto, "defaults.toml drives the mode");
        assert_eq!(cctp.poll_interval_ms, 1000, "shared defaults merge in");

        let src = src.replace(
            "attester_seed = \"seed\"",
            "attester_seed = \"seed\"\nrelayer = \"manual\"",
        );
        let (inst, _) = load_str(&src, &wiring()).unwrap();
        assert_eq!(
            inst.services.cctp.as_ref().unwrap().relayer,
            RelayerMode::Manual,
            "the instance can drive relays by hand"
        );
    }

    #[test]
    fn unresolved_env_is_reported_then_fatal_for_services() {
        let (_, unresolved) = load_str(minimal_instance(), &[]).unwrap();
        assert!(unresolved.is_empty(), "no refs in the minimal instance");
        let src = minimal_instance().replace("http://reth:8545", "${RETH_URL}");
        let (inst, unresolved) = load_str(&src, &[]).unwrap();
        assert_eq!(unresolved, vec!["RETH_URL".to_string()]);
        assert_eq!(l1(&inst).source_url, "${RETH_URL}", "raw ref kept");
        let (inst2, unresolved2) = load_str(&src, &wiring()).unwrap();
        assert!(unresolved2.is_empty());
        assert_eq!(l1(&inst2).source_url, "http://reth:8545");
    }

    #[test]
    fn mirror_overrides_apply_and_unknown_paths_fail() {
        let (inst, _) = load_str(
            minimal_instance(),
            &vars(&[("MIRROR__SERVICES__ENGINE__LOOPS__POLL_INTERVAL_MS", "2000")]),
        )
        .unwrap();
        assert_eq!(engine(&inst).loops.poll_interval_ms, 2000);
        let err = load_str(
            minimal_instance(),
            &vars(&[("MIRROR__SERVICES__ENGINE__LOOPS__POL_INTERVAL_MS", "2000")]),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("POL_INTERVAL_MS"), "{err}");
    }

    #[test]
    fn check_flags_fork_offset_bounds() {
        let (mut inst, _) = load_str(minimal_instance(), &wiring()).unwrap();
        inst.chains.l1.as_mut().unwrap().fork_offset = 32;
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("fork_offset"), "{err}");
        inst.chains.l1.as_mut().unwrap().fork_offset = 20_000;
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("state window"), "{err}");
    }

    #[test]
    fn check_flags_secret_shaped_jwt_path_and_bad_urls() {
        let (mut inst, _) = load_str(minimal_instance(), &wiring()).unwrap();
        let engine = inst.services.engine.as_mut().unwrap();
        engine.source_sync.jwt_path = Some("/secrets/jwt.hex".into());
        inst.check().unwrap();
        let engine = inst.services.engine.as_mut().unwrap();
        engine.source_sync.jwt_path = Some(format!("0x{}", "ab".repeat(32)).into());
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("jwt_path"), "{err}");
        let engine = inst.services.engine.as_mut().unwrap();
        engine.source_sync.jwt_path = Some("/secrets/jwt.hex".into());
        engine.anvil.m_url = "anvil-m:8545".into();
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("m_url"), "{err}");
    }

    #[test]
    fn the_real_mainnet_mirror_instance_loads_and_checks() {        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/instances/mainnet-mirror.toml"
        ));
        let (inst, unresolved) = Instance::inspect(path).unwrap();
        assert_eq!(inst.instance.name, "mainnet-mirror");
        // 47 market/protocol entries + the dapp's 41-address mainnet
        // deployment (contracts/deployments/ethereum.json; measured complete
        // 2026-10-23). Update this count deliberately when the audit changes.
        assert_eq!(inst.watchlist.len(), 88, "the audited watchlist is complete");
        assert_eq!(l1(&inst).fork_offset, 128);
        assert_eq!(engine(&inst).verifier.probes.len(), 11);
        let mut names = unresolved.clone();
        names.sort();
        assert_eq!(
            names,
            vec![
                "ANVIL_M_URL", "ANVIL_T_URL", "BEACON_URL", "ENGINE_API_URL", "JWT_PATH",
                "RETH_URL", "STATE_DIR"
            ],
            "the wiring the instance references"
        );
    }

    #[test]
    fn the_venue_instance_loads_checks_and_takes_its_sections() {
        let (mut inst, unresolved) = load_str(minimal_venue_instance(), &[]).unwrap();
        assert!(unresolved.is_empty());
        // From defaults.toml (the venue service's shared conventions):
        assert_eq!(
            inst.services.venue.as_ref().unwrap().poll_interval_ms,
            1000
        );
        assert_eq!(inst.services.venue.as_ref().unwrap().chunk_size, 500);
        // The ethereum engine's stub was pruned before the merge: no
        // [services.engine] on a venue load.
        assert!(inst.services.engine.is_none());
        assert!(inst.chains.l1.is_none());
        inst.check().unwrap();
        let (chain, venue) = inst.take_venue().unwrap();
        assert_eq!(chain.source_url, "http://hl-rpc:8545");
        assert_eq!(venue.anvil_url, "http://anvil-v:8545");
        // And the sections are gone after the take (moved, not copied).
        assert!(inst.take_venue().is_err());
    }

    #[test]
    fn profile_coherence_is_enforced() {
        // A venue instance carrying ethereum sections:
        let src = format!(
            "{}\n[chains.l1]\nkind = \"ethereum\"\nsource_url = \"http://reth:8545\"\nfollow = \"latest\"\nfollow_margin_blocks = 2\nfork_offset = 128\n",
            minimal_venue_instance()
        );
        let (inst, _) = load_str(&src, &[]).unwrap();
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("must not carry [chains.l1]"), "{err}");

        // An ethereum instance carrying venue sections:
        let src = format!(
            "{}\n[chains.venue]\nkind = \"hyperliquid\"\nsource_url = \"http://hl-rpc:8545\"\napi_url = \"https://api.hyperliquid.xyz\"\nws_url = \"wss://api.hyperliquid.xyz/ws\"\n",
            minimal_instance()
        );
        let (inst, _) = load_str(&src, &wiring()).unwrap();
        let err = inst.check().unwrap_err();
        assert!(
            format!("{err}").contains("must not carry [chains.venue]"),
            "{err}"
        );

        // An ethereum instance carrying the simulator's section:
        let src = format!(
            "{}\n[services.sim]\nstate_dir = \"/sim\"\nmarkets = [\"BTC\"]\npoll_interval_ms = 5000\n[services.sim.info]\naddr = \"0.0.0.0\"\nport = 8090\n[services.sim.control]\naddr = \"0.0.0.0\"\nport = 8091\n",
            minimal_instance()
        );
        let (inst, _) = load_str(&src, &wiring()).unwrap();
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("must not carry [services.sim]"), "{err}");
    }

    #[test]
    fn the_simulator_sections_load_and_take() {
        let src = format!(
            "{}\n[services.sim]\nstate_dir = \"/sim\"\nmarkets = [\"BTC\", \"ETH\"]\npoll_interval_ms = 5000\n[services.sim.info]\nport = 8090\n[services.sim.control]\nport = 8091\n",
            minimal_venue_instance()
        );
        let (mut inst, _) = load_str(&src, &[]).unwrap();
        inst.check().unwrap();
        let (chain, sim) = inst.take_sim().unwrap();
        assert_eq!(chain.api_url, "https://api.hyperliquid.xyz");
        assert_eq!(sim.markets, vec!["BTC", "ETH"]);
        assert_eq!(sim.info.port, 8090);
        assert_eq!(sim.control.port, 8091);
        assert!(sim.engine_admin_url.is_none());
        assert!(inst.take_sim().is_err(), "the sections are moved, not copied");
        // The engine's take still works on an instance that also carries the sim.
        let (mut inst2, _) = load_str(&src, &[]).unwrap();
        let (_, venue) = inst2.take_venue().unwrap();
        assert_eq!(venue.anvil_url, "http://anvil-v:8545");
    }

    #[test]
    fn the_venue_instance_file_loads() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/instances/venue-mirror.toml"
        ));
        let (inst, unresolved) = Instance::inspect(path).unwrap();
        assert_eq!(inst.instance.name, "venue-mirror");
        assert_eq!(inst.instance.profile, Profile::Venue);
        assert_eq!(inst.chains.venue.as_ref().unwrap().kind, ChainKind::Hyperliquid);
        let mut names = unresolved.clone();
        names.sort();
        assert_eq!(
            names,
            vec![
                "ANVIL_V_URL",
                "CCTP_RELAY_KEY",
                "CCTP_STATE_DIR",
                "ENGINE_ADMIN_URL",
                "HL_RPC_URL",
                "SIM_STATE_DIR",
                "STATE_DIR",
                "T_RPC_URL"
            ],
            "the wiring the venue instance references"
        );
    }

    /// `[services.cctp]` is optional under both profiles and cross-cutting:
    /// a venue instance may carry it (and take it), and an ethereum instance
    /// may too — unlike the simulator, which the ethereum profile forbids.
    #[test]
    fn the_cctp_section_is_optional_and_cross_cutting() {
        let src = format!(
            "{}\n[services.cctp]\nstate_dir = \"/cctp\"\nt_url = \"http://anvil-t:8545\"\nv_url = \"http://anvil-v:8545\"\nrelay_key = \"0x{}\"\nattester_seed = \"devnet\"\n[services.cctp.iris]\nport = 8095\n[services.cctp.control]\nport = 8096\n",
            minimal_venue_instance(),
            "59".repeat(32)
        );
        let (mut inst, _) = load_str(&src, &[]).unwrap();
        inst.check().unwrap();
        // The defaults filled the shared numbers.
        let cctp = inst.services.cctp.as_ref().unwrap();
        assert_eq!(cctp.poll_interval_ms, 1000);
        assert_eq!(cctp.finality_delay_secs, 0);
        assert_eq!(cctp.expiration_blocks, 0);
        assert_eq!(cctp.scan_chunk_blocks, 2000);
        assert_eq!(cctp.receipt_timeout_secs, 30);
        assert_eq!(cctp.balance_low_wei, U256::from(100_000_000_000_000_000u64));
        assert!(cctp.t_start_block.is_none());
        let section = inst.take_cctp().unwrap();
        assert_eq!(section.iris.port, 8095);
        assert_eq!(section.control.port, 8096);
        assert!(inst.take_cctp().is_err(), "the section is moved, not copied");

        // Absent on a deployment that does not run it (the defaults stub was
        // pruned before the merge).
        let (inst, _) = load_str(minimal_venue_instance(), &[]).unwrap();
        assert!(inst.services.cctp.is_none());

        // The ethereum profile may carry it (the bridge spans both mirrors).
        let src = format!(
            "{}\n[services.cctp]\nstate_dir = \"/cctp\"\nt_url = \"http://anvil-t:8545\"\nv_url = \"http://anvil-v:8545\"\nrelay_key = \"0x{}\"\nattester_seed = \"devnet\"\n[services.cctp.iris]\nport = 8095\n[services.cctp.control]\nport = 8096\n",
            minimal_instance(),
            "59".repeat(32)
        );
        let (inst, _) = load_str(&src, &wiring()).unwrap();
        inst.check().unwrap();

        // The relay key must be a secret, never a path; the seed must be
        // non-empty; the gas thresholds must be ordered.
        let src = format!(
            "{}\n[services.cctp]\nstate_dir = \"/cctp\"\nt_url = \"http://anvil-t:8545\"\nv_url = \"http://anvil-v:8545\"\nrelay_key = \"/etc/mirror/relay.key\"\nattester_seed = \"devnet\"\n[services.cctp.iris]\nport = 8095\n[services.cctp.control]\nport = 8096\n",
            minimal_venue_instance()
        );
        let (inst, _) = load_str(&src, &[]).unwrap();
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("relay_key"), "{err}");
        let src = src.replace("attester_seed = \"devnet\"", "attester_seed = \"\"");
        let (inst, _) = load_str(&src, &[]).unwrap();
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("attester_seed"), "{err}");
    }

    #[test]
    fn design_assertions() {
        // Public types stay Send + Sync + Sized + Unpin (Rust-for-Rustaceans).
        fn is_normal<T: Send + Sync + Sized + Unpin>() {}
        is_normal::<Instance>();
        is_normal::<InstanceMeta>();
        is_normal::<Chains>();
        is_normal::<ChainL1>();
        is_normal::<ChainVenue>();
        is_normal::<EngineSection>();
        is_normal::<VenueSection>();
        is_normal::<SimSection>();
        is_normal::<CctpSection>();
        is_normal::<LoopsSection>();
        is_normal::<VerifierSection>();
        is_normal::<SourceSyncSection>();
        is_normal::<JournalSection>();
        is_normal::<BlobsSection>();
        is_normal::<HttpSection>();
        is_normal::<ConfigError>();
    }
}
