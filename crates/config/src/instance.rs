//! The instance file's shape + cross-validation (`check`, §19.5).
use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::sections::Services;
use crate::ConfigError;

/// One mirror deployment: identity, chains, watchlist, service sections.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub instance: InstanceMeta,
    pub chains: Chains,
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

/// `[chains]`: one entry per chain the mirror serves.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Chains {
    pub l1: ChainL1,
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

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChainKind {
    Ethereum,
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
        if self.instance.profile == Profile::Venue {
            issues.push("profile \"venue\": the venue sections land in R3".into());
        }

        let l1 = &self.chains.l1;
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
        check_http_url("chains.l1.source_url", &l1.source_url, &mut issues);

        let eng = &self.services.engine;
        check_http_url("services.engine.anvil.m_url", &eng.anvil.m_url, &mut issues);
        check_http_url("services.engine.anvil.t_url", &eng.anvil.t_url, &mut issues);
        for (i, url) in eng.blobs.beacon_urls.iter().enumerate() {
            check_http_url(
                &format!("services.engine.blobs.beacon_urls[{i}]"),
                url,
                &mut issues,
            );
        }
        if let Some(url) = &eng.source_sync.engine_url {
            check_http_url("services.engine.source_sync.engine_url", url, &mut issues);
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
        if eng.journal.dir.as_os_str().is_empty() {
            issues.push("services.engine.journal.dir must not be empty".into());
        }

        let ports = [("services.engine.http.port", eng.http.port)];
        for (i, (name, port)) in ports.iter().enumerate() {
            for (other_name, other_port) in ports.iter().skip(i + 1) {
                if port == other_port {
                    issues.push(format!(
                        "port {port} is used by both {name} and {other_name}"
                    ));
                }
            }
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid { issues })
        }
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
            "anvil" => Ok(vec![format!("FORK_OFFSET={}", self.chains.l1.fork_offset)]),
            other => Err(ConfigError::Invalid {
                issues: vec![format!("unknown section `{other}` (known: anvil)")],
            }),
        }
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

/// A 64+ character hex blob is secret material, not a path.
fn looks_like_secret(value: &str) -> bool {
    let hex = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    hex.len() >= 64 && hex.chars().all(|c| c.is_ascii_hexdigit())
}
