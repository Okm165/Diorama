//! `mirror-anvil` — the anvil control plane: the typed RPC client (cheatcodes,
//! batching, measured error recognition), the measured padding rules, and the
//! prestateTracer fallback. No chain semantics live here.
mod backoff;
mod bal;
mod client;
mod error;
mod pad;
mod trace;
mod writes;

pub use backoff::Backoff;
pub use bal::*;
pub use client::*;
pub use error::*;
pub use pad::*;
pub use trace::*;
pub use writes::*;

#[cfg(test)]
mod design_assertions {
    /// The Rust-for-Rustaceans pin (plan §11.3 rule 11).
    fn is_normal<T: Send + Sync + Sized + Unpin>() {}

    #[test]
    fn public_types_are_normal() {
        is_normal::<crate::Backoff>();
        is_normal::<crate::Rpc>();
        is_normal::<crate::Call>();
        is_normal::<crate::Error>();
        is_normal::<crate::Write>();
    }
}
