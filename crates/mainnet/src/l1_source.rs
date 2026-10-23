//! `L1Source: Source` — the mainnet profile's unshadow re-sync source:
//! reth, at the block the devnet's state reflects (the applied cursor; the
//! engine passes the cursor to the admin surface, which resolves the anchor).
use alloy_primitives::{Address, B256, U256};
use mirror_devnet::source::{Fut, Source};

/// The reth client is cloned into the source (cheap: reqwest's client is
/// `Arc`-backed); the source is chosen at boot and never re-created (rule 15).
pub struct L1Source {
    reth: crate::rpc::Rpc,
}

impl L1Source {
    pub fn new(reth: crate::rpc::Rpc) -> Self {
        Self { reth }
    }
}

impl Source for L1Source {
    fn head(&self) -> Fut<'_, mirror_anvil::Result<u64>> {
        Box::pin(self.reth.block_number())
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
