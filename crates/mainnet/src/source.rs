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
use futures_util::future::BoxFuture;
use mirror_devnet::source::{Capabilities, RawSource, Source};
use std::sync::Arc;

/// What reth serves (measured): the native BAL rung, traces, a 10,064-block
/// state window, WS heads; no system transactions (Ethereum has none).
pub(crate) const DECLARED: Capabilities = Capabilities {
    native_bal: true,
    traces: true,
    historical: true,
    heads_ws: true,
    system_txs: false,
    system_tx_receipts: false,
};

/// The profile's truth source: reth, the native BAL rung with the trace
/// fallback.
pub(crate) fn reth(rpc: Arc<mirror_anvil::Rpc>) -> Arc<dyn Source> {
    Arc::new(RethSource::new(rpc))
}

/// The reth handle is shared (`Arc`): one client per endpoint, referenced by
/// the context and the source objects (the source is chosen at boot and never
/// re-created — rule 15). The raw reads are the spine's [`RawSource`]; this
/// type adds the mainnet's rungs (the native BAL with its commitment check,
/// the trace rung) and its declaration.
pub struct RethSource {
    inner: RawSource,
    reth: Arc<mirror_anvil::Rpc>,
}

impl RethSource {
    pub fn new(reth: Arc<mirror_anvil::Rpc>) -> Self {
        Self {
            inner: RawSource::new(reth.clone(), DECLARED),
            reth,
        }
    }

    /// The per-tx prestate traces of a block, merged (the fallback for nodes
    /// without block-level tracing).
    async fn trace_block_per_tx(&self, n: u64) -> mirror_anvil::Result<Vec<mirror_anvil::Write>> {
        use alloy_network_primitives::TransactionResponse as _;
        let block = self.reth.block_by_number(n, mirror_anvil::Txs::Full).await?;
        let mut out = Vec::new();
        for tx in mirror_devnet::scan::block_txs(&block) {
            let trace = self.reth.trace_tx_prestate(tx.tx_hash()).await?;
            out.extend(mirror_anvil::flatten_trace(&trace));
        }
        Ok(out)
    }
}

impl Source for RethSource {
    fn capabilities(&self) -> Capabilities {
        DECLARED
    }

    // The raw reads are the spine's (one home for how a source read is
    // performed); the rungs below are this source's own.
    fn head(&self) -> BoxFuture<'_, mirror_anvil::Result<u64>> {
        self.inner.head()
    }

    fn header(&self, n: u64) -> BoxFuture<'_, mirror_anvil::Result<Header>> {
        self.inner.header(n)
    }

    fn balance_at(&self, address: Address, block: u64) -> BoxFuture<'_, mirror_anvil::Result<U256>> {
        self.inner.balance_at(address, block)
    }

    fn nonce_at(&self, address: Address, block: u64) -> BoxFuture<'_, mirror_anvil::Result<u64>> {
        self.inner.nonce_at(address, block)
    }

    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<B256>> {
        self.inner.storage_at(address, slot, block)
    }

    fn code_at(
        &self,
        address: Address,
        block: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<alloy_primitives::Bytes>> {
        self.inner.code_at(address, block)
    }

    /// The native rung: reth records EIP-7928 BALs (a block without one — an
    /// older range, a node without the recording — answers `None` and the
    /// composer falls back per block).
    fn bal(
        &self,
        n: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<Option<alloy_eips::eip7928::BlockAccessList>>> {
        Box::pin(async move {
            // TRUST: the native list is the parent's own record. The guard:
            // EIP-7928's commitment — the header's `block_access_list_hash`
            // must equal keccak256(rlp(BAL)) (`verify_bal_commitment`), so a
            // list the header does not commit to is refused, never applied.
            //
            // EIP-7928: the header commits to the list
            // (`block_access_list_hash = keccak256(rlp(BAL))`), so the native
            // retrieval is authenticated against the block's own commitment
            // whenever the block carries one. An *absent* list (null, or an
            // endpoint without the method) is the per-block fallback's case; a
            // *mismatched* one is refused — never applied.
            let Some(bal) = self.reth.block_access_list(n).await? else {
                return Ok(None);
            };
            let header = self.header(n).await?;
            mirror_devnet::bal::verify_bal_commitment(header.inner.block_access_list_hash, &bal)
                .map_err(|detail| mirror_anvil::Error::Corrupt {
                    what: "block access list",
                    detail: format!("block {n}: {detail}"),
                })?;
            Ok(Some(bal))
        })
    }

    /// The trace rung: the prestateTracer diff per block, as values (the
    /// change document the trace already is).
    ///
    /// Nodes without block-level tracing answer an *empty list* rather than an
    /// error (measured: anvil returns `[]` for every tracer variant), so a
    /// block with transactions and an empty trace falls back to the tx-level
    /// tracer — the scanner's own path — instead of silently composing
    /// nothing.
    fn touched(&self, from: u64, to: u64) -> BoxFuture<'_, mirror_anvil::Result<TouchedSet>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The reth declaration, pinned: the boot audit compares the live
    /// endpoint against exactly these flags, and the trace rung is the touched
    /// set's kind (the venue's
    /// `the_public_source_declares_the_measured_capabilities` pins its own
    /// declaration the same way).
    #[test]
    fn the_reth_source_declares_the_measured_capabilities() {
        let rpc = Arc::new(
            mirror_anvil::Rpc::new("http://127.0.0.1:1", std::time::Duration::from_secs(1)).unwrap(),
        );
        let source = RethSource::new(rpc);
        assert_eq!(source.capabilities(), DECLARED);
        assert_eq!(
            DECLARED,
            Capabilities {
                native_bal: true,
                traces: true,
                historical: true,
                heads_ws: true,
                system_txs: false,
                system_tx_receipts: false,
            },
            "the declaration the boot audit compares against"
        );
        assert_eq!(source.touched_kind(), DiffKind::Trace);
    }
}
