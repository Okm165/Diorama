//! `RethSource: Source` — the mainnet profile's source: reth, the local fork
//! source (BALs, traces, a 10,064-block state window; WS heads available).
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::Header;
use mirror_devnet::source::{Capabilities, Fut, Source};

/// The reth client is cloned into the source (cheap: reqwest's client is
/// `Arc`-backed); the source is chosen at boot and never re-created (rule 15).
pub struct RethSource {
    reth: crate::rpc::Rpc,
}

impl RethSource {
    pub fn new(reth: crate::rpc::Rpc) -> Self {
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
}
