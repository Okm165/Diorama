//! The instance file's shape + cross-validation (`check`, §19.5).
use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::sections::{CctpSection, EngineSection, Services, SimSection, VenueSection};
use crate::ConfigError;

/// One mirror deployment: identity, chains, watchlist, service sections.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub instance: InstanceMeta,
    pub chains: Chains,
    /// The watched addresses (the ethereum profile's parent-diff scope; the
    /// venue has no parent stream, so it is empty there).
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
    Ethereum,
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
    /// the offset (anvil init containers read it via `mirror-config get`).
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

impl Instance {
    /// The watched addresses, as the engine's COW scope.
    pub fn watch_set(&self) -> HashSet<Address> {
        self.watchlist.iter().map(|w| w.address).collect()
    }

    /// Cross-validation (§19.5). Every failure names the key and the reason.
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
        let ethereum = self.instance.profile == Profile::Ethereum;
        let profile = if ethereum { "ethereum" } else { "venue" };
        let needs = if ethereum {
            [
                ("[chains.l1]", self.chains.l1.is_some()),
                ("[services.engine]", self.services.engine.is_some()),
            ]
        } else {
            [
                ("[chains.venue]", self.chains.venue.is_some()),
                ("[services.venue]", self.services.venue.is_some()),
            ]
        };
        for (section, present) in needs {
            if !present {
                issues.push(format!("profile \"{profile}\" requires {section}"));
            }
        }
        let forbids: &[(&str, bool)] = if ethereum {
            &[
                ("[chains.venue]", self.chains.venue.is_some()),
                ("[services.venue]", self.services.venue.is_some()),
                ("[services.sim]", self.services.sim.is_some()),
            ]
        } else {
            &[
                ("[chains.l1]", self.chains.l1.is_some()),
                ("[services.engine]", self.services.engine.is_some()),
            ]
        };
        for (section, present) in forbids {
            if *present {
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
            check_http_url("services.engine.anvil.m_url", &eng.anvil.m_url, issues);
            check_http_url("services.engine.anvil.t_url", &eng.anvil.t_url, issues);
            for (i, url) in eng.blobs.beacon_urls.iter().enumerate() {
                check_http_url(
                    &format!("services.engine.blobs.beacon_urls[{i}]"),
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
            if eng.state_dir.as_os_str().is_empty() {
                issues.push("services.engine.state_dir must not be empty".into());
            }
            if let Some(fb) = &eng.feed_bridge {
                if fb.poll_interval_ms == 0 {
                    issues.push("services.engine.feed_bridge.poll_interval_ms must be > 0".into());
                }
                if fb.feeds.is_empty() {
                    issues.push("services.engine.feed_bridge.feeds must not be empty".into());
                }
                let mut names = std::collections::HashSet::new();
                for (i, feed) in fb.feeds.iter().enumerate() {
                    if feed.name.is_empty() {
                        issues.push(format!("services.engine.feed_bridge.feeds[{i}].name must not be empty"));
                    }
                    if !names.insert(feed.name.clone()) {
                        issues.push(format!("services.engine.feed_bridge.feeds[{i}].name `{}` is duplicated", feed.name));
                    }
                }
            }
            if eng.journal.dir.as_os_str().is_empty() {
                issues.push("services.engine.journal.dir must not be empty".into());
            }
        }
        if let Some(venue) = &self.services.venue {
            check_http_url("services.venue.anvil_url", &venue.anvil_url, issues);
            if venue.state_dir.as_os_str().is_empty() {
                issues.push("services.venue.state_dir must not be empty".into());
            }
            if venue.poll_interval_ms == 0 {
                issues.push("services.venue.poll_interval_ms must be > 0".into());
            }
            if venue.chunk_size == 0 {
                issues.push("services.venue.chunk_size must be > 0".into());
            }
        }
        if let Some(sim) = &self.services.sim {
            if sim.state_dir.as_os_str().is_empty() {
                issues.push("services.sim.state_dir must not be empty".into());
            }
            if sim.markets.is_empty() {
                issues.push("services.sim.markets must name at least one market".into());
            }
            if sim.poll_interval_ms == 0 {
                issues.push("services.sim.poll_interval_ms must be > 0".into());
            }
            if let Some(url) = &sim.engine_admin_url {
                check_http_url("services.sim.engine_admin_url", url, issues);
            }
            if let Some(url) = &sim.anvil_url {
                check_http_url("services.sim.anvil_url", url, issues);
            }
        }
        if let Some(cctp) = &self.services.cctp {
            check_cctp(cctp, issues);
        }
    }

    /// The ethereum profile's sections, moved out for the run path (the check
    /// already validated presence; a missing one is still a named error, never
    /// a silent default).
    pub fn take_ethereum(&mut self) -> Result<(ChainL1, EngineSection), ConfigError> {
        let l1 = self
            .chains
            .l1
            .take()
            .ok_or_else(|| mismatch("profile \"ethereum\" requires [chains.l1]"))?;
        let engine = self
            .services
            .engine
            .take()
            .ok_or_else(|| mismatch("profile \"ethereum\" requires [services.engine]"))?;
        Ok((l1, engine))
    }

    /// The venue profile's sections, moved out for the run path.
    pub fn take_venue(&mut self) -> Result<(ChainVenue, VenueSection), ConfigError> {
        let chain = self
            .chains
            .venue
            .take()
            .ok_or_else(|| mismatch("profile \"venue\" requires [chains.venue]"))?;
        let venue = self
            .services
            .venue
            .take()
            .ok_or_else(|| mismatch("profile \"venue\" requires [services.venue]"))?;
        Ok((chain, venue))
    }

    /// The simulator's sections, moved out for the run path.
    pub fn take_sim(&mut self) -> Result<(ChainVenue, SimSection), ConfigError> {
        let chain = self
            .chains
            .venue
            .take()
            .ok_or_else(|| mismatch("profile \"venue\" requires [chains.venue]"))?;
        let sim = self
            .services
            .sim
            .take()
            .ok_or_else(|| mismatch("the simulator requires [services.sim]"))?;
        Ok((chain, sim))
    }

    /// The CCTP bridge's section, moved out for the run path. Cross-cutting:
    /// it needs no chain section (its own `t_url`/`v_url` are the endpoints).
    pub fn take_cctp(&mut self) -> Result<CctpSection, ConfigError> {
        self.services
            .cctp
            .take()
            .ok_or_else(|| mismatch("the CCTP bridge requires [services.cctp]"))
    }

    /// The ethereum profile's endpoints: the pre-context reads the run path
    /// needs (`Ctx::new` takes the sections itself).
    pub fn ethereum_urls(&self) -> Result<(String, String, String), ConfigError> {
        let l1 = self
            .chains
            .l1
            .as_ref()
            .ok_or_else(|| mismatch("profile \"ethereum\" requires [chains.l1]"))?;
        let engine = self
            .services
            .engine
            .as_ref()
            .ok_or_else(|| mismatch("profile \"ethereum\" requires [services.engine]"))?;
        Ok((
            l1.source_url.clone(),
            engine.anvil.m_url.clone(),
            engine.anvil.t_url.clone(),
        ))
    }

    /// The venue profile's endpoints (the fork source and anvil V).
    pub fn venue_urls(&self) -> Result<(String, String), ConfigError> {
        let chain = self
            .chains
            .venue
            .as_ref()
            .ok_or_else(|| mismatch("profile \"venue\" requires [chains.venue]"))?;
        let venue = self
            .services
            .venue
            .as_ref()
            .ok_or_else(|| mismatch("profile \"venue\" requires [services.venue]"))?;
        Ok((chain.source_url.clone(), venue.anvil_url.clone()))
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
                let l1 = self
                    .chains
                    .l1
                    .as_ref()
                    .ok_or_else(|| mismatch("export-env anvil requires [chains.l1]"))?;
                Ok(vec![format!("FORK_OFFSET={}", l1.fork_offset)])
            }
            other => Err(ConfigError::Invalid {
                issues: vec![format!("unknown section `{other}` (known: anvil)")],
            }),
        }
    }
}

fn mismatch(what: &str) -> ConfigError {
    ConfigError::Invalid {
        issues: vec![what.to_string()],
    }
}

/// The CCTP bridge's coherence: endpoints, the secret-shaped relay key (never
/// a path), the deterministic attester seed, and ordered gas thresholds.
fn check_cctp(cctp: &CctpSection, issues: &mut Vec<String>) {
    if cctp.state_dir.as_os_str().is_empty() {
        issues.push("services.cctp.state_dir must not be empty".into());
    }
    check_http_url("services.cctp.t_url", &cctp.t_url, issues);
    check_http_url("services.cctp.v_url", &cctp.v_url, issues);
    // After `${VAR}` expansion the value must be the 32-byte hex key; a raw
    // reference is checked once the deployment expands it.
    if !cctp.relay_key.contains("${") && !looks_like_secret(&cctp.relay_key) {
        issues.push(
            "services.cctp.relay_key must be a 32-byte hex key (or a ${VAR} reference), \
             never a path"
                .into(),
        );
    }
    if cctp.attester_seed.trim().is_empty() {
        issues.push("services.cctp.attester_seed must not be empty".into());
    }
    if cctp.poll_interval_ms == 0 {
        issues.push("services.cctp.poll_interval_ms must be > 0".into());
    }
    if cctp.scan_chunk_blocks == 0 {
        issues.push("services.cctp.scan_chunk_blocks must be > 0".into());
    }
    if cctp.receipt_timeout_secs == 0 {
        issues.push("services.cctp.receipt_timeout_secs must be > 0".into());
    }
    if cctp.balance_critical_wei >= cctp.balance_low_wei {
        issues.push("services.cctp.balance_critical_wei must be below balance_low_wei".into());
    }
}

fn check_http_url(key: &str, value: &str, issues: &mut Vec<String>) {
    if value.contains("${") {
        // Deploy-time wiring: the reference is validated after expansion
        // (services); `inspect` reports the unresolved names separately.
        return;
    }
    let host = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"));
    let ok = matches!(host, Some(h) if !h.is_empty() && !h.starts_with('/'));
    if !ok {
        issues.push(format!("{key} \"{value}\" is not an http(s) URL with a host"));
    }
}

fn check_ws_url(key: &str, value: &str, issues: &mut Vec<String>) {
    if value.contains("${") {
        return;
    }
    let host = value
        .strip_prefix("ws://")
        .or_else(|| value.strip_prefix("wss://"));
    let ok = matches!(host, Some(h) if !h.is_empty() && !h.starts_with('/'));
    if !ok {
        issues.push(format!("{key} \"{value}\" is not a ws(s) URL with a host"));
    }
}

/// A 64+ character hex blob is secret material, not a path.
fn looks_like_secret(value: &str) -> bool {
    let hex = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    hex.len() >= 64 && hex.chars().all(|c| c.is_ascii_hexdigit())
}
