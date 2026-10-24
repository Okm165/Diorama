//! The JSON-RPC boundary's error taxonomy (boundary-discipline; §11.3 rule 1).
//!
//! A call has three answers: the value, a **fault** — [`Error::Transport`] (no
//! answer reached us), [`Error::Status`] (an HTTP refusal whose body was not a
//! JSON-RPC answer), [`Error::Rpc`] (the node refused) — or **corrupt** —
//! [`Error::Decode`] / [`Error::BatchDecode`] (an answer that parsed as HTTP
//! but not as the expected shape). The body is the protocol's answer whatever
//! the HTTP status (measured: drpc answers `-32601` with HTTP 400); corrupt
//! data dies here, at the boundary, while the failure is still named.
//!
//! Policy is a typed predicate, never inferred from message text
//! (retry-discipline): [`Error::is_method_not_found`] is the capability gate
//! every read path branches on. The measured retry matrix lives here as the
//! record (no caller consults it today): `-32603 Internal error` is the only
//! node code worth retrying; `-32600` / `-32601` / `-32602` are content
//! refusals that reproduce identically; a connect-phase transport fault
//! provably never sent the bytes (safe to resend) while a post-send timeout
//! leaves a write's fate unknown — reconcile against the chain (the dedup
//! truth), never a silent resend. Prefer the `set*` cheatcodes over
//! `addBalance`, which double-credits on retries (measured).
/// The node's error payload, verbatim: alloy's official wire type
/// (`alloy_json_rpc::ErrorPayload`) — code, message, and the optional
/// `data` (revert data / validation details) preserved as raw JSON, never
/// bucketed. A spec change lands here as a compile error, not a silent
/// mismatch.
pub use alloy_json_rpc::ErrorPayload as RpcError;

/// Codes the client synthesizes for answers that never came from the node
/// (a batch response missing an item, a result that would not decode). Kept
/// outside the JSON-RPC ranges so they cannot be confused with node codes.
const CLIENT_ERROR_CODE: i64 = -1;

/// The node did not include this batch item's answer.
pub(crate) fn missing_from_batch(id: u64) -> RpcError {
    RpcError {
        code: CLIENT_ERROR_CODE,
        message: format!("batch response is missing item id {id}").into(),
        data: None,
    }
}

/// Everything the anvil/reth boundary can fail with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("building the http client")]
    ClientBuild {
        #[source]
        source: reqwest::Error,
    },

    #[error("{endpoint}: {method}: transport")]
    Transport {
        endpoint: String,
        method: String,
        #[source]
        source: reqwest::Error,
    },

    /// The server never answered within the caller's boot deadline — a
    /// coordinated restart (the anvil re-forking, the node unwinding) is the
    /// normal cause. The last fault is kept so the boot error shows why it
    /// stayed down.
    #[error("{what}: no answer within {timeout_secs}s of waiting")]
    NotReady {
        what: &'static str,
        timeout_secs: u64,
        #[source]
        source: Box<Error>,
    },

    /// An HTTP refusal whose body was not a JSON-RPC answer (gateway page,
    /// auth wall, empty body): the transport answered, but not with the
    /// protocol. A JSON-RPC error on a non-2xx status is [`Error::Rpc`].
    #[error("{endpoint}: {method}: HTTP {status}: {body}")]
    Status {
        endpoint: String,
        method: String,
        status: u16,
        body: String,
    },

    /// The node's typed refusal. The field is named `payload`, not `source`:
    /// alloy's `ErrorPayload` implements `Display`, not `Error`, and
    /// thiserror treats a field named `source` as the error source. The
    /// payload is inlined in the message so it stays complete: code, message,
    /// and `data` verbatim.
    #[error("{endpoint}: {method}: {payload}")]
    Rpc {
        endpoint: String,
        method: String,
        payload: RpcError,
    },

    #[error("{endpoint}: {method}: decoding the answer")]
    Decode {
        endpoint: String,
        method: String,
        #[source]
        source: serde_json::Error,
    },

    /// A typed call's return did not decode: the interface's own generated
    /// decoder named it (a wrong contract revision, a target with no code).
    /// Distinct from [`Error::Rpc`] so a boundary that reads *absence* from a
    /// node's refusal never reads a decode fault as absence too.
    #[error("abi {what}: {source}")]
    Abi {
        what: &'static str,
        #[source]
        source: alloy_sol_types::Error,
    },

    /// A request that could not be encoded (a bug in our own types; named so
    /// it can never masquerade as a node fault).
    #[error("{method}: encoding the request")]
    Encode {
        method: String,
        #[source]
        source: serde_json::Error,
    },

    /// A batch answer that did not parse; the request is preserved so the
    /// correlation can be reconstructed from the error alone.
    #[error("{endpoint}: batch response did not parse (request was {request})")]
    BatchDecode {
        endpoint: String,
        request: String,
        #[source]
        source: serde_json::Error,
    },

    /// An answer that parsed but is not the expected shape (tracer output).
    #[error("{what}: {detail}")]
    Corrupt { what: &'static str, detail: String },

    /// The node does not have the block: a fact the caller gates on, named.
    #[error("{endpoint}: block {n} is not available")]
    BlockMissing { endpoint: String, n: u64 },
}

impl Error {
    /// The node does not know the method (measured: a reth without
    /// `eth_getBlockAccessList` answers `-32601`). Callers gate on it to fall
    /// back to older data sources.
    pub fn is_method_not_found(&self) -> bool {
        matches!(self, Error::Rpc { payload, .. } if payload.code == -32601)
    }

    /// The endpoint the failure belongs to (for logs and error aggregation).
    pub fn endpoint(&self) -> &str {
        match self {
            Error::ClientBuild { .. }
            | Error::Abi { .. }
            | Error::Corrupt { .. }
            | Error::Encode { .. } => "<local>",
            Error::NotReady { what, .. } => what,
            Error::Transport { endpoint, .. }
            | Error::Status { endpoint, .. }
            | Error::Rpc { endpoint, .. }
            | Error::Decode { endpoint, .. }
            | Error::BatchDecode { endpoint, .. }
            | Error::BlockMissing { endpoint, .. } => endpoint,
        }
    }
}

/// The crate's result: `Result<T>` everywhere (the error is fixed).
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_not_found_is_typed_not_text_matched() {
        let err = Error::Rpc {
            endpoint: "http://reth:8545".into(),
            method: "eth_getBlockAccessList".into(),
            payload: RpcError {
                code: -32601,
                message: "the method eth_getBlockAccessList does not exist/is not available".into(),
                data: None,
            },
        };
        assert!(err.is_method_not_found());
    }

    /// This error is wrapped by the engine's, which must stay under clippy's
    /// 128-byte `result_large_err` threshold; the rare revert `data` is boxed
    /// for exactly this reason.
    #[test]
    fn error_size_is_bounded() {
        assert!(
            std::mem::size_of::<Error>() <= 128,
            "anvil Error grew to {} bytes",
            std::mem::size_of::<Error>()
        );
    }
}
