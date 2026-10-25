//! `mirror-mainnet` — the Mainnet Mirror's engine: keep T's watch-list
//! state fresh from EIP-7928 BALs and expose a small admin surface. One
//! Anvil (T), one source (reth).
//!
//! T is a fork-mode anvil of Ethereum (chain id 1): it starts as reth's state
//! at the pin, and the applier **applies** the parent's blocks from there —
//! reth's native EIP-7928 BALs (the trace rung for a block that carries
//! none), the declared watch-list turned into concrete keys, and the changes
//! landed on T under the same COW filter the venue mirror uses (devnet-owned
//! keys are never clobbered), with T's clock aligned to the applied parent
//! block. What the venue profile supplies as poll windows (the HL public
//! API), the mainnet supplies as one parent block per applied block; the
//! spine's seams make the two the same pipeline.
//!
//! The substrate's half is delivered by the shared crates:
//!
//! - the devnet plane (`mirror-devnet`): the shadow registry (COW), the
//!   journal, the cursor state with `epoch_pin`, the admin surface, the
//!   chain-generic scanner, the probe runner, the declared closures;
//! - the anvil boundary (`mirror-anvil`): the typed client, the measured
//!   padding/batching rules, the trace flattening;
//! - the driver (this crate): boot asserts, reset detection, the epoch rule,
//!   the per-block probes, the steady-state apply, the shadow scan, and the
//!   source-sync top-up.
//!
//! The feed bridge (the parent's price feeds onto T's mock aggregators) is
//! the mainnet's own half and lands on top of this driver; the venue project
//! is never compiled here (the closure matrix enforces it).
//! Clocks: `block` = the **L1 parent height** (the applied mainnet block);
//! `head` = the compared chain's head (T); `block_txs` = a count, not a clock;
//! `age_ms` = the feed bridge's confirmation-window age, not a clock.
mod apply;
mod ctx;
mod driver;
mod error;
mod feed_bridge;
mod http;
mod metrics;
mod profile;
mod reset;
mod selftest;
mod service;
mod source;
mod source_sync;

pub use error::{Error, Result};
/// The `/status` document's typed shape (the shared fields flattened beside
/// this profile's own).
pub use http::StatusResponse;
pub use service::main_entry;

/// Wall-clock milliseconds (the feed bridge's confirmation-window ages) — the
/// shared definition lives in `mirror_config`'s boot module; this crate-root
/// binding keeps `crate::now_ms()` the one spelling at the call sites. Not
/// re-exported: the entry points are this crate's public surface.
use mirror_config::boot::now_ms;

#[cfg(test)]
mod design_assertions {
    /// The Rust-for-Rustaceans pin (plan §11.3 rule 11): every public type is
    /// Send + Sync + Sized + Unpin by construction, asserted at compile time.
    fn is_normal<T: Send + Sync + Sized + Unpin>() {}

    #[test]
    fn public_types_are_normal() {
        is_normal::<crate::ctx::Ctx>();
        is_normal::<crate::metrics::MainnetMetrics>();
        is_normal::<crate::feed_bridge::FeedBridge>();
        is_normal::<crate::feed_bridge::FeedStatus>();
        is_normal::<crate::feed_bridge::FeedBridgeMetrics>();
        is_normal::<crate::source_sync::beacon::BeaconClient>();
        is_normal::<crate::source_sync::beacon::TopUpBody>();
        is_normal::<crate::source_sync::beacon::BeaconBlock>();
        is_normal::<crate::source_sync::engine_api::EngineClient>();
        is_normal::<crate::source::RethSource>();
        is_normal::<crate::reset::CursorHash>();
        is_normal::<crate::profile::Wiring>();
        is_normal::<crate::profile::Deps>();
        is_normal::<crate::selftest::Targets>();
        is_normal::<crate::http::StatusResponse>();
    }
}
