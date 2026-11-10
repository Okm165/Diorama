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
pub(crate) use alloy_json_rpc::ErrorPayload as RpcError;

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

    /// A client was built with no endpoint: the source's read pool must name
    /// at least one node.
    #[error("at least one RPC endpoint is required")]
    NoEndpoints,

    /// An endpoint that does not carry the pool's scheme. The config boundary
    /// validates the list, but the transport keeps its own guard so a
    /// hand-built client cannot smuggle a non-HTTP scheme into an HTTP pool
    /// (or a non-WS scheme into the venue's websocket pool).
    #[error("{endpoint:?} is not a valid endpoint for this transport")]
    InvalidEndpoint { endpoint: String },

    /// One endpoint attribute is unusable (a zero weight would make weighted
    /// admission undefined; a zero log-range cap would make every chunk
    /// empty). Refused at pool construction, where the config's typo is
    /// still attributable to one URL.
    #[error("{endpoint}: invalid endpoint attribute: {detail}")]
    InvalidEndpointAttribute { endpoint: String, detail: String },

    /// The same URL was declared twice with different attributes. Keeping
    /// the first silently would make the deployment's actual chain/weight
    /// depend on list order; the config is ambiguous, so the pool refuses it.
    #[error("{endpoint}: duplicate URL with conflicting attributes: {detail}")]
    ConflictingEndpointAttributes { endpoint: String, detail: String },

    /// An `eth_getLogs` range exceeds the endpoint's declared cap. The client
    /// refuses it locally instead of forwarding a request the provider will
    /// stall or reject (measured: a 57,100-block range stalled the public
    /// HyperEVM RPC for ~30 s). Callers that want a wide scan use
    /// [`crate::Rpc::logs_range`], which splits into bounded chunks.
    #[error(
        "{method}: block range {from}..={to} is {blocks} blocks, above the endpoint cap of {cap}; split it with Rpc::logs_range"
    )]
    LogRangeTooWide {
        method: String,
        from: u64,
        to: u64,
        blocks: u64,
        cap: u64,
    },

    /// A source endpoint answered a different chain id. A wrong-chain
    /// provider is a silent correctness fault if it is allowed to serve, so
    /// the boot assert names it and refuses the pool.
    #[error("{endpoint} is chain {got}, expected {want}")]
    WrongChain {
        endpoint: String,
        got: u64,
        want: u64,
    },

    /// One endpoint answered with a transport/throttle/plan refusal rather
    /// than the node's answer. The pool moves to the next endpoint; the
    /// caller sees this only when every round is exhausted.
    #[error("{endpoint}: {method}: endpoint fault: {detail}")]
    EndpointFault {
        endpoint: String,
        method: String,
        detail: String,
    },

    /// Every endpoint in the pool refused across every round. The last fault
    /// is kept so the caller can diagnose the pool, and a throttled body is
    /// **never** surfaced as a successful answer (the old `last_answer`
    /// path did exactly that).
    #[error("{attempts} endpoint attempt(s) failed; last: {last}")]
    EndpointsExhausted {
        attempts: u32,
        #[source]
        last: Box<Error>,
    },

    /// Every endpoint is inside its cooldown window and no emergency probe is
    /// due. This is a retry-later signal, not a node refusal: the caller can
    /// honour `Retry-After` instead of treating it as an answer.
    #[error("every endpoint is cooling; retry in {retry_after_ms} ms")]
    PoolCooling { retry_after_ms: u64 },

    /// A write was attempted through a multi-endpoint read pool. A post-send
    /// timeout is ambiguous (the bytes may have landed); failing over would
    /// duplicate a non-idempotent write. Writes use a single-endpoint client.
    #[error("a pooled read client cannot submit writes; build a single-endpoint write client")]
    PooledWrite,

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

    /// An answer that parsed but is not the expected shape: tracer output,
    /// or a BAL the header does not commit to (EIP-7928).
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

    /// The endpoint's response-size refusal (measured live: `-32008`
    /// `Response is too big` carrying the endpoint's own cap in `data` — the
    /// public HyperEVM RPC caps answers at 1 MiB). The logs range halves on
    /// it and retries: the cap is the endpoint's, the chunk size is ours.
    pub fn is_response_too_big(&self) -> bool {
        matches!(self, Error::Rpc { payload, .. } if payload.code == -32008)
    }

    /// A **transient server-side execution timeout** (measured live: the
    /// public HyperEVM RPC answers `-32000 execution aborted (timeout = 5s)`
    /// under load). It is not the method's answer: another endpoint may serve
    /// the same request, and the same endpoint may serve a narrower
    /// `eth_getLogs` range. The pool fails over on it; the composer halves the
    /// range. A deterministic `-32000` (a revert, an unsupported block) is
    /// deliberately not matched.
    pub fn is_execution_timeout(&self) -> bool {
        matches!(self, Error::Rpc { payload, .. }
            if payload.code == -32000
                && payload.message.to_ascii_lowercase().contains("timeout"))
    }

    /// Anvil's history-pruned refusals — two measured shapes for the same
    /// `--prune-history` window: a `debug_traceTransaction` answers `-32000`
    /// "historical state needed to replay block N is not available", while a
    /// state read (`eth_getStorageAt`) answers `-32602` "BlockOutOfRangeError:
    /// block height is H but requested was R". A block older than the window
    /// can never be replayed: the caller names it and moves on, never retries.
    pub fn is_history_unavailable(&self) -> bool {
        matches!(self, Error::Rpc { payload, .. }
            if (payload.code == -32000
                && payload.message.contains("historical state needed to replay"))
                || (payload.code == -32602
                    && payload.message.contains("BlockOutOfRangeError")))
    }

    /// The node does not have the requested block yet: its head moved past the
    /// number but the block is not served (or the endpoint is behind the
    /// requested height). A follow treats this as "wait", never as a pass
    /// failure; the head/followed gauges still expose the lag.
    pub fn is_block_unavailable(&self) -> bool {
        match self {
            Error::BlockMissing { .. } => true,
            Error::Rpc { payload, .. } => {
                (payload.code == -32001
                    && (payload.message.contains("block not found")
                        || payload.message.contains("not available")))
                    || (payload.code == -32000
                        && payload.message.contains("unsupported block number"))
                    || (payload.code == -32603 && payload.message.contains("invalid block height"))
                    // Measured onfinality shape for a log range whose head moved
                    // past the requested window while the request was in flight.
                    || (payload.code == -32602
                        && (payload.message.contains("block range extends beyond current head")
                            || payload.message.contains("beyond current head")))
            }
            Error::EndpointsExhausted { last, .. } => last.is_block_unavailable(),
            _ => false,
        }
    }

    /// The contract's dispatcher refused the selector: a revert carrying no
    /// data (measured 2026-10-25: mainnet's older TokenMessenger revision
    /// answers code 3 with `data: "0x"` for a fee surface it does not have).
    /// A revert *with* data is a real fault, never absence.
    pub fn is_bare_revert(&self) -> bool {
        matches!(self, Error::Rpc { payload, .. }
            if payload.code == 3
                && payload
                    .data
                    .as_deref()
                    .is_none_or(|d| d.get().trim_matches('"') == "0x"))
    }

    /// The endpoint the failure belongs to (for logs and error aggregation).
    pub fn endpoint(&self) -> &str {
        match self {
            Error::ClientBuild { .. }
            | Error::NoEndpoints
            | Error::InvalidEndpoint { .. }
            | Error::InvalidEndpointAttribute { .. }
            | Error::ConflictingEndpointAttributes { .. }
            | Error::WrongChain { .. }
            | Error::Abi { .. }
            | Error::Corrupt { .. }
            | Error::Encode { .. }
            | Error::EndpointsExhausted { .. }
            | Error::PoolCooling { .. }
            | Error::PooledWrite
            | Error::LogRangeTooWide { .. } => "<local>",
            Error::NotReady { what, .. } => what,
            Error::Transport { endpoint, .. }
            | Error::EndpointFault { endpoint, .. }
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

    /// The bare revert is typed by code and empty data, never text: a revert
    /// carrying a custom error's data is a real fault, and a method refusal
    /// is not a revert.
    #[test]
    fn bare_revert_is_typed_not_text_matched() {
        let revert = |data: Option<&str>| Error::Rpc {
            endpoint: "http://anvil-t:8545".into(),
            method: "eth_call".into(),
            payload: RpcError {
                code: 3,
                message: "execution reverted".into(),
                data: data.map(|d| serde_json::value::RawValue::from_string(d.into()).unwrap()),
            },
        };
        assert!(revert(Some("\"0x\"")).is_bare_revert());
        assert!(revert(None).is_bare_revert());
        assert!(!revert(Some("\"0x08c379a0\"")).is_bare_revert());
        let method = Error::Rpc {
            endpoint: "http://reth:8545".into(),
            method: "eth_getBlockAccessList".into(),
            payload: RpcError {
                code: -32601,
                message: "no such method".into(),
                data: None,
            },
        };
        assert!(!method.is_bare_revert());
    }

    /// The history-window refusal has two measured shapes — a trace's `-32000`
    /// "historical state needed to replay" and a state read's `-32602`
    /// "BlockOutOfRangeError" — and both classify; an unrelated `-32000` (a
    /// node-side fault) does not, so it keeps failing the walk closed.
    #[test]
    fn history_unavailable_matches_both_measured_shapes() {
        let rpc = |code: i64, message: &'static str| Error::Rpc {
            endpoint: "http://anvil-t:8545".into(),
            method: "debug_traceTransaction".into(),
            payload: RpcError {
                code,
                message: message.into(),
                data: None,
            },
        };
        assert!(rpc(
            -32000,
            "historical state needed to replay block 26256874 is not available"
        )
        .is_history_unavailable());
        assert!(rpc(
            -32602,
            "BlockOutOfRangeError: block height is 26259938 but requested was 26259712"
        )
        .is_history_unavailable());
        assert!(!rpc(-32000, "some other node fault").is_history_unavailable());
    }

    /// Every measured source-lag shape classifies as a wait; an unrelated
    /// `-32602` invalid-params answer does not.
    #[test]
    fn block_unavailable_matches_the_source_lag_shapes() {
        let rpc = |code: i64, message: &'static str| Error::Rpc {
            endpoint: "https://hyperliquid.api.onfinality.io/evm/public".into(),
            method: "eth_getLogs".into(),
            payload: RpcError {
                code,
                message: message.into(),
                data: None,
            },
        };
        assert!(rpc(-32001, "block not found: 0x2fba3bd").is_block_unavailable());
        assert!(rpc(-32000, "unsupported block number").is_block_unavailable());
        assert!(rpc(-32603, "invalid block height: 50047695").is_block_unavailable());
        assert!(rpc(-32602, "block range extends beyond current head block").is_block_unavailable());
        assert!(
            !rpc(-32602, "invalid argument 0: hex string has odd length").is_block_unavailable()
        );
    }

    /// The public HyperEVM RPC's transient server timeout is an endpoint
    /// fault, not the method's answer: the pool may fail over and the
    /// composer may narrow the range. A deterministic `-32000` stays an
    /// answer.
    #[test]
    fn execution_timeout_is_a_transient_fault() {
        let rpc = |code: i64, message: &'static str| Error::Rpc {
            endpoint: "https://rpc.hyperliquid.xyz/evm".into(),
            method: "eth_getLogs".into(),
            payload: RpcError {
                code,
                message: message.into(),
                data: None,
            },
        };
        assert!(rpc(-32000, "execution aborted (timeout = 5s)").is_execution_timeout());
        assert!(rpc(-32000, "request timeout").is_execution_timeout());
        assert!(!rpc(-32000, "execution reverted").is_execution_timeout());
        assert!(!rpc(-32603, "internal error").is_execution_timeout());
        assert!(!rpc(-32005, "rate limited").is_execution_timeout());
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
