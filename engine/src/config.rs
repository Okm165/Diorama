//! Engine configuration (mirrors k8s/base/engine-config.yaml).
use alloy_primitives::{Address, Bytes};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub reth: RethConfig,
    pub anvil: AnvilConfig,
    #[serde(default)]
    pub loops: LoopsConfig,
    #[serde(default)]
    pub verifier: VerifierConfig,
    #[serde(default)]
    pub watchlist: Vec<WatchEntry>,
    #[serde(default)]
    pub journal: JournalConfig,
    #[serde(default)]
    pub blobs: BlobsConfig,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default)]
    pub http: HttpConfig,
}

fn default_true() -> bool {
    true
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("/state")
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading engine config {}", path.display()))?;
        serde_yaml::from_str(&s).context("parsing engine config")
    }

    pub fn watch_set(&self) -> HashSet<Address> {
        self.watchlist.iter().map(|w| w.address).collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RethConfig {
    pub url: String,
    #[serde(default)]
    pub follow: FollowHead,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FollowHead {
    /// Justified checkpoint block: ~1 epoch behind head, reorg-immune in practice.
    #[default]
    Safe,
    Latest,
    Finalized,
}

impl FollowHead {
    pub fn tag(&self) -> &'static str {
        match self {
            FollowHead::Safe => "safe",
            FollowHead::Latest => "latest",
            FollowHead::Finalized => "finalized",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnvilConfig {
    pub m_url: String,
    pub t_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoopsConfig {
    pub poll_interval_ms: u64,
    pub max_gap_blocks: u64,
    pub chunk_size: usize,
    pub replay_into_m: bool,
    pub apply_into_t: bool,
    /// Converge M's state from the parent BAL before each replay: M's execution
    /// cannot reproduce blockhash-dependent values (A17), so a single divergence
    /// would otherwise cascade forever. Applying mainnet's authoritative
    /// post-state keeps M a faithful mirror; execution still runs (traces +
    /// verification). Set false for pure-replay conformance runs.
    #[serde(default = "default_true")]
    pub repair_m: bool,
    pub on_divergence: OnDivergence,
}

impl Default for LoopsConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 1000,
            max_gap_blocks: 5000,
            chunk_size: 500,
            replay_into_m: true,
            apply_into_t: true,
            repair_m: true,
            on_divergence: OnDivergence::FreezeM,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnDivergence {
    /// Record every divergence (drift.jsonl + metrics) and keep replaying.
    /// M's state is converged from the parent BAL before the next block, so
    /// divergences (inherent: a fork cannot reproduce blockhash-dependent
    /// values, A17) never cascade. This is the default: the mirror stays live
    /// and the operator watches mirror_divergences_total.
    #[default]
    Continue,
    /// Stop the replay loop (M) on any divergence; the diff applier (T) keeps
    /// running. Use for conformance runs where any divergence must be inspected
    /// before proceeding.
    FreezeM,
    /// Stop both loops on divergence.
    HaltAll,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VerifierConfig {
    pub enabled: bool,
    pub drift_sample_keys: usize,
    pub type4_refund_per_auth: u64,
    pub probes: Vec<Probe>,
}

impl Default for VerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            drift_sample_keys: 8,
            type4_refund_per_auth: 12_500,
            probes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Probe {
    pub name: String,
    pub to: Address,
    pub data: Bytes,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchEntry {
    pub address: Address,
    #[serde(default)]
    #[allow(dead_code)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct JournalConfig {
    pub dir: PathBuf,
    pub blocks: u64,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("/state/journal"),
            blocks: 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BlobsConfig {
    pub enabled: bool,
    /// Beacon-API base URLs, tried in order (local Lighthouse first).
    pub beacon_urls: Vec<String>,
}

impl Default for BlobsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            beacon_urls: vec!["http://lighthouse:5052".into()],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    pub addr: String,
    pub port: u16,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0".into(),
            port: 8080,
        }
    }
}
