//! The devnet-state boundary's error taxonomy (boundary-discipline).
//!
//! The registries persist as JSON files, so a load has three answers:
//!
//! - **absent** — a fact (first boot): the caller starts empty, never an error;
//! - **fault** — [`Error::Io`]: the file could not be read, named with its path;
//! - **corrupt** — [`Error::Corrupt`]: the bytes parsed as a file but not as the
//!   state, and it dies *here*, at the load point, with the path named — a
//!   corrupt registry must never silently become an empty one (the severity of
//!   that collapse is an overwrite: the parent diffs would clobber devnet
//!   writes whose marks vanished).
//!
//! Re-sync writes go through the anvil boundary ([`Error::Rpc`]);
//! [`Error::Poisoned`] is the shared-state policy (see [`crate::Shared`]).
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
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

    #[error(transparent)]
    Rpc(#[from] mirror_anvil::Error),

    /// A panic happened while a shared registry was locked: the value may be
    /// mid-update, so it is refused loudly — never recovered silently, never
    /// turned into a per-call panic.
    #[error("shared state `{what}` is poisoned (a panic occurred mid-update)")]
    Poisoned { what: &'static str },

    /// The caller paired a write with a before-value of a different kind — an
    /// internal contract violation, surfaced as a typed fault rather than a
    /// panic (a library must not abort its host on a caller bug).
    #[error("journal entry: {write_kind} write paired with a {before_kind} before-value")]
    KindMismatch {
        write_kind: &'static str,
        before_kind: &'static str,
    },
}

/// The crate's result: `Result<T>` everywhere (the error is fixed).
pub type Result<T, E = Error> = std::result::Result<T, E>;
