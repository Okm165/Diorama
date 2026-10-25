//! `mirror-venue` — the Venue Mirror's engine: the fork driver over
//! anvil V.
//!
//! V is a fork-mode anvil of HyperEVM (chain id 999): it starts as the source's
//! state at the pin, and the driver **follows** the HL public API from there —
//! the generic composer synthesizes a BAL from the window's envelopes (txs,
//! receipts, system txs), the declared closures turn it into concrete keys, and
//! the changed keys land on V under the same COW filter the mainnet mirror
//! uses (devnet-owned keys are never clobbered). What the mainnet profile
//! supplies as a parent stream (one block per applied parent block, aligned
//! clocks), the venue supplies as poll windows; the spine's seams make the two
//! the same pipeline.
//!
//! The substrate's half is delivered by the shared crates:
//!
//! - the devnet plane (`mirror-devnet`): the shadow registry (COW), the
//!   journal, the cursor state with `epoch_pin`, the admin surface, the
//!   chain-generic scanner, the probe runner, the declared closures;
//! - the anvil boundary (`mirror-anvil`): the typed client, the measured
//!   padding/batching rules, the trace flattening;
//! - the driver (this crate): boot asserts, reset detection, the epoch rule,
//!   probes-at-pin, the steady-state scan, and the source follow.
//!
//! The simulator (market/account planes), the daloc-facing API and the
//! bridges are the venue's own half and land on top of this driver; the
//! mainnet project is never compiled here (the closure matrix enforces it).
//! Clocks: `block` = the **V height** (HyperEVM blocks; the venue profile has
//! no second chain); `pin` = V's fork base (a block number, not a clock — the
//! epoch key); the simulator's clock is its served `time` (ms,
//! self-advancing), never a block label; `age_ms` = the served book's
//! staleness age (wall ms since the last live context update — the freshness
//! gate's figure, not a clock).
mod api;
mod bridge;
mod ctx;
mod driver;
mod error;
mod feed;
mod follow;
mod http;
mod metrics;
mod profile;
mod reset;
mod service;
mod sim;
mod source;
mod wire;

pub use error::{Error, Result};
pub use service::{main_entry, sim_entry};

/// Wall-clock milliseconds (the wire's `time` fields and the sim clock's
/// base) — the shared definition lives in `mirror-config`'s boot module; this
/// crate-root binding keeps `crate::now_ms()` the one spelling at the call
/// sites. Not re-exported: the entry points are this crate's public surface.
use mirror_config::boot::now_ms;

#[cfg(test)]
mod design_assertions {
    /// The Rust-for-Rustaceans pin (plan §11.3 rule 11): every public type is
    /// Send + Sync + Sized + Unpin by construction, asserted at compile time.
    fn is_normal<T: Send + Sync + Sized + Unpin>() {}

    #[test]
    fn public_types_are_normal() {
        is_normal::<crate::metrics::VenueMetrics>();
        is_normal::<crate::feed::MarketFeed>();
        is_normal::<crate::feed::Overrides>();
        is_normal::<crate::feed::PriceOverride>();
        is_normal::<crate::feed::budget::Budget>();
        is_normal::<crate::feed::metrics::FeedMetrics>();
        is_normal::<crate::feed::rest::RestClient>();
        is_normal::<crate::feed::ws::WsClient>();
        is_normal::<crate::sim::Sim>();
        is_normal::<crate::sim::SimState>();
        is_normal::<crate::sim::metrics::SimMetrics>();
        is_normal::<crate::sim::DealClass>();
        is_normal::<crate::sim::AdvanceOutcome>();
        is_normal::<crate::sim::account::Account>();
        is_normal::<crate::sim::seed::AccountSeed>();
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
