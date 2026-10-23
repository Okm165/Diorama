//! `mirror-devnet` — the devnet plane: shadow registry, journal, state, the
//! admin surface, deal, probes, metrics, and the unshadow re-sync `Source`.
//! The devnet axioms live here; the projects compose these with chain clients.
pub mod admin;
pub mod deal;
pub mod epoch;
pub mod error;
pub mod journal;
pub mod metrics;
pub mod plane;
pub mod probes;
pub mod scan;
pub mod shadow;
pub mod shared;
pub mod source;
pub mod state;
pub mod store;

pub use error::{Error, Result};
pub use shared::Shared;
