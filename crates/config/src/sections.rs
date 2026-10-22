//! Service sections: one per service; each service reads its own (§19.3).
//!
//! Values live here only as *shapes*; the numbers live in
//! `config/defaults.toml` (shared) or the instance file (deployment-specific).
//! Fields carry no serde defaults: a key missing from both layers is a load
//! error naming it.
use alloy_primitives::{Address, Bytes};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[services]`: one section per service; the venue sections land in R3.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Services {
    pub engine: EngineSection,
}

/// `[services.engine]`: the Mainnet Mirror engine.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EngineSection {
    /// Devnet state root: cursors, shadow registry, journal.
    pub state_dir: PathBuf,
    pub anvil: EngineAnvil,
    pub reth: EngineReth,
    pub loops: LoopsSection,
    pub verifier: VerifierSection,
    pub source_sync: SourceSyncSection,
    pub journal: JournalSection,
    pub blobs: BlobsSection,
    pub http: HttpSection,
}

/// The engine's anvil clients (M = verified replay chain, T = devnet chain).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EngineAnvil {
    pub m_url: String,
    pub t_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EngineReth {
    pub request_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LoopsSection {
    pub poll_interval_ms: u64,
    pub max_gap_blocks: u64,
    /// Max JSON-RPC batch size for raw-tx submissions and cheatcode writes.
    pub chunk_size: usize,
    pub replay_into_m: bool,
    pub apply_into_t: bool,
    /// Converge M's state from the parent BAL before each replay: M's
    /// execution cannot reproduce blockhash-dependent values (A17), so a
    /// single divergence would otherwise cascade forever. Applying mainnet's
    /// authoritative post-state keeps M a faithful mirror; execution still
    /// runs (traces + verification). Set false for pure-replay conformance
    /// runs.
    pub repair_m: bool,
    pub on_divergence: OnDivergence,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnDivergence {
    /// Record every divergence (drift.jsonl + metrics) and keep replaying.
    /// M's state is converged from the parent BAL before the next block, so
    /// divergences (inherent: a fork cannot reproduce blockhash-dependent
    /// values, A17) never cascade. This is the default: the mirror stays
    /// live and the operator watches mirror_divergences_total.
    Continue,
    /// Stop the replay loop (M) on any divergence; the diff applier (T)
    /// keeps running. Use for conformance runs where any divergence must be
    /// inspected before proceeding.
    FreezeM,
    /// Stop both loops on divergence.
    HaltAll,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VerifierSection {
    pub enabled: bool,
    pub drift_sample_keys: usize,
    /// State probes re-run against M after every repair.
    pub probes: Vec<Probe>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub name: String,
    pub to: Address,
    pub data: Bytes,
}

/// Top-up sync: feed reth the execution payloads it is missing, straight from
/// the local consensus client over the Engine API (EIP-8379 choreography).
/// This is what keeps the mirror self-contained: no EL P2P peers are required
/// to close a gap after a checkpoint sync or an EL restart.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceSyncSection {
    pub enabled: bool,
    pub interval_ms: u64,
    /// Payloads delivered per pass (each is a full block execution).
    pub max_per_pass: u64,
    /// reth's authenticated engine API endpoint.
    pub engine_url: Option<String>,
    /// JWT secret shared with the consensus client (a path, never the value).
    pub jwt_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JournalSection {
    pub dir: PathBuf,
    pub blocks: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BlobsSection {
    pub enabled: bool,
    /// Beacon-API base URLs, tried in order (local Lighthouse first).
    pub beacon_urls: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HttpSection {
    pub addr: String,
    pub port: u16,
}
