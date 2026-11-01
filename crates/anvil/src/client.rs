//! Thin JSON-RPC client on alloy's official wire model: typed helpers +
//! chunked batching with per-item errors.
//!
//! Boundary rule (`answer`): a body that parses as a JSON-RPC answer *is*
//! the node's answer, whatever the HTTP status; the status only classifies
//! bodies that are not answers (gateway pages, auth walls, empty bodies).
//! Measured: drpc answers `-32601` with HTTP 400 — status-first classification
//! turned the node's typed refusal into a transport fault and the BAL→trace
//! fallback never engaged.
//!
//! Batching matters for two measured reasons: writes are 9.6-18 ms each when
//! issued one at a time, and public endpoints reject large batches with HTTP 413
//! (local reth tolerates the sizes we use here, but callers chunk regardless).
//!
//! The envelope (`Request`, `Response`, `ResponsePacket`, `ErrorPayload`, `Id`)
//! is `alloy_json_rpc`'s; the results and params are alloy's per-namespace
//! types (`alloy_rpc_types_eth`, `…-anvil`, `…-trace`, `…-txpool`). A wire
//! change upstream lands as a compile error here, never as a silent mismatch.
use crate::error::{missing_from_batch, Error, Result, RpcError};
use crate::trace::{Key, Value};
use alloy_eips::eip7928::BlockAccessList;
use alloy_json_rpc::{
    Id, Request, RequestPacket, Response, ResponsePacket, ResponsePayload, SerializedRequest,
};
use alloy_primitives::{Address, Bytes, TxKind, B256, U256, U64};
use alloy_rpc_types_anvil::{Forking, NodeInfo};
use alloy_rpc_types_eth::{
    state::StateOverride, Block, BlockNumberOrTag, Filter, Header, Log, Transaction,
    TransactionReceipt, TransactionRequest,
};
use alloy_rpc_types_trace::geth::{
    GethDebugTracingOptions, GethTrace, PreStateConfig, TraceResult,
};
use alloy_rpc_types_txpool::TxpoolStatus;
use serde::{de::DeserializeOwned, de::Error as _, Serialize};
use serde_json::value::RawValue;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::pool::{now_ms, Attempt, AttemptOutcome, EndpointPool, PoolError, PoolPolicy, Scheme};

/// Positional params for methods that take none: the spec's `[]` (the
/// discouraged alternative is `null`, which not every node accepts). Public
/// so a batched caller can stage a no-param call beside typed ones.
pub const NO_PARAMS: [u8; 0] = [];

/// The JSON-RPC code a node answers when it does not implement a method
/// (measured: drpc answers `-32601` on HTTP 400 — see the module docs).
const METHOD_NOT_FOUND: i64 = -32601;

/// One call to send: alloy's own request object, params erased to the
/// official `RawValue` — exactly the shape a request serializes to. The id is
/// assigned per send; while staging, `Id::None`.
pub type Call = Request<Box<RawValue>>;

/// One chunk's answers: each call's raw result or its JSON-RPC error (the
/// batch's own shape; the official `RawValue` keeps the values untouched).
pub type ChunkAnswers = Vec<std::result::Result<Box<RawValue>, RpcError>>;

/// Stage a call. Params serialize to the JSON-RPC positional form: a tuple
/// for several (`(U64::from(n), true)`), a 1-tuple for one (`(raw,)`),
/// [`NO_PARAMS`] for none.
pub fn call(method: &'static str, params: impl Serialize) -> Result<Call> {
    let params = serde_json::value::to_raw_value(&params).map_err(|source| Error::Encode {
        method: method.to_string(),
        source,
    })?;
    Ok(Request::new(method, Id::None, params))
}

/// The batch's sub-chunk concurrency: one request in flight. The batching's
/// win is the request *count* (a wide window is a handful of bodies, not one
/// per block), and the public HyperEVM RPC's burst limiter refused concurrent
/// bodies while one-at-a-time traffic — the pre-batching era's own pattern —
/// passed (measured live). Sequential sub-batches keep the follow inside that
/// proven-safe shape.
const BATCH_CONCURRENCY: usize = 1;

/// Whether a block read carries its transactions' bodies: the RPC's own
/// `full` flag, named (a bare `true` at a call site says nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Txs {
    /// Transaction hashes only.
    Hashes,
    /// Full transaction bodies.
    Full,
}

impl Txs {
    /// The wire flag (`eth_getBlockByNumber`'s second parameter).
    fn full(self) -> bool {
        matches!(self, Txs::Full)
    }
}

/// The target's automine mode: `anvil_setAutomine`'s own flag, named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Automine {
    On,
    Off,
}

/// The batch's item cap, measured: the public HyperEVM RPC answers
/// `Exceeded max limit of 20` for a larger batch (live), so every sub-batch
/// stays at or below it — the client chunks transparently and sequentially.
const BATCH_MAX_ITEMS: usize = 20;

/// The local anvil's item contract, measured live: 500 items per body are
/// answered (1000 too — 500 is the conservative bound the write path
/// documents). Callers that *know* their target is the local devnet chain
/// ride this; the public cap stays [`BATCH_MAX_ITEMS`].
pub const ANVIL_BATCH_MAX_ITEMS: usize = 500;

/// The throttle's JSON-RPC code (outside the protocol ranges, like the
/// client's own synthesized codes).
const RATE_LIMIT_CODE: i64 = -32005;
/// The node's internal error: the measured retry matrix in `error` names it
/// the one node code worth retrying (measured live: the system-tx read
/// answers it transiently under load).
const INTERNAL_ERROR_CODE: i64 = -32603;
/// The retryable refusals: the throttle and the node's internal error. A
/// single error object is parsed; a batch array is scanned for the codes
/// (distinctive strings — a full parse of a large answer is not worth it).
///
/// A `-32603` carrying `invalid block height` is not an internal fault: it is
/// the node's answer about a height it does not serve (measured: every public
/// HyperEVM endpoint answers the capability audit's future-block probe this
/// way). Cooling endpoints for it turned one audit into an all-cooling pool
/// and a spurious WARN; the caller's block-unavailable predicate owns the wait.
fn is_retryable(text: &str) -> bool {
    let retryable = |error: &serde_json::Value| {
        let code = error
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let message = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
        code == RATE_LIMIT_CODE
            || (code == INTERNAL_ERROR_CODE
                && !message
                    .to_ascii_lowercase()
                    .contains("invalid block height"))
    };
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        // Parse the batch rather than string-searching a compact spelling: a
        // pretty-printed `"code": -32005` is the same refusal.
        return serde_json::from_str::<Vec<serde_json::Value>>(trimmed)
            .map(|items| {
                items
                    .iter()
                    .any(|item| item.get("error").is_some_and(retryable))
            })
            .unwrap_or(false);
    }
    if !trimmed.starts_with('{') {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|v| v.get("error").map(retryable))
        .unwrap_or(false)
}

/// An endpoint's **plan/capability refusal** — not the method's answer.
///
/// Measured live 2026-10-29: drpc's free plan answers a batch of more than
/// three requests with `code 31` / “Batch of more than 3 requests are not
/// allowed on free plan”. The same batch is answered by the official RPC (20
/// items) and the other endpoints, so the pool must fail over rather than
/// surface one endpoint's plan limit as the call's result. A method-level
/// refusal (`-32601`, a revert) is still the node's answer and is not failed
/// over.
///
/// The message arms keep the failover working if an endpoint re-codes its
/// plan limit; the code arm is drpc's measured code.
fn endpoint_policy_refusal(text: &str) -> bool {
    let policy = |code: i64, message: &str| {
        code == 31
            || message.contains("free plan")
            || message.contains("Batch of more than")
            || message.contains("batch limit")
    };
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        // A batch: any item carrying a plan refusal means this endpoint cannot
        // serve the body; another endpoint can.
        return serde_json::from_str::<Vec<serde_json::Value>>(trimmed)
            .map(|items| {
                items.iter().any(|item| {
                    let Some(error) = item.get("error") else {
                        return false;
                    };
                    let code = error
                        .get("code")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    let message = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
                    policy(code, message)
                })
            })
            .unwrap_or(false);
    }
    if !trimmed.starts_with('{') {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return false;
    };
    let Some(error) = value.get("error") else {
        return false;
    };
    let code = error
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let message = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
    policy(code, message)
}

/// An endpoint's **capability refusal** — it cannot serve this request, but
/// another provider may.
///
/// Measured live: drpc answers `-32000` / “unsupported block number” for a
/// block its free plan does not retain, while the archive endpoint serves the
/// same request. The error is a method-level JSON-RPC error by shape, so the
/// old classifier returned it as the node's answer and never tried the next
/// endpoint. These messages are provider capability, not a deterministic
/// revert: a revert (`execution reverted`) stays the node's answer and is
/// never failed over.
fn endpoint_capability_refusal(text: &str) -> bool {
    let capability = |message: &str| {
        let message = message.to_ascii_lowercase();
        [
            "unsupported block number",
            "pruned history unavailable",
            "missing trie node",
            "header not found",
            "block not found",
            "requested block is not available",
            "historical state",
            "archive",
            "state is not available",
        ]
        .iter()
        .any(|needle| message.contains(needle))
    };
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        return serde_json::from_str::<Vec<serde_json::Value>>(trimmed)
            .map(|items| {
                items.iter().any(|item| {
                    let Some(error) = item.get("error") else {
                        return false;
                    };
                    let code = error
                        .get("code")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    let message = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
                    code == -32000 && capability(message)
                })
            })
            .unwrap_or(false);
    }
    if !trimmed.starts_with('{') {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return false;
    };
    let Some(error) = value.get("error") else {
        return false;
    };
    let code = error
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let message = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
    code == -32000 && capability(message)
}

/// Whether an HTTP answer is an endpoint fault rather than the node's own
/// answer: the retryable JSON-RPC refusals, an endpoint plan or capability
/// refusal (a provider that cannot retain/serve the requested block), HTTP
/// 429, or a 5xx body that is not a JSON-RPC answer (a gateway page). A
/// method-level JSON-RPC error on any status — a revert, an unknown method —
/// is the node's answer and is never failed over.
fn endpoint_fault(status: reqwest::StatusCode, text: &str) -> bool {
    // An auth wall, a wrong path, a body-size refusal or a timeout is the
    // endpoint's refusal, not the method's answer: another endpoint may serve
    // the same request. A JSON-RPC error on any status stays the node's
    // answer (the measured drpc `-32601`/HTTP 400 case).
    let endpoint_status = matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::PAYLOAD_TOO_LARGE
            | reqwest::StatusCode::TOO_MANY_REQUESTS
    );
    // A 2xx body that is not JSON is a gateway page, not a JSON-RPC answer.
    // (A valid JSON-RPC body is always an object or an array.)
    let not_json = status.is_success() && serde_json::from_str::<serde_json::Value>(text).is_err();
    is_retryable(text)
        || endpoint_policy_refusal(text)
        || endpoint_capability_refusal(text)
        || endpoint_status
        || not_json
        || (status.is_server_error() && serde_json::from_str::<serde_json::Value>(text).is_err())
}

/// One HTTP client and a **read pool** of endpoints, cloned cheaply
/// (reqwest's client and the pool are `Arc`-backed).
///
/// A single-endpoint client (`new`) is the write client: its methods are the
/// same and every request goes to that one node. A pooled client
/// (`with_urls`) is the engine's **source** read client: each request starts
/// at a random healthy endpoint and, on a transport fault, a throttled
/// answer, or an endpoint-level HTTP failure, moves to the next endpoint in
/// the rotated order. A failed endpoint is cooled down by the shared
/// [`EndpointPool`] and retried in a later round. The first success wins.
///
/// Writes must use a single-endpoint client — the spine's `build_clients`
/// enforces that by construction (source pooled, target single). A pooled
/// client is not a write surface: a post-send timeout would duplicate a
/// non-idempotent write on the next endpoint.
pub struct Rpc {
    http: reqwest::Client,
    pool: Arc<EndpointPool>,
    next_id: AtomicU64,
    /// Whether this client may submit writes. A multi-endpoint read pool is
    /// not a write surface: a post-send timeout would be ambiguous and a
    /// failover would duplicate the write.
    write: bool,
}

impl Clone for Rpc {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            pool: Arc::clone(&self.pool),
            next_id: AtomicU64::new(1),
            write: self.write,
        }
    }
}

/// The method the three block helpers share.
const BLOCK_BY_NUMBER: &str = "eth_getBlockByNumber";

/// The one method name the three call helpers share.
const ETH_CALL: &str = "eth_call";

/// A chain's fork source and block: the pin a forked node started at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkConfig {
    pub url: String,
    pub block: u64,
}

/// The pool drain's outcome: the depth the mined block carried, or an empty
/// pool (V's producer rule: an idle chain stays static; T's prepend runs the
/// same check with its timestamp step around it).
pub enum Mined {
    Block { pending: u64 },
    Empty,
}

impl Rpc {
    /// Build a **single-endpoint** client for `url`. This is the write client
    /// (the target anvil) and a one-endpoint source. It makes exactly one
    /// attempt per call: a single endpoint has no failover, and a write must
    /// never be silently retried.
    pub fn new(url: &str, timeout: Duration) -> Result<Self> {
        let pool = EndpointPool::new(
            &[url.to_string()],
            Scheme::Http,
            PoolPolicy {
                rounds: 1,
                ..PoolPolicy::default()
            },
        )?;
        Self::with_pool_inner(pool, timeout, true)
    }

    /// Build a **read pool** over `urls`. Each request starts at a random
    /// healthy endpoint and fails over to the next one; the first success
    /// wins, and a failed endpoint is cooled down by the shared pool. Used by
    /// the spine for the source client only.
    pub fn with_urls(urls: &[String], timeout: Duration) -> Result<Self> {
        let pool = EndpointPool::new(urls, Scheme::Http, PoolPolicy::default())?;
        Self::with_pool_inner(pool, timeout, false)
    }

    /// Build a read client over a URL list under the deployment's shared pool
    /// policy. The configured path (CCTP chains, the beacon lane) uses this so
    /// a per-deployment round/cooldown budget is not silently replaced by the
    /// crate default.
    pub fn with_urls_and_policy(
        urls: &[String],
        timeout: Duration,
        policy: PoolPolicy,
    ) -> Result<Self> {
        let pool = EndpointPool::new(urls, Scheme::Http, policy)?;
        Self::with_pool_inner(pool, timeout, false)
    }

    /// Build a read client over an already-constructed pool. The REST and WS
    /// lanes use the same pool type; this constructor is the JSON-RPC seam.
    pub fn with_pool(pool: EndpointPool, timeout: Duration) -> Result<Self> {
        Self::with_pool_inner(pool, timeout, false)
    }

    fn with_pool_inner(pool: EndpointPool, timeout: Duration, write: bool) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|source| Error::ClientBuild { source })?;
        Ok(Self {
            http,
            pool: Arc::new(pool),
            next_id: AtomicU64::new(1),
            write,
        })
    }

    /// Refuse a write on a multi-endpoint read pool.
    fn ensure_write(&self) -> Result<()> {
        if self.write {
            Ok(())
        } else {
            Err(Error::PooledWrite)
        }
    }

    /// The primary endpoint (the first declared). `/admin/epoch` and the
    /// status document read this one; the read pool may use any of them.
    pub fn url(&self) -> &str {
        self.pool.primary()
    }

    /// Every endpoint, in declaration order.
    pub fn urls(&self) -> Vec<String> {
        self.pool.urls().into_iter().map(str::to_string).collect()
    }

    /// The shared health/rotation pool (for status and diagnostics).
    pub fn pool(&self) -> &EndpointPool {
        &self.pool
    }

    fn id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // ---- typed error constructors (one site per variant) ----------------

    fn transport(endpoint: &str, method: &str, source: reqwest::Error) -> Error {
        Error::Transport {
            endpoint: endpoint.to_string(),
            method: method.to_string(),
            source,
        }
    }

    fn status(endpoint: &str, method: &str, status: u16, body: &str) -> Error {
        Error::Status {
            endpoint: endpoint.to_string(),
            method: method.to_string(),
            status,
            body: truncate(body, 300).to_string(),
        }
    }

    fn rpc(endpoint: &str, method: &str, payload: RpcError) -> Error {
        Error::Rpc {
            endpoint: endpoint.to_string(),
            method: method.to_string(),
            payload,
        }
    }

    fn decode(endpoint: &str, method: &str, source: serde_json::Error) -> Error {
        Error::Decode {
            endpoint: endpoint.to_string(),
            method: method.to_string(),
            source,
        }
    }

    fn encode(endpoint: &str, method: &str, source: serde_json::Error) -> Error {
        Error::Encode {
            method: format!("{method} ({endpoint})"),
            source,
        }
    }

    // ---- transport ------------------------------------------------------

    fn request(
        &self,
        endpoint: &str,
        method: &str,
        params: impl Serialize,
    ) -> Result<SerializedRequest> {
        // Params are method-specific; erasing them to the official `RawValue`
        // is exactly what a request serializes to anyway.
        let params = serde_json::value::to_raw_value(&params)
            .map_err(|source| Self::encode(endpoint, method, source))?;
        Request::new(method.to_string(), Id::Number(self.id()), params)
            .serialize()
            .map_err(|source| Self::encode(endpoint, method, source))
    }

    /// One HTTP round trip to **one** endpoint: the raw status and body. The
    /// boundary decision belongs to [`Self::answer`] (single) /
    /// [`Self::classify`] (batch).
    async fn post_raw_at(
        &self,
        endpoint: &str,
        method: &str,
        body: &RawValue,
    ) -> Result<(reqwest::StatusCode, String)> {
        let resp = self
            .http
            .post(endpoint)
            .header(reqwest::header::CONTENT_TYPE, crate::JSON_CONTENT_TYPE)
            .body(body.get().to_string())
            .send()
            .await
            .map_err(|source| Self::transport(endpoint, method, source))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|source| Self::transport(endpoint, method, source))?;
        Ok((status, text))
    }

    /// The read path's one send: the shared [`EndpointPool::execute`] owns
    /// admission, failover, cooldown and the per-call budget. A
    /// single-endpoint client (the target) sees exactly one attempt.
    ///
    /// A fault is a transport error, a retryable JSON-RPC refusal (`-32005`
    /// throttle / `-32603` internal), an endpoint-level HTTP refusal (auth,
    /// wrong path, 413, 429, 5xx), or a 2xx body that is not JSON-RPC (a
    /// gateway page). A valid JSON-RPC error on any status is the node's
    /// answer and is **not** failed over: it is the same answer every
    /// endpoint would give (a bad method, a revert).
    ///
    /// When every round is exhausted the call returns
    /// [`Error::EndpointsExhausted`] or [`Error::PoolCooling`] — never a
    /// throttled/plan-refusal body as a successful answer.
    async fn post_raw_throttled(
        &self,
        method: &str,
        body: &RawValue,
    ) -> Result<(String, reqwest::StatusCode, String)> {
        // A single endpoint has no failover: one attempt, and its body is the
        // node's answer whatever it is (the caller's `answer` classifies a
        // throttle/refusal). This preserves the write client's semantics and
        // avoids turning one node's retryable refusal into a pool outage.
        if self.pool.len() == 1 {
            return self.post_raw_once(method, body).await;
        }
        let attempt = RpcAttempt {
            rpc: self,
            method,
            body,
        };
        match self.pool.execute(&attempt, &[]).await {
            Ok(served) => Ok((
                self.pool.url(served.index).to_string(),
                served.value.0,
                served.value.1,
            )),
            Err(error) => {
                let typed = pool_error(error);
                if typed.is_block_unavailable() {
                    tracing::debug!(
                        method,
                        endpoints = self.pool.len(),
                        error = %typed,
                        "every endpoint is behind the requested block; waiting"
                    );
                } else {
                    tracing::warn!(
                        method,
                        endpoints = self.pool.len(),
                        error = %typed,
                        "every endpoint in the pool refused; surfacing a typed exhaustion"
                    );
                }
                Err(typed)
            }
        }
    }

    /// The single-endpoint path: one attempt, and the body is returned even
    /// when it is a retryable refusal (the caller classifies it as the node's
    /// answer). No failover, no retry — the write client's contract.
    async fn post_raw_once(
        &self,
        method: &str,
        body: &RawValue,
    ) -> Result<(String, reqwest::StatusCode, String)> {
        let endpoint = self.pool.url(0);
        self.pool.record_attempt(0);
        match self.post_raw_at(endpoint, method, body).await {
            Ok((status, text)) => {
                if endpoint_fault(status, &text) {
                    self.pool.record_failure(0, now_ms());
                } else {
                    self.pool.record_success(0, now_ms());
                }
                Ok((endpoint.to_string(), status, text))
            }
            Err(fault) => {
                self.pool.record_failure(0, now_ms());
                Err(fault)
            }
        }
    }

    /// One endpoint's attempt for the shared executor: the pool records the
    /// health and owns the rotation; this owns one try and its classification.
    async fn attempt_endpoint(
        &self,
        index: usize,
        method: &str,
        body: &RawValue,
    ) -> Result<(reqwest::StatusCode, String)> {
        let endpoint = self.pool.url(index);
        match self.post_raw_at(endpoint, method, body).await {
            Ok((status, text)) if !endpoint_fault(status, &text) => Ok((status, text)),
            Ok((status, text)) => {
                tracing::debug!(
                    endpoint = %endpoint,
                    method,
                    status = status.as_u16(),
                    "endpoint fault; cooling it down and trying the next endpoint"
                );
                Err(Error::EndpointFault {
                    endpoint: endpoint.to_string(),
                    method: method.to_string(),
                    detail: format!("HTTP {}: {}", status.as_u16(), truncate(&text, 300)),
                })
            }
            Err(fault) => {
                tracing::debug!(
                    endpoint = %endpoint,
                    method,
                    error = %fault,
                    "endpoint transport fault; cooling it down and trying the next endpoint"
                );
                Err(fault)
            }
        }
    }
}

/// One JSON-RPC request against one endpoint, for the shared pool executor.
struct RpcAttempt<'a> {
    rpc: &'a Rpc,
    method: &'a str,
    body: &'a RawValue,
}

impl Attempt for RpcAttempt<'_> {
    type Output = (reqwest::StatusCode, String);
    type Fault = Error;

    async fn attempt(&self, index: usize) -> AttemptOutcome<Self::Output, Self::Fault> {
        match self
            .rpc
            .attempt_endpoint(index, self.method, self.body)
            .await
        {
            Ok(answer) => AttemptOutcome::Served(answer),
            Err(fault) => AttemptOutcome::Fault(fault),
        }
    }
}

/// The pool's typed exhaustion in the client's error taxonomy. A cooling
/// pool is not an endpoint answer: it is a retry-later signal, kept distinct
/// so a caller can honour `Retry-After` instead of treating it as a refusal.
fn pool_error(error: PoolError<Error>) -> Error {
    match error {
        PoolError::Exhausted { attempts, last } => Error::EndpointsExhausted {
            attempts,
            last: Box::new(last.unwrap_or(Error::NoEndpoints)),
        },
        PoolError::Cooling { retry_after_ms } => Error::PoolCooling { retry_after_ms },
    }
}

impl Rpc {
    /// The JSON-RPC-over-HTTP boundary rule: the body is the node's answer
    /// whenever it parses as one, whatever the HTTP status; the status only
    /// classifies bodies that are not answers (gateway pages, auth walls,
    /// empty bodies). A JSON-RPC error on a non-2xx status is the node's
    /// typed refusal, not a transport fault.
    fn classify(
        endpoint: &str,
        method: &str,
        status: reqwest::StatusCode,
        text: &str,
    ) -> Result<ResponsePacket> {
        match serde_json::from_str::<ResponsePacket>(text) {
            Ok(packet) => Ok(packet),
            Err(_) if !status.is_success() => {
                Err(Self::status(endpoint, method, status.as_u16(), text))
            }
            Err(source) => Err(Self::decode(endpoint, method, source)),
        }
    }

    /// The node's answer as a result or the node's own refusal.
    fn response_result(response: Response) -> std::result::Result<Box<RawValue>, RpcError> {
        match response.payload {
            ResponsePayload::Success(result) => Ok(result),
            ResponsePayload::Failure(error) => Err(error),
        }
    }

    /// One call's answer: exactly one `Response` (a batch body here is corrupt).
    fn answer(
        endpoint: &str,
        method: &str,
        status: reqwest::StatusCode,
        text: &str,
    ) -> Result<Box<RawValue>> {
        match Self::classify(endpoint, method, status, text)? {
            ResponsePacket::Single(response) => Self::response_result(response)
                .map_err(|payload| Self::rpc(endpoint, method, payload)),
            ResponsePacket::Batch(_) => Err(Self::decode(
                endpoint,
                method,
                serde_json::Error::custom("a batch answer to a single call"),
            )),
        }
    }

    /// Single call, raw result. On a pooled client the call starts at a
    /// random endpoint and fails over; the answer's own endpoint names any
    /// JSON-RPC refusal.
    pub async fn call_raw(&self, method: &str, params: impl Serialize) -> Result<Box<RawValue>> {
        let value = serde_json::to_value(&params).map_err(|source| Error::Encode {
            method: method.to_string(),
            source,
        })?;
        self.check_log_range(method, &value)?;
        let request = self.request(self.url(), method, params)?;
        let (endpoint, status, text) = self
            .post_raw_throttled(method, &request.take_request())
            .await?;
        Self::answer(&endpoint, method, status, &text)
    }

    /// Refuse an over-cap `eth_getLogs` before it reaches an endpoint. A
    /// wide range is the measured outage shape (a 57,100-block request stalled
    /// the public HyperEVM RPC ~30 s and cascaded through the indexer, the
    /// engine and the CCTP bridge); wide callers use [`Self::logs_range`],
    /// which splits into these bounded chunks.
    fn check_log_range(&self, method: &str, params: &serde_json::Value) -> Result<()> {
        if method != "eth_getLogs" {
            return Ok(());
        }
        let Some((from, to)) = log_range_of(params) else {
            return Ok(());
        };
        if to < from {
            return Ok(());
        }
        let cap = self.log_range_cap();
        let blocks = to.saturating_sub(from).saturating_add(1);
        if blocks > cap {
            return Err(Error::LogRangeTooWide {
                method: method.to_string(),
                from,
                to,
                blocks,
                cap,
            });
        }
        Ok(())
    }

    /// A batch is another door to `eth_getLogs`; guard each item so an
    /// over-cap range cannot ride a batch past the single-call check.
    fn check_batch_log_ranges(&self, calls: &[Call]) -> Result<()> {
        for call in calls {
            let params: serde_json::Value =
                serde_json::from_str(call.params.get()).map_err(|source| Error::Encode {
                    method: call.meta.method.to_string(),
                    source,
                })?;
            self.check_log_range(&call.meta.method, &params)?;
        }
        Ok(())
    }

    /// The narrowest declared `eth_getLogs` cap in this client's pool: a
    /// pooled call may land on any endpoint, so a chunk is safe only under the
    /// minimum.
    pub fn log_range_cap(&self) -> u64 {
        self.pool.log_range_cap()
    }

    /// `eth_getLogs` over an inclusive block range, split into chunks no wider
    /// than the pool's narrowest declared cap. The filter's own block bounds
    /// are replaced per chunk; every other predicate (addresses, topics) is
    /// preserved. Logs are concatenated in range order.
    pub async fn logs_range(&self, filter: &Filter, from: u64, to: u64) -> Result<Vec<Log>> {
        let mut logs = Vec::new();
        for (start, end) in log_chunks(from, to, self.log_range_cap()) {
            let filter = filter
                .clone()
                .from_block(BlockNumberOrTag::Number(start))
                .to_block(BlockNumberOrTag::Number(end));
            logs.extend(self.call::<Vec<Log>>("eth_getLogs", (filter,)).await?);
        }
        Ok(logs)
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<T> {
        let value = self.call_raw(method, params).await?;
        serde_json::from_str(value.get()).map_err(|source| Self::decode(self.url(), method, source))
    }

    /// Result may legitimately be null (e.g. `eth_getBlockByNumber` of a
    /// pruned/future block, `eth_getBlockAccessList` before activation):
    /// `null` is the `None` arm, a value is `Some`, corruption still dies here.
    pub async fn call_opt<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<Option<T>> {
        self.call(method, params).await
    }

    /// Batched call. Returns per-item results in request order.
    ///
    /// The batch is split so that no single HTTP body exceeds `max_bytes`
    /// (anvil rejects bodies >= 2 MB with `-32600 Invalid request`; public
    /// endpoints answer 413) and no sub-batch exceeds `max_items`.
    pub async fn batch(&self, calls: &[Call]) -> Result<ChunkAnswers> {
        self.batch_limited(calls, BATCH_MAX_ITEMS, 1 << 20).await
    }

    pub async fn batch_limited(
        &self,
        calls: &[Call],
        max_items: usize,
        max_bytes: usize,
    ) -> Result<ChunkAnswers> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        self.check_batch_log_ranges(calls)?;
        // The chunks ride concurrently (bounded): each is an independent HTTP
        // body, and the join keeps the calls' order (chunk order, item
        // order). The sequential form made one wide window cost the sum of
        // every round-trip (measured live: ~450 s for a 300-block window
        // against the public source — the follow could never keep up).
        let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
        let mut start = 0usize;
        while start < calls.len() {
            let end = chunk_end(calls, start, max_items, max_bytes);
            ranges.push(start..end);
            start = end;
        }
        let mut results: Vec<Option<Result<ChunkAnswers>>> =
            (0..ranges.len()).map(|_| None).collect();
        let mut set = tokio::task::JoinSet::new();
        let mut next = 0usize;
        while next < ranges.len() || !set.is_empty() {
            while set.len() < BATCH_CONCURRENCY && next < ranges.len() {
                let me = self.clone();
                let chunk = calls[ranges[next].clone()].to_vec();
                let idx = next;
                set.spawn(async move { (idx, me.batch_once(&chunk).await) });
                next += 1;
            }
            match set.join_next().await {
                Some(Ok((idx, r))) => results[idx] = Some(r),
                Some(Err(join)) => return Err(self.batch_task_error(&join)),
                None => break,
            }
        }
        let mut out = Vec::with_capacity(calls.len());
        for (idx, r) in results.into_iter().enumerate() {
            let chunk = match r {
                Some(r) => r?,
                None => {
                    return Err(
                        self.batch_task_error_text(format!("batch chunk {idx} never completed"))
                    )
                }
            };
            out.extend(chunk);
        }
        Ok(out)
    }

    /// A spawned chunk task's failure as the crate's typed error (the code
    /// stays outside the JSON-RPC ranges, like [`missing_from_batch`]'s).
    fn batch_task_error(&self, join: &tokio::task::JoinError) -> Error {
        self.batch_task_error_text(format!("batch task failed: {join}"))
    }

    fn batch_task_error_text(&self, message: String) -> Error {
        Error::Rpc {
            endpoint: self.url().to_string(),
            method: "batch".to_string(),
            payload: RpcError {
                code: -1,
                message: message.into(),
                data: None,
            },
        }
    }

    async fn batch_once(
        &self,
        calls: &[Call],
    ) -> Result<Vec<std::result::Result<Box<RawValue>, RpcError>>> {
        let (ids, body) = self.batch_request(calls)?;
        let mut by_id = Self::index_by_id(self.post_batch(&body).await?);
        let mut out = Vec::with_capacity(calls.len());
        for id in ids {
            match by_id.remove(&id) {
                Some(response) => out.push(Self::response_result(response)),
                None => out.push(Err(missing_from_batch(id))),
            }
        }
        Ok(out)
    }

    /// The typed form of [`Self::batch`]: one method over many params, each
    /// answer decoded to `Option<T>` (a null result is `None`; corruption
    /// still dies here). An item's RPC error becomes the crate's typed error,
    /// so the per-item error type never leaves the crate. The per-block reads
    /// of a wide window ride one body — the sequential form cost three
    /// round-trips per block against the public source, slower than its own
    /// block cadence (measured live).
    pub async fn batch_typed<T: DeserializeOwned, P: Serialize>(
        &self,
        method: &'static str,
        params: &[P],
    ) -> Result<Vec<Option<T>>> {
        self.batch_typed_limited(method, params, BATCH_MAX_ITEMS)
            .await
    }

    /// [`Self::batch_typed`] with an explicit sub-batch item cap: receipt-
    /// heavy methods answer multi-megabyte bodies, so their cap sits far
    /// below the default (measured live: one 300-block receipts batch made
    /// the public endpoint throttle — the bodies, not the call count, are
    /// the budget).
    pub async fn batch_typed_limited<T: DeserializeOwned, P: Serialize>(
        &self,
        method: &'static str,
        params: &[P],
        max_items: usize,
    ) -> Result<Vec<Option<T>>> {
        let calls: Vec<Call> = params
            .iter()
            .map(|p| call(method, p))
            .collect::<Result<Vec<_>>>()?;
        let items = self.batch_limited(&calls, max_items, 1 << 20).await?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(self.decode_batch_item::<T>(method, item)?);
        }
        Ok(out)
    }

    /// A mixed batch: each item as its raw value (`None` for a null result);
    /// an item's RPC error becomes the crate's typed error. The caller owns
    /// each item's decode — the batch may mix methods (the read plan's four
    /// key kinds ride one body).
    pub async fn batch_raw(&self, calls: &[Call]) -> Result<Vec<Option<Box<RawValue>>>> {
        self.batch_raw_limited(calls, BATCH_MAX_ITEMS).await
    }

    /// [`Self::batch_raw`] with the caller's own item cap: a local anvil
    /// answers 500 items per body, so a write-side or journal reader rides
    /// one body where the public cap (20) would take dozens. The byte cap
    /// (1 MiB) is unchanged.
    pub async fn batch_raw_limited(
        &self,
        calls: &[Call],
        max_items: usize,
    ) -> Result<Vec<Option<Box<RawValue>>>> {
        let items = self.batch_limited(calls, max_items, 1 << 20).await?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match item {
                Ok(raw) => out.push(
                    serde_json::from_str::<Option<Box<RawValue>>>(raw.get())
                        .map_err(|source| Self::decode(self.url(), "batch", source))?,
                ),
                Err(payload) => {
                    return Err(Error::Rpc {
                        endpoint: self.url().to_string(),
                        method: "batch".to_string(),
                        payload,
                    })
                }
            }
        }
        Ok(out)
    }

    /// One batch item decoded: `Ok(raw)` parses (null → `None`), an item's
    /// RPC error maps to [`Error::Rpc`].
    fn decode_batch_item<T: DeserializeOwned>(
        &self,
        method: &str,
        item: std::result::Result<Box<RawValue>, RpcError>,
    ) -> Result<Option<T>> {
        match item {
            Ok(raw) => serde_json::from_str::<Option<T>>(raw.get())
                .map_err(|source| Self::decode(self.url(), method, source)),
            Err(payload) => Err(Error::Rpc {
                endpoint: self.url().to_string(),
                method: method.to_string(),
                payload,
            }),
        }
    }

    /// The batch request's ids and its serialized body (alloy's request
    /// packet types; the params are already `RawValue`).
    fn batch_request(&self, calls: &[Call]) -> Result<(Vec<u64>, Box<RawValue>)> {
        let ids: Vec<u64> = calls.iter().map(|_| self.id()).collect();
        let requests = calls
            .iter()
            .zip(&ids)
            .map(|(call, id)| {
                let mut call = call.clone();
                call.meta.id = Id::Number(*id);
                call.serialize()
            })
            .collect::<serde_json::Result<Vec<SerializedRequest>>>()
            .map_err(|source| Self::encode(self.url(), "batch", source))?;
        let body = RequestPacket::Batch(requests)
            .serialize()
            .map_err(|source| Self::encode(self.url(), "batch", source))?;
        Ok((ids, body))
    }

    /// Post one batch body and classify the answer — the same boundary rule as
    /// `answer` (a body that parses as answers *is* the answers, whatever the
    /// HTTP status); a body that does not is a `BatchDecode` naming the
    /// request text, and a single answer to a batch call is corrupt.
    async fn post_batch(&self, body: &RawValue) -> Result<Vec<Response>> {
        let (endpoint, status, text) = self.post_raw_throttled("batch", body).await?;
        let packet = match Self::classify(&endpoint, "batch", status, &text) {
            Ok(packet) => packet,
            Err(Error::Decode { source, .. }) => {
                return Err(Error::BatchDecode {
                    endpoint,
                    request: truncate(body.get(), 600).to_string(),
                    source,
                })
            }
            Err(e) => return Err(e),
        };
        match packet {
            ResponsePacket::Batch(responses) => Ok(responses),
            // A single answer to a batch: an *error* is the node's refusal
            // (a throttled batch is answered with one error object) —
            // surfaced as such, never as a decode fault; a single *success*
            // is a server that mishandled the batch.
            ResponsePacket::Single(response) => match Self::response_result(response) {
                Err(payload) => Err(Self::rpc(&endpoint, "batch", payload)),
                Ok(_) => Err(Self::decode(
                    &endpoint,
                    "batch",
                    serde_json::Error::custom("a single answer to a batch call"),
                )),
            },
        }
    }

    /// Index a batch's answers by id. Items without a numeric id are not ours
    /// (subscription pushes, a different client's response); our ids that the
    /// node did not answer surface as `missing_from_batch`, never as a silent
    /// skip.
    fn index_by_id(responses: Vec<Response>) -> std::collections::HashMap<u64, Response> {
        responses
            .into_iter()
            .filter_map(|response| match response.id {
                Id::Number(id) => Some((id, response)),
                Id::String(_) | Id::None => None,
            })
            .collect()
    }

    // ---- eth namespace -------------------------------------------------

    pub async fn block_number(&self) -> Result<u64> {
        let quantity: U64 = self.call("eth_blockNumber", NO_PARAMS).await?;
        Ok(quantity.to::<u64>())
    }

    /// Wait (bounded) until the server answers `eth_blockNumber`, so a
    /// coordinated restart — the anvil re-forking, the node unwinding — does
    /// not turn the caller into CrashLoopBackOff. Warns every 30 s while
    /// waiting; on the deadline the last fault is returned, named by `what`.
    ///
    /// The one boot-wait every service uses (the mainnet engine's anvil, the
    /// venue's V, the CCTP bridge's two chains): one implementation, one
    /// failure shape.
    pub async fn wait_until_up(&self, what: &'static str, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let mut last_log = std::time::Instant::now();
        loop {
            match self.block_number().await {
                Ok(head) => {
                    tracing::info!(server = what, head, "rpc server is up");
                    return Ok(());
                }
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::NotReady {
                            what,
                            timeout_secs: timeout.as_secs(),
                            source: Box::new(e),
                        });
                    }
                    if last_log.elapsed() > Duration::from_secs(30) {
                        tracing::warn!(server = what, error = %e, "waiting for the rpc server to come up");
                        last_log = std::time::Instant::now();
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// The chain's id (`eth_chainId`): the venue's boot assert reads it.
    pub async fn chain_id(&self) -> Result<u64> {
        let quantity: U64 = self.call("eth_chainId", NO_PARAMS).await?;
        Ok(quantity.to::<u64>())
    }

    /// `eth_chainId` against **one** endpoint, outside the pool's rotation:
    /// the boot identity assert must name the provider that answered wrong,
    /// not whichever endpoint the rotation picked.
    pub async fn chain_id_at(&self, index: usize) -> Result<u64> {
        let endpoint = self.pool.url(index);
        let request = self.request(endpoint, "eth_chainId", NO_PARAMS)?;
        self.pool.record_attempt(index);
        let (status, text) = match self
            .post_raw_at(endpoint, "eth_chainId", &request.take_request())
            .await
        {
            Ok(answer) => answer,
            Err(fault) => {
                // A transport failure is this endpoint's fault: cool it so the
                // boot assert does not keep selecting it on every later read.
                self.pool.record_failure(index, now_ms());
                return Err(fault);
            }
        };
        match Self::answer(endpoint, "eth_chainId", status, &text) {
            Ok(value) => {
                let quantity: U64 = serde_json::from_str(value.get())
                    .map_err(|source| Self::decode(endpoint, "eth_chainId", source))?;
                self.pool.record_success(index, now_ms());
                Ok(quantity.to::<u64>())
            }
            Err(fault) => {
                self.pool.record_failure(index, now_ms());
                Err(fault)
            }
        }
    }

    /// Assert the pool's reachable endpoints are `expected`. A wrong-chain
    /// provider must fail the boot, never poison a read. An unreachable
    /// endpoint is a *fault*, not a wrong chain: it is cooled by
    /// [`Self::chain_id_at`] and the pool boots on the remaining endpoints
    /// (failover is the point), but a pool with no reachable endpoint is a
    /// boot refusal.
    pub async fn verify_chain(&self, expected: u64) -> Result<()> {
        // The cheap half first: a declared chain id that disagrees is a config
        // fault attributable to one URL, before any request.
        self.pool.verify_declared_chain(expected)?;
        let mut verified = 0u32;
        let mut last: Option<Error> = None;
        for index in 0..self.pool.len() {
            match self.chain_id_at(index).await {
                Ok(got) if got == expected => verified += 1,
                Ok(got) => {
                    return Err(Error::WrongChain {
                        endpoint: self.pool.url(index).to_string(),
                        got,
                        want: expected,
                    });
                }
                Err(fault) => last = Some(fault),
            }
        }
        if verified == 0 {
            return Err(last.unwrap_or(Error::NoEndpoints));
        }
        Ok(())
    }

    pub async fn block_by_number(&self, n: u64, txs: Txs) -> Result<Block> {
        self.call_opt(BLOCK_BY_NUMBER, (U64::from(n), txs.full()))
            .await?
            .ok_or_else(|| Error::BlockMissing {
                endpoint: self.url().to_string(),
                n,
            })
    }

    pub async fn block_by_number_opt(&self, n: u64, txs: Txs) -> Result<Option<Block>> {
        self.call_opt(BLOCK_BY_NUMBER, (U64::from(n), txs.full()))
            .await
    }

    /// The head plus one block in a single body: two reads a follow needs
    /// together (its binding constraint is the endpoint's request budget) —
    /// the same decodes and the same missing-block fault as the single-call
    /// forms.
    pub async fn head_and_block(&self, n: u64) -> Result<(u64, Block)> {
        let calls = [
            call("eth_blockNumber", NO_PARAMS)?,
            call(BLOCK_BY_NUMBER, (U64::from(n), false))?,
        ];
        let mut answers = self.batch_raw(&calls).await?.into_iter();
        let head: U64 = self.decode_item("head_and_block", answers.next().flatten())?;
        let block: Block = answers
            .next()
            .flatten()
            .ok_or_else(|| Error::BlockMissing {
                endpoint: self.url().to_string(),
                n,
            })
            .and_then(|raw| self.decode_item("head_and_block", Some(raw)))?;
        Ok((head.to::<u64>(), block))
    }

    /// One batch item decoded, a missing item named as a fault (the strict
    /// shape the single-call helpers give).
    fn decode_item<T: DeserializeOwned>(
        &self,
        method: &str,
        raw: Option<Box<RawValue>>,
    ) -> Result<T> {
        let raw = raw.ok_or_else(|| {
            Self::decode(
                self.url(),
                method,
                serde_json::Error::custom("a null answer"),
            )
        })?;
        serde_json::from_str(raw.get()).map_err(|source| Self::decode(self.url(), method, source))
    }

    /// `tag` is the protocol's own block-parameter string: `latest`,
    /// `finalized`, `safe`, `pending`, `earliest`, or a hex height.
    pub async fn block_by_tag(&self, tag: &str, txs: Txs) -> Result<Option<Block>> {
        self.call_opt(BLOCK_BY_NUMBER, (tag, txs.full())).await
    }

    pub async fn block_access_list(&self, n: u64) -> Result<Option<BlockAccessList>> {
        match self
            .call_opt::<BlockAccessList>("eth_getBlockAccessList", (U64::from(n),))
            .await
        {
            Ok(v) => Ok(v),
            // Older builds / non-reth endpoints may not know the method: a
            // typed predicate, never a message-text match.
            Err(e) if e.is_method_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn receipts(&self, n: u64) -> Result<Vec<TransactionReceipt>> {
        self.call("eth_getBlockReceipts", (U64::from(n),)).await
    }

    /// `eth_getSystemTxsByBlockNumber` (HyperEVM): the protocol's Core→EVM
    /// pseudo-transactions (they carry a fabricated signature, `gasPrice 0x0`
    /// and a real `from`; measured 2026-10-24). The public RPC strips them
    /// from block-shaped responses but serves them here; a chain without the
    /// method answers an empty list, so callers need no capability check to
    /// be safe.
    pub async fn system_txs(&self, n: u64) -> Result<Vec<Transaction>> {
        match self
            .call_opt::<Vec<Transaction>>("eth_getSystemTxsByBlockNumber", (U64::from(n),))
            .await
        {
            Ok(Some(v)) => Ok(v),
            // A method that exists answers a list; `null` is not an empty
            // list. Reading it as empty would silently drop a block's
            // system transactions.
            Ok(None) => Err(Error::Corrupt {
                what: "eth_getSystemTxsByBlockNumber",
                detail: format!("block {n} answered null, not a list"),
            }),
            Err(e) if e.is_method_not_found() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub async fn logs(&self, n: u64) -> Result<Vec<Log>> {
        let block = BlockNumberOrTag::Number(n);
        let filter = Filter::new().from_block(block).to_block(block);
        self.call("eth_getLogs", (filter,)).await
    }

    fn call_tx(to: Address, data: &Bytes) -> TransactionRequest {
        TransactionRequest {
            to: Some(TxKind::Call(to)),
            input: data.clone().into(),
            ..Default::default()
        }
    }

    pub async fn call_at(&self, to: Address, data: &Bytes, block: u64) -> Result<Bytes> {
        let params = (Self::call_tx(to, data), BlockNumberOrTag::Number(block));
        self.call(ETH_CALL, params).await
    }

    /// eth_call against the current state, including unmined cheatcode writes
    /// (the latest-state read: the probes and the composed-BAL parity compare
    /// the devnet chain against the parent).
    pub async fn call_latest(&self, to: Address, data: &Bytes) -> Result<Bytes> {
        self.call(
            ETH_CALL,
            (Self::call_tx(to, data), BlockNumberOrTag::Latest),
        )
        .await
    }

    /// Encode a typed call, run it against the current state, and decode the
    /// return with the interface's own generated decoder — the one shape
    /// every typed read shares (the `SolCall` comes from the contract
    /// bindings; a decode fault is [`Error::Abi`], never a node fault).
    pub async fn call_decode<C: alloy_sol_types::SolCall>(
        &self,
        to: Address,
        call: &C,
    ) -> Result<C::Return> {
        let data = Bytes::from(call.abi_encode());
        let out = self.call_latest(to, &data).await?;
        C::abi_decode_returns(&out).map_err(|source| Error::Abi {
            what: C::SIGNATURE,
            source,
        })
    }

    /// eth_call with a state override, returned as raw bytes. Used to discover
    /// ERC20 balance slots without mining anything (anvil's own dealERC20 probe
    /// needs mined writes and fails on a no-mining fork).
    ///
    /// A transport/RPC fault is a typed error, never collapsed into `None`: a
    /// probe that could not run must not masquerade as "this slot is not the
    /// balance slot" (boundary-discipline: a fault is not an empty answer).
    pub async fn call_with_override(
        &self,
        to: Address,
        data: &Bytes,
        overrides: &StateOverride,
    ) -> Result<Bytes> {
        let params = (Self::call_tx(to, data), BlockNumberOrTag::Latest, overrides);
        self.call(ETH_CALL, params).await
    }

    pub async fn balance_at(&self, addr: Address, block: u64) -> Result<U256> {
        self.call(crate::ETH_GET_BALANCE, (addr, U64::from(block)))
            .await
    }

    pub async fn storage_at(&self, addr: Address, slot: U256, block: u64) -> Result<B256> {
        self.call("eth_getStorageAt", (addr, slot, U64::from(block)))
            .await
    }

    /// Read a storage word at the node's latest state: the block parameter
    /// `0` names a real block on a fork (the source's genesis-era state, i.e.
    /// empty), so "latest" must be spelled out — measured with the canary.
    pub async fn storage_at_latest(&self, addr: Address, slot: U256) -> Result<B256> {
        self.call("eth_getStorageAt", (addr, slot, BlockNumberOrTag::Latest))
            .await
    }

    pub async fn code_at(&self, addr: Address, block: u64) -> Result<Bytes> {
        self.call("eth_getCode", (addr, U64::from(block))).await
    }

    pub async fn nonce_at(&self, addr: Address, block: u64) -> Result<u64> {
        let quantity: U64 = self
            .call("eth_getTransactionCount", (addr, U64::from(block)))
            .await?;
        Ok(quantity.to::<u64>())
    }

    /// The read plan's batched form: the four key kinds map to their RPC
    /// methods in one mixed body, decoded to the write vocabulary. A wide
    /// window's plan reaches thousands of keys (every transfer's holder), and
    /// the per-key form cost a round-trip per key against the public source
    /// (measured live: a 300-block catch-up window ground for minutes). A
    /// null answer for a key is a fault, never a silent zero.
    pub async fn read_keys(&self, keys: &[Key], block: u64) -> Result<Vec<Value>> {
        self.read_keys_limited(keys, block, BATCH_MAX_ITEMS).await
    }

    /// [`Self::read_keys`] with the caller's own item cap (the anvil
    /// contract where the target is the local devnet chain).
    pub async fn read_keys_limited(
        &self,
        keys: &[Key],
        block: u64,
        max_items: usize,
    ) -> Result<Vec<Value>> {
        let calls = Self::key_calls(keys, block)?;
        let raws = self.batch_raw_limited(&calls, max_items).await?;
        let mut out = Vec::with_capacity(keys.len());
        for (key, raw) in keys.iter().zip(raws) {
            let raw = raw.ok_or_else(|| self.null_key_read())?;
            out.push(self.decode_key(key, &raw)?);
        }
        Ok(out)
    }

    /// The wire calls for a set of key reads — the mapping [`Self::read_keys`]
    /// rides, public so a caller that needs per-item tolerance composes its
    /// own [`Self::batch`] instead of duplicating it.
    pub fn key_calls(keys: &[Key], block: u64) -> Result<Vec<Call>> {
        keys.iter()
            .map(|key| match key {
                Key::Storage { address, slot } => {
                    call("eth_getStorageAt", (*address, *slot, U64::from(block)))
                }
                Key::Balance(address) => call(crate::ETH_GET_BALANCE, (*address, U64::from(block))),
                Key::Nonce(address) => {
                    call("eth_getTransactionCount", (*address, U64::from(block)))
                }
                Key::Code(address) => call("eth_getCode", (*address, U64::from(block))),
            })
            .collect()
    }

    /// The wire calls for a set of `eth_call`s at one block tag — the same
    /// encoding [`Self::call_at`] and [`Self::call_latest`] send.
    pub fn call_calls(calls: &[(Address, Bytes)], at: BlockNumberOrTag) -> Result<Vec<Call>> {
        calls
            .iter()
            .map(|(to, data)| call(ETH_CALL, (Self::call_tx(*to, data), at)))
            .collect()
    }

    /// One key's answer, decoded to its value kind.
    fn decode_key(&self, key: &Key, raw: &RawValue) -> Result<Value> {
        let method = "batch";
        Ok(match key {
            Key::Storage { .. } => {
                let word: B256 = serde_json::from_str(raw.get())
                    .map_err(|source| Self::decode(self.url(), method, source))?;
                Value::Storage(U256::from_be_bytes(word.0))
            }
            Key::Balance(_) => Value::Balance(
                serde_json::from_str(raw.get())
                    .map_err(|source| Self::decode(self.url(), method, source))?,
            ),
            Key::Nonce(_) => {
                let quantity: U64 = serde_json::from_str(raw.get())
                    .map_err(|source| Self::decode(self.url(), method, source))?;
                Value::Nonce(quantity.to::<u64>())
            }
            Key::Code(_) => Value::Code(
                serde_json::from_str(raw.get())
                    .map_err(|source| Self::decode(self.url(), method, source))?,
            ),
        })
    }

    /// A null answer to a key read: the same refusal the single-key path's
    /// decode raises.
    fn null_key_read(&self) -> Error {
        Self::decode(
            self.url(),
            "batch",
            serde_json::Error::custom("a null answer to a key read"),
        )
    }

    /// `debug_traceBlockByNumber` with prestateTracer diffMode (BAL fallback
    /// and write-tracking): one alloy `TraceResult` per transaction.
    pub async fn trace_block_prestate(&self, n: u64) -> Result<Vec<TraceResult>> {
        self.call(
            crate::TRACE_BLOCK_METHOD,
            (U64::from(n), prestate_diff_options()),
        )
        .await
    }

    pub async fn trace_tx_prestate(&self, hash: B256) -> Result<GethTrace> {
        self.call("debug_traceTransaction", (hash, prestate_diff_options()))
            .await
    }

    /// `debug_traceBlockByNumber` with the callTracer: one call-frame tree per
    /// transaction. The venue bridge needs the *internal* calls (a contract
    /// calling CoreWriter attributes the action to itself, not to the tx
    /// sender).
    pub async fn trace_block_calls(&self, n: u64) -> Result<Vec<TraceResult>> {
        let options = GethDebugTracingOptions::call_tracer(Default::default());
        self.call(crate::TRACE_BLOCK_METHOD, (U64::from(n), options))
            .await
    }

    // ---- anvil namespace -----------------------------------------------

    pub async fn anvil_set_automine(&self, mode: Automine) -> Result<()> {
        self.ensure_write()?;

        self.call_raw("anvil_setAutomine", (matches!(mode, Automine::On),))
            .await?;
        Ok(())
    }

    pub async fn anvil_mine(&self, blocks: u64) -> Result<()> {
        self.ensure_write()?;

        self.call_raw("anvil_mine", (U64::from(blocks),)).await?;
        Ok(())
    }

    /// Mine one block iff the pool is non-empty; `Some(pending)` is the
    /// pool's depth when a block was mined, `None` when it was empty (V's
    /// producer rule: an idle chain stays static; T's prepend runs the same
    /// check with its timestamp step around it).
    pub async fn mine_pending(&self) -> Result<Mined> {
        let status = self.txpool_status().await?;
        let pending = status.pending;
        if pending == 0 {
            return Ok(Mined::Empty);
        }
        self.anvil_mine(1).await?;
        Ok(Mined::Block { pending })
    }

    /// Set the next block's timestamp alone (the devnet prepend block: the
    /// diffs' own context alignment follows separately).
    pub async fn anvil_set_next_block_timestamp(&self, timestamp: u64) -> Result<()> {
        self.ensure_write()?;

        self.call_raw("anvil_setNextBlockTimestamp", (U64::from(timestamp),))
            .await?;
        Ok(())
    }

    /// Align the next block's context fields T owns (timestamp/coinbase/
    /// prevRandao/baseFee); the gas limit stays the devnet's own.
    pub async fn set_next_block_context(&self, timestamp: u64, header: &Header) -> Result<()> {
        self.apply_env_calls(Self::context_calls(timestamp, header)?)
            .await
    }

    /// The context fields shared by both chains (the parent header verbatim).
    fn context_calls(timestamp: u64, header: &Header) -> Result<Vec<Call>> {
        let mut calls = vec![
            call("anvil_setNextBlockTimestamp", (U64::from(timestamp),))?,
            call("anvil_setCoinbase", (header.beneficiary,))?,
            call("anvil_setNextBlockPrevRandao", (header.mix_hash,))?,
        ];
        if let Some(base_fee) = header.base_fee_per_gas {
            calls.push(call(
                "anvil_setNextBlockBaseFeePerGas",
                (U256::from(base_fee),),
            )?);
        }
        Ok(calls)
    }

    /// Send the alignment batch and surface the first refusal with its method.
    /// Batched because single writes are 9.6-18 ms each (measured).
    async fn apply_env_calls(&self, calls: Vec<Call>) -> Result<()> {
        self.ensure_write()?;

        let results = self.batch(&calls).await?;
        for (call, result) in calls.iter().zip(results) {
            if let Err(payload) = result {
                return Err(Self::rpc(self.url(), &call.meta.method, payload));
            }
        }
        Ok(())
    }

    pub async fn anvil_node_info(&self) -> Result<NodeInfo> {
        self.call("anvil_nodeInfo", NO_PARAMS).await
    }

    /// The chain's fork source + block from `anvil_nodeInfo` (None when the
    /// chain was not forked).
    pub async fn fork_config(&self) -> Result<Option<ForkConfig>> {
        let info = self.anvil_node_info().await?;
        Ok(info
            .fork_config
            .fork_url
            .zip(info.fork_config.fork_block_number)
            .map(|(url, block)| ForkConfig { url, block }))
    }

    /// The fork base block (`anvil_nodeInfo.forkConfig.forkBlockNumber`).
    ///
    /// Three answers, per boundary discipline: `Some(base)` on an anvil fork;
    /// `None` when the method is absent (a real chain — the caller's own
    /// configured start governs) or the chain is not forked; a *fault*
    /// (transport, malformed answer) propagates — a fault must never read as
    /// a fact. Callers scanning below a fork base must know: anvil forwards
    /// such ranges upstream, and pre-pin blocks are the *source's* history.
    pub async fn fork_block(&self) -> Result<Option<u64>> {
        match self.anvil_node_info().await {
            Ok(info) => Ok(info.fork_config.fork_block_number),
            Err(Error::Rpc { payload, .. }) if payload.code == METHOD_NOT_FOUND => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// `anvil_reset` to a fresh fork at the source's *latest* block (the
    /// venue's `/admin/epoch`: the driver's canary detects the reset and runs
    /// the epoch rule — new base = new epoch, same base = restore). Always
    /// explicit: the measured no-params form drops the fork entirely.
    pub async fn anvil_reset_fork_latest(&self, url: &str) -> Result<()> {
        self.ensure_write()?;

        let forking = Forking {
            json_rpc_url: Some(url.to_string()),
            block_number: None,
        };
        self.call_raw("anvil_reset", (ResetParams { forking },))
            .await?;
        Ok(())
    }

    /// (pending, queued) counts from txpool_status.
    pub async fn txpool_status(&self) -> Result<TxpoolStatus> {
        self.call("txpool_status", NO_PARAMS).await
    }

    /// The full pool content (per sender, per nonce) — used to find what is
    /// still pending before mining.
    /// `anvil_impersonateAccount` — the devnet's substitution path (the
    /// attester manager, the bridge's system-address senders).
    pub async fn anvil_impersonate_account(&self, addr: Address) -> Result<()> {
        self.ensure_write()?;

        self.call_raw("anvil_impersonateAccount", (addr,)).await?;
        Ok(())
    }

    /// `anvil_stopImpersonatingAccount`.
    pub async fn anvil_stop_impersonating_account(&self, addr: Address) -> Result<()> {
        self.ensure_write()?;

        self.call_raw("anvil_stopImpersonatingAccount", (addr,))
            .await?;
        Ok(())
    }

    /// `anvil_setBalance` — the impersonated senders (the bridge's system
    /// addresses) need gas on V.
    pub async fn anvil_set_balance(&self, addr: Address, balance: U256) -> Result<()> {
        self.ensure_write()?;

        self.call_raw("anvil_setBalance", (addr, balance)).await?;
        Ok(())
    }

    /// `eth_sendTransaction` from an arbitrary sender (V runs
    /// auto-impersonate): the bridge's system-address completion path.
    pub async fn send_transaction(&self, tx: TransactionRequest) -> Result<B256> {
        self.ensure_write()?;

        self.call("eth_sendTransaction", (tx,)).await
    }

    /// One storage word, silently (the venue driver's chain-instance canary;
    /// the same measured wire rules as [`crate::apply_writes`]' storage form).
    pub async fn anvil_set_storage_at(&self, addr: Address, slot: U256, value: B256) -> Result<()> {
        self.ensure_write()?;

        self.call_raw(
            "anvil_setStorageAt",
            (addr, B256::from(slot.to_be_bytes::<32>()), value),
        )
        .await?;
        Ok(())
    }
}

/// The `anvil_reset` params envelope: `alloy-rpc-types-anvil` models the
/// `Forking` object, and alloy's own client sends it *untagged* — measured,
/// anvil accepts only the tagged `{ "forking": … }` wrapper, so the wrapper
/// is ours (their `Forking` deserializer accepts both forms; its serializer
/// does not emit the tag).
#[derive(serde::Serialize)]
struct ResetParams {
    forking: Forking,
}

/// The measured tracer options:
/// `{"tracer":"prestateTracer","tracerConfig":{"diffMode":true}}`.
fn prestate_diff_options() -> GethDebugTracingOptions {
    GethDebugTracingOptions::prestate_tracer(PreStateConfig {
        diff_mode: Some(true),
        ..Default::default()
    })
}

/// Truncate to at most `n` bytes without splitting a UTF-8 character: a node's
/// answer must not panic the boundary that reads it.
pub fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        &s[..s.floor_char_boundary(n)]
    }
}

/// The inclusive `(fromBlock, toBlock)` of an `eth_getLogs` params value.
/// JSON-RPC params are positional, so the filter is the first array element;
/// a bare filter object is accepted too (some tooling stages it that way).
/// Tags (`latest`, `safe`, …) and absent bounds have no fixed range and are
/// left to the endpoint.
fn log_range_of(params: &serde_json::Value) -> Option<(u64, u64)> {
    let filter = params
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(params);
    let block = |value: Option<&serde_json::Value>| match value {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s
            .strip_prefix("0x")
            .and_then(|hex| u64::from_str_radix(hex, 16).ok()),
        _ => None,
    };
    Some((
        block(filter.get("fromBlock"))?,
        block(filter.get("toBlock"))?,
    ))
}

/// Split an inclusive block range into chunks no wider than `cap` (floored at
/// one so a zero can never spin). An empty range (`to < from`) yields no
/// chunks; the last chunk is clamped to `to`.
fn log_chunks(from: u64, to: u64, cap: u64) -> Vec<(u64, u64)> {
    if to < from {
        return Vec::new();
    }
    let cap = cap.max(1);
    let mut chunks = Vec::new();
    let mut start = from;
    loop {
        let end = start.saturating_add(cap - 1).min(to);
        chunks.push((start, end));
        if end >= to {
            return chunks;
        }
        start = end + 1;
    }
}

/// One chunk's end, from `start`: walk while the item count and the estimated
/// body size allow. The params are already serialized JSON, so the size
/// estimate is exact enough for the chunk decision.
fn chunk_end(calls: &[Call], start: usize, max_items: usize, max_bytes: usize) -> usize {
    let mut end = start;
    let mut bytes = 2usize; // brackets
    while end < calls.len() && (end - start) < max_items {
        let estimate = calls[end].meta.method.len() + calls[end].params.get().len() + 64;
        if end > start && bytes + estimate > max_bytes {
            break;
        }
        bytes += estimate + 1;
        end += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    /// Measured: drpc answers a JSON-RPC `-32601` on HTTP 400. The body is
    /// the node's answer, so the typed predicate must see it — otherwise the
    /// BAL→trace fallback never engages and the engine retries forever.
    #[test]
    fn jsonrpc_error_on_non_2xx_is_the_nodes_answer() {
        let err = Rpc::answer(
                "http://127.0.0.1:1",
                "eth_getBlockAccessList",
                reqwest::StatusCode::BAD_REQUEST,
                r#"{"id":7,"jsonrpc":"2.0","error":{"code":-32601,"message":"method is not available"}}"#,
            )
            .expect_err("an error answer is an error");
        assert!(err.is_method_not_found(), "{err}");
        match err {
            Error::Rpc { payload, .. } => assert_eq!(payload.code, -32601),
            other => panic!("expected the Rpc class, got {other:?}"),
        }
    }

    /// A gateway page on a 5xx is not an answer: the HTTP status classifies it.
    #[test]
    fn non_answer_body_on_non_2xx_keeps_the_status_class() {
        let err = Rpc::answer(
            "http://127.0.0.1:1",
            "eth_call",
            reqwest::StatusCode::BAD_GATEWAY,
            "<html>bad gateway</html>",
        )
        .expect_err("a page is not an answer");
        assert!(matches!(err, Error::Status { status: 502, .. }), "{err}");
    }

    /// A 2xx body with neither `result` nor `error` is not a JSON-RPC answer.
    #[test]
    fn non_answer_body_on_2xx_is_corrupt() {
        let err = Rpc::answer(
            "http://127.0.0.1:1",
            "eth_call",
            reqwest::StatusCode::OK,
            r#"{"message":"hello"}"#,
        )
        .expect_err("not an answer");
        assert!(matches!(err, Error::Decode { .. }), "{err}");
    }

    /// `result: null` is a legitimate empty answer (anvil cheatcodes return
    /// null on success), not corruption and not a method-not-found.
    #[test]
    fn null_result_is_an_empty_answer() {
        let value = Rpc::answer(
            "http://127.0.0.1:1",
            "anvil_setAutomine",
            reqwest::StatusCode::OK,
            r#"{"id":1,"jsonrpc":"2.0","result":null}"#,
        )
        .expect("null result parses");
        assert_eq!(value.get(), "null");
        assert!(serde_json::from_str::<Option<()>>(value.get())
            .unwrap()
            .is_none());
    }

    /// The pool validates, trims, and deduplicates its endpoints.
    #[test]
    fn a_pool_is_validated_and_deduplicated() {
        let pool = Rpc::with_urls(
            &[
                " http://a:8545/ ".to_string(),
                "http://a:8545".to_string(),
                "https://b:443".to_string(),
            ],
            Duration::from_secs(1),
        )
        .expect("a valid pool");
        assert_eq!(pool.urls(), ["http://a:8545", "https://b:443"]);
        assert_eq!(pool.url(), "http://a:8545");
        assert!(Rpc::with_urls(&[], Duration::from_secs(1)).is_err());
        assert!(Rpc::with_urls(&["ftp://a".to_string()], Duration::from_secs(1)).is_err());
    }

    /// A multi-endpoint read pool is not a write surface: a post-send timeout
    /// is ambiguous, and failing over would duplicate a non-idempotent write.
    #[tokio::test]
    async fn a_pooled_read_client_refuses_writes() {
        let pool = Rpc::with_urls(
            &["http://a:8545".to_string(), "http://b:8545".to_string()],
            Duration::from_secs(1),
        )
        .expect("a valid pool");
        let error = pool
            .anvil_set_balance(Address::ZERO, U256::ZERO)
            .await
            .expect_err("a pooled write is refused");
        assert!(matches!(error, Error::PooledWrite), "{error}");
    }

    /// The boot identity assert: a wrong-chain endpoint refuses the pool even
    /// when another endpoint is correct; an unreachable one is a fault the
    /// pool can fail over from, but a pool with no reachable endpoint fails.
    #[tokio::test]
    async fn a_wrong_chain_endpoint_refuses_the_pool() {
        let (wrong, wrong_server) = serve_once(r#"{"id":1,"jsonrpc":"2.0","result":"0x2b"}"#);
        let (right, right_server) = serve_once(r#"{"id":1,"jsonrpc":"2.0","result":"0x2a"}"#);
        let pool = Rpc::with_urls(&[wrong, right], Duration::from_secs(2)).expect("a valid pool");
        let error = pool.verify_chain(42).await.expect_err("chain 43 is not 42");
        assert!(
            matches!(
                error,
                Error::WrongChain {
                    got: 43,
                    want: 42,
                    ..
                }
            ),
            "{error}"
        );
        wrong_server.join().expect("the wrong server exits");
        right_server.join().expect("the right server exits");

        let (right, right_server) = serve_once(r#"{"id":1,"jsonrpc":"2.0","result":"0x2a"}"#);
        let pool = Rpc::with_urls(&[right], Duration::from_secs(2)).expect("a valid pool");
        pool.verify_chain(42)
            .await
            .expect("the right chain is accepted");
        right_server.join().expect("the right server exits");
    }

    /// A null system-tx answer is a fault, not an empty list: a block's
    /// system transactions must not be silently dropped.
    #[tokio::test]
    async fn a_null_system_txs_answer_is_a_fault() {
        let (url, server) = serve_once(r#"{"id":1,"jsonrpc":"2.0","result":null}"#);
        let rpc = Rpc::new(&url, Duration::from_secs(2)).expect("a valid client");
        let error = rpc
            .system_txs(1)
            .await
            .expect_err("null is not a list of system txs");
        assert!(matches!(error, Error::Corrupt { .. }), "{error}");
        server.join().expect("the test server exits");
    }

    /// A pool with a dead endpoint and a live one succeeds; the live answer is
    /// the call's answer regardless of the random start.
    #[tokio::test]
    async fn a_pool_fails_over_past_a_dead_endpoint() {
        let (live, server) = serve_once(r#"{"id":1,"jsonrpc":"2.0","result":"0x2a"}"#);
        let pool = Rpc::with_urls(
            &["http://127.0.0.1:1".to_string(), live],
            Duration::from_secs(2),
        )
        .expect("a valid pool");
        let value: U64 = pool
            .call("eth_blockNumber", NO_PARAMS)
            .await
            .expect("the live endpoint answers");
        assert_eq!(value.to::<u64>(), 42);
        server.join().expect("the test server exits");
    }

    /// A retryable refusal is an endpoint fault, not the node's answer: the
    /// pool moves to the next endpoint and returns its result.
    #[tokio::test]
    async fn a_pool_fails_over_past_a_throttled_endpoint() {
        let (throttled, first) = serve_once(
            r#"{"id":1,"jsonrpc":"2.0","error":{"code":-32005,"message":"rate limited"}}"#,
        );
        let (live, second) = serve_once(r#"{"id":1,"jsonrpc":"2.0","result":"0x2a"}"#);
        let pool =
            Rpc::with_urls(&[throttled, live], Duration::from_secs(2)).expect("a valid pool");
        let value: U64 = pool
            .call("eth_blockNumber", NO_PARAMS)
            .await
            .expect("the live endpoint answers");
        assert_eq!(value.to::<u64>(), 42);
        first.join().expect("the throttled server exits");
        second.join().expect("the live server exits");
    }

    /// An endpoint's plan refusal is a fault (the next endpoint can answer
    /// the same body), while a method-level error is the node's answer.
    #[test]
    fn a_plan_refusal_is_an_endpoint_fault_but_a_method_error_is_not() {
        assert!(endpoint_policy_refusal(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":31,"message":"Batch of more than 3 requests are not allowed on free plan, to use this feature register paid account at drpc.org"}}"#
        ));
        assert!(endpoint_policy_refusal(
            r#"[{"jsonrpc":"2.0","id":1,"error":{"code":31,"message":"free plan"}}]"#
        ));
        assert!(!endpoint_policy_refusal(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#
        ));
        assert!(!endpoint_policy_refusal(
            r#"{"jsonrpc":"2.0","id":1,"result":"0x2a"}"#
        ));
    }

    /// A provider's archive/retention limit is a capability refusal, not the
    /// method's answer: another endpoint can serve the same block. A
    /// deterministic revert stays the node's answer.
    #[test]
    fn a_capability_refusal_fails_over_but_a_revert_does_not() {
        assert!(endpoint_capability_refusal(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"unsupported block number"}}"#
        ));
        assert!(endpoint_capability_refusal(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"pruned history unavailable: requested 26293001, earliest 26295128"}}"#
        ));
        assert!(endpoint_capability_refusal(
            r#"[{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"missing trie node"}}]"#
        ));
        assert!(!endpoint_capability_refusal(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"execution reverted"}}"#
        ));
        assert!(!endpoint_capability_refusal(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#
        ));
        assert!(endpoint_fault(
            reqwest::StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"unsupported block number"}}"#
        ));
        assert!(!endpoint_fault(
            reqwest::StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"execution reverted"}}"#
        ));
    }

    /// A pool fails over past a batch plan refusal: the same body is answered
    /// by the next endpoint (measured live: drpc's free plan refuses a batch
    /// of more than three, the official RPC answers twenty).
    #[tokio::test]
    async fn a_pool_fails_over_past_a_batch_plan_refusal() {
        let (limited, first) = serve_once(
            r#"{"id":1,"jsonrpc":"2.0","error":{"code":31,"message":"Batch of more than 3 requests are not allowed on free plan"}}"#,
        );
        let (live, second) = serve_once(r#"{"id":1,"jsonrpc":"2.0","result":"0x2a"}"#);
        let pool = Rpc::with_urls(&[limited, live], Duration::from_secs(2)).expect("a valid pool");
        // The pool's order contains both endpoints, so whichever it starts
        // with, the refusal is skipped and the live endpoint answers.
        let value: U64 = pool
            .call("eth_blockNumber", NO_PARAMS)
            .await
            .expect("the live endpoint answers");
        assert_eq!(value.to::<u64>(), 42);
        first.join().expect("the refusing server exits");
        second.join().expect("the live server exits");
    }

    /// One HTTP request's answer, on a std thread: the smallest server that
    /// exercises the transport without a test-only dependency.
    fn serve_once(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("local address");
        let handle = std::thread::spawn(move || {
            // A successful failover may never reach this endpoint (the live
            // one answered first). Wait bounded, then exit: a leaked accept
            // would hang the test's join, not the code under test.
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(accepted) => break accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        (format!("http://{address}"), handle)
    }

    /// A node's `invalid block height` answer is not a retryable internal
    /// fault: the capability audit's future-block probe must not cool every
    /// endpoint (and log a pool-exhaustion WARN) just for asking.
    #[test]
    fn invalid_block_height_is_an_answer_not_a_retryable_fault() {
        assert!(!is_retryable(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"invalid block height: 50047695"}}"#
        ));
        assert!(is_retryable(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"internal error"}}"#
        ));
        assert!(is_retryable(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32005,"message":"rate limited"}}"#
        ));
        assert!(!is_retryable(
            r#"[{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"invalid block height: 50047695"}}]"#
        ));
    }

    /// A batch body answering a single call is corrupt, not a transport fault.
    #[test]
    fn batch_body_for_single_call_is_corrupt() {
        let err = Rpc::answer(
            "http://127.0.0.1:1",
            "eth_call",
            reqwest::StatusCode::OK,
            r#"[{"id":1,"jsonrpc":"2.0","result":"0x"}]"#,
        )
        .expect_err("a batch is not a single answer");
        assert!(matches!(err, Error::Decode { .. }), "{err}");
    }

    /// The configured read client carries the instance's pool policy, not the
    /// crate default: the CCTP chains and the beacon lane must share the same
    /// round/cooldown budget as both mirrors.
    #[test]
    fn a_configured_policy_is_preserved_by_the_read_client() {
        let policy = PoolPolicy {
            rounds: 7,
            max_attempts: 9,
            cooldown_base_ms: 250,
            cooldown_max_ms: 5_000,
            ..PoolPolicy::default()
        };
        let rpc = Rpc::with_urls_and_policy(
            &["http://127.0.0.1:1".to_string()],
            std::time::Duration::from_secs(1),
            policy,
        )
        .expect("a valid URL builds");
        assert_eq!(rpc.pool().policy(), policy);
    }

    fn rpc_with_log_cap(cap: u64) -> Rpc {
        let spec = crate::EndpointSpec {
            url: "http://127.0.0.1:1".into(),
            roles: Default::default(),
            chain_id: None,
            weight: 1,
            max_log_range: cap,
        };
        let pool = EndpointPool::from_specs([spec], Scheme::Http, PoolPolicy::default())
            .expect("a valid pool");
        Rpc::with_pool(pool, Duration::from_secs(1)).expect("a valid client")
    }

    /// A wide range is split at the declared cap, inclusive at both ends.
    #[test]
    fn log_ranges_split_into_bounded_chunks() {
        assert_eq!(log_chunks(10, 10, 10), vec![(10, 10)]);
        assert_eq!(log_chunks(10, 19, 10), vec![(10, 19)]);
        assert_eq!(
            log_chunks(10, 25, 10),
            vec![(10, 19), (20, 25)],
            "the last chunk is clamped to the requested end"
        );
        assert_eq!(log_chunks(10, 9, 10), Vec::<(u64, u64)>::new());
        assert_eq!(
            log_chunks(1, 3, 0),
            vec![(1, 1), (2, 2), (3, 3)],
            "a zero cap is floored at one, never an infinite loop"
        );
    }

    /// An over-cap range is refused locally with the numbers in the error,
    /// before any endpoint sees it; exactly the cap is allowed.
    #[test]
    fn an_over_cap_log_range_is_refused_locally() {
        let rpc = rpc_with_log_cap(900);
        let over = serde_json::to_value((Filter::new().from_block(100).to_block(1_000),))
            .expect("filter serializes");
        let err = rpc
            .check_log_range("eth_getLogs", &over)
            .expect_err("1,000 blocks is over the 900 cap");
        match err {
            Error::LogRangeTooWide {
                from,
                to,
                blocks,
                cap,
                ..
            } => {
                assert_eq!((from, to, blocks, cap), (100, 1_000, 901, 900));
            }
            other => panic!("expected LogRangeTooWide, got {other}"),
        }
        let at_cap = serde_json::to_value((Filter::new().from_block(100).to_block(999),))
            .expect("filter serializes");
        rpc.check_log_range("eth_getLogs", &at_cap)
            .expect("900 blocks is the declared cap");
        let other_method = serde_json::to_value((Filter::new().from_block(1).to_block(1_000_000),))
            .expect("filter serializes");
        rpc.check_log_range("eth_getBlockByNumber", &other_method)
            .expect("only eth_getLogs is range-bounded");
    }
}
