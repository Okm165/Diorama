//! Service sections: one per service; each service reads its own (§19.3).
//!
//! Values live here only as *shapes*; the numbers live in
//! `config/defaults.toml` (shared) or the instance file (deployment-specific).
//! Fields carry no serde defaults: a key missing from both layers is a load
//! error naming it.
use alloy_primitives::{Address, Bytes, U256};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[services]`: one section per service; each service reads its own (§19.3).
/// The engine's key is the same for both profiles — what differs is the
/// profile's shape (`[services.engine.anvil]` carries M + T or V, and the
/// coherence check requires exactly the profile's fields). The venue's
/// simulator is `[services.venue_sim]`; `[services.cctp_bridge]` is element 3,
/// optional under either profile (the bridge spans both mirrors, §15).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Services {
    pub engine: Option<EngineSection>,
    pub venue_sim: Option<VenueSimSection>,
    pub cctp_bridge: Option<CctpSection>,
}

/// Who drives relays: the relayer's own tick (`auto`) or only the control
/// trigger (`manual`) — the deterministic mode for expiry/reattest tests.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RelayerMode {
    Auto,
    Manual,
}

/// `[services.cctp_bridge]`: the CCTP bridge (plan §15): Iris-compatible API (8095)
/// + control API (8096), one watcher per chain and one relayer.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CctpSection {
    /// Bridge state root (`cctp.json`: watched messages + lifecycle).
    pub state_dir: PathBuf,
    /// T's RPC (Ethereum, CCTP domain 0).
    pub t_url: String,
    /// V's RPC (HyperEVM, CCTP domain 19).
    pub v_url: String,
    /// The daloc-facing Iris API (8095).
    pub iris: HttpSection,
    /// The bridge's control API (8096).
    pub control: HttpSection,
    /// The relay wallet's 32-byte hex key: signs `receiveMessage` and the
    /// control transfers, and pays gas on both chains. A `${VAR}` reference
    /// keeps the secret out of the file; never a path.
    pub relay_key: String,
    /// The devnet attester's deterministic seed (a 32-byte hex key is used
    /// verbatim; any other string is hashed, §15.3).
    pub attester_seed: String,
    /// One watcher pass per interval.
    pub poll_interval_ms: u64,
    /// The emulated PENDING window (0 = attest immediately).
    pub finality_delay_secs: u64,
    /// The served `expirationBlock` window in destination blocks (0 = no
    /// expiry). Anchored at the destination's head at signing/reattest time;
    /// the contract rejects at `block.number == expirationBlock` and the
    /// relay's estimate runs at the pending block (head + 1), so a relayable
    /// window needs `>= 2` (measured, ADR 0007).
    pub expiration_blocks: u64,
    /// `eth_getLogs` range per scan call.
    pub scan_chunk_blocks: u64,
    /// How long a submitted tx may wait for a receipt inside one tick.
    pub receipt_timeout_secs: u64,
    /// Who drives relays: the relayer's own tick (`auto`) or only
    /// `POST /control/relay` (`manual`) — the deterministic mode for
    /// expiry/reattest tests (§15.4).
    pub relayer: RelayerMode,
    /// Gas alerts (daloc's defaults: 0.1 ETH low / 0.01 ETH critical, wei).
    pub balance_low_wei: U256,
    pub balance_critical_wei: U256,
    /// First block the watcher scans on T / V. Absent = 0 (genesis: correct
    /// but thousands of chunked calls on a deep fork — pin the deployment
    /// block).
    pub t_start_block: Option<u64>,
    pub v_start_block: Option<u64>,
}

/// `[services.venue_sim]`: the venue simulator (info + control API).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VenueSimSection {
    /// Simulator state root: accounts + market overrides (`sim.json`).
    pub state_dir: PathBuf,
    /// The markets whose books are cached and whose contexts are tracked; the
    /// account plane prices every position from these (a position in an
    /// untracked coin is a config error at the read that needs it).
    pub markets: Vec<String>,
    /// REST cache cadence (WS pushes are the primary freshness source; the
    /// budget meter gates this poller).
    pub poll_interval_ms: u64,
    /// The daloc-facing info API (8090).
    pub info: HttpSection,
    /// The control API (8091).
    pub control: HttpSection,
    /// The venue engine's admin base (`/admin/epoch`), called by
    /// `/control/epoch_new`; absent means epoch_new refuses.
    pub engine_admin_url: Option<String>,
    /// Anvil V (the devnet chain): the bridge watches its blocks for the
    /// Core↔EVM boundary (system-address transfers, CoreWriter actions).
    /// Absent disables the bridge (the sim still serves reads).
    pub anvil_url: Option<String>,
}

/// `[services.engine]`: the mirror engine — one section for both profiles
/// (plan §19.3's grammar). What differs by profile is the anvil endpoints
/// (`[services.engine.anvil]`: M + T or V) and which sub-sections the
/// profile's code reads; `loops`/`journal`/`verifier` are the shared homes —
/// the venue's cadence, write chunk and journal ring are the same knobs under
/// the same keys.
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
    /// Optional: the feed bridge (absent = the devnet's own price writers own
    /// the mocks, the pre-mirror arrangement).
    pub feed_bridge: Option<FeedBridgeSection>,
    /// Canonical addresses the devnet's fixture deploy replaces via
    /// `anvil_setCode` (daloc's mock tokens and seeded aTokens): whole-account
    /// owned, so the parent diffs never clobber them (plan §14.1). The list is
    /// exactly what the deploy mutates — only watched addresses can be
    /// clobbered, and these are the watched ones it replaces.
    #[serde(default)]
    pub fixture_addresses: Vec<Address>,
}

/// `[services.engine.feed_bridge]`: live parent-chain feed answers →
/// `updateAnswer` on the devnet's mock aggregators (plan §7.1, Phase 0).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FeedBridgeSection {
    /// Poll cadence for the source aggregators.
    pub poll_interval_ms: u64,
    /// How long a published answer may stay unconfirmed before the bridge
    /// re-publishes it (the outcome-unknown reconciliation: an anvil pool is
    /// not durable and a same-nonce sender can displace ours — measured: a
    /// deploy sharing the writer account wedged a feed in `pending` forever).
    /// Defaults to 60 s: comfortably beyond a parent block (~12 s) and a pool
    /// wait, and the re-send is idempotent (a new round with the same answer).
    #[serde(default = "default_confirm_window_ms")]
    pub confirm_window_ms: u64,
    /// The account that submits `updateAnswer` on T (auto-impersonate makes
    /// any address usable; the mock's own access control decides who may).
    /// Give it a *dedicated* account: sharing one with a concurrent sender
    /// (e.g. daloc's deployer, which is anvil account 0) races nonces.
    pub writer: Address,
    /// The feeds to bridge (one row per mock aggregator).
    pub feeds: Vec<FeedSection>,
}

/// 60 s: the confirmation window's default (see [`FeedBridgeSection`]).
const fn default_confirm_window_ms() -> u64 {
    60_000
}

/// One bridged feed: the parent chain's aggregator is the source, the
/// devnet's mock aggregator the write target.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FeedSection {
    pub name: String,
    pub target: Address,
    pub source: Address,
}

/// `[services.engine.anvil]`: the profile's anvil endpoints — M (the verified
/// replay chain) and T (the devnet chain) for the mainnet profile, V (the
/// fork) for the venue's. The coherence check requires exactly the profile's
/// set; the names mirror the deployment wiring (`ANVIL_{M,T,V}_URL`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EngineAnvil {
    pub m_url: Option<String>,
    pub t_url: Option<String>,
    pub v_url: Option<String>,
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
    /// State probes: re-run against M after every repair (ethereum), and at
    /// the pin against the fork source (venue) — devnet-owned targets are
    /// skipped and counted (COW means that state is the devnet's, not the
    /// fork base's).
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
    /// Beacon-API base URLs, tried in order (local Lighthouse first); empty
    /// for a profile with no beacon sources (the venue's chain has none).
    #[serde(default)]
    pub beacon_urls: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HttpSection {
    pub addr: String,
    pub port: u16,
}
