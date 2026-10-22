//! The venue boundary's error taxonomy (boundary-discipline).
//!
//! The sub-boundaries keep their own vocabularies with their modules —
//! `mirror_anvil::Error` (the JSON-RPC boundary), `mirror_devnet::Error` (the
//! devnet plane, incl. the admin refusals) — and compose here. V itself has
//! three answers at boot: it is the venue chain, it is not (a typed refusal
//! naming what answered), or it never came up (a boot timeout).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] mirror_config::ConfigError),

    #[error(transparent)]
    Rpc(#[from] mirror_anvil::Error),

    #[error(transparent)]
    Devnet(#[from] mirror_devnet::Error),

    /// Anvil V answered with a different chain id: the profile would fork the
    /// wrong chain (state, addresses and the CCTP domain all follow from it).
    #[error("anvil-v is not the venue chain: chain id {got}, expected {want}")]
    WrongChain { got: u64, want: u64 },

    /// An endpoint that never answered within its boot window; the last
    /// failure is kept so the boot error shows why it stayed down.
    #[error("{name} did not answer RPC within {timeout_secs}s")]
    NotReady {
        name: &'static str,
        timeout_secs: u64,
        #[source]
        source: Option<mirror_anvil::Error>,
    },

    /// The HTTP listener could not bind.
    #[error("binding the http api at {addr}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{what}: {path}")]
    Io {
        what: &'static str,
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The crate's result: `Result<T>` everywhere (the error is fixed).
pub type Result<T, E = Error> = std::result::Result<T, E>;
