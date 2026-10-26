//! The mainnet boundary's error taxonomy (boundary-discipline; §11.3 rule 1).
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
    Config(#[from] diorama_config::ConfigError),

    #[error(transparent)]
    Rpc(diorama_anvil::Error),

    #[error(transparent)]
    Devnet(#[from] diorama_devnet::Error),

    #[error(transparent)]
    EngineApi(#[from] EngineApiError),

    #[error(transparent)]
    Beacon(#[from] BeaconError),

    /// IO on the engine's own boundary (the HTTP api's listener path; the
    /// spine's `ShellFault::Io` maps here).
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

    // ---- the feed bridge --------------------------------------
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
impl From<diorama_anvil::Error> for Error {
    fn from(e: diorama_anvil::Error) -> Self {
        match e {
            diorama_anvil::Error::Abi { what, source } => Error::FeedBridgeAbi {
                what,
                detail: source.to_string(),
            },
            other => Error::Rpc(other),
        }
    }
}

impl diorama_devnet::engine::ShellError for Error {}

// The shell contract (`diorama_devnet::engine::ShellError`): this taxonomy
// accepts the spine's shared errors — the devnet's own, the anvil boundary,
// and the shared `ShellFault`s the boot/serve helpers construct (the
// conversion is the spine's one expansion). The rest of the taxonomy stays
// this mirror's own.
diorama_devnet::shell_fault_conversion!(Error);

#[cfg(test)]
mod tests {
    use super::*;

    /// A wrapped fault keeps its cause chain (the venue's test pins the same
    /// rule under the same name).
    #[test]
    fn a_wrapped_fault_keeps_its_cause_chain() {
        let inner = Error::FeedBridgeUnconfigured;
        let wrapped = Error::Step {
            step: "apply",
            block: 7,
            source: Box::new(inner),
        };
        assert!(wrapped.to_string().contains("apply block 7"), "{wrapped}");
        let source = std::error::Error::source(&wrapped).expect("source is preserved");
        assert!(source.to_string().contains("feed bridge"), "{source}");
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
    /// The spine's shared fault converts into this taxonomy, this mirror's
    /// message and all (the other mirror's test pins its own wording).
    #[test]
    fn the_shell_fault_converts_into_the_taxonomy() {
        let e: Error = diorama_devnet::engine::ShellFault::WrongChain {
            anvil: "anvil-t",
            got: 999,
            want: 1,
        }
        .into();
        assert!(matches!(e, Error::WrongChain { got: 999, want: 1, .. }), "{e}");
        assert!(e.to_string().contains("is not the mainnet chain"), "{e}");
    }

    /// The anvil ABI boundary's conversion: a contract answer that does not
    /// decode becomes the feed bridge's own named fault, never a generic
    /// transport error (the venue has no bridge, so this aspect is one-sided
    /// by design — its own `Io`/API faults are pinned in its `error.rs`).
    #[test]
    fn an_abi_fault_becomes_the_feed_bridges_own() {
        let abi =
            <alloy_sol_types::sol_data::Bool as alloy_sol_types::SolType>::abi_decode(&[2u8])
                .expect_err("2 is not a bool");
        let e: Error = diorama_anvil::Error::Abi {
            what: "decimals",
            source: abi,
        }
        .into();
        assert!(
            matches!(e, Error::FeedBridgeAbi { what: "decimals", .. }),
            "{e}"
        );
    }
}
