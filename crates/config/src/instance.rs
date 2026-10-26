//! The instance file's shape + cross-validation (`check`, §19.5).
use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::sections::{
    CctpSection, EngineAnvil, EngineSection, FeedBridgeSection, Services, VenueSimSection,
};
use crate::ConfigError;

/// The section requirements' one spelling each: a message naming a missing
/// section must read the same wherever it is raised.
const VENUE_CHAIN_REQUIRED: &str = "profile \"venue\" requires [chains.venue]";
const VENUE_ENGINE_REQUIRED: &str = "profile \"venue\" requires [services.engine]";
const MAINNET_CHAIN_REQUIRED: &str = "profile \"mainnet\" requires [chains.l1]";
const MAINNET_ENGINE_REQUIRED: &str = "profile \"mainnet\" requires [services.engine]";

/// One mirror deployment: identity, chains, watchlist, service sections.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub instance: InstanceMeta,
    pub chains: Chains,
    /// The watched addresses: the parent-diff scope on the mainnet profile,
    /// and the account-field scope of the venue's poll follow (both mirrors
    /// follow their parent; the mechanisms differ — see
    /// `docs/ARCHITECTURE.md`, "One product, two chains").
    #[serde(default)]
    pub watchlist: Vec<WatchEntry>,
    pub services: Services,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstanceMeta {
    pub name: String,
    pub namespace: String,
    pub profile: Profile,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Mainnet,
    Venue,
}

/// `[chains]`: one entry per chain the mirror serves. Exactly one is present
/// (the profile pins which).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Chains {
    pub l1: Option<ChainL1>,
    pub venue: Option<ChainVenue>,
}

/// `[chains.l1]`: the Ethereum source. `kind` only - the chain id, CCTP
/// domain and canonical addresses are code facts.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChainL1 {
    pub kind: ChainKind,
    pub source_url: String,
    pub follow: FollowHead,
    pub follow_margin_blocks: u64,
    /// Blocks between the follow head and the fork base; the single home of
    /// the offset (anvil init containers read it via `diorama-config get`).
    pub fork_offset: u64,
}

/// `[chains.venue]`: the venue's fork source and public API. `kind` only -
/// chain id 999, CCTP domain 19 and the canonical HyperEVM addresses are code
/// facts. V pins at the source's `latest` (or an explicit block chosen at
/// deploy time by the anvil init container), so there is no follow/offset
/// policy here: the source is archive-deep (measured: ≥5M blocks).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChainVenue {
    pub kind: ChainKind,
    /// The EVM RPC (V's fork source).
    pub source_url: String,
    /// The info REST API the simulator's market plane reads.
    pub api_url: String,
    /// The websocket API the simulator's market plane subscribes to.
    pub ws_url: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChainKind {
    Ethereum,
    Hyperliquid,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FollowHead {
    /// Justified checkpoint block: ~1 epoch behind head, reorg-immune in practice.
    #[default]
    Safe,
    Latest,
    Finalized,
}

impl FollowHead {
    pub fn tag(self) -> &'static str {
        match self {
            FollowHead::Safe => "safe",
            FollowHead::Latest => "latest",
            FollowHead::Finalized => "finalized",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WatchEntry {
    pub address: Address,
    #[serde(default)]
    pub note: Option<String>,
}

/// reth's state window: a pin deeper than this cannot be replayed (measured:
/// the BAL/state serve window is 10,064 blocks).
const RETH_STATE_WINDOW_BLOCKS: u64 = 10_064;
/// The safe-head lag the pin must clear (~32-64 blocks, measured).
const MIN_FORK_OFFSET_BLOCKS: u64 = 64;

/// Whether a profile-gated section is present (the coherence check's own
/// vocabulary: `needs` wants `Present`, `forbids` wants `Missing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Present,
    Missing,
}

impl Presence {
    /// The section's own answer.
    fn of(present: bool) -> Self {
        if present {
            Presence::Present
        } else {
            Presence::Missing
        }
    }
}

impl Instance {
    /// The watched addresses, as the engine's COW scope.
    pub fn watch_set(&self) -> HashSet<Address> {
        self.watchlist.iter().map(|w| w.address).collect()
    }

    /// Cross-validation. Every failure names the key and the reason.
    pub fn check(&self) -> Result<(), ConfigError> {
        let mut issues = Vec::new();
        self.check_profile_coherence(&mut issues);
        self.check_chains(&mut issues);
        self.check_services(&mut issues);
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid { issues })
        }
    }

    /// Exactly one profile's sections are present.
    fn check_profile_coherence(&self, issues: &mut Vec<String>) {
        let mainnet = self.instance.profile == Profile::Mainnet;
        let profile = if mainnet { "mainnet" } else { "venue" };
        let needs: &[(&str, Presence)] = if mainnet {
            &[
                ("[chains.l1]", Presence::of(self.chains.l1.is_some())),
                ("[services.engine]", Presence::of(self.services.engine.is_some())),
            ]
        } else {
            &[
                ("[chains.venue]", Presence::of(self.chains.venue.is_some())),
                ("[services.engine]", Presence::of(self.services.engine.is_some())),
            ]
        };
        for (section, presence) in needs {
            if *presence == Presence::Missing {
                issues.push(format!("profile \"{profile}\" requires {section}"));
            }
        }
        let forbids: &[(&str, Presence)] = if mainnet {
            &[
                ("[chains.venue]", Presence::of(self.chains.venue.is_some())),
                ("[services.venue_sim]", Presence::of(self.services.venue_sim.is_some())),
            ]
        } else {
            &[("[chains.l1]", Presence::of(self.chains.l1.is_some()))]
        };
        for (section, presence) in forbids {
            if *presence == Presence::Present {
                issues.push(format!("profile \"{profile}\" must not carry {section}"));
            }
        }
    }

    fn check_chains(&self, issues: &mut Vec<String>) {
        if let Some(l1) = &self.chains.l1 {
            if l1.kind != ChainKind::Ethereum {
                issues.push("chains.l1.kind must be \"ethereum\"".into());
            }
            if l1.fork_offset < MIN_FORK_OFFSET_BLOCKS {
                issues.push(format!(
                    "chains.l1.fork_offset {} is below the safe-head lag (~{} blocks): \
                     the pin would sit at or behind the follow head",
                    l1.fork_offset, MIN_FORK_OFFSET_BLOCKS
                ));
            }
            if l1.fork_offset >= RETH_STATE_WINDOW_BLOCKS {
                issues.push(format!(
                    "chains.l1.fork_offset {} is outside reth's {}-block state window",
                    l1.fork_offset, RETH_STATE_WINDOW_BLOCKS
                ));
            }
            check_http_url("chains.l1.source_url", &l1.source_url, issues);
        }
        if let Some(venue) = &self.chains.venue {
            if venue.kind != ChainKind::Hyperliquid {
                issues.push("chains.venue.kind must be \"hyperliquid\"".into());
            }
            check_http_url("chains.venue.source_url", &venue.source_url, issues);
            check_http_url("chains.venue.api_url", &venue.api_url, issues);
            check_ws_url("chains.venue.ws_url", &venue.ws_url, issues);
        }
    }

    fn check_services(&self, issues: &mut Vec<String>) {
        if let Some(eng) = &self.services.engine {
            check_engine(eng, issues);
        }
        if let Some(sim) = &self.services.venue_sim {
            check_venue_sim(sim, issues);
        }
        if let Some(cctp) = &self.services.cctp_bridge {
            check_cctp(cctp, issues);
        }
    }

    /// The mainnet profile's sections, moved out for the run path (the check
    /// already validated presence; a missing one is still a named error, never
    /// a silent default).
    pub fn take_mainnet(&mut self) -> Result<(ChainL1, EngineSection), ConfigError> {
        take_pair(
            self.chains.l1.take(),
            MAINNET_CHAIN_REQUIRED,
            self.services.engine.take(),
            MAINNET_ENGINE_REQUIRED,
        )
    }

    /// The venue profile's sections, moved out for the run path.
    pub fn take_venue(&mut self) -> Result<(ChainVenue, EngineSection), ConfigError> {
        take_pair(
            self.chains.venue.take(),
            VENUE_CHAIN_REQUIRED,
            self.services.engine.take(),
            VENUE_ENGINE_REQUIRED,
        )
    }

    /// The simulator's sections, moved out for the run path.
    pub fn take_venue_sim(&mut self) -> Result<(ChainVenue, VenueSimSection), ConfigError> {
        take_pair(
            self.chains.venue.take(),
            VENUE_CHAIN_REQUIRED,
            self.services.venue_sim.take(),
            "the simulator requires [services.venue_sim]",
        )
    }

    /// The CCTP bridge's section, moved out for the run path. Cross-cutting:
    /// it needs no chain section (its own `t_url`/`v_url` are the endpoints).
    pub fn take_cctp(&mut self) -> Result<CctpSection, ConfigError> {
        required(
            self.services.cctp_bridge.take(),
            "the CCTP bridge requires [services.cctp_bridge]",
        )
    }

    /// The mainnet profile's endpoints: the pre-context reads the run path
    /// needs (`Ctx::new` takes the sections itself).
    pub fn mainnet_urls(&self) -> Result<(String, String), ConfigError> {
        let (l1, engine) = take_pair(
            self.chains.l1.as_ref(),
            MAINNET_CHAIN_REQUIRED,
            self.services.engine.as_ref(),
            MAINNET_ENGINE_REQUIRED,
        )?;
        Ok((
            l1.source_url.clone(),
            required(
                engine.anvil.target_url.clone(),
                "profile \"mainnet\" requires [services.engine.anvil].target_url",
            )?,
        ))
    }

    /// The venue profile's endpoints (the fork source and anvil V).
    pub fn venue_urls(&self) -> Result<(String, String), ConfigError> {
        let (chain, engine) = take_pair(
            self.chains.venue.as_ref(),
            VENUE_CHAIN_REQUIRED,
            self.services.engine.as_ref(),
            VENUE_ENGINE_REQUIRED,
        )?;
        let target_url = required(
            engine.anvil.target_url.clone(),
            "profile \"venue\" requires [services.engine.anvil].target_url",
        )?;
        Ok((chain.source_url.clone(), target_url))
    }

    /// The instance as a TOML value (the shape the loader produced).
    pub fn to_value(&self) -> Result<toml::Value, ConfigError> {
        toml::Value::try_from(self).map_err(|e| ConfigError::Invalid {
            issues: vec![format!("serializing the instance: {e}")],
        })
    }

    /// One value by dotted path (`chains.l1.fork_offset`), for scripts.
    pub fn get(&self, path: &str) -> Result<toml::Value, ConfigError> {
        let value = self.to_value()?;
        let mut cursor = &value;
        for segment in path.split('.') {
            cursor = cursor.get(segment).ok_or_else(|| ConfigError::Invalid {
                issues: vec![format!("no such key `{path}`")],
            })?;
        }
        Ok(cursor.clone())
    }

    /// Shell-ready `KEY=VALUE` lines for one section. The anvil pods' init
    /// containers resolve their pin with `FORK_OFFSET` from here.
    pub fn export_env(&self, section: &str) -> Result<Vec<String>, ConfigError> {
        match section {
            "anvil" => {
                let l1 = required(
                    self.chains.l1.as_ref(),
                    "export-env anvil requires [chains.l1]",
                )?;
                Ok(vec![format!("FORK_OFFSET={}", l1.fork_offset)])
            }
            "client" | "daloc" => self.client_env(),
            other => Err(ConfigError::Invalid {
                issues: vec![format!("unknown section `{other}` (known: anvil, client)")],
            }),
        }
    }

    /// The client-facing endpoint block: our values under
    /// their names — we provide values, never rename their interface. The
    /// keys are **exactly** the `test-endpoints` ConfigMaps' spellings
    /// (`k8s/base/nodeports.yaml` for the first chain,
    /// `k8s/overlays/diorama-venue/nodeports.yaml` for the venue's
    /// `CHAIN_1_`-prefixed block): one fact, one spelling, whichever surface
    /// renders it. The chain URL is the deployment wiring (the same env the
    /// pods read); the service URLs are in-cluster DNS built from the
    /// instance's own ports, under the manifests' Service names (`engine`,
    /// `venue-sim`, `cctp-bridge` — `k8s/base/engine.yaml`, `base/venue/`).
    fn client_env(&self) -> Result<Vec<String>, ConfigError> {
        let mut out = match self.instance.profile {
            Profile::Mainnet => self.client_env_mainnet()?,
            Profile::Venue => self.client_env_venue()?,
        };
        // The bridge spans both mirrors: its Iris base is part of the block
        // whichever profile the instance carries (the base, their
        // client appends /v2/messages/... itself).
        if let Some(cctp) = &self.services.cctp_bridge {
            out.push(format!("IRIS_API_URL=http://cctp-bridge:{}", cctp.iris.port));
        }
        Ok(out)
    }

    /// The mainnet profile's client block: the first chain's keys.
    fn client_env_mainnet(&self) -> Result<Vec<String>, ConfigError> {
        let engine = self.services.engine.as_ref().ok_or_else(|| {
            mismatch("export-env client (mainnet profile) requires [services.engine]")
        })?;
        let t_url = engine.anvil.target_url.as_deref().ok_or_else(|| {
            mismatch(
                "export-env client (mainnet profile) requires [services.engine.anvil].target_url",
            )
        })?;
        let t = resolved("ANVIL_T_URL", t_url)?;
        Ok(vec![
            format!("CHAIN_RPC_URL={t}"),
            format!("CHAIN_WS_URL={}", ws_of(&t)),
            format!("DIORAMA_ADMIN_URL=http://engine:{}", engine.http.port),
        ])
    }

    /// The venue profile's client block: the `CHAIN_1_`-prefixed second-chain
    /// keys and the simulator's URLs.
    fn client_env_venue(&self) -> Result<Vec<String>, ConfigError> {
        let engine = self.services.engine.as_ref().ok_or_else(|| {
            mismatch("export-env client (venue profile) requires [services.engine]")
        })?;
        let v_url = engine.anvil.target_url.as_deref().ok_or_else(|| {
            mismatch("export-env client (venue profile) requires [services.engine.anvil].target_url")
        })?;
        let v = resolved("ANVIL_V_URL", v_url)?;
        let sim = self.services.venue_sim.as_ref().ok_or_else(|| {
            mismatch("export-env client (venue profile) requires [services.venue_sim]")
        })?;
        Ok(vec![
            format!("CHAIN_1_CHAIN_RPC_URL={v}"),
            format!("CHAIN_1_CHAIN_WS_URL={}", ws_of(&v)),
            format!("HL_INFO_API_URL=http://venue-sim:{}", sim.info.port),
            format!("VENUE_CONTROL_URL=http://venue-sim:{}", sim.control.port),
            format!("VENUE_ACCOUNT_FEEDS=http://venue-sim:{}", sim.info.port),
            format!("VENUE_RATE_FEEDS=http://venue-sim:{}", sim.info.port),
            format!("VENUE_ADMIN_URL=http://engine:{}", engine.http.port),
        ])
    }
}

/// A value that must be concrete for the export: a lingering `${VAR}`
/// reference means the deployment wiring was not set, and the export's whole
/// purpose is values, never references.
fn resolved(name: &str, value: &str) -> Result<String, ConfigError> {
    if value.contains("${") {
        return Err(ConfigError::Invalid {
            issues: vec![format!(
                "export-env client: {name} is not set (deployment wiring)"
            )],
        });
    }
    Ok(value.to_string())
}

/// The websocket form of an http URL — anvil serves WS on its HTTP port.
fn ws_of(url: &str) -> String {
    match url.strip_prefix("http") {
        Some(rest) => format!("ws{rest}"),
        None => url.to_string(),
    }
}

/// Take two required sections at once (the profiles' pairs), each named when
/// absent: the same rule as [`required`], named for the call-site shape.
fn take_pair<A, B>(
    a: Option<A>,
    a_msg: &str,
    b: Option<B>,
    b_msg: &str,
) -> Result<(A, B), ConfigError> {
    Ok((required(a, a_msg)?, required(b, b_msg)?))
}

/// Take a required section, naming the need when it is absent: the one shape
/// every `take_*`/`*_urls` read uses.
fn required<T>(section: Option<T>, what: &str) -> Result<T, ConfigError> {
    section.ok_or_else(|| mismatch(what))
}

fn mismatch(what: &str) -> ConfigError {
    ConfigError::Invalid {
        issues: vec![what.to_string()],
    }
}

/// The CCTP bridge's coherence: endpoints, the secret-shaped relay key (never
/// a path), the deterministic attester seed, and ordered gas thresholds.
fn check_cctp(cctp: &CctpSection, issues: &mut Vec<String>) {
    require_path(&cctp.state_dir, "services.cctp_bridge.state_dir", issues);
    check_http_url("services.cctp_bridge.t_url", &cctp.t_url, issues);
    check_http_url("services.cctp_bridge.v_url", &cctp.v_url, issues);
    // After `${VAR}` expansion the value must be the 32-byte hex key; a raw
    // reference is checked once the deployment expands it.
    if !cctp.relay_key.contains("${") && !looks_like_secret(&cctp.relay_key) {
        issues.push(
            "services.cctp_bridge.relay_key must be a 32-byte hex key (or a ${VAR} reference), \
             never a path"
                .into(),
        );
    }
    if cctp.attester_seed.trim().is_empty() {
        issues.push("services.cctp_bridge.attester_seed must not be empty".into());
    }
    if cctp.poll_interval_ms == 0 {
        issues.push("services.cctp_bridge.poll_interval_ms must be > 0".into());
    }
    if cctp.scan_chunk_blocks == 0 {
        issues.push("services.cctp_bridge.scan_chunk_blocks must be > 0".into());
    }
    if cctp.receipt_timeout_secs == 0 {
        issues.push("services.cctp_bridge.receipt_timeout_secs must be > 0".into());
    }
    if cctp.balance_critical_wei >= cctp.balance_low_wei {
        issues.push("services.cctp_bridge.balance_critical_wei must be below balance_low_wei".into());
    }
}

/// A section's path must be set: one rule, four sites (the message names the
/// section's field).
fn require_path(path: &std::path::Path, field: &str, issues: &mut Vec<String>) {
    if path.as_os_str().is_empty() {
        issues.push(format!("{field} must not be empty"));
    }
}

fn check_http_url(key: &str, value: &str, issues: &mut Vec<String>) {
    check_url(key, value, ["http://", "https://"], "http(s)", issues);
}

fn check_ws_url(key: &str, value: &str, issues: &mut Vec<String>) {
    check_url(key, value, ["ws://", "wss://"], "ws(s)", issues);
}

/// One URL check: the value carries one of `schemes` and a host — or it is a
/// deploy-time `${VAR}` reference, validated after expansion (`inspect`
/// reports the unresolved names separately).
fn check_url(
    key: &str,
    value: &str,
    schemes: [&str; 2],
    what: &str,
    issues: &mut Vec<String>,
) {
    if value.contains("${") {
        return;
    }
    let host = schemes.iter().find_map(|s| value.strip_prefix(s));
    let ok = matches!(host, Some(h) if !h.is_empty() && !h.starts_with('/'));
    if !ok {
        issues.push(format!("{key} \"{value}\" is not a {what} URL with a host"));
    }
}

/// A 64+ character hex blob is secret material, not a path.
fn looks_like_secret(value: &str) -> bool {
    let hex = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    hex.len() >= 64 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// The anvil shape: the profile's target endpoint, a parseable http URL. One
/// field for both profiles (the endpoint is a deployment fact).
fn check_engine_anvil(anvil: &EngineAnvil, issues: &mut Vec<String>) {
    match &anvil.target_url {
        Some(url) => check_http_url("services.engine.anvil.target_url", url, issues),
        None => issues.push("the engine requires [services.engine.anvil].target_url".into()),
    }
}

/// The engine's section: the anvil shape, the beacon list, the source-sync
/// boundary, the state dirs, and the feed bridge.
fn check_engine(eng: &EngineSection, issues: &mut Vec<String>) {
    check_engine_anvil(&eng.anvil, issues);
    for (i, url) in eng.beacon.urls.iter().enumerate() {
        check_http_url(
            &format!("services.engine.beacon.urls[{i}]"),
            url,
            issues,
        );
    }
    if let Some(url) = &eng.source_sync.engine_url {
        check_http_url("services.engine.source_sync.engine_url", url, issues);
    }
    if let Some(path) = &eng.source_sync.jwt_path {
        if looks_like_secret(&path.to_string_lossy()) {
            issues.push(
                "services.engine.source_sync.jwt_path must be a file path, \
                 not the secret itself"
                    .into(),
            );
        }
    }
    require_path(&eng.state_dir, "services.engine.state_dir", issues);
    require_path(&eng.journal.dir, "services.engine.journal.dir", issues);
    if let Some(fb) = &eng.feed_bridge {
        check_feed_bridge(fb, issues);
    }
}

/// One feed bridge: cadence coherence, at least one feed, unique names.
fn check_feed_bridge(fb: &FeedBridgeSection, issues: &mut Vec<String>) {
    if fb.poll_interval_ms == 0 {
        issues.push("services.engine.feed_bridge.poll_interval_ms must be > 0".into());
    }
    if fb.confirm_window_ms < fb.poll_interval_ms {
        issues.push(
            "services.engine.feed_bridge.confirm_window_ms must be >= poll_interval_ms".into(),
        );
    }
    if fb.feeds.is_empty() {
        issues.push("services.engine.feed_bridge.feeds must not be empty".into());
    }
    let mut names = HashSet::new();
    for (i, feed) in fb.feeds.iter().enumerate() {
        if feed.name.is_empty() {
            issues.push(format!(
                "services.engine.feed_bridge.feeds[{i}].name must not be empty"
            ));
        }
        if !names.insert(feed.name.clone()) {
            issues.push(format!(
                "services.engine.feed_bridge.feeds[{i}].name `{}` is duplicated",
                feed.name
            ));
        }
    }
}

/// The simulator's section: the state dir, at least one market, the cadence
/// and the optional chain URLs.
fn check_venue_sim(sim: &VenueSimSection, issues: &mut Vec<String>) {
    require_path(&sim.state_dir, "services.venue_sim.state_dir", issues);
    if sim.markets.is_empty() {
        issues.push("services.venue_sim.markets must name at least one market".into());
    }
    if sim.poll_interval_ms == 0 {
        issues.push("services.venue_sim.poll_interval_ms must be > 0".into());
    }
    if let Some(url) = &sim.engine_admin_url {
        check_http_url("services.venue_sim.engine_admin_url", url, issues);
    }
    if let Some(url) = &sim.anvil_url {
        check_http_url("services.venue_sim.anvil_url", url, issues);
    }
}

#[cfg(test)]
mod tests {
    use super::FollowHead;

    /// The follow-head tags are the config's own spelling: the mainnet
    /// engine's `follow_tag()` delegates to this mapping (one home; the test
    /// moved here from the engine's ctx, where it pinned nothing of its own).
    #[test]
    fn the_follow_head_tags_are_the_configs_spelling() {
        assert_eq!(FollowHead::Safe.tag(), "safe");
        assert_eq!(FollowHead::Latest.tag(), "latest");
        assert_eq!(FollowHead::Finalized.tag(), "finalized");
    }
}
