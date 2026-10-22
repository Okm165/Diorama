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

    /// A persisted state file parsed as JSON but not as its state: it dies at
    /// the load point, path named (never silently becomes empty state).
    #[error("{what}: {path}")]
    Corrupt {
        what: &'static str,
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("encoding {what}")]
    Encode {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },

    // ---- the venue's public API boundary (market plane, passthrough) ------
    #[error("the venue api transport failed")]
    Http(#[from] reqwest::Error),

    /// The venue refused the call (non-2xx); its body carries the reason.
    #[error("the venue api answered {status}: {body}")]
    ApiStatus { status: u16, body: String },

    #[error("decoding a venue api response as {what}")]
    Decode {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },

    /// A read that needs a coin's live prices and the market plane has none
    /// (and no override owns it): a named fault, never a zero price.
    #[error("no live market context for `{coin}`")]
    NoMarket { coin: String },

    // ---- control-plane refusals (the sim maps them to 4xx) ----------------
    #[error("invalid control request: {reason}")]
    BadRequest { reason: String },

    /// A control route that needs wiring the deployment did not provide.
    #[error("{what} is not configured for this deployment")]
    NotConfigured { what: &'static str },

    /// A market read before the first venue poll landed (or an untouched
    /// account the venue cannot answer for): warming up, 503.
    #[error("the market plane is warming up (no venue data yet)")]
    Warming,

    /// A read for a target the sim does not have (a coin with no book, a
    /// margin table id the meta lacks): named, 404 at the API.
    #[error("not found: {what}")]
    NotFound { what: String },
}

/// The crate's result: `Result<T>` everywhere (the error is fixed).
pub type Result<T, E = Error> = std::result::Result<T, E>;
