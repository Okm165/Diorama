//! The engine's error taxonomy (boundary-discipline; §11.3 rule 1).
//!
//! Variants classify by **failure class**, never by call site; each carries the
//! structured fields an operator needs (endpoint, block, path). The
//! sub-boundaries keep their own vocabularies with their modules —
//! [`EngineApiError`] (reth authrpc), [`BeaconError`] / [`BlobError`] (beacon
//! API + KZG), `mirror_devnet::Error` (the devnet plane, incl. the admin
//! refusals) — and compose here.
//!
//! Policy carriers: the devnet's `NoCursor` / `InvalidRewind` are admin
//! *request refusals* — `http.rs` maps them to 4xx while every other variant is
//! a server-side fault (5xx, or 503 for a poisoned registry).
use alloy_primitives::B256;
use std::path::PathBuf;

pub use crate::blob::{BeaconError, BlobError};
pub use crate::engine_api::EngineApiError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] mirror_config::ConfigError),

    #[error(transparent)]
    Rpc(#[from] mirror_anvil::Error),

    #[error(transparent)]
    Devnet(#[from] mirror_devnet::Error),

    #[error(transparent)]
    EngineApi(#[from] EngineApiError),

    #[error(transparent)]
    Beacon(#[from] BeaconError),

    #[error(transparent)]
    Blob(#[from] BlobError),

    /// Engine-local file IO (drift log, pin file).
    #[error("{what}: {path}")]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A drift-log entry could not be serialized (a bug in an entry type,
    /// never a runtime condition): named so the gap cannot look like a clean
    /// run.
    #[error("drift log: encoding the entry")]
    DriftEncode {
        #[source]
        source: serde_json::Error,
    },

    /// The HTTP listener could not bind.
    #[error("binding the http api at {addr}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    /// An endpoint that never answered within its boot window; the last
    /// failure is kept so the boot error shows why it stayed down.
    #[error("{name} did not answer RPC within {timeout_secs}s")]
    NotReady {
        name: &'static str,
        timeout_secs: u64,
        #[source]
        source: Option<mirror_anvil::Error>,
    },

    /// The selftest's scratch anvil was not started with a fork pin.
    #[error("{chain} has no fork pin - start it with --fork-url and --fork-block-number")]
    NoForkPin { chain: &'static str },

    /// A phase of the selftest (replay/apply of one block) failed; wraps the
    /// inner fault so the phase and the block are both named.
    #[error("{step} block {block}: {source}")]
    Step {
        step: &'static str,
        block: u64,
        #[source]
        source: Box<Error>,
    },

    // ---- replay (M) ------------------------------------------------------
    #[error("M head {m_head} does not precede block {n}")]
    ReplayOrder { m_head: u64, n: u64 },

    #[error("block {block}: block env failed: {source}")]
    BlockEnv {
        block: u64,
        #[source]
        source: Box<mirror_anvil::Error>,
    },

    /// A reth pre-balance read for the withdrawal credits failed. The field
    /// is named `payload`: alloy's `ErrorPayload` implements `Display`, not
    /// `Error`, so thiserror's automatic source detection must not see it.
    #[error("block {block}: reth pre-balance: {payload}")]
    PreBalance {
        block: u64,
        payload: mirror_anvil::RpcError,
    },

    #[error("block {block}: raw tx {index} send failed")]
    TxSend {
        block: u64,
        index: usize,
        #[source]
        source: mirror_anvil::Error,
    },

    #[error("block {block}: {skipped} blob tx(s) have no sidecar; not mining a partial block")]
    BlobsUnavailable { block: u64, skipped: usize },

    #[error("block {block}: replay aborted: {rejected} raw tx(s) rejected")]
    ReplayAborted { block: u64, rejected: usize },

    #[error("block {block}: replay aborted after retry")]
    ReplayRetryExhausted { block: u64 },

    #[error("block {block}: blob tx {hash} could not be re-encoded")]
    BlobReencode {
        block: u64,
        hash: B256,
        #[source]
        source: BlobError,
    },

    // ---- applier (T) -----------------------------------------------------
    #[error("T head {t_head} is more than one block behind the parent cursor {n}")]
    ApplierBehind { t_head: u64, n: u64 },

    /// A devnet block's shadow scan could not trace some of its txs. The scan
    /// stops before that block (the watermark must not advance past it): its
    /// writes would stay unmarked, and the next parent diff would overwrite
    /// the devnet's state. The per-tx causes are logged; this names the block.
    #[error("block {block}: shadow scan failed for {failed} tx trace(s); the block is re-scanned")]
    ShadowScan { block: u64, failed: usize },

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
}

/// The crate's result: `Result<T>` everywhere (the error is fixed).
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_step_wrapper_keeps_the_chain_walkable() {
        let inner = Error::ReplayOrder { m_head: 7, n: 7 };
        let wrapped = Error::Step {
            step: "replay",
            block: 7,
            source: Box::new(inner),
        };
        assert!(wrapped.to_string().contains("replay block 7"), "{wrapped}");
        let source = std::error::Error::source(&wrapped).expect("source is preserved");
        assert!(source.to_string().contains("M head 7"), "{source}");
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
