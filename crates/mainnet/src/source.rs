//! `RethSource: Source` — the mainnet profile's source: reth, the local fork
//! source (BALs, traces, a 10,064-block state window; WS heads available).
//!
//! Both composer rungs come from here: [`Source::bal`] is the native rung
//! (`eth_getBlockAccessList`), [`Source::touched`] the trace rung — the
//! prestateTracer diff is already a change document, so the touched set is
//! values-only (no addresses to plan: the trace carries the post-values, and
//! re-reading them would be a wasted round trip).
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::Header;
use mirror_devnet::bal::DiffKind;
use mirror_devnet::closure::TouchedSet;
use mirror_devnet::source::{Capabilities, Fut, Source};
use std::sync::Arc;

/// The reth handle is shared (`Arc`): one client per endpoint, referenced by
/// the context and the source objects (the source is chosen at boot and never
/// re-created — rule 15).
pub struct RethSource {
    reth: Arc<crate::rpc::Rpc>,
}

impl RethSource {
    pub fn new(reth: Arc<crate::rpc::Rpc>) -> Self {
        Self { reth }
    }
}

impl Source for RethSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_bal: true,
            traces: true,
            historical: true,
            heads_ws: true,
            system_txs: false,
            // Ethereum has no system transactions; nothing to serve receipts for.
            system_tx_receipts: false,
        }
    }

    fn head(&self) -> Fut<'_, mirror_anvil::Result<u64>> {
        Box::pin(self.reth.block_number())
    }

    fn header(&self, n: u64) -> Fut<'_, mirror_anvil::Result<Header>> {
        Box::pin(async move {
            let block = self.reth.block_by_number(n, false).await?;
            Ok(block.header)
        })
    }

    fn balance_at(&self, address: Address, block: u64) -> Fut<'_, mirror_anvil::Result<U256>> {
        Box::pin(self.reth.balance_at(address, block))
    }

    fn nonce_at(&self, address: Address, block: u64) -> Fut<'_, mirror_anvil::Result<u64>> {
        Box::pin(self.reth.nonce_at(address, block))
    }

    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> Fut<'_, mirror_anvil::Result<B256>> {
        Box::pin(self.reth.storage_at(address, slot, block))
    }

    fn code_at(
        &self,
        address: Address,
        block: u64,
    ) -> Fut<'_, mirror_anvil::Result<alloy_primitives::Bytes>> {
        Box::pin(self.reth.code_at(address, block))
    }

    /// The native rung: reth records EIP-7928 BALs (a block without one — an
    /// older range, a node without the recording — answers `None` and the
    /// composer falls back per block).
    fn bal(
        &self,
        n: u64,
    ) -> Fut<'_, mirror_anvil::Result<Option<alloy_eips::eip7928::BlockAccessList>>> {
        Box::pin(self.reth.block_access_list(n))
    }

    /// The trace rung: the prestateTracer diff per block, as values (the
    /// change document the trace already is).
    ///
    /// Nodes without block-level tracing answer an *empty list* rather than an
    /// error (measured: anvil returns `[]` for every tracer variant), so a
    /// block with transactions and an empty trace falls back to the tx-level
    /// tracer — the scanner's own path — instead of silently composing
    /// nothing.
    fn touched(&self, from: u64, to: u64) -> Fut<'_, mirror_anvil::Result<TouchedSet>> {
        Box::pin(async move {
            let mut touched = TouchedSet::default();
            for n in (from + 1)..=to {
                let traces = self.reth.trace_block_prestate(n).await?;
                if traces.is_empty() {
                    for w in self.trace_block_per_tx(n).await? {
                        touched.value(w);
                    }
                } else {
                    for w in mirror_anvil::flatten_trace_block(&traces) {
                        touched.value(w);
                    }
                }
            }
            Ok(touched)
        })
    }

    fn touched_kind(&self) -> DiffKind {
        DiffKind::Trace
    }
}

impl RethSource {
    /// The per-tx prestate traces of a block, merged (the fallback for nodes
    /// without block-level tracing).
    async fn trace_block_per_tx(&self, n: u64) -> mirror_anvil::Result<Vec<mirror_anvil::Write>> {
        use alloy_network_primitives::TransactionResponse as _;
        let block = self.reth.block_by_number(n, true).await?;
        let mut out = Vec::new();
        for tx in mirror_devnet::scan::block_txs(&block) {
            let trace = self.reth.trace_tx_prestate(tx.tx_hash()).await?;
            out.extend(mirror_anvil::flatten_trace(&trace));
        }
        Ok(out)
    }
}
