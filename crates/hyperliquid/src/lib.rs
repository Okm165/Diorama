//! `mirror-hyperliquid` — the Venue Mirror's engine: the fork driver over
//! anvil V.
//!
//! V is a fork-mode anvil of HyperEVM (chain id 999): static between re-pins,
//! so the ethereum profile's parent stream machinery (replay, BAL diffs, the
//! prepend rule, the clock offset) has no counterpart here. What remains is
//! the substrate's half, delivered by the shared crates:
//!
//! - the devnet plane (`mirror-devnet`): the shadow registry (COW), the
//!   journal, the cursor state with `epoch_pin`, the admin surface, the
//!   chain-generic scanner, the probe runner;
//! - the anvil boundary (`mirror-anvil`): the typed client, the measured
//!   padding/batching rules, the trace flattening;
//! - the driver (this crate): boot asserts, reset detection, the epoch rule,
//!   probes-at-pin, and the steady-state scan.
//!
//! The simulator (market/account planes), the daloc-facing API and the
//! bridges are the venue's own half and land on top of this driver; the
//! ethereum stack is never compiled here (the closure matrix enforces it).
pub mod api;
pub mod bridge;
pub mod ctx;
pub mod driver;
pub mod error;
pub mod feed;
pub mod http;
pub mod sim;
pub mod store;
pub mod venue_source;
pub mod wire;

pub use ctx::Ctx;
pub use error::{Error, Result};

/// Wall-clock milliseconds (the wire's `time` fields and the sim clock's
/// base) — the shared definition lives in `mirror-config`'s boot module; this
/// re-export keeps `crate::now_ms()` the one spelling at the call sites.
pub use mirror_config::boot::now_ms;

#[cfg(test)]
mod design_assertions {
    /// The Rust-for-Rustaceans pin (plan §11.3 rule 11): every public type is
    /// Send + Sync + Sized + Unpin by construction, asserted at compile time.
    fn is_normal<T: Send + Sync + Sized + Unpin>() {}

    #[test]
    fn public_types_are_normal() {
        is_normal::<crate::ctx::VenueMetrics>();
        is_normal::<crate::feed::MarketFeed>();
        is_normal::<crate::feed::Overrides>();
        is_normal::<crate::feed::PriceOverride>();
        is_normal::<crate::feed::budget::Budget>();
        is_normal::<crate::feed::metrics::FeedMetrics>();
        is_normal::<crate::feed::rest::RestClient>();
        is_normal::<crate::feed::ws::WsClient>();
        is_normal::<crate::sim::Sim>();
        is_normal::<crate::sim::SimState>();
        is_normal::<crate::sim::SimMetrics>();
        is_normal::<crate::sim::DealClass>();
        is_normal::<crate::sim::AdvanceOutcome>();
        is_normal::<crate::sim::account::Account>();
        is_normal::<crate::sim::account::AccountSeed>();
        is_normal::<crate::sim::account::Position>();
        is_normal::<crate::sim::account::Liquidation>();
        is_normal::<crate::api::ApiState>();
        is_normal::<crate::bridge::Bridge>();
        is_normal::<crate::bridge::metrics::BridgeMetrics>();
        is_normal::<crate::wire::InfoRequest>();
        is_normal::<crate::wire::ClearinghouseState>();
        is_normal::<crate::wire::MetaAndAssetCtxs>();
        is_normal::<crate::wire::L2Book>();
        is_normal::<crate::wire::SpotMeta>();
    }
}
