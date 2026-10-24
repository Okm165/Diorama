//! The source seam: where the mirror reads the chain it mirrors.
//!
//! One implementation per profile, chosen at boot (`Arc<dyn Source>` — a
//! boundary, not a hot loop; the two projects never monomorphize each other's
//! types). `RethSource` reads reth (BALs, traces, a 10,064-block state
//! window); `VenueSource` reads the HL public RPC (latest-only state, no
//! traces/BAL/proofs — measured 2026-10-24). The trait is exactly what the
//! pipeline and a hand-back ask for: capabilities, the head, a block header,
//! and the three account reads an unshadow restores.
use alloy_primitives::{Address, B256, U256};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed future: the boundary is dynamic, so each call boxes once. These
/// are per-block and admin-path reads, never a hot loop.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a source can serve. Probed once at boot and logged; every read path
/// gates on it — never assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Capabilities {
    /// `eth_getBlockAccessList` (reth; anvil ≥ Amsterdam records its own).
    pub native_bal: bool,
    /// `debug_traceBlockByNumber{prestateTracer, diffMode}`.
    pub traces: bool,
    /// State reads at arbitrary blocks; when false, historical reads are
    /// refused with the numbers, never answered with a newer value.
    pub historical: bool,
    /// `eth_subscribe newHeads` (reth; the HL public RPC documents none).
    pub heads_ws: bool,
    /// `eth_getSystemTxsByBlockNumber` (HyperEVM's Core→EVM completions).
    pub system_txs: bool,
    /// Receipts (and therefore logs) for the system txs. The HL public RPC
    /// serves the txs but **not** their receipts (measured 2026-10-24: a
    /// system tx's receipt is `null` and `eth_getBlockReceipts` returns only
    /// the block's normal txs), so a composer on this source must decode the
    /// system txs' calldata for their touched accounts; a nanoreth archive
    /// serves the receipts.
    pub system_tx_receipts: bool,
}

/// The truth an unshadow restores from, and the pipeline's read side.
pub trait Source: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    /// The source's own head: the anchor before the first apply, when the
    /// devnet has no cursor yet (nothing applied to reflect).
    fn head(&self) -> Fut<'_, mirror_anvil::Result<u64>>;

    /// The block's header: identity (number/hash/parent) plus the context the
    /// applier aligns to (timestamp, coinbase, prevRandao, gas limit, base
    /// fee). The official type — no mirror-owned block shape.
    fn header(&self, n: u64) -> Fut<'_, mirror_anvil::Result<alloy_rpc_types_eth::Header>>;

    fn balance_at(&self, address: Address, block: u64) -> Fut<'_, mirror_anvil::Result<U256>>;

    fn nonce_at(&self, address: Address, block: u64) -> Fut<'_, mirror_anvil::Result<u64>>;

    /// A storage word: the wire form (32 bytes), converted at the write site.
    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> Fut<'_, mirror_anvil::Result<B256>>;

    fn code_at(&self, address: Address, block: u64) -> Fut<'_, mirror_anvil::Result<alloy_primitives::Bytes>>;

    /// Read one key. The default composes the four typed reads, so a source
    /// implements only the primitives it can serve; the composer reads its
    /// plan through this seam (one call shape for every key kind).
    fn read_key(&self, key: mirror_anvil::Key, block: u64) -> Fut<'_, mirror_anvil::Result<mirror_anvil::Value>> {
        Box::pin(async move {
            Ok(match key {
                mirror_anvil::Key::Storage { address, slot } => {
                    let word = self.storage_at(address, slot, block).await?;
                    mirror_anvil::Value::Storage(U256::from_be_bytes(word.0))
                }
                mirror_anvil::Key::Balance(address) => {
                    mirror_anvil::Value::Balance(self.balance_at(address, block).await?)
                }
                mirror_anvil::Key::Nonce(address) => {
                    mirror_anvil::Value::Nonce(self.nonce_at(address, block).await?)
                }
                mirror_anvil::Key::Code(address) => {
                    mirror_anvil::Value::Code(self.code_at(address, block).await?)
                }
            })
        })
    }

    /// The native BAL for block `n`, when the source records one (EIP-7928).
    /// Default: none — the composer falls to the synthesized rung (the
    /// capability flag selects the rung; this answers per block, because a
    /// node can serve some blocks and not others).
    fn bal(&self, n: u64) -> Fut<'_, mirror_anvil::Result<Option<alloy_eips::eip7928::BlockAccessList>>> {
        let _ = n;
        Box::pin(async move { Ok(None) })
    }

    /// The window's touches: the addresses and logs the declared closures
    /// plan from, plus any changed keys the source already carries values for
    /// (the trace rung). Every source implements this — it is the synthesized
    /// rung's data side.
    fn touched(&self, from: u64, to: u64) -> Fut<'_, mirror_anvil::Result<crate::closure::TouchedSet>>;

    /// What this source's touched rung is (`Trace` for a trace-based source,
    /// `Poll` for the envelope rung): the window BAL's `kind` label.
    fn touched_kind(&self) -> crate::bal::DiffKind;
}

impl Capabilities {
    /// Verify the method-presence flags against the live endpoint and return
    /// the flags that disagree with the declaration (empty = all agree). The
    /// engines call this once at boot and log any disagreement: a declared
    /// capability that the endpoint does not serve is a misconfiguration,
    /// named at the only moment it is cheap to see.
    ///
    /// Each probe targets a block that does not exist yet: a method that
    /// exists answers an error that is *not* `-32601` (or a null result), a
    /// method that does not answers exactly that. No traces or BALs are
    /// actually fetched — the probe never pays a full trace's cost.
    ///
    /// Behavioral capabilities (`historical`, `heads_ws`) are not probed:
    /// "the block parameter is ignored" cannot be distinguished from "the
    /// value did not change" by a read. They stay declarations with their
    /// measurements, enforced at the read path (the venue source refuses a
    /// non-latest read rather than silently answering a newer value).
    pub async fn verify_methods(
        &self,
        rpc: &mirror_anvil::Rpc,
        future: u64,
    ) -> Vec<&'static str> {
        use alloy_primitives::U64;
        let mut disagree = Vec::new();

        let bal = rpc
            .call::<serde_json::Value>("eth_getBlockAccessList", (U64::from(future),))
            .await;
        if self.native_bal != method_present(bal) {
            disagree.push("native_bal");
        }

        let trace = rpc
            .call::<serde_json::Value>(
                "debug_traceBlockByNumber",
                (
                    U64::from(future),
                    serde_json::json!({"tracer": "prestateTracer", "tracerConfig": {"diffMode": true}}),
                ),
            )
            .await;
        if self.traces != method_present(trace) {
            disagree.push("traces");
        }

        let system = rpc
            .call::<serde_json::Value>("eth_getSystemTxsByBlockNumber", (U64::from(future),))
            .await;
        if self.system_txs != method_present(system) {
            disagree.push("system_txs");
        }

        disagree
    }
}

/// Did the method exist? Anything but `-32601` means yes (a missing block is
/// an answer from a method that exists).
fn method_present<T>(r: mirror_anvil::Result<T>) -> bool {
    match r {
        Ok(_) => true,
        Err(e) => !e.is_method_not_found(),
    }
}

/// A source over a raw RPC: the reads are forwarded verbatim and the
/// capabilities are the caller's declaration. For reads that are
/// latest-by-design (the venue's poll composer reads the public RPC at
/// latest, where the pin guard would refuse every read) and for tools; a
/// profile whose reads carry invariants (the venue's pin-exact
/// [`crate::source::Source`] for unshadow restores) keeps its own
/// implementation.
pub struct RawSource {
    rpc: Arc<mirror_anvil::Rpc>,
    capabilities: Capabilities,
}

impl RawSource {
    /// The handle is shared (`Arc`): one client per endpoint, referenced by
    /// every object that reads through it.
    pub fn new(rpc: Arc<mirror_anvil::Rpc>, capabilities: Capabilities) -> Self {
        Self { rpc, capabilities }
    }

    /// The raw client, for the calls the state seam does not model
    /// (envelopes, receipts, system txs).
    pub fn rpc(&self) -> &mirror_anvil::Rpc {
        &self.rpc
    }
}

impl Source for RawSource {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn head(&self) -> Fut<'_, mirror_anvil::Result<u64>> {
        Box::pin(self.rpc.block_number())
    }

    fn header(&self, n: u64) -> Fut<'_, mirror_anvil::Result<alloy_rpc_types_eth::Header>> {
        Box::pin(async move { Ok(self.rpc.block_by_number(n, false).await?.header) })
    }

    fn balance_at(&self, address: Address, block: u64) -> Fut<'_, mirror_anvil::Result<U256>> {
        Box::pin(self.rpc.balance_at(address, block))
    }

    fn nonce_at(&self, address: Address, block: u64) -> Fut<'_, mirror_anvil::Result<u64>> {
        Box::pin(self.rpc.nonce_at(address, block))
    }

    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> Fut<'_, mirror_anvil::Result<B256>> {
        Box::pin(self.rpc.storage_at(address, slot, block))
    }

    fn code_at(
        &self,
        address: Address,
        block: u64,
    ) -> Fut<'_, mirror_anvil::Result<alloy_primitives::Bytes>> {
        Box::pin(self.rpc.code_at(address, block))
    }

    /// The envelope rung: the generic EVM touches (txs, receipts, and — when
    /// declared — HyperEVM's system txs), no values (the state reads fill
    /// them).
    fn touched(
        &self,
        from: u64,
        to: u64,
    ) -> Fut<'_, mirror_anvil::Result<crate::closure::TouchedSet>> {
        let system_txs = self.capabilities.system_txs;
        Box::pin(crate::bal::envelope_touches(&self.rpc, from, to, system_txs))
    }

    fn touched_kind(&self) -> crate::bal::DiffKind {
        crate::bal::DiffKind::Poll
    }
}
