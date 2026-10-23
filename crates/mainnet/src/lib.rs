//! mainnet-engine: replay mainnet blocks onto M, keep T's watch-list state fresh
//! from EIP-7928 BALs, verify every block, and expose a small admin surface.
//! Clocks: `block` = the **L1 parent height** (the replayed/applied mainnet
//! block); `head` = the compared chain's head (M in the replay, T in the
//! applier); `block_txs` = a count, not a clock; `age_ms` = the feed bridge's
//! confirmation-window age, not a clock. M/T heights are named (`m_head`,
//! `t_head`, `cursor_t` = T-height) and never logged bare.
mod bal;
mod blob;
mod ctx;
mod diff;
mod driver;
mod drift;
mod engine_api;
mod error;
mod l1_source;
pub mod feed_bridge;
mod http;
mod metrics;
mod replay;
mod selftest;
mod source_sync;
mod verify;

// R1/R2 extraction shims: the frozen `crate::rpc::*`, `crate::bal::*` and
// `crate::shadow|journal|state` paths keep resolving; the code lives in
// `mirror-anvil` / `mirror-devnet` (zero call-site churn). `metrics` is the
// profile's own module here (`mirror_devnet::metrics` is imported by name).
pub use mirror_anvil as rpc;
pub use mirror_devnet::{journal, shadow, state};

pub use error::{Error, Result};
pub use service::main_entry;

mod service;

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
