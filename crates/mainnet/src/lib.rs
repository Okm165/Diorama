//! `mirror-mainnet` — the Mainnet Mirror's engine: keep T's watch-list
//! state fresh from EIP-7928 BALs and expose a small admin surface. One
//! Anvil (T), one source (reth).
//!
//! Clocks: `block` = the **L1 parent height** (the applied mainnet block);
//! `head` = the compared chain's head (T); `block_txs` = a count, not a clock;
//! `age_ms` = the feed bridge's confirmation-window age, not a clock.
mod beacon;
mod ctx;
mod apply;
mod driver;
mod drift;
mod engine_api;
mod error;
mod feed_bridge;
mod http;
mod metrics;
mod profile;
mod selftest;
mod service;
mod source;
mod source_sync;

pub use error::{Error, Result};
pub use service::main_entry;

#[cfg(test)]
mod design_assertions {
    /// Public types stay Send + Sync + Sized + Unpin (Rust-for-Rustaceans);
    /// the engine hands `Ctx` to every loop and the HTTP surface.
    #[test]
    fn design_assertions() {
        fn is_normal<T: Send + Sync + Sized + Unpin>() {}
        is_normal::<crate::ctx::Ctx>();
    }
}
