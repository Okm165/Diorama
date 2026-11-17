//! The source seam: where the mirror reads the chain it mirrors.
//!
//! One implementation per profile, chosen at boot (`Arc<dyn Source>` — a
//! boundary, not a hot loop; the two projects never monomorphize each other's
//! types). `RethSource` (the mainnet crate) reads reth (BALs, traces, a
//! 10,064-block state window); the venue's profile composes the spine's
//! [`PinnedSource`] over [`RawSource`] for the HL public RPC (latest-only
//! state, no traces/BAL/proofs — measured 2026-10-24). The trait is exactly
//! what the pipeline and a hand-back ask for: capabilities, the head, a block
//! header, and the three account reads an unshadow restores.
use alloy_primitives::{Address, B256, U256, U64};
// The seam's reads are dynamic (`&dyn Source`): one box per call, the
// standard `futures_util` form. These are per-block and admin-path reads,
// never a hot loop.
use futures_util::future::BoxFuture;
use std::sync::Arc;

/// A source capability, named: what a source can answer beyond the plain
/// reads (the composer and the probes branch on these). Probed once at boot
/// and logged; every read path gates on it — never assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `eth_getBlockAccessList` (reth; anvil ≥ Amsterdam records its own).
    NativeBal,
    /// `debug_traceBlockByNumber{prestateTracer, diffMode}`.
    Traces,
    /// State reads at arbitrary blocks; when absent, historical reads are
    /// refused with the numbers, never answered with a newer value.
    Historical,
    /// `eth_getSystemTxsByBlockNumber` (HyperEVM's Core→EVM completions).
    SystemTxs,
    /// Receipts (and therefore logs) for the system txs. The HL public RPC
    /// serves the txs but **not** their receipts (measured 2026-10-24: a
    /// system tx's receipt is `null` and `eth_getBlockReceipts` returns only
    /// the block's normal txs), so a composer on this source must decode the
    /// system txs' calldata for their touched accounts; a nanoreth archive
    /// serves the receipts.
    SystemTxReceipts,
}

impl Capability {
    /// The wire/log spelling (the boot log and the disagreement list).
    pub fn name(self) -> &'static str {
        match self {
            Capability::NativeBal => "native_bal",
            Capability::Traces => "traces",
            Capability::Historical => "historical",
            Capability::SystemTxs => "system_txs",
            Capability::SystemTxReceipts => "system_tx_receipts",
        }
    }

    /// Bridge from the config vocabulary ([`diorama_config::Capability`]] —
    /// the single place the two spellings meet. Config-only capabilities
    /// (`batch`, the market planes) have no source-level meaning and map to
    /// `None`: they gate admission, not the composer.
    pub fn from_config(cap: diorama_config::Capability) -> Option<Self> {
        Some(match cap {
            diorama_config::Capability::Bal => Self::NativeBal,
            diorama_config::Capability::Traces => Self::Traces,
            diorama_config::Capability::Historical => Self::Historical,
            diorama_config::Capability::SystemTxs => Self::SystemTxs,
            diorama_config::Capability::SystemTxReceipts => Self::SystemTxReceipts,
            diorama_config::Capability::Batch
            | diorama_config::Capability::MarketRest
            | diorama_config::Capability::MarketWs => return None,
        })
    }
}

/// The set a source declares.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Capabilities {
    set: std::collections::BTreeSet<Capability>,
}

impl Capabilities {
    /// The empty set (a source that can serve nothing beyond the plain reads).
    pub fn none() -> Self {
        Self::default()
    }

    /// The declared set.
    pub fn of(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            set: capabilities.into_iter().collect(),
        }
    }

    /// Does the source declare this capability?
    pub fn has(&self, capability: Capability) -> bool {
        self.set.contains(&capability)
    }

    /// Every declared capability (the disagreement report's left side).
    pub fn names(&self) -> Vec<Capability> {
        self.set.iter().copied().collect()
    }
}

/// The truth an unshadow restores from, and the pipeline's read side.
pub trait Source: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    /// The source's own head: the anchor before the first apply, when the
    /// devnet has no cursor yet (nothing applied to reflect).
    fn head(&self) -> BoxFuture<'_, diorama_anvil::Result<u64>>;

    /// The block's header: identity (number/hash/parent) plus the context the
    /// applier aligns to (timestamp, coinbase, prevRandao, gas limit, base
    /// fee). The official type — no mirror-owned block shape.
    fn header(&self, n: u64) -> BoxFuture<'_, diorama_anvil::Result<alloy_rpc_types_eth::Header>>;

    fn balance_at(
        &self,
        address: Address,
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<U256>>;

    fn nonce_at(&self, address: Address, block: u64) -> BoxFuture<'_, diorama_anvil::Result<u64>>;

    /// A storage word: the wire form (32 bytes), converted at the write site.
    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<B256>>;

    fn code_at(
        &self,
        address: Address,
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<alloy_primitives::Bytes>>;

    /// Read one key. The default composes the four typed reads, so a source
    /// implements only the primitives it can serve; the composer reads its
    /// plan through this seam (one call shape for every key kind).
    fn read_key(
        &self,
        key: diorama_anvil::Key,
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<diorama_anvil::Value>> {
        Box::pin(async move {
            Ok(match key {
                diorama_anvil::Key::Storage { address, slot } => {
                    let word = self.storage_at(address, slot, block).await?;
                    diorama_anvil::Value::Storage(U256::from_be_bytes(word.0))
                }
                diorama_anvil::Key::Balance(address) => {
                    diorama_anvil::Value::Balance(self.balance_at(address, block).await?)
                }
                diorama_anvil::Key::Nonce(address) => {
                    diorama_anvil::Value::Nonce(self.nonce_at(address, block).await?)
                }
                diorama_anvil::Key::Code(address) => {
                    diorama_anvil::Value::Code(self.code_at(address, block).await?)
                }
            })
        })
    }

    /// Read a plan of keys: the default is the sequential composition (every
    /// source stays correct); the RPC sources override with one mixed batch.
    /// A wide window's plan reaches thousands of keys (every transfer's
    /// holder), and the per-key form cost a round-trip per key against the
    /// public source (measured live: a 300-block catch-up window ground for
    /// minutes).
    fn read_keys(
        &self,
        keys: &[diorama_anvil::Key],
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<Vec<diorama_anvil::Value>>> {
        let keys = keys.to_vec();
        Box::pin(async move {
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                out.push(self.read_key(key, block).await?);
            }
            Ok(out)
        })
    }

    /// The native BAL for block `n`, when the source records one (EIP-7928).
    /// Default: none — the composer falls to the synthesized rung (the
    /// capability flag selects the rung; this answers per block, because a
    /// node can serve some blocks and not others).
    fn bal(
        &self,
        n: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<Option<alloy_eips::eip7928::BlockAccessList>>> {
        let _ = n;
        Box::pin(async move { Ok(None) })
    }

    /// The window's touches: the addresses and logs the declared closures
    /// plan from, plus any changed keys the source already carries values for
    /// (the trace rung). Every source implements this — it is the synthesized
    /// rung's data side.
    fn touched(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<crate::closure::TouchedSet>>;

    /// What this source's touched rung is (`Trace` for a trace-based source,
    /// `Poll` for the envelope rung): the window BAL's `kind` label.
    fn touched_kind(&self) -> crate::bal::DiffKind;
}

impl Capabilities {
    /// Is this capability provable by a read? `historical` is
    /// behavioral ("the block parameter is ignored" cannot be distinguished
    /// from "the value did not change" by a read) and `system_tx_receipts` is
    /// the absence of a value — they stay declarations with their
    /// measurements, enforced at the read path (the venue source refuses a
    /// non-latest read rather than silently answering a newer value).
    pub fn probeable(capability: Capability) -> bool {
        matches!(
            capability,
            Capability::NativeBal | Capability::Traces | Capability::SystemTxs
        )
    }

    /// The probe that proves a capability, at `head`: the method and its
    /// params. `None` for a behavioral capability (see [`Self::probeable`]).
    ///
    /// Each probe targets an existing block — a future height would let
    /// "block not found" masquerade as a capability answer — and speaks the
    /// block as a **hex quantity**, the EIP-1898 block identifier every node
    /// takes; a bare JSON integer is refused by reth (`-32602`, measured
    /// 2026-11-12: the audit read a healthy node as having no capabilities).
    /// One exception:
    /// HyperEVM's system-tx index lags the head (measured 2026-11-10:
    /// `-32603 invalid block height` at the head while the same block answers
    /// normally ~1 000 blocks later), so that probe reads a settled block; a
    /// genuinely absent method still answers `-32601` there.
    fn probe(capability: Capability, head: u64) -> Option<(&'static str, serde_json::Value)> {
        Some(match capability {
            Capability::NativeBal => (
                "eth_getBlockAccessList",
                serde_json::json!([U64::from(head)]),
            ),
            Capability::Traces => (
                diorama_anvil::TRACE_BLOCK_METHOD,
                serde_json::json!([
                    U64::from(head),
                    {"tracer": "prestateTracer", "tracerConfig": {"diffMode": true}}
                ]),
            ),
            Capability::SystemTxs => (
                "eth_getSystemTxsByBlockNumber",
                serde_json::json!([U64::from(head.saturating_sub(SYSTEM_TX_PROBE_LAG))]),
            ),
            Capability::Historical | Capability::SystemTxReceipts => {
                return None
            }
        })
    }
}

/// How far below the head the system-tx capability probe reads.
///
/// HyperEVM's `eth_getSystemTxsByBlockNumber` only indexes settled blocks; the
/// live endpoint answered `-32603 invalid block height` at the head on
/// 2026-11-10 while the same block answered normally ~1 000 blocks later. Two
/// times the measured lag is the probe's safety window.
const SYSTEM_TX_PROBE_LAG: u64 = 2_048;

/// What one capability probe proved.
///
/// Tri-state on purpose: only two answers are definitive (a value is presence,
/// `-32601` is absence). Every other fault — transport, HTTP, another node
/// error code, a body that does not decode — is **unknown**, and an unknown
/// answer must never be read as presence: that is how a claim nobody can serve
/// gets certified.
#[derive(Debug)]
enum Probe {
    Present,
    Absent,
    Unknown(diorama_anvil::Error),
}

fn classify_probe<T>(answer: diorama_anvil::Result<T>) -> Probe {
    match answer {
        Ok(_) => Probe::Present,
        Err(error) if error.is_method_not_found() => Probe::Absent,
        Err(error) => Probe::Unknown(error),
    }
}

/// **The capability audit: claims are verified where they are made, endpoint
/// by endpoint.**
///
/// Every pool member is probed for the capabilities *it* declares, through
/// [`diorama_anvil::Rpc::call_endpoint`] — never through the pool's rotation.
/// A pooled probe attributes one provider's answer to another, and on the live
/// HyperEVM pool a sloppy JSON-RPC envelope from one provider (both `result`
/// and `error` in one answer) became a whole-boot fault that took the venue
/// engine down, while the method was simply absent (measured 2026-11-12).
///
/// Three outcomes per claim:
/// - **confirmed**: the method answered, the capability is credited to the
///   pool's measured set and logged;
/// - **absent** (`-32601`): the claim and the provider disagree — named
///   loudly, never credited (this is exactly how provider drift reaches the
///   follow if it goes unnoticed: sentio dropped `system_txs` under a live
///   claim and cost four follow passes before the log was read);
/// - **unverified** (transport, HTTP, decode, another code): unknown, never
///   credited, and the endpoint is cooled by the client's own bookkeeping.
///
/// The audit **fails closed on the one fact that predicts a follow failure**:
/// a capability the composer *needs* that no endpoint confirmed. A false claim
/// on a backup is survivable and only warned about; a coverage gap is not.
pub async fn audit_capabilities(
    what: &str,
    needs: Capabilities,
    rpc: &diorama_anvil::Rpc,
) -> crate::Result<()> {
    let mut confirmed: Vec<Capability> = Vec::new();
    let mut gaps: Vec<String> = Vec::new();
    for (index, spec) in rpc.pool().specs().iter().enumerate() {
        for capability in audit_endpoint(what, index, spec, rpc, &mut gaps).await {
            if !confirmed.contains(&capability) {
                confirmed.push(capability);
            }
        }
    }
    // Fail closed on a needed capability nobody confirmed. Behavioral needs
    // are exempt: no read can prove them, so the declaration stands and the
    // read path enforces it.
    let missing: Vec<&'static str> = needs
        .names()
        .into_iter()
        .filter(|need| Capabilities::probeable(*need) && !confirmed.contains(need))
        .map(Capability::name)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(crate::Error::CapabilityGap {
        what: what.to_string(),
        missing: missing.join(","),
        detail: gaps.join("; "),
    })
}

/// What a declared role is proven by, if anything: its source-plane
/// capability and the method+params that demonstrate it at `head`.
fn role_probe(role: &str, head: u64) -> Option<(Capability, &'static str, serde_json::Value)> {
    let capability = diorama_config::Capability::parse(role).and_then(Capability::from_config)?;
    let (method, params) = Capabilities::probe(capability, head)?;
    Some((capability, method, params))
}

/// The one log line a disproved capability claim earns: the endpoint, the
/// role, and whether the pool retired it for this process.
fn note_absent(
    what: &str,
    spec: &diorama_anvil::EndpointSpec,
    capability: Capability,
    retired: bool,
) {
    tracing::warn!(
        source = what,
        url = %spec.url,
        capability = capability.name(),
        retired,
        "a declared capability is not served by this endpoint - the claim and the provider \
         disagree; the claim is retired for this process"
    );
}

/// One endpoint's claims, probed: the capabilities it proved, with every
/// disagreement or unknown answer appended to `gaps`.
async fn audit_endpoint(
    what: &str,
    index: usize,
    spec: &diorama_anvil::EndpointSpec,
    rpc: &diorama_anvil::Rpc,
    gaps: &mut Vec<String>,
) -> Vec<Capability> {
    let head = match rpc
        .call_endpoint::<U64>(index, "eth_blockNumber", diorama_anvil::NO_PARAMS)
        .await
    {
        Ok(head) => head.to::<u64>(),
        Err(error) => {
            // Not even a height: the endpoint is cooling for the process and
            // every claim it made is unverified.
            gaps.push(format!("{}: unreachable ({error})", spec.url));
            return Vec::new();
        }
    };
    let mut confirmed: Vec<Capability> = Vec::new();
    for role in &spec.roles {
        // A config-plane role (batch, the market planes) has no read to probe;
        // a behavioral capability has no read that could prove it.
        let Some((capability, method, params)) = role_probe(role, head) else {
            continue;
        };
        match classify_probe(
            rpc.call_endpoint::<serde_json::Value>(index, method, params)
                .await,
        ) {
            Probe::Present => confirmed.push(capability),
            Probe::Absent => {
                // The disagreement is a fact about this endpoint, not a note
                // about the audit: retire the claim so the pool stops
                // rotating the gated method onto an endpoint that answers
                // `-32601` for it. A claim that is not retired (declared under
                // another spelling, or the pool is single-endpoint) stays a
                // gap, exactly as before.
                let retired = rpc.retire_capability(&spec.url, role);
                note_absent(what, spec, capability, retired);
                gaps.push(format!("{}: {} absent", spec.url, capability.name()));
            }
            Probe::Unknown(error) => {
                gaps.push(format!(
                    "{}: {} unverified ({error})",
                    spec.url,
                    capability.name()
                ));
            }
        }
    }
    tracing::info!(
        source = what,
        url = %spec.url,
        declared = ?spec.roles,
        confirmed = ?confirmed.iter().map(|c| c.name()).collect::<Vec<_>>(),
        "capability audit: endpoint claims vs measured answers"
    );
    confirmed
}

/// A source over a raw RPC: the reads are forwarded verbatim and the
/// capabilities are the caller's declaration. For reads that are
/// latest-by-design (the venue's follow reads the public RPC at
/// latest, where the pin guard would refuse every read) and for tools; the
/// pin-exact shape for unshadow restores is [`PinnedSource`] over this.
pub struct RawSource {
    rpc: Arc<diorama_anvil::Rpc>,
    capabilities: Capabilities,
}

impl RawSource {
    /// The handle is shared (`Arc`): one client per endpoint, referenced by
    /// every object that reads through it.
    pub fn new(rpc: Arc<diorama_anvil::Rpc>, capabilities: Capabilities) -> Self {
        Self { rpc, capabilities }
    }

    /// The devnet chain as the composer's target: the synthesis rung reads it
    /// for the diff (traces yes; no BAL — the target is only read, never
    /// composed from).
    pub fn target(rpc: Arc<diorama_anvil::Rpc>) -> Self {
        Self::new(
            rpc,
            Capabilities::of([Capability::Traces, Capability::Historical]),
        )
    }
}

/// The composer's target: the devnet chain's raw reads (T / V) — the one
/// builder both profiles call.
pub fn target(rpc: Arc<diorama_anvil::Rpc>) -> Arc<dyn Source> {
    Arc::new(RawSource::target(rpc))
}

impl Source for RawSource {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn head(&self) -> BoxFuture<'_, diorama_anvil::Result<u64>> {
        Box::pin(self.rpc.block_number())
    }

    fn header(&self, n: u64) -> BoxFuture<'_, diorama_anvil::Result<alloy_rpc_types_eth::Header>> {
        Box::pin(async move {
            Ok(self
                .rpc
                .block_by_number(n, diorama_anvil::Txs::Hashes)
                .await?
                .header)
        })
    }

    fn balance_at(
        &self,
        address: Address,
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<U256>> {
        Box::pin(self.rpc.balance_at(address, block))
    }

    fn nonce_at(&self, address: Address, block: u64) -> BoxFuture<'_, diorama_anvil::Result<u64>> {
        Box::pin(self.rpc.nonce_at(address, block))
    }

    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<B256>> {
        Box::pin(self.rpc.storage_at(address, slot, block))
    }

    fn code_at(
        &self,
        address: Address,
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<alloy_primitives::Bytes>> {
        Box::pin(self.rpc.code_at(address, block))
    }

    /// The read plan in one mixed body: the four key kinds map to their RPC
    /// methods (the client owns the mapping and the decode).
    fn read_keys(
        &self,
        keys: &[diorama_anvil::Key],
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<Vec<diorama_anvil::Value>>> {
        let keys = keys.to_vec();
        Box::pin(async move { self.rpc.read_keys(&keys, block).await })
    }

    /// The envelope rung: the generic EVM touches (txs, receipts, and — when
    /// declared — HyperEVM's system txs), no values (the state reads fill
    /// them).
    fn touched(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<crate::closure::TouchedSet>> {
        let system_txs = crate::bal::SystemTxs::of(self.capabilities.has(Capability::SystemTxs));
        Box::pin(crate::bal::envelope_touches(
            &self.rpc, from, to, system_txs,
        ))
    }

    fn touched_kind(&self) -> crate::bal::DiffKind {
        crate::bal::DiffKind::Poll
    }
}

/// A pinned, latest-only source: the endpoint serves only its *latest* state
/// (the block parameter is ignored — measured), so a state read is exact only
/// while the source's head is still the pin — and the pin *is* the head. The
/// guard refuses with the numbers instead of silently answering from newer
/// state ("never hand back a poisoned value"); a source that declares
/// `historical` (reth's 10,064-block window, an archive) needs no check and
/// passes through. Headers and envelopes are historical-safe by measurement
/// and are never guarded.
pub struct PinnedSource {
    inner: RawSource,
    pin: Arc<std::sync::atomic::AtomicU64>,
}

impl PinnedSource {
    /// The handle is shared (`Arc`): one client per endpoint, referenced by
    /// every object that reads through it.
    pub fn new(
        rpc: Arc<diorama_anvil::Rpc>,
        pin: Arc<std::sync::atomic::AtomicU64>,
        capabilities: Capabilities,
    ) -> Self {
        Self {
            inner: RawSource::new(rpc, capabilities),
            pin,
        }
    }

    /// The pin guard: every state read is only exact while the source's head
    /// is still the pin. The declaration drives it — a source that can serve
    /// historical state needs no check; this one cannot (measured: the block
    /// parameter is ignored, latest is all there is).
    async fn guard(&self) -> diorama_anvil::Result<()> {
        if self.inner.capabilities().has(Capability::Historical) {
            return Ok(());
        }
        let Some(pin) = crate::pin::resolved_pin(&self.pin) else {
            return Err(diorama_anvil::Error::Corrupt {
                what: "pinned source",
                detail: "the fork pin is not resolved yet".into(),
            });
        };
        let head = self.inner.head().await?;
        if head != pin {
            return Err(diorama_anvil::Error::Corrupt {
                what: "pinned source",
                detail: format!(
                    "the source advanced past the pin (pin {pin}, head {head}): it \
                     serves only its latest state for eth_call (measured), so the \
                     pin's truth cannot be read back. A re-pin restores the whole \
                     source state (the re-fork is the wholesale hand-back); a \
                     *selective* hand-back is possible only while the source's head \
                     is still the pin — seconds on a 1s-block source"
                ),
            });
        }
        Ok(())
    }
}

impl Source for PinnedSource {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    /// The pin is the head: the source cannot serve history, so its own head
    /// would drift past every read's truth (a latest-by-design reader — the
    /// venue's follow — takes the raw source instead).
    fn head(&self) -> BoxFuture<'_, diorama_anvil::Result<u64>> {
        Box::pin(async move {
            if let Some(pin) = crate::pin::resolved_pin(&self.pin) {
                return Ok(pin);
            }
            self.inner.head().await
        })
    }

    /// Historical-safe on the public source (unlike state reads): the header
    /// is what makes forking and cursor hashes possible at all. No guard.
    fn header(&self, n: u64) -> BoxFuture<'_, diorama_anvil::Result<alloy_rpc_types_eth::Header>> {
        self.inner.header(n)
    }

    fn balance_at(
        &self,
        address: Address,
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<U256>> {
        Box::pin(async move {
            self.guard().await?;
            self.inner.balance_at(address, block).await
        })
    }

    fn nonce_at(&self, address: Address, block: u64) -> BoxFuture<'_, diorama_anvil::Result<u64>> {
        Box::pin(async move {
            self.guard().await?;
            self.inner.nonce_at(address, block).await
        })
    }

    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<B256>> {
        Box::pin(async move {
            self.guard().await?;
            self.inner.storage_at(address, slot, block).await
        })
    }

    fn code_at(
        &self,
        address: Address,
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<alloy_primitives::Bytes>> {
        Box::pin(async move {
            self.guard().await?;
            self.inner.code_at(address, block).await
        })
    }

    /// One guard for the whole plan (the per-key form paid it per key), then
    /// the batched read.
    fn read_keys(
        &self,
        keys: &[diorama_anvil::Key],
        block: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<Vec<diorama_anvil::Value>>> {
        let keys = keys.to_vec();
        Box::pin(async move {
            self.guard().await?;
            self.inner.read_keys(&keys, block).await
        })
    }

    /// Envelope reads are historical-safe, so the pin guard does not apply.
    fn touched(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, diorama_anvil::Result<crate::closure::TouchedSet>> {
        self.inner.touched(from, to)
    }

    fn touched_kind(&self) -> crate::bal::DiffKind {
        self.inner.touched_kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    /// The test's capability builder: one knob, named.
    #[derive(Clone, Copy)]
    enum History {
        Served,
        Refused,
    }

    fn caps(history: History) -> Capabilities {
        match history {
            History::Served => Capabilities::of([Capability::Historical]),
            History::Refused => Capabilities::none(),
        }
    }

    fn pinned(capabilities: Capabilities, pin: u64) -> PinnedSource {
        // The endpoint is never reached by the declaration and unresolved-pin
        // branches: both decide before any read.
        PinnedSource::new(
            Arc::new(
                diorama_anvil::Rpc::new("http://127.0.0.1:1", Duration::from_secs(1)).unwrap(),
            ),
            Arc::new(AtomicU64::new(pin)),
            capabilities,
        )
    }

    /// A source that declares history passes the guard untouched (reth's
    /// window, an archive): the pin never refuses a historical read.
    #[tokio::test]
    async fn a_historical_source_is_never_guarded() {
        assert!(pinned(caps(History::Served), crate::pin::PIN_UNRESOLVED)
            .guard()
            .await
            .is_ok());
        assert!(pinned(caps(History::Served), 7).guard().await.is_ok());
    }

    /// An unresolved pin refuses before any read, naming the state; a
    /// resolved pin moves the check to the head read (the transport fault
    /// proves the read was attempted, not skipped).
    #[tokio::test]
    async fn an_unresolved_pin_refuses_before_any_read() {
        let err = pinned(caps(History::Refused), crate::pin::PIN_UNRESOLVED)
            .guard()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pin is not resolved"), "{err}");
        let err = pinned(caps(History::Refused), 7).guard().await.unwrap_err();
        assert!(
            matches!(err, diorama_anvil::Error::Transport { .. }),
            "{err}"
        );
    }

    /// The pinned head is the pin, not the source's (the guard's premise): a
    /// resolved pin answers without a read; unresolved falls through to the
    /// source (the transport fault proves the read was attempted).
    #[tokio::test]
    async fn a_pinned_head_is_the_pin_not_the_sources() {
        assert_eq!(pinned(caps(History::Refused), 7).head().await.unwrap(), 7);
        let err = pinned(caps(History::Refused), crate::pin::PIN_UNRESOLVED)
            .head()
            .await
            .unwrap_err();
        assert!(
            matches!(err, diorama_anvil::Error::Transport { .. }),
            "{err}"
        );
    }

    /// Capability probes are tri-state: a value is presence, `-32601` is
    /// absence, and every other fault is unknown — never presence.
    #[test]
    fn a_capability_probe_never_reads_an_unknown_fault_as_presence() {
        assert!(matches!(classify_probe(Ok(())), Probe::Present));
        assert!(matches!(
            classify_probe::<()>(Err(method_not_found())),
            Probe::Absent
        ));
        assert!(matches!(
            classify_probe::<()>(Err(diorama_anvil::Error::Corrupt {
                what: "probe",
                detail: "node answered something else".into(),
            })),
            Probe::Unknown(_)
        ));
    }

    /// A behavioral capability is never probed by a read, and a method-plane
    /// capability always is: the probe table is the audit's whole decision.
    #[test]
    fn only_method_plane_capabilities_are_probeable() {
        for cap in [
            Capability::NativeBal,
            Capability::Traces,
            Capability::SystemTxs,
        ] {
            assert!(Capabilities::probeable(cap), "{cap:?}");
            assert!(Capabilities::probe(cap, 100).is_some(), "{cap:?}");
        }
        for cap in [
            Capability::Historical,
            Capability::SystemTxReceipts,
        ] {
            assert!(!Capabilities::probeable(cap), "{cap:?}");
            assert!(Capabilities::probe(cap, 100).is_none(), "{cap:?}");
        }
    }

    /// The system-tx probe reads a settled block, never the head: HyperEVM's
    /// index lags the head by roughly 1 000 blocks (measured 2026-11-10), and
    /// probing the head turned a healthy source into a boot fault.
    #[test]
    fn the_system_tx_probe_reads_a_settled_block() {
        let (_, params) = Capabilities::probe(Capability::SystemTxs, 10_000).unwrap();
        assert_eq!(
            params[0],
            serde_json::json!(format!("{:#x}", 10_000 - SYSTEM_TX_PROBE_LAG))
        );
    }

    /// A probe's block is the EIP-1898 **hex quantity**, never a bare JSON
    /// integer: reth refuses `[26401407]` with `-32602` (measured 2026-11-12,
    /// which made a healthy node look capability-less at boot).
    #[test]
    fn a_capability_probe_speaks_a_hex_block_quantity() {
        let (method, params) = Capabilities::probe(Capability::NativeBal, 26_401_407).unwrap();
        assert_eq!(method, "eth_getBlockAccessList");
        assert_eq!(params, serde_json::json!(["0x192da7f"]));
    }

    fn method_not_found() -> diorama_anvil::Error {
        diorama_anvil::Error::Rpc {
            endpoint: "http://127.0.0.1:1".into(),
            method: "eth_getBlockAccessList".into(),
            payload: alloy_json_rpc::ErrorPayload {
                code: -32601,
                message: "method not found".into(),
                data: None,
            },
        }
    }
}
