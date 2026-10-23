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

    // ---- admin request refusals (the engine maps them to 4xx) ------------
    #[error("no cursor yet: nothing has been applied to the devnet chain")]
    NoCursor,

    #[error("rewind {blocks} exceeds the applied range (cursor {cursor})")]
    InvalidRewind { blocks: u64, cursor: u64 },

    #[error("rewind {blocks} exceeds the journal ring ({keep} blocks): older before-values are pruned")]
    RewindBeyondRing { blocks: u64, keep: u64 },

    #[error("invalid shadow request: {reason}")]
    InvalidShadow { reason: &'static str },

    /// A devnet block's shadow scan could not trace some of its txs. The scan
    /// stops before that block (the watermark must not advance past it): its
    /// writes would stay unmarked, and the next parent diff would overwrite
    /// the devnet's state. The per-tx causes are logged; this names the block.
    #[error("block {block}: shadow scan failed for {failed} tx trace(s); the block is re-scanned")]
    ShadowScan { block: u64, failed: usize },
}

impl Error {
    /// A refusal the caller can fix: the HTTP surfaces answer 4xx. The
    /// taxonomy is shared here so both engines classify identically (the
    /// grouping comment above the variants is the fact; this is the
    /// predicate).
    pub fn is_request_refusal(&self) -> bool {
        matches!(
            self,
            Error::NoCursor
                | Error::InvalidRewind { .. }
                | Error::RewindBeyondRing { .. }
                | Error::InvalidShadow { .. }
        )
    }

    /// A server-side state that blocks correct answers until a restart: the
    /// HTTP surfaces answer 503.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Error::Poisoned { .. })
    }
}

/// The crate's result: `Result<T>` everywhere (the error is fixed).
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_refusals_render_the_numbers_they_refuse() {
        let e = Error::InvalidRewind {
            blocks: 50,
            cursor: 10,
        };
        let rendered = e.to_string();
        assert!(rendered.contains("50") && rendered.contains("10"), "{rendered}");
        assert!(Error::NoCursor.to_string().contains("cursor"));
        let rendered = Error::InvalidShadow {
            reason: "`all` requires `clear`",
        }
        .to_string();
        assert!(rendered.contains("`all` requires `clear`"), "{rendered}");
    }
}
