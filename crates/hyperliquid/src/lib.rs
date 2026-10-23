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
//! Clocks: `block` = the **V height** (HyperEVM blocks; the venue profile has
//! no second chain); the simulator's clock is its served `time` (ms,
//! self-advancing), never a block label.
pub mod api;
pub mod bridge;
pub mod ctx;
pub mod driver;
pub mod error;
pub mod feed;
pub mod http;
pub mod metrics;
pub mod sim;
pub mod venue_source;
pub mod wire;

pub use ctx::Ctx;
pub use error::{Error, Result};

/// Wall-clock milliseconds (the wire's `time` fields and the sim clock's
/// base) — the shared definition lives in `mirror-config`'s boot module; this
/// re-export keeps `crate::now_ms()` the one spelling at the call sites.
pub use mirror_config::boot::now_ms;

/// The Venue Mirror engine's entrypoint (the bin is the shell over this; the
/// same shell as `mainnet-engine`, R3).
///
/// Boot order (plan §7.2): wait for anvil V, assert the chain, reconcile the
/// registry (reset detection + the epoch rule; the reconcile also anchors the
/// epoch pin before any writes exist — the counterpart of the ethereum
/// engine's explicit boot anchor), run the at-pin probes, then serve the HTTP
/// surface and the driver loop. A boot fault is fatal: a driver over the wrong
/// chain or a broken registry must not serve.
///
/// Shutdown mirrors the ethereum engine's: the signal aborts the tasks; every
/// mutation persists its state as it happens (shadow registry, cursor), so
/// there is nothing to drain.
pub async fn main_entry(args: Vec<String>) -> Result<()> {
    mirror_config::boot::init_logging();
    let config = mirror_config::boot::service_config_or_exit(&args);
    run(config).await
}

async fn run(config: std::path::PathBuf) -> Result<()> {
    // One instance file, many readers: this pod provides only the venue
    // engine's wiring. `inspect` reports the other services' unresolved
    // references instead of failing on them; the engine's own sections are
    // then required here, by name (the cctp bridge's pattern).
    let (cfg, unresolved) = mirror_config::Instance::inspect(&config)?;
    if let (Some(chain), Some(engine)) = (cfg.chains.venue.as_ref(), cfg.services.engine.as_ref())
    {
        let state_dir = engine.state_dir.to_string_lossy().to_string();
        let v_url = engine.anvil.v_url.as_deref().unwrap_or_default();
        mirror_config::require_resolved_logged(
            "venue engine wiring",
            &[&chain.source_url, v_url, &state_dir],
            &unresolved,
        )?;
    }
    let (source_url, v_url) = cfg.venue_urls()?;
    let ctx = std::sync::Arc::new(Ctx::new(cfg, &source_url, &v_url)?);

    driver::boot(&ctx).await?;
    let mut tasks = Vec::new();
    {
        let ctx = ctx.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = http::serve(ctx).await {
                tracing::error!(error = %e, "http server exited");
            }
        }));
    }
    {
        let ctx = ctx.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = driver::run(ctx).await {
                tracing::error!(error = %e, "venue driver exited");
            }
        }));
    }

    mirror_config::boot::shutdown_signal().await;
    tracing::info!("shutdown signal received");
    for t in tasks {
        t.abort();
    }
    Ok(())
}

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
