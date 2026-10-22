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
pub mod ctx;
pub mod driver;
pub mod error;
pub mod http;
pub mod venue_source;

pub use ctx::Ctx;
pub use error::{Error, Result};
