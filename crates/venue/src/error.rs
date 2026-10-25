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
    #[error("{anvil} is not the venue chain: chain id {got}, expected {want}")]
    WrongChain {
        anvil: &'static str,
        got: u64,
        want: u64,
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

    // ---- request refusals (the sim maps them to 4xx) ----------------------
    /// A refusal of the caller's request. The reason carries its own surface
    /// context ("invalid info request: …" from the info boundary; the venue's
    /// own string from the order lane), so no prefix is added here — the
    /// control plane and `/info` both serve exactly what their reason says.
    #[error("{reason}")]
    BadRequest { reason: String },

    /// A control route that needs wiring the deployment did not provide.
    #[error("{what} is not configured for this deployment")]
    NotConfigured { what: &'static str },

    /// The selftest's scratch anvil was not started with a fork pin.
    #[error("{chain} has no fork pin - start it with --fork-url and --fork-block-number")]
    NoForkPin { chain: &'static str },

    /// A money mutation would leave the decimal range: refused cleanly.
    /// rust_decimal panics on overflow, and a panic under the state lock
    /// poisons the whole account plane — so every arithmetic step that an
    /// input can reach is checked (measured: `Decimal::MAX` + 1 in `deal`
    /// bricked every account path until a restart).
    #[error("amount overflow: {what}")]
    Overflow { what: &'static str },

    /// A division by zero: refused cleanly (rust_decimal's `/` panics on it,
    /// the same measured hazard as the overflow above — a panic under the
    /// state lock poisons every account path).
    #[error("division by zero: {what}")]
    DivideByZero { what: &'static str },

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

impl mirror_devnet::engine::ShellError for Error {}

/// The shell contract (`mirror_devnet::engine::ShellError`): this taxonomy
/// accepts the spine's shared errors — the devnet's own, the anvil boundary,
/// and the shared [`mirror_devnet::engine::ShellFault`]s the boot/serve
/// helpers construct. The rest of the taxonomy stays the venue's own.
impl From<mirror_devnet::engine::ShellFault> for Error {
    fn from(fault: mirror_devnet::engine::ShellFault) -> Self {
        match fault {
            mirror_devnet::engine::ShellFault::Bind { addr, source } => {
                Error::Bind { addr, source }
            }
            mirror_devnet::engine::ShellFault::Io {
                what,
                path,
                source,
            } => Error::Io {
                what,
                path: path.into(),
                source,
            },
            mirror_devnet::engine::ShellFault::WrongChain { anvil, got, want } => {
                Error::WrongChain { anvil, got, want }
            }
        }
    }
}

impl Error {
    /// A refused request with its reason — one constructor for the most
    /// common error (the routes answer it as a 400 with the reason verbatim).
    pub(crate) fn bad_request(reason: impl Into<String>) -> Self {
        Self::BadRequest {
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wrapped boundary fault keeps its cause chain (the mainnet's `Step`
    /// wrapper test pins the same rule).
    #[test]
    fn the_source_chain_is_preserved() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e = Error::Io {
            what: "journal directory",
            path: "/tmp/x".into(),
            source: io,
        };
        let source = std::error::Error::source(&e).expect("source is preserved");
        assert!(source.to_string().contains("missing"), "{source}");
    }
}

#[cfg(test)]
mod size_guard {
    use super::*;

    /// The error travels in every `Result` on the hot paths; clippy's
    /// `result_large_err` threshold is 128 bytes (the mainnet's mirror test
    /// pins its own enum the same way).
    #[test]
    fn error_size_is_bounded() {
        assert!(
            std::mem::size_of::<Error>() <= 128,
            "venue Error grew to {} bytes",
            std::mem::size_of::<Error>()
        );
    }
    /// The spine's shared fault converts into this taxonomy, this mirror's
    /// message and all (the other mirror's test pins its own wording).
    #[test]
    fn the_shell_fault_converts_into_the_taxonomy() {
        let e: Error = mirror_devnet::engine::ShellFault::WrongChain {
            anvil: "anvil-v",
            got: 1,
            want: 999,
        }
        .into();
        assert!(matches!(e, Error::WrongChain { got: 1, want: 999, .. }), "{e}");
        assert!(e.to_string().contains("is not the venue chain"), "{e}");
    }

}
