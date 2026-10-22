//! The unshadow re-sync source (plan §11.3): where a handed-back key's truth
//! comes from.
//!
//! One implementation per binary, chosen at boot — the trait is deliberately
//! `Arc<dyn Source>` (rule 15: a boundary, not a hot loop; the two projects
//! never monomorphize each other's types). `EthereumSource` reads reth at the
//! applied cursor; the venue profile's `VenueSource` (R3) reads the fork
//! source at the pin. The trait is exactly what a hand-back asks for: the
//! source's head (the anchor before the first apply) and the three account
//! reads an unshadow restores.
use alloy_primitives::{Address, B256, U256};
use std::future::Future;
use std::pin::Pin;

/// A boxed future: the boundary is dynamic (rule 15), so each call boxes once.
/// These are admin-path reads (a handful per hand-back), never per-block work.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The truth an unshadow restores from.
pub trait Source: Send + Sync {
    /// The source's own head: the anchor before the first apply, when the
    /// devnet has no cursor yet (nothing applied to reflect).
    fn head(&self) -> Fut<'_, mirror_anvil::Result<u64>>;

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
