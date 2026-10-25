//! The engine's error taxonomy (boundary-discipline; §11.3 rule 1).
//!
//! Variants classify by **failure class**, never by call site; each carries the
//! structured fields an operator needs (endpoint, block, path). The
//! sub-boundaries keep their own vocabularies with their modules —
//! [`EngineApiError`] (reth authrpc), [`BeaconError`] (the beacon API) — and
//! compose here.
//!
//! Policy carriers: the devnet's `NoCursor` / `InvalidRewind` are admin
//! *request refusals* — `http.rs` maps them to 4xx while every other variant is
//! a server-side fault (5xx, or 503 for a poisoned registry).
use std::path::PathBuf;

pub use crate::source_sync::beacon::BeaconError;
pub use crate::source_sync::engine_api::EngineApiError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] mirror_config::ConfigError),

    #[error(transparent)]
    Rpc(mirror_anvil::Error),

    #[error(transparent)]
    Devnet(#[from] mirror_devnet::Error),

    #[error(transparent)]
    EngineApi(#[from] EngineApiError),

    #[error(transparent)]
    Beacon(#[from] BeaconError),

    /// Engine-local file IO (drift log, pin file).
    #[error("{what}: {path}")]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The HTTP listener could not bind.
    #[error("binding the http api at {addr}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    /// An anvil answered with a different chain id: the profile would fork the
    /// wrong chain (state, addresses and every measured fact follow from it).
    #[error("{anvil} is not the mainnet chain: chain id {got}, expected {want}")]
    WrongChain {
        anvil: &'static str,
        got: u64,
        want: u64,
    },

    /// The selftest's scratch anvil was not started with a fork pin.
    #[error("{chain} has no fork pin - start it with --fork-url and --fork-block-number")]
    NoForkPin { chain: &'static str },

    /// A phase of the selftest (apply of one block) failed; wraps the
    /// inner fault so the phase and the block are both named.
    #[error("{step} block {block}: {source}")]
    Step {
        step: &'static str,
        block: u64,
        #[source]
        source: Box<Error>,
    },

    // ---- applier (T) -----------------------------------------------------
    #[error("T head {t_head} is more than one block behind the parent cursor {n}")]
    ApplierBehind { t_head: u64, n: u64 },

    /// T was re-forked while this block was being applied (a re-pin raced the
    /// fetch): applying would land parent n's writes on the new base. The next
    /// pass reconciles — the cursor sits below the base, so the epoch rule
    /// clears the stale writes and the applier resumes from the new base.
    #[error("T was re-pinned during the apply of block {n}; the next pass reconciles")]
    ResetDuringApply { n: u64 },

    // ---- top-up sync -----------------------------------------------------
    #[error(
        "top-up gap: reth head {el_head}, next CL payload {number} at slot {slot} \
         (missing ancestors are outside the CL's retained history)"
    )]
    TopUpGap { el_head: u64, number: u64, slot: u64 },

    #[error("reth answered {status} for payload {number}: {message}")]
    PayloadRejected {
        number: u64,
        status: String,
        message: String,
    },

    // ---- the feed bridge (plan §7.1) --------------------------------------
    /// A feed-bridge admin call arrived while the bridge is not configured.
    #[error("the feed bridge is not configured ([services.engine.feed_bridge])")]
    FeedBridgeUnconfigured,

    /// One feed could not be prepared at boot: the cause is named with it.
    #[error("feed `{feed}`: reading {what}")]
    FeedBridgeFeed {
        feed: String,
        what: &'static str,
        #[source]
        source: Box<Error>,
    },

    /// The mock and the real aggregator disagree on decimals: writing the
    /// source's answer unscaled would poison every consumer.
    #[error("feed `{feed}`: source decimals {have} != target decimals {want}")]
    FeedBridgeDecimals { feed: String, have: u8, want: u8 },

    #[error("feed bridge ABI: {what}: {detail}")]
    FeedBridgeAbi { what: &'static str, detail: String },

    #[error("feed bridge: no feed named `{feed}`")]
    FeedBridgeUnknown { feed: String },

    #[error("feed `{feed}`: the target aggregator is not installed yet (the devnet's fixture deploy installs it)")]
    FeedBridgeWaiting { feed: String },
}

/// The crate's result: `Result<T>` everywhere (the error is fixed).
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The anvil boundary's conversion: a contract answer that does not decode is
/// the feed bridge's own boundary fact ("the target has no code yet", named
/// by [`Error::FeedBridgeAbi`]); every other anvil fault is the transparent
/// [`Error::Rpc`].
impl From<mirror_anvil::Error> for Error {
    fn from(e: mirror_anvil::Error) -> Self {
        match e {
            mirror_anvil::Error::Abi { what, source } => Error::FeedBridgeAbi {
                what,
                detail: source.to_string(),
            },
            other => Error::Rpc(other),
        }
    }
}

/// The shared shell's error contract (`mirror_devnet::engine::ShellError`):
/// the constructors the shared boot/serve helpers need. The rest of the
/// taxonomy stays the engine's own.
impl mirror_devnet::engine::ShellError for Error {
    fn bind(addr: String, source: std::io::Error) -> Self {
        Error::Bind { addr, source }
    }

    fn io(what: &'static str, path: String, source: std::io::Error) -> Self {
        Error::Io {
            what,
            path: path.into(),
            source,
        }
    }

    fn wrong_chain(anvil: &'static str, got: u64, want: u64) -> Self {
        Error::WrongChain { anvil, got, want }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_step_wrapper_keeps_the_chain_walkable() {
        let inner = Error::NoForkPin { chain: "anvil-t" };
        let wrapped = Error::Step {
            step: "apply",
            block: 7,
            source: Box::new(inner),
        };
        assert!(wrapped.to_string().contains("apply block 7"), "{wrapped}");
        let source = std::error::Error::source(&wrapped).expect("source is preserved");
        assert!(source.to_string().contains("fork pin"), "{source}");
    }
}

#[cfg(test)]
mod size_guard {
    use super::*;

    /// The error travels in every `Result` on the hot paths; clippy's
    /// `result_large_err` threshold is 128 bytes. The fix for a breach is to
    /// shrink the variant that grew (box a rare payload), never to allow the
    /// lint — this test makes the next breach fail here first.
    #[test]
    fn error_size_is_bounded() {
        assert!(
            std::mem::size_of::<Error>() <= 128,
            "engine Error grew to {} bytes",
            std::mem::size_of::<Error>()
        );
    }
}
