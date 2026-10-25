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

/// The JSON content type every transport in the workspace sends (and the
/// venue's HTTP surface answers): one home — a typo here would break a
/// client silently (the repeated-literal reading's finding).
pub const JSON_CONTENT_TYPE: &str = "application/json";

/// The balance read the transport and the CCTP helpers share.
pub const ETH_GET_BALANCE: &str = "eth_getBalance";

/// The `latest` block tag: the read the devnet's scanners and the services'
/// chain reads ask for (the repeated-literal reading's finding).
pub const LATEST: &str = "latest";

/// The block-level prestate tracer's method: the trace rung's call and the
/// capability audit's check, one spelling.
pub const TRACE_BLOCK_METHOD: &str = "debug_traceBlockByNumber";
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
