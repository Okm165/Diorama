//! `diorama-config` — the mirrors' configuration: one grammar, three layers.
//!
//! `config/defaults.toml` (global, embedded at build time) <-
//! `config/instances/<name>/instance.toml` (one deployment, mounted) <-
//! `DIORAMA__*` environment overrides (operator debug tooling, logged loudly).
//! The merge is a deep merge; arrays replace, never concatenate; unknown keys
//! are errors at every level.
//!
//! `${VAR}` references in string values resolve from the environment at load
//! time (deploy-time wiring, §19.4). [`Instance::inspect`] is the one entry
//! every service and the tooling share: it reports unresolved wiring instead
//! of failing on it — each service then requires only its own sections by
//! name (`require_resolved` / `require_resolved_logged`; one instance file,
//! many readers).
//!
//! The loader is the only environment reader: services never call
//! `std::env::var` themselves ([`boot::init_logging`] is the one exception,
//! and it lives here for exactly that reason).

pub mod boot;
mod endpoints;
mod env;
mod instance;
mod sections;

pub use endpoints::{EndpointGroup, Endpoints, WsEndpoints};
pub use env::{require_resolved, require_resolved_logged};

pub use instance::{
    ChainKind, ChainL1, ChainVenue, Chains, FollowHead, Instance, InstanceMeta, Profile,
    WatchEntry, RETH_STATE_WINDOW_BLOCKS,
};
pub use sections::{
    BeaconSection, CctpChain, CctpLane, CctpSection, ClosuresSection, EngineAnvil, EngineReth,
    EngineSection, FeedBridgeSection, FeedSection, FixedSlotsSpec, HttpSection, JournalSection,
    LaneFee, LoopsSection, MappingSpec, Probe, RelayerMode, Services, SourceSyncSection, Sweep,
    VenueSimSection, VerifierSection,
};

use std::path::Path;

use serde::Deserialize;

/// The workspace defaults: the single home of every shared number. Embedded
/// so services and the CLI read the same file without a second mount.
pub(crate) const DEFAULTS: &str = include_str!("../../../config/defaults.toml");

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
    /// The service and tooling entry: layered load, `${VAR}` expansion and
    /// structural cross-validation. The instance is returned even when
    /// references are unresolved (the deployment environment resolves each
    /// service's own); the names are reported for the caller to require
    /// per-section (`require_resolved` / `require_resolved_logged`) — one
    /// instance file, many readers.
    pub fn inspect(path: &Path) -> Result<(Self, Vec<String>), ConfigError> {
        let vars: Vec<(String, String)> = std::env::vars().collect();
        let (mut instance, unresolved) = load_merged(path, &vars)?;
        instance.resolve_endpoints()?;
        instance.check()?;
        Ok((instance, unresolved))
    }
}

/// The layered pipeline: parse the instance and its optional project
/// specialization, deep-merge (`defaults <- project <- instance`), expand
/// `${VAR}`, apply `DIORAMA__*` overrides, deserialize into the typed
/// instance.
///
/// The project layer is the generic/project split: `instance.toml` carries
/// deployment facts (identity, profile, paths, endpoint references), while
/// `project = "daloc"` names a `daloc.toml` beside it (gitignored) that
/// carries the product-specific watchlist, fixtures, probes and endpoint
/// groups. A deployment without a project file is a generic mirror.
fn load_merged(
    path: &Path,
    vars: &[(String, String)],
) -> Result<(Instance, Vec<String>), ConfigError> {
    let src = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut merged: toml::Value =
        toml::from_str(DEFAULTS).map_err(|source| ConfigError::Parse {
            what: "config/defaults.toml".into(),
            source,
        })?;
    let over: toml::Value = toml::from_str(&src).map_err(|source| ConfigError::Parse {
        what: path.display().to_string(),
        source,
    })?;
    let project = load_project(path, &over, vars)?;
    // Presence and profile checks see the effective overlay: a project may
    // carry the simulator/CCTP sections, and the instance may override them.
    let mut combined = project
        .clone()
        .unwrap_or(toml::Value::Table(Default::default()));
    merge(&mut combined, over.clone());
    // The defaults carry every profile's section tree (the workspace
    // conventions of each service). A load sees only the sections its own
    // profile owns: without this, a venue instance would inherit the
    // mainnet engine's stub and fail deserialization on the missing fields
    // (and vice versa).
    prune_other_profile(&mut merged, profile_of(&combined));
    // Optional services: the defaults hold their shared numbers, but a
    // deployment that does not run the service must not inherit a
    // half-filled stub (the section would deserialize as present-but-broken).
    prune_absent_services(&mut merged, &combined);
    if let Some(project) = project {
        merge(&mut merged, project);
    }
    merge(&mut merged, over);
    let unresolved = env::expand(&mut merged, vars);
    env::apply_overrides(&mut merged, vars)?;
    let instance = Instance::deserialize(merged).map_err(|source| ConfigError::Parse {
        what: format!("{} (merged with config/defaults.toml)", path.display()),
        source,
    })?;
    Ok((instance, unresolved))
}

/// Load the optional project specialization named by `[instance].project`
/// (or `DIORAMA_PROJECT_FILE`). The path is relative to the instance file;
/// a bare name gains `.toml` (`project = "daloc"` -> `daloc.toml`). A named
/// project that is missing is a boot error, never a silent fallback: the
/// specialization carries addresses and watchlists the mirror must not
/// invent.
fn load_project(
    instance_path: &Path,
    over: &toml::Value,
    vars: &[(String, String)],
) -> Result<Option<toml::Value>, ConfigError> {
    let Some(raw) = project_reference(over, vars) else {
        return Ok(None);
    };
    let path = project_path(instance_path, &raw);
    let src = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let value: toml::Value = toml::from_str(&src).map_err(|source| ConfigError::Parse {
        what: path.display().to_string(),
        source,
    })?;
    Ok(Some(value))
}

/// The project reference: `DIORAMA_PROJECT_FILE` wins, else
/// `[instance].project`. Empty is no project.
fn project_reference(over: &toml::Value, vars: &[(String, String)]) -> Option<String> {
    let from_env = vars
        .iter()
        .find(|(key, _)| key == "DIORAMA_PROJECT_FILE")
        .map(|(_, value)| value.clone());
    let declared = over
        .get("instance")
        .and_then(|instance| instance.get("project"))
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    from_env.or(declared).filter(|raw| !raw.trim().is_empty())
}

/// Resolve the project file beside the instance. A bare name gains `.toml`
/// (`project = "daloc"` -> `daloc.toml`); a relative path is relative to the
/// instance file. A missing live file falls back to the tracked
/// `<name>.example.toml` template (loudly) so a fresh clone's config check
/// works; a deployment that mounts the real file never takes that branch.
fn project_path(instance_path: &Path, raw: &str) -> std::path::PathBuf {
    let raw = if raw.ends_with(".toml") || raw.contains('/') {
        raw.to_string()
    } else {
        format!("{raw}.toml")
    };
    let candidate = Path::new(&raw);
    let path = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        instance_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(candidate)
    };
    if path.exists() {
        return path;
    }
    let example = path.with_file_name(format!(
        "{}.example.toml",
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("project")
    ));
    if example.exists() && !candidate.is_absolute() {
        tracing::warn!(
            project = %path.display(),
            example = %example.display(),
            "project specialization is absent; loading the tracked example template"
        );
        example
    } else {
        path
    }
}

/// The profile an instance file declares, if it declares one (a missing
/// profile is left to the deserializer, which names it).
fn profile_of(instance_value: &toml::Value) -> Option<&str> {
    instance_value.get("instance")?.get("profile")?.as_str()
}

/// Drop the sections the profile does not own from the defaults layer.
fn prune_other_profile(defaults: &mut toml::Value, profile: Option<&str>) {
    // The engine's section is shared by the two mirror profiles; the bridge
    // profile owns no chain, engine or simulator. One process = one role.
    let (chain_sections, service_sections): (&[&str], &[&str]) = match profile {
        Some("mainnet") => (&["venue"], &["venue_sim"]),
        Some("venue") => (&["l1"], &[]),
        Some("cctp") => (&["l1", "venue"], &["engine", "venue_sim"]),
        _ => return,
    };
    let Some(root) = defaults.as_table_mut() else {
        return;
    };
    if let Some(chains) = root.get_mut("chains").and_then(|v| v.as_table_mut()) {
        for section in chain_sections {
            chains.remove(*section);
        }
    }
    if let Some(services) = root.get_mut("services").and_then(|v| v.as_table_mut()) {
        for section in service_sections {
            services.remove(*section);
        }
    }
}

/// Optional services (not pinned by the profile): the defaults' stub is kept
/// only when the instance file carries the section. The simulator is optional
/// (a venue deployment may run only the engine); the CCTP bridge is its own
/// `cctp` profile and never rides a mirror instance.
fn prune_absent_services(defaults: &mut toml::Value, over: &toml::Value) {
    for section in ["venue_sim", "cctp_bridge"] {
        let present = over.get("services").and_then(|s| s.get(section)).is_some();
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

    /// The wiring the diorama-mainnet instance references (its deploy-time env).
    fn wiring() -> Vec<(String, String)> {
        vars(&[
            ("RETH_URL", "http://reth:8545"),
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
profile = "mainnet"

[chains.l1]
kind = "ethereum"
source_url = "http://reth:8545"
follow = "latest"
fork_offset = 128

[services.engine]
state_dir = "/state"

[services.engine.anvil]
target_url = "http://anvil-t:8545"

[services.engine.loops]
chunk_size = 500

[services.engine.source_sync]
engine_url = "http://reth:8551"
jwt_path = "/secrets/jwt.hex"

[services.engine.journal]
dir = "/state/journal"

[services.engine.beacon]
urls = ["http://lighthouse:5052"]

[services.engine.verifier]
probes = []

[services.engine.http]
port = 8080
"#
    }

    fn load_str(
        src: &str,
        vars: &[(String, String)],
    ) -> Result<(Instance, Vec<String>), ConfigError> {
        let over: toml::Value = toml::from_str(src).unwrap();
        let mut merged: toml::Value = toml::from_str(DEFAULTS).unwrap();
        // Same pipeline as the real loader, pruning included.
        prune_other_profile(&mut merged, profile_of(&over));
        prune_absent_services(&mut merged, &over);
        merge(&mut merged, over);
        let unresolved = env::expand(&mut merged, vars);
        env::apply_overrides(&mut merged, vars)?;
        let mut instance = Instance::deserialize(merged).map_err(|source| ConfigError::Parse {
            what: "test".into(),
            source,
        })?;
        instance.resolve_endpoints()?;
        Ok((instance, unresolved))
    }

    /// The mainnet sections, for the tests' direct field reads.
    fn l1(inst: &Instance) -> &ChainL1 {
        inst.chains.l1.as_ref().expect("[chains.l1]")
    }

    fn engine(inst: &Instance) -> &EngineSection {
        inst.services.engine.as_ref().expect("[services.engine]")
    }

    fn minimal_cctp_instance() -> &'static str {
        r#"
watchlist = []

[instance]
name = "c"
namespace = "c"
profile = "cctp"

[services.cctp_bridge]
state_dir = "/cctp"
t_url = "http://t:8545"
v_url = "http://v:8545"
relay_key = "0x5959595959595959595959595959595959595959595959595959595959595959"
attester_seed = "devnet"
fast_burn_allowance = 56854459.08
scan_chunk_blocks = 2000

[services.cctp_bridge.iris]
port = 8095

[services.cctp_bridge.control]
port = 8096
"#
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

[services.engine]
state_dir = "/state"

[services.engine.anvil]
target_url = "http://anvil-v:8545"

[services.engine.loops]
chunk_size = 500

[services.engine.journal]
dir = "/state/journal"

[services.engine.verifier]
probes = []

[services.engine.http]
port = 8080
"#
    }

    #[test]
    fn daloc_export_uses_the_manifests_spellings() {
        // The two `test-endpoints` ConfigMaps are the other surface of these
        // facts (`k8s/base/nodeports.yaml` + `overlays/diorama-venue/`): one
        // spelling on both sides, or the harness's env silently carries the
        // wrong name (the venue is the second chain, so its keys carry the
        // merge script's `CHAIN_1_` prefix).
        let (inst, _) = load_str(minimal_instance(), &wiring()).unwrap();
        assert_eq!(
            inst.export_env("daloc").unwrap(),
            vec![
                "CHAIN_RPC_URL=http://anvil-t.t.svc.cluster.local:8545",
                "CHAIN_WS_URL=ws://anvil-t.t.svc.cluster.local:8545",
                "DIORAMA_ADMIN_URL=http://engine.t.svc.cluster.local:8080",
            ]
        );

        let venue_src = format!(
            "{}\n[services.venue_sim]\nstate_dir = \"/sim\"\nmarkets = [\"BTC\"]\n\n[services.venue_sim.info]\nport = 8090\n\n[services.venue_sim.control]\nport = 8091\n",
            minimal_venue_instance()
        );
        let (venue, _) = load_str(&venue_src, &[]).unwrap();
        assert_eq!(
            venue.export_env("daloc").unwrap(),
            vec![
                "CHAIN_1_CHAIN_RPC_URL=http://anvil-v.v.svc.cluster.local:8545",
                "CHAIN_1_CHAIN_WS_URL=ws://anvil-v.v.svc.cluster.local:8545",
                "HL_INFO_API_URL=http://venue-sim.v.svc.cluster.local:8090",
                "VENUE_CONTROL_URL=http://venue-sim.v.svc.cluster.local:8091",
                "VENUE_ADMIN_URL=http://engine.v.svc.cluster.local:8080",
            ]
        );
    }

    #[test]
    fn defaults_and_instance_deep_merge() {
        let (inst, unresolved) = load_str(minimal_instance(), &wiring()).unwrap();
        assert!(unresolved.is_empty());
        // From defaults.toml:
        assert_eq!(engine(&inst).loops.poll_interval_ms, 1000);
        assert_eq!(engine(&inst).loops.prepend_rounds, 3);
        assert_eq!(engine(&inst).loops.status_log_interval_ms, 60_000);
        assert_eq!(engine(&inst).beacon.request_timeout_secs, 30);
        assert_eq!(engine(&inst).boot_timeout_secs, 900);
        assert_eq!(engine(&inst).drift_rotate_bytes, 16 * 1024 * 1024);
        assert_eq!(engine(&inst).source_sync.error_backoff_ms, 5_000);
        assert_eq!(engine(&inst).source_sync.report_interval_ms, 5_000);
        assert_eq!(engine(&inst).source_sync.request_timeout_secs, 180);
        assert_eq!(l1(&inst).follow_margin_blocks, 2);
        assert_eq!(engine(&inst).journal.blocks, 1024);
        // From the instance:
        assert_eq!(l1(&inst).fork_offset, 128);
        assert_eq!(engine(&inst).http.port, 8080);
    }

    /// The loop cadence is exactly the configured interval: no code-side
    /// floor, and the retry/backoff sites reuse this one value.
    #[test]
    fn the_loop_cadence_is_exactly_the_configured_interval() {
        let (inst, _) = load_str(minimal_instance(), &wiring()).unwrap();
        let loops = &engine(&inst).loops;
        assert_eq!(loops.poll(), std::time::Duration::from_millis(1000));
    }

    #[test]
    fn arrays_replace_never_concatenate() {
        let src = minimal_instance().replace(
            r#"urls = ["http://lighthouse:5052"]"#,
            r#"urls = ["http://a:1", "http://b:2"]"#,
        );
        let (inst, _) = load_str(&src, &wiring()).unwrap();
        assert_eq!(engine(&inst).beacon.urls.len(), 2);
    }

    /// The shared endpoint policy is declared once in `config/defaults.toml`;
    /// the crate's `PoolPolicy::default` is the test/tool fallback. Pinning
    /// them equal keeps the second home from drifting from the first.
    #[test]
    fn the_endpoint_policy_defaults_match_the_declared_table() {
        let (inst, _) = load_str(minimal_instance(), &wiring()).unwrap();
        assert_eq!(inst.endpoint_defaults, diorama_anvil::PoolPolicy::default());
    }

    /// A feed bridge has no honest confirmation window unless the deployment
    /// declares one: the old 60 s serde default was an invented freshness
    /// bound. Omitting it is a load error naming the key.
    #[test]
    fn a_feed_bridge_requires_its_confirmation_window() {
        let src = format!(
            "{}\n[services.engine.feed_bridge]\npoll_interval_ms = 1000\nwriter = \"0x00000000000000000000000000000000000000a1\"\nfeeds = [{{ name = \"ETH/USD\", target = \"0x00000000000000000000000000000000000000a2\", source = \"0x00000000000000000000000000000000000000a3\" }}]\n",
            minimal_instance()
        );
        let err = load_str(&src, &wiring()).expect_err("no confirmation window declared");
        let mut text = err.to_string();
        let mut source = std::error::Error::source(&err);
        while let Some(cause) = source {
            text.push_str(&format!("\n{cause}"));
            source = cause.source();
        }
        assert!(text.contains("confirm_window_ms"), "{text}");

        let declared = src.replace(
            "poll_interval_ms = 1000",
            "poll_interval_ms = 1000\nconfirm_window_ms = 60000",
        );
        let (inst, _) = load_str(&declared, &wiring()).expect("the declared window loads");
        assert_eq!(
            engine(&inst)
                .feed_bridge
                .as_ref()
                .expect("[services.engine.feed_bridge]")
                .confirm_window_ms,
            60_000
        );
    }

    #[test]
    fn cctp_relayer_mode_defaults_to_auto_and_the_instance_can_drive_it_manually() {
        let src = minimal_venue_instance().replace(
            "[services.engine.http]",
            r#"[services.cctp_bridge]
state_dir = "/state/cctp"
t_url = "http://t:8545"
v_url = "http://v:8545"
relay_key = "0x11"
attester_seed = "seed"
fast_burn_allowance = 56854459.08
scan_chunk_blocks = 2000

[services.cctp_bridge.iris]
port = 8095

[services.cctp_bridge.control]
port = 8096

[services.engine.http]"#,
        );
        let (inst, _) = load_str(&src, &wiring()).unwrap();
        let cctp = inst
            .services
            .cctp_bridge
            .as_ref()
            .expect("[services.cctp_bridge]");
        assert_eq!(
            cctp.relayer,
            RelayerMode::Auto,
            "defaults.toml drives the mode"
        );
        assert_eq!(cctp.poll_interval_ms, 1000, "shared defaults merge in");
        assert_eq!(cctp.rpc_timeout_secs, 30);
        assert_eq!(cctp.boot_timeout_secs, 300);
        assert_eq!(cctp.receipt_poll_ms, 500);
        assert_eq!(cctp.max_send_attempts, 5);
        assert_eq!(cctp.status_log_interval_ms, 60_000);

        let src = src.replace(
            "attester_seed = \"seed\"",
            "attester_seed = \"seed\"\nrelayer = \"manual\"",
        );
        let (inst, _) = load_str(&src, &wiring()).unwrap();
        assert_eq!(
            inst.services.cctp_bridge.as_ref().unwrap().relayer,
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
        assert_eq!(
            l1(&inst).source_urls.primary(),
            "${RETH_URL}",
            "raw ref kept"
        );
        let (inst2, unresolved2) = load_str(&src, &wiring()).unwrap();
        assert!(unresolved2.is_empty());
        assert_eq!(l1(&inst2).source_urls.primary(), "http://reth:8545");
    }

    #[test]
    fn diorama_overrides_apply_and_unknown_paths_fail() {
        let (inst, _) = load_str(
            minimal_instance(),
            &vars(&[("DIORAMA__SERVICES__ENGINE__LOOPS__POLL_INTERVAL_MS", "2000")]),
        )
        .unwrap();
        assert_eq!(engine(&inst).loops.poll_interval_ms, 2000);
        let err = load_str(
            minimal_instance(),
            &vars(&[("DIORAMA__SERVICES__ENGINE__LOOPS__POL_INTERVAL_MS", "2000")]),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("POL_INTERVAL_MS"), "{err}");
    }

    /// A zero boot budget or drift rotation threshold is refused by name: the
    /// former would disable the wait, the latter grow the log without bound.
    #[test]
    fn check_flags_zero_engine_bounds() {
        let (mut inst, _) = load_str(minimal_instance(), &wiring()).unwrap();
        inst.check().unwrap();
        let engine = inst.services.engine.as_mut().unwrap();
        engine.boot_timeout_secs = 0;
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("boot_timeout_secs"), "{err}");
        let engine = inst.services.engine.as_mut().unwrap();
        engine.boot_timeout_secs = 900;
        engine.drift_rotate_bytes = 0;
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("drift_rotate_bytes"), "{err}");
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

    /// The simulator's declared memory/cadence facts are required: a zero
    /// terminal bound, bridge cadence or gas stipend is a config error, not a
    /// silent code-side floor.
    #[test]
    fn check_flags_zero_simulator_bounds() {
        let src = format!(
            "{}\n[services.venue_sim]\nstate_dir = \"/sim\"\nmarkets = [\"BTC\"]\n\n[services.venue_sim.info]\nport = 8090\n\n[services.venue_sim.control]\nport = 8091\n",
            minimal_venue_instance()
        );
        let (mut inst, _) = load_str(&src, &[]).unwrap();
        inst.check().unwrap();
        let sim = inst.services.venue_sim.as_mut().unwrap();
        sim.terminal_orders = 0;
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("terminal_orders"), "{err}");
        let sim = inst.services.venue_sim.as_mut().unwrap();
        sim.terminal_orders = 1024;
        sim.bridge_poll_interval_ms = 0;
        let err = inst.check().unwrap_err();
        assert!(
            format!("{err}").contains("bridge_poll_interval_ms"),
            "{err}"
        );
        let sim = inst.services.venue_sim.as_mut().unwrap();
        sim.bridge_poll_interval_ms = 1000;
        sim.system_gas_stipend_wei = U256::ZERO;
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("system_gas_stipend_wei"), "{err}");
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
        engine.anvil.target_url = Some("not-a-url".into());
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("target_url"), "{err}");
    }

    #[test]
    fn the_real_mainnet_diorama_instance_loads_and_checks() {
        // The tracked instance names the gitignored live project (`daloc.toml`),
        // which a deployment may specialize. This test is about the **tracked
        // template**, so copy the instance with the project pointed at the
        // example and load that: a live specialization must not change the
        // audited count, and CI has no live file at all.
        let instance_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/instances/diorama-mainnet/instance.toml"
        ));
        let dir = std::env::temp_dir().join(format!(
            "diorama-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let instance_src = std::fs::read_to_string(instance_path).unwrap();
        std::fs::write(
            dir.join("instance.toml"),
            instance_src.replace("project = \"daloc\"", "project = \"daloc.example\""),
        )
        .unwrap();
        std::fs::copy(
            instance_path.with_file_name("daloc.example.toml"),
            dir.join("daloc.example.toml"),
        )
        .unwrap();
        let path = dir.join("instance.toml");
        let (inst, unresolved) = Instance::inspect(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(inst.instance.name, "diorama-mainnet");
        // 44 market/protocol entries + the dapp's 44-address mainnet
        // deployment in the tracked template. Update this count deliberately
        // when the audit changes.
        assert_eq!(
            inst.watchlist.len(),
            88,
            "the audited example watchlist is complete"
        );
        assert_eq!(l1(&inst).fork_offset, 128);
        // 10 probes: the Aave USDT price probe was removed after the
        // stable-feed pulse pinned USDT to 1.0 and the probe had no tolerance
        // (measured 2026-10-31; the mainnet engine now reports `failed:0`).
        assert_eq!(engine(&inst).verifier.probes.len(), 10);
        let mut names = unresolved.clone();
        names.sort();
        assert_eq!(
            names,
            vec![
                "ANVIL_T_URL",
                "BEACON_URL",
                "ENGINE_API_URL",
                "JWT_PATH",
                "RETH_URL",
                "STATE_DIR"
            ],
            "the wiring the instance references"
        );
    }

    #[test]
    fn the_venue_instance_loads_checks_and_takes_its_sections() {
        let (mut inst, unresolved) = load_str(minimal_venue_instance(), &[]).unwrap();
        assert!(unresolved.is_empty());
        // From defaults.toml (the shared engine conventions): the venue's
        // cadence and write chunk are the loops section's.
        assert_eq!(
            inst.services
                .engine
                .as_ref()
                .unwrap()
                .loops
                .poll_interval_ms,
            1000
        );
        assert_eq!(inst.services.engine.as_ref().unwrap().loops.chunk_size, 500);
        // The other profile's chain was pruned before the merge, and the
        // simulator (absent from this instance) with it.
        assert!(inst.chains.l1.is_none());
        assert!(inst.services.venue_sim.is_none());
        inst.check().unwrap();
        let (chain, engine) = inst.take_venue().unwrap();
        assert_eq!(chain.source_urls.primary(), "http://hl-rpc:8545");
        assert_eq!(
            engine.anvil.target_url.as_deref(),
            Some("http://anvil-v:8545")
        );
        // And the sections are gone after the take (moved, not copied).
        assert!(inst.take_venue().is_err());
    }

    #[test]
    fn profile_coherence_is_enforced() {
        // A venue instance carrying mainnet sections:
        let src = format!(
            "{}\n[chains.l1]\nkind = \"ethereum\"\nsource_url = \"http://reth:8545\"\nfollow = \"latest\"\nfollow_margin_blocks = 2\nfork_offset = 128\n",
            minimal_venue_instance()
        );
        let (inst, _) = load_str(&src, &[]).unwrap();
        let err = inst.check().unwrap_err();
        assert!(
            format!("{err}").contains("must not carry [chains.l1]"),
            "{err}"
        );

        // A mainnet instance carrying venue sections:
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

        // A mainnet instance carrying the simulator's section:
        let src = format!(
            "{}\n[services.venue_sim]\nstate_dir = \"/sim\"\nmarkets = [\"BTC\"]\nterminal_orders = 1024\nbridge_poll_interval_ms = 1000\nsystem_gas_stipend_wei = \"1000000000000000000\"\npoll_interval_ms = 5000\ntick_interval_ms = 1000\nheartbeat_interval_secs = 60\nrest_timeout_secs = 10\nrpc_timeout_secs = 30\nadmin_timeout_secs = 45\n[services.venue_sim.info]\naddr = \"0.0.0.0\"\nport = 8090\n[services.venue_sim.control]\naddr = \"0.0.0.0\"\nport = 8091\n",
            minimal_instance()
        );
        let (inst, _) = load_str(&src, &wiring()).unwrap();
        let err = inst.check().unwrap_err();
        assert!(
            format!("{err}").contains("must not carry [services.venue_sim]"),
            "{err}"
        );
    }

    #[test]
    fn the_simulator_sections_load_and_take() {
        let src = format!(
            "{}\n[services.venue_sim]\nstate_dir = \"/sim\"\nmarkets = [\"BTC\", \"ETH\"]\npoll_interval_ms = 5000\n[services.venue_sim.info]\nport = 8090\n[services.venue_sim.control]\nport = 8091\n",
            minimal_venue_instance()
        );
        let (mut inst, _) = load_str(&src, &[]).unwrap();
        inst.check().unwrap();
        let (chain, sim) = inst.take_venue_sim().unwrap();
        assert_eq!(chain.api_urls.primary(), "https://api.hyperliquid.xyz");
        assert_eq!(sim.markets, vec!["BTC", "ETH"]);
        assert_eq!(sim.info.port, 8090);
        assert_eq!(sim.control.port, 8091);
        assert_eq!(sim.tick_interval_ms, 1000);
        assert_eq!(sim.terminal_orders, 1024);
        assert_eq!(sim.bridge_poll_interval_ms, 1000);
        assert_eq!(
            sim.system_gas_stipend_wei,
            U256::from(1_000_000_000_000_000_000u64)
        );
        assert_eq!(sim.heartbeat_interval_secs, 60);
        assert_eq!(sim.rest_timeout_secs, 10);
        assert_eq!(sim.rpc_timeout_secs, 30);
        assert_eq!(sim.admin_timeout_secs, 45);
        assert!(sim.engine_admin_url.is_none());
        assert!(
            inst.take_venue_sim().is_err(),
            "the sections are moved, not copied"
        );
        // The engine's take still works on an instance that also carries the sim.
        let (mut inst2, _) = load_str(&src, &[]).unwrap();
        let (_, engine2) = inst2.take_venue().unwrap();
        assert_eq!(
            engine2.anvil.target_url.as_deref(),
            Some("http://anvil-v:8545")
        );
    }

    #[test]
    fn the_venue_instance_file_loads() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/instances/diorama-venue/instance.toml"
        ));
        let (inst, unresolved) = Instance::inspect(path).unwrap();
        assert_eq!(inst.instance.name, "diorama-venue");
        assert_eq!(inst.instance.profile, Profile::Venue);
        assert_eq!(
            inst.chains.venue.as_ref().unwrap().kind,
            ChainKind::Hyperliquid
        );
        let mut names = unresolved.clone();
        names.sort();
        assert_eq!(
            names,
            vec![
                "ANVIL_V_URL",
                "ENGINE_ADMIN_URL",
                "HL_INFO_API_URL",
                "HL_RPC_URLS",
                "HL_WS_API_URL",
                "SIM_STATE_DIR",
                "STATE_DIR"
            ],
            "the wiring the venue instance references"
        );
    }

    /// The bridge is its own profile: it carries only
    /// `[services.cctp_bridge]`, never a chain, engine or simulator. That is
    /// what lets a deployment run one venue, two venues, four mainnets and
    /// several bridges as independent instances sharing the same spine.
    /// A singular target (the anvil write endpoint, the engine API, the sim
    /// admin URL) must resolve to exactly one endpoint. Silently taking the
    /// first entry of a two-endpoint group looked like failover but wrote to
    /// whichever endpoint the group happened to list first.
    #[test]
    fn a_group_target_must_resolve_to_one_endpoint() {
        let src = minimal_venue_instance().replace(
            "target_url = \"http://anvil-v:8545\"",
            "target_url = \"@anvil\"",
        ) + r#"
[endpoints.anvil]
http = ["http://a:8545", "http://b:8545"]
"#;
        let err = load_str(&src, &[]).expect_err("a two-endpoint target is a config error");
        assert!(
            err.to_string().contains("a single endpoint is required"),
            "{err}"
        );
    }

    /// Endpoint attributes survive into the pool: chain id, weight and the
    /// `eth_getLogs` cap. A URL declared twice with different attributes is
    /// ambiguous and refused at the config boundary.
    #[test]
    fn endpoint_group_attributes_are_carried_and_conflicts_refused() {
        let src = minimal_venue_instance().replace(
            "source_url = \"http://hl-rpc:8545\"",
            "source_url = \"@hyperevm\"",
        ) + r#"
[endpoints.hyperevm]
http = ["http://a:8545", "http://b:8545"]
chain_id = 999
weight = 5
max_log_range = 900
"#;
        let (inst, _) = load_str(&src, &[]).unwrap();
        inst.check().expect("the group attributes are valid");
        let urls = inst.chains.venue.as_ref().unwrap().source_urls.as_slice();
        let specs = inst.endpoint_specs(urls);
        assert_eq!(specs.len(), 2);
        for spec in &specs {
            assert_eq!(spec.chain_id, Some(999));
            assert_eq!(spec.weight, 5);
            assert_eq!(spec.max_log_range, 900);
        }

        let conflict = src
            + r#"
[endpoints.conflicting]
http = ["http://a:8545"]
chain_id = 1
"#;
        let (inst, _) = load_str(&conflict, &[]).unwrap();
        let err = inst.check().expect_err("one URL cannot be two chains");
        assert!(
            err.to_string().contains("different roles/chain_id/weight"),
            "{err}"
        );
    }

    /// The source list is a URL list too; a malformed member must fail at the
    /// config boundary, not at the first pool call.
    #[test]
    fn every_source_url_list_is_validated() {
        let src = minimal_venue_instance().replace(
            "source_url = \"http://hl-rpc:8545\"",
            "source_url = \"http://bad host\"",
        );
        let (inst, _) = load_str(&src, &[]).expect("the list parses at the schema boundary");
        let err = inst
            .check()
            .expect_err("a malformed source host is refused");
        assert!(
            err.to_string().contains("not a http(s) URL with a host"),
            "{err}"
        );
    }

    /// A bridge with one chain cannot route anything; it used to pass `check`
    /// and fail only at boot in `build_lanes`.
    #[test]
    fn a_one_chain_cctp_is_refused() {
        let src = r#"
watchlist = []
[instance]
name = "c"
namespace = "c"
profile = "cctp"
[services.cctp_bridge]
state_dir = "/cctp"
relay_key = "0x5959595959595959595959595959595959595959595959595959595959595959"
attester_seed = "devnet"
fast_burn_allowance = 56854459.08
scan_chunk_blocks = 2000
[[services.cctp_bridge.chains]]
id = "ethereum"
family = "ethereum"
chain_id = 1
domain = 0
rpc_urls = ["http://t:8545"]
usdc = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
token_messenger = "0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d"
message_transmitter = "0x81D40F21F12A8F0E3252Bccb954D722d4c464B64"
[services.cctp_bridge.iris]
port = 8095
[services.cctp_bridge.control]
port = 8096
"#;
        let (inst, _) = load_str(src, &[]).expect("the schema parses");
        let err = inst.check().expect_err("one chain is not a bridge");
        assert!(err.to_string().contains("at least two chains"), "{err}");
    }

    /// A zero `token_minter` is a config error, not a boot-time contract
    /// assertion.
    #[test]
    fn a_zero_token_minter_is_refused() {
        let src = r#"
watchlist = []
[instance]
name = "c"
namespace = "c"
profile = "cctp"
[services.cctp_bridge]
state_dir = "/cctp"
relay_key = "0x5959595959595959595959595959595959595959595959595959595959595959"
attester_seed = "devnet"
fast_burn_allowance = 56854459.08
scan_chunk_blocks = 2000
[[services.cctp_bridge.chains]]
id = "ethereum"
family = "ethereum"
chain_id = 1
domain = 0
rpc_urls = ["http://t:8545"]
usdc = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
token_messenger = "0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d"
message_transmitter = "0x81D40F21F12A8F0E3252Bccb954D722d4c464B64"
token_minter = "0x0000000000000000000000000000000000000000"
[[services.cctp_bridge.chains]]
id = "hyperevm"
family = "hyperliquid"
chain_id = 999
domain = 19
rpc_urls = ["http://v:8545"]
usdc = "0xb88339CB7199b77E23DB6E890353E22632Ba630f"
token_messenger = "0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d"
message_transmitter = "0x81D40F21F12A8F0E3252Bccb954D722d4c464B64"
[services.cctp_bridge.iris]
port = 8095
[services.cctp_bridge.control]
port = 8096
"#;
        let (inst, _) = load_str(src, &[]).expect("the schema parses");
        let err = inst.check().expect_err("a zero minter is refused");
        assert!(
            err.to_string()
                .contains("token_minter must not be the zero address"),
            "{err}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // the fixture is the profile's section map
    fn the_cctp_profile_is_standalone() {
        let (mut inst, unresolved) = load_str(minimal_cctp_instance(), &[]).unwrap();
        assert!(unresolved.is_empty());
        inst.check().unwrap();
        // The defaults filled the shared numbers.
        let cctp = inst.services.cctp_bridge.as_ref().unwrap();
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
        assert!(
            inst.take_cctp().is_err(),
            "the section is moved, not copied"
        );

        // A mirror instance must not carry the bridge section: one process =
        // one role, and several bridges compose as several instances.
        let src = format!(
            "{}\n[services.cctp_bridge]\nstate_dir = \"/cctp\"\nt_url = \"http://t:8545\"\nv_url = \"http://v:8545\"\nrelay_key = \"0x{}\"\nattester_seed = \"devnet\"\nfast_burn_allowance = 56854459.08\nscan_chunk_blocks = 2000\n[services.cctp_bridge.iris]\nport = 8095\n[services.cctp_bridge.control]\nport = 8096\n",
            minimal_venue_instance(),
            "59".repeat(32)
        );
        let (inst, _) = load_str(&src, &[]).unwrap();
        let err = inst.check().unwrap_err();
        assert!(
            format!("{err}").contains("must not carry [services.cctp_bridge]"),
            "{err}"
        );

        // And the bridge profile must not carry a mirror's sections.
        let src = format!(
            "{}\n[chains.l1]\nkind = \"ethereum\"\nsource_url = \"http://reth:8545\"\nfollow = \"latest\"\nfollow_margin_blocks = 2\nfork_offset = 128\n",
            minimal_cctp_instance()
        );
        let (inst, _) = load_str(&src, &[]).unwrap();
        let err = inst.check().unwrap_err();
        assert!(
            format!("{err}").contains("must not carry [chains.l1]"),
            "{err}"
        );

        // The relay key must be a secret, never a path; the seed must be
        // non-empty; the gas thresholds must be ordered.
        let src = minimal_cctp_instance().replace(
            "relay_key = \"0x5959595959595959595959595959595959595959595959595959595959595959\"",
            "relay_key = \"/etc/mirror/relay.key\"",
        );
        let (inst, _) = load_str(&src, &[]).unwrap();
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("relay_key"), "{err}");
        let src =
            minimal_cctp_instance().replace("attester_seed = \"devnet\"", "attester_seed = \"\"");
        let (inst, _) = load_str(&src, &[]).unwrap();
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("attester_seed"), "{err}");
    }

    /// The bridge's composable topology: N chains and directed lanes, with
    /// the endpoint groups as the single URL home. This is the schema the
    /// "two venues / four mainnets / several providers" deployments compose
    /// from; the legacy t_url/v_url pair remains a two-chain shorthand.
    #[test]
    #[allow(clippy::too_many_lines)] // the fixture is the schema example
    fn a_cctp_instance_composes_arbitrary_chains_and_lanes() {
        let src = r#"
watchlist = []

[instance]
name = "c"
namespace = "c"
profile = "cctp"

[endpoints.t]
http = ["http://t:8545", "http://t2:8545"]

[endpoints.v]
http = ["http://v:8545"]

[endpoints.arb]
http = ["http://arb:8545"]

[services.cctp_bridge]
state_dir = "/cctp"
relay_key = "0x5959595959595959595959595959595959595959595959595959595959595959"
attester_seed = "devnet"
fast_burn_allowance = 56854459.08
scan_chunk_blocks = 2000

[[services.cctp_bridge.chains]]
id = "ethereum"
family = "ethereum"
chain_id = 1
domain = 0
rpc_urls = "@t"
usdc = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
token_messenger = "0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d"
message_transmitter = "0x81D40F21F12A8F0E3252Bccb954D722d4c464B64"

[[services.cctp_bridge.chains]]
id = "hyperevm"
family = "hyperliquid"
chain_id = 999
domain = 19
rpc_urls = "@v"
usdc = "0xb88339CB7199b77E23DB6E890353E22632Ba630f"
token_messenger = "0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d"
message_transmitter = "0x81D40F21F12A8F0E3252Bccb954D722d4c464B64"

[[services.cctp_bridge.chains]]
id = "arbitrum"
family = "ethereum"
chain_id = 42161
domain = 3
rpc_urls = "@arb"
usdc = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831"
token_messenger = "0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d"
message_transmitter = "0x81D40F21F12A8F0E3252Bccb954D722d4c464B64"

[[services.cctp_bridge.lanes]]
source = "ethereum"
destination = "hyperevm"

[[services.cctp_bridge.lanes]]
source = "hyperevm"
destination = "ethereum"

[[services.cctp_bridge.lanes]]
source = "arbitrum"
destination = "ethereum"

[services.cctp_bridge.iris]
port = 8095

[services.cctp_bridge.control]
port = 8096
"#;
        let (inst, _) = load_str(src, &[]).unwrap();
        inst.check().expect("the composed topology is coherent");
        let cctp = inst.services.cctp_bridge.as_ref().unwrap();
        assert_eq!(cctp.chains.len(), 3);
        assert_eq!(cctp.lanes.len(), 3);
        assert_eq!(
            cctp.chains[0].rpc_urls.as_slice(),
            ["http://t:8545", "http://t2:8545"]
        );

        // A lane naming an undeclared chain is a load error, not a guessed
        // direction.
        let broken = src.replace("destination = \"hyperevm\"", "destination = \"missing\"");
        let (inst, _) = load_str(&broken, &[]).unwrap();
        let err = inst.check().unwrap_err();
        assert!(format!("{err}").contains("missing"), "{err}");
    }

    #[test]
    fn the_cctp_client_export_is_its_iris_base() {
        let (inst, _) = load_str(minimal_cctp_instance(), &[]).unwrap();
        assert_eq!(
            inst.export_env("client").unwrap(),
            vec!["IRIS_API_URL=http://cctp-bridge.c.svc.cluster.local:8095"]
        );
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
        is_normal::<VenueSimSection>();
        is_normal::<VenueSimSection>();
        is_normal::<CctpSection>();
        is_normal::<LoopsSection>();
        is_normal::<VerifierSection>();
        is_normal::<SourceSyncSection>();
        is_normal::<JournalSection>();
        is_normal::<BeaconSection>();
        is_normal::<HttpSection>();
        is_normal::<ConfigError>();
    }
}
