//! The CCTP boundary's error taxonomy (boundary-discipline).
//!
//! Three vocabularies compose here: the config loader's, the JSON-RPC
//! boundary's ([`mirror_anvil::Error`], which itself separates a node's typed
//! refusal from a transport fault), and this crate's own chain-side facts:
//!
//! - a **wiring** mismatch (chain id, `localDomain`, message version) is a
//!   boot refusal naming the number that answered — the bridge must never
//!   move value on a chain it does not recognize;
//! - a persisted state file has three answers — **absent** (a fact: first
//!   boot), **fault** ([`Error::Io`]), **corrupt** ([`Error::Corrupt`], which
//!   dies at the load point, path named);
//! - a **refusal** ([`Error::BadRequest`], [`Error::AttesterInstall`]) names
//!   its reason and the numbers it refused.
use alloy_primitives::B256;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] mirror_config::ConfigError),

    #[error(transparent)]
    Rpc(#[from] mirror_anvil::Error),

    #[error("{what}: {path}")]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{what}: {path}")]
    Corrupt {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("encoding {what}")]
    Encode {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },

    /// A panic happened while a shared registry was locked: the value may be
    /// mid-update, so it is refused loudly, never recovered silently.
    #[error("shared state `{what}` is poisoned (a panic occurred mid-update)")]
    Poisoned { what: &'static str },

    /// A chain answered with a different identity than the domain's protocol
    /// facts pin (chain id, `localDomain`, message version, the peer
    /// messenger): boot refuses rather than bridging on an unrecognized
    /// deployment.
    #[error("{chain}: {what} is {got}, expected {want}")]
    Wiring {
        chain: &'static str,
        what: &'static str,
        got: String,
        want: String,
    },

    /// A domain that is not one this bridge serves (a message's destination,
    /// or a control request's source/destination).
    #[error("domain {domain} is not one this bridge serves")]
    UnknownDomain { domain: u32 },

    /// Fewer bytes than a V2 message's fixed part: decoding further would
    /// misread fields (the contracts' own `_validateMessageFormat` guard).
    #[error("message is {len} bytes; a V2 burn message needs at least {want}")]
    MessageTooShort { len: usize, want: usize },

    /// An ABI decode/encode of a contract answer failed: the boundary that
    /// reads the chain must not guess a value.
    #[error("abi {what}")]
    Abi {
        what: &'static str,
        #[source]
        source: alloy_sol_types::Error,
    },

    /// Signing failed (k256). The bridge refuses to serve an attestation it
    /// could not produce.
    #[error("signing {what}: {detail}")]
    Sign { what: &'static str, detail: String },

    /// A key-shaped config value is not a usable secp256k1 key.
    #[error("invalid {what}: {detail}")]
    InvalidKey { what: &'static str, detail: String },

    /// The request was refused before any chain interaction, naming why.
    #[error("invalid request: {reason}")]
    BadRequest { reason: String },

    /// The attester installation did not take: serving attestations the
    /// destination would reject is worse than not serving at all.
    #[error("{chain}: attester installation failed: {reason}")]
    AttesterInstall { chain: &'static str, reason: String },

    /// A submitted transaction was mined but reverted.
    #[error("{chain}: tx {tx} reverted")]
    TxReverted { chain: &'static str, tx: B256 },
}

/// The crate's result: `Result<T>` everywhere (the error is fixed).
pub type Result<T, E = Error> = std::result::Result<T, E>;
