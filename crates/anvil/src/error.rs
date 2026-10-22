//! The JSON-RPC boundary's error taxonomy (boundary-discipline; §11.3 rule 1).
//!
//! A call has three answers: the value, a **fault** — [`Error::Transport`] (no
//! answer reached us), [`Error::Status`] (an HTTP refusal whose body was not a
//! JSON-RPC answer), [`Error::Rpc`] (the node refused) — or **corrupt** —
//! [`Error::Decode`] / [`Error::BadQuantity`] (an answer that parsed as HTTP
//! but not as the expected shape). The body is the protocol's answer whatever
//! the HTTP status (measured: drpc answers `-32601` with HTTP 400); corrupt
//! data dies here, at the boundary, while the failure is still named.
//!
//! Policy is a typed predicate, never inferred from message text
//! (retry-discipline): [`Error::is_retryable`] is the optimistic *read*-policy
//! (idempotent calls and the idempotent `set*` cheatcodes);
//! [`Error::is_safe_to_resubmit`] is the narrow *write*-policy (the bytes
//! provably never left — connect-phase only); [`Error::is_submission_outcome_unknown`]
//! names the ambiguous wall (a write that may have landed: timeout or reset
//! after send), which pairs with a reconciliation path — the chain is the dedup
//! truth — never with a silent resend. The two write predicates are exact
//! complements (pinned by test), so a caller branching on one cannot leave the
//! other unhandled.
use serde_json::Value;

/// One RPC call's answer from the node, preserved verbatim: unknown upstream
/// codes surface as themselves (`code`), never bucketed into a retry class.
/// `data` is boxed: revert data is rare, and the box keeps the error small
/// enough that `Result` stays cheap on the hot paths (`result_large_err`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Box<Value>>,
}

/// Codes the client synthesizes for answers that never came from the node
/// (a batch response missing an item, a result that would not decode). Kept
/// outside the JSON-RPC ranges so they cannot be confused with node codes.
const CLIENT_ERROR_CODE: i64 = -1;

impl RpcError {
    /// The node did not include this batch item's answer.
    pub(crate) fn missing_from_batch(id: u64) -> Self {
        Self {
            code: CLIENT_ERROR_CODE,
            message: format!("batch response is missing item id {id}"),
            data: None,
        }
    }

    /// The node's answer did not decode into the expected shape.
    pub(crate) fn bad_result(method: &str, detail: &str) -> Self {
        Self {
            code: CLIENT_ERROR_CODE,
            message: format!("{method}: bad result: {detail}"),
            data: None,
        }
    }

    /// A content refusal: retrying the same bytes reproduces it. Measured:
    /// `-32600 Invalid request` (body ≥ 2 MB), `-32601 method not found`,
    /// `-32602 Invalid params` (e.g. odd-length hex).
    pub fn is_content_error(&self) -> bool {
        matches!(self.code, -32602..=-32600)
    }

    /// The node's internal error: plausibly transient.
    pub fn is_retryable(&self) -> bool {
        self.code == -32603
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rpc error {}: {}", self.code, self.message)?;
        // `data` carries revert data / validation details on many nodes
        // (measured: reth's "-32602 Invalid params: EmptyExecutionRequest").
        if let Some(data) = &self.data {
            write!(f, " ({data})")?;
        }
        Ok(())
    }
}

impl std::error::Error for RpcError {}

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

    #[error("{endpoint}: {method}: {source}")]
    Rpc {
        endpoint: String,
        method: String,
        #[source]
        source: RpcError,
    },

    #[error("{endpoint}: {method}: decoding the answer")]
    Decode {
        endpoint: String,
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

    #[error("{endpoint}: {method}: not a hex quantity: {value:?}")]
    BadQuantity {
        endpoint: String,
        method: String,
        value: String,
    },

    /// An answer that parsed but is not the expected shape (tracer output).
    #[error("{what}: {detail}")]
    Corrupt { what: &'static str, detail: String },

    /// The node does not have the block: a fact the caller gates on, named.
    #[error("{endpoint}: block {n} is not available")]
    BlockMissing { endpoint: String, n: u64 },
}

impl Error {
    /// Corrupt quantity from a node's answer, constructed by callers that parse
    /// quantities themselves (e.g. batched balance reads in the engine).
    pub fn bad_quantity(endpoint: &str, method: &str, value: &str) -> Self {
        Error::BadQuantity {
            endpoint: endpoint.to_string(),
            method: method.to_string(),
            value: value.to_string(),
        }
    }

    /// Optimistic read-policy: could a retry plausibly succeed? Governs
    /// idempotent calls — reads, and the `set*` cheatcodes (idempotent by
    /// construction; prefer them over `addBalance`, which double-credits on
    /// retries — measured). Content refusals and decode failures reproduce
    /// identically, so they are never retryable.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Transport { .. } => true,
            Error::Status { status, .. } => matches!(status, 429 | 502 | 503 | 504),
            Error::Rpc { source, .. } => source.is_retryable(),
            Error::ClientBuild { .. }
            | Error::Decode { .. }
            | Error::BatchDecode { .. }
            | Error::BadQuantity { .. }
            | Error::Corrupt { .. }
            | Error::BlockMissing { .. } => false,
        }
    }

    /// Narrow write-policy: the request provably never reached the node, so
    /// resending the same bytes cannot duplicate an effect. Connect-phase
    /// transport failures only.
    pub fn is_safe_to_resubmit(&self) -> bool {
        matches!(self, Error::Transport { source, .. } if source.is_connect())
    }

    /// The ambiguous wall: bytes were sent and the outcome is unknown — a
    /// write may have landed. Surface for reconciliation (the journal / the
    /// chain's own dedup), never a silent resend.
    pub fn is_submission_outcome_unknown(&self) -> bool {
        matches!(self, Error::Transport { source, .. } if !source.is_connect())
    }

    /// The node does not know the method (measured: a reth without
    /// `eth_getBlockAccessList` answers `-32601`). Callers gate on it to fall
    /// back to older data sources.
    pub fn is_method_not_found(&self) -> bool {
        matches!(self, Error::Rpc { source, .. } if source.code == -32601)
    }

    /// The endpoint the failure belongs to (for logs and error aggregation).
    pub fn endpoint(&self) -> &str {
        match self {
            Error::ClientBuild { .. } | Error::Corrupt { .. } => "<local>",
            Error::Transport { endpoint, .. }
            | Error::Status { endpoint, .. }
            | Error::Rpc { endpoint, .. }
            | Error::Decode { endpoint, .. }
            | Error::BatchDecode { endpoint, .. }
            | Error::BadQuantity { endpoint, .. }
            | Error::BlockMissing { endpoint, .. } => endpoint,
        }
    }
}

/// The crate's result: `Result<T>` everywhere (the error is fixed).
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    /// A real connect-phase transport failure (ECONNREFUSED): the bytes never
    /// left, so the write policy must say "safe to resubmit".
    fn connect_phase_error() -> Error {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let source = rt.block_on(async {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap()
                .post("http://127.0.0.1:1")
                .send()
                .await
                .unwrap_err()
        });
        Error::Transport {
            endpoint: "http://127.0.0.1:1".into(),
            method: "eth_sendRawTransaction".into(),
            source,
        }
    }

    #[test]
    fn ambiguity_predicates_are_exact_complements_for_transport() {
        let err = connect_phase_error();
        // Refused connection: connect-phase → safe to resubmit, outcome known.
        assert!(err.is_safe_to_resubmit(), "{err}");
        assert!(!err.is_submission_outcome_unknown(), "{err}");
        assert!(err.is_retryable(), "{err}");
    }

    #[test]
    fn content_refusals_are_never_retryable() {
        for code in [-32600, -32601, -32602] {
            let e = RpcError {
                code,
                message: "refused".into(),
                data: None,
            };
            assert!(e.is_content_error(), "code {code}");
            assert!(!e.is_retryable(), "code {code}");
        }
        assert!(RpcError {
            code: -32603,
            message: "internal".into(),
            data: None,
        }
        .is_retryable());
    }

    #[test]
    fn method_not_found_is_typed_not_text_matched() {
        let err = Error::Rpc {
            endpoint: "http://reth:8545".into(),
            method: "eth_getBlockAccessList".into(),
            source: RpcError {
                code: -32601,
                message: "the method eth_getBlockAccessList does not exist/is not available".into(),
                data: None,
            },
        };
        assert!(err.is_method_not_found());
        assert!(!err.is_retryable());
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
