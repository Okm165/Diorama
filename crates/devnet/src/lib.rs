//! `mirror-devnet` — the devnet plane: shadow registry, journal, state, metrics.
//! The devnet axioms live here; the engine composes these with chain clients.
pub mod error;
pub mod journal;
pub mod metrics;
pub mod shadow;
pub mod shared;
pub mod state;

pub use error::{Error, Result};
pub use shared::Shared;
