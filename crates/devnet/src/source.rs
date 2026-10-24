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
}
