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
use crate::backoff::{Backoff, Progress};
use crate::error::{missing_from_batch, Error, Result, RpcError};
use crate::trace::{Key, Value};
use alloy_eips::eip7928::BlockAccessList;
use alloy_json_rpc::{
    Id, Request, RequestPacket, Response, ResponsePacket, ResponsePayload, SerializedRequest,
};
use alloy_primitives::{Address, Bytes, B256, TxKind, U256, U64};
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
use std::time::Duration;

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
/// The throttle's retry bound (the shared backoff rides between the tries).
const RATE_LIMIT_TRIES: u32 = 4;

/// The retryable refusals: the throttle and the node's internal error. A
/// single error object is parsed; a batch array is scanned for the codes
/// (distinctive strings — a full parse of a large answer is not worth it).
fn is_retryable(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        return trimmed.contains("\"code\":-32005") || trimmed.contains("\"code\":-32603");
    }
    if !trimmed.starts_with('{') {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|v| v.get("error")?.get("code")?.as_i64())
        .map(|code| code == RATE_LIMIT_CODE || code == INTERNAL_ERROR_CODE)
        .unwrap_or(false)
}

/// One HTTP client + URL, cloned cheaply (reqwest's client is `Arc`-backed).
/// A clone starts a fresh request-id counter; ids only need to correlate a
/// request with its own response.
pub struct Rpc {
    http: reqwest::Client,
    url: String,
    next_id: AtomicU64,
}

impl Clone for Rpc {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            url: self.url.clone(),
            next_id: AtomicU64::new(1),
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

impl Rpc {
    /// Build a client for `url`. Fallible: the TLS backend's initialization can
    /// fail, and a boundary that cannot be built is a boot error naming itself,
    /// not a panic.
    pub fn new(url: &str, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|source| Error::ClientBuild { source })?;
        Ok(Self {
            http,
            url: url.trim_end_matches('/').to_string(),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // ---- typed error constructors (one site per variant) ----------------

    fn transport(&self, method: &str, source: reqwest::Error) -> Error {
        Error::Transport {
            endpoint: self.url.clone(),
            method: method.to_string(),
            source,
        }
    }

    fn status(&self, method: &str, status: u16, body: &str) -> Error {
        Error::Status {
            endpoint: self.url.clone(),
            method: method.to_string(),
            status,
            body: truncate(body, 300).to_string(),
        }
    }

    fn rpc(&self, method: &str, payload: RpcError) -> Error {
        Error::Rpc {
            endpoint: self.url.clone(),
            method: method.to_string(),
            payload,
        }
    }

    fn decode(&self, method: &str, source: serde_json::Error) -> Error {
        Error::Decode {
            endpoint: self.url.clone(),
            method: method.to_string(),
            source,
        }
    }

    fn encode(&self, method: &str, source: serde_json::Error) -> Error {
        Error::Encode {
            method: format!("{method} ({})", self.url),
            source,
        }
    }

    // ---- transport ------------------------------------------------------

    fn request(&self, method: &str, params: impl Serialize) -> Result<SerializedRequest> {
        // Params are method-specific; erasing them to the official `RawValue`
        // is exactly what a request serializes to anyway.
        let params = serde_json::value::to_raw_value(&params)
            .map_err(|source| self.encode(method, source))?;
        Request::new(method.to_string(), Id::Number(self.id()), params)
            .serialize()
            .map_err(|source| self.encode(method, source))
    }

    /// One HTTP round trip: the raw status and body. The boundary decision
    /// belongs to [`Self::answer`] (single) / [`Self::classify`] (batch).
    async fn post_raw(
        &self,
        method: &str,
        body: &RawValue,
    ) -> Result<(reqwest::StatusCode, String)> {
        let resp = self
            .http
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, crate::JSON_CONTENT_TYPE)
            .body(body.get().to_string())
            .send()
            .await
            .map_err(|source| self.transport(method, source))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|source| self.transport(method, source))?;
        Ok((status, text))
    }

    /// The retryable refusals — the public endpoint's throttle (`-32005`,
    /// "rate limited") and the node's internal error (`-32603`) — ride a
    /// bounded backoff before surfacing. A wide catch-up window issues many
    /// bodies, and a transient refusal must not fail the pass (measured
    /// live: bursts tripped the limiter and the internal error came and went
    /// under load). Every other refusal is the node's answer.
    async fn post_raw_throttled(
        &self,
        method: &str,
        body: &RawValue,
    ) -> Result<(reqwest::StatusCode, String)> {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(8));
        // The retries ride a bounded backoff; the last attempt's answer
        // surfaces whatever it is (the loop backs off only while another
        // attempt remains).
        for _ in 1..RATE_LIMIT_TRIES {
            let (status, text) = self.post_raw(method, body).await?;
            if !is_retryable(&text) {
                return Ok((status, text));
            }
            tokio::time::sleep(backoff.delay()).await;
            backoff.record(Progress::Stalled);
        }
        self.post_raw(method, body).await
    }

    /// The JSON-RPC-over-HTTP boundary rule: the body is the node's answer
    /// whenever it parses as one, whatever the HTTP status; the status only
    /// classifies bodies that are not answers (gateway pages, auth walls,
    /// empty bodies). A JSON-RPC error on a non-2xx status is the node's
    /// typed refusal, not a transport fault.
    fn classify(&self, method: &str, status: reqwest::StatusCode, text: &str) -> Result<ResponsePacket> {
        match serde_json::from_str::<ResponsePacket>(text) {
            Ok(packet) => Ok(packet),
            Err(_) if !status.is_success() => Err(self.status(method, status.as_u16(), text)),
            Err(source) => Err(self.decode(method, source)),
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
        &self,
        method: &str,
        status: reqwest::StatusCode,
        text: &str,
    ) -> Result<Box<RawValue>> {
        match self.classify(method, status, text)? {
            ResponsePacket::Single(response) => {
                Self::response_result(response).map_err(|payload| self.rpc(method, payload))
            }
            ResponsePacket::Batch(_) => Err(self.decode(
                method,
                serde_json::Error::custom("a batch answer to a single call"),
            )),
        }
    }

    /// Single call, raw result.
    pub async fn call_raw(&self, method: &str, params: impl Serialize) -> Result<Box<RawValue>> {
        let request = self.request(method, params)?;
        let (status, text) = self
            .post_raw_throttled(method, &request.take_request())
            .await?;
        self.answer(method, status, &text)
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<T> {
        let value = self.call_raw(method, params).await?;
        serde_json::from_str(value.get()).map_err(|source| self.decode(method, source))
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
                    return Err(self.batch_task_error_text(format!(
                        "batch chunk {idx} never completed"
                    )))
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
            endpoint: self.url.clone(),
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
        self.batch_typed_limited(method, params, BATCH_MAX_ITEMS).await
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
                        .map_err(|source| self.decode("batch", source))?,
                ),
                Err(payload) => {
                    return Err(Error::Rpc {
                        endpoint: self.url.clone(),
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
                .map_err(|source| self.decode(method, source)),
            Err(payload) => Err(Error::Rpc {
                endpoint: self.url.clone(),
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
            .map_err(|source| self.encode("batch", source))?;
        let body = RequestPacket::Batch(requests)
            .serialize()
            .map_err(|source| self.encode("batch", source))?;
        Ok((ids, body))
    }

    /// Post one batch body and classify the answer — the same boundary rule as
    /// `answer` (a body that parses as answers *is* the answers, whatever the
    /// HTTP status); a body that does not is a `BatchDecode` naming the
    /// request text, and a single answer to a batch call is corrupt.
    async fn post_batch(&self, body: &RawValue) -> Result<Vec<Response>> {
        let (status, text) = self.post_raw_throttled("batch", body).await?;
        let packet = match self.classify("batch", status, &text) {
            Ok(packet) => packet,
            Err(Error::Decode { source, .. }) => {
                return Err(Error::BatchDecode {
                    endpoint: self.url.clone(),
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
                Err(payload) => Err(self.rpc("batch", payload)),
                Ok(_) => Err(self.decode(
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

    pub async fn block_by_number(&self, n: u64, txs: Txs) -> Result<Block> {
        self.call_opt(BLOCK_BY_NUMBER, (U64::from(n), txs.full()))
            .await?
            .ok_or_else(|| Error::BlockMissing {
                endpoint: self.url.clone(),
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
                endpoint: self.url.clone(),
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
            self.decode(method, serde_json::Error::custom("a null answer"))
        })?;
        serde_json::from_str(raw.get()).map_err(|source| self.decode(method, source))
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
            Ok(v) => Ok(v.unwrap_or_default()),
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
        self.call(ETH_CALL, (Self::call_tx(to, data), BlockNumberOrTag::Latest))
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
        self.call(crate::ETH_GET_BALANCE, (addr, U64::from(block))).await
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

    /// The wire calls for a set of `eth_call`s — `Some(block)` pins the
    /// number, `None` reads latest (the probes' two shapes) — the same
    /// encoding [`Self::call_at`] and [`Self::call_latest`] send.
    pub fn call_calls(calls: &[(Address, Bytes)], block: Option<u64>) -> Result<Vec<Call>> {
        calls
            .iter()
            .map(|(to, data)| {
                let at = match block {
                    Some(n) => BlockNumberOrTag::Number(n),
                    None => BlockNumberOrTag::Latest,
                };
                call(ETH_CALL, (Self::call_tx(*to, data), at))
            })
            .collect()
    }

    /// One key's answer, decoded to its value kind.
    fn decode_key(&self, key: &Key, raw: &RawValue) -> Result<Value> {
        let method = "batch";
        Ok(match key {
            Key::Storage { .. } => {
                let word: B256 = serde_json::from_str(raw.get())
                    .map_err(|source| self.decode(method, source))?;
                Value::Storage(U256::from_be_bytes(word.0))
            }
            Key::Balance(_) => Value::Balance(
                serde_json::from_str(raw.get()).map_err(|source| self.decode(method, source))?,
            ),
            Key::Nonce(_) => {
                let quantity: U64 = serde_json::from_str(raw.get())
                    .map_err(|source| self.decode(method, source))?;
                Value::Nonce(quantity.to::<u64>())
            }
            Key::Code(_) => Value::Code(
                serde_json::from_str(raw.get()).map_err(|source| self.decode(method, source))?,
            ),
        })
    }

    /// A null answer to a key read: the same refusal the single-key path's
    /// decode raises.
    fn null_key_read(&self) -> Error {
        self.decode(
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
        self.call(
            "debug_traceTransaction",
            (hash, prestate_diff_options()),
        )
        .await
    }

    /// `debug_traceBlockByNumber` with the callTracer: one call-frame tree per
    /// transaction. The venue bridge needs the *internal* calls (a contract
    /// calling CoreWriter attributes the action to itself, not to the tx
    /// sender).
    pub async fn trace_block_calls(&self, n: u64) -> Result<Vec<TraceResult>> {
        let options = GethDebugTracingOptions::call_tracer(Default::default());
        self.call(crate::TRACE_BLOCK_METHOD, (U64::from(n), options)).await
    }

    // ---- anvil namespace -----------------------------------------------

    pub async fn anvil_set_automine(&self, mode: Automine) -> Result<()> {
        self.call_raw("anvil_setAutomine", (matches!(mode, Automine::On),))
            .await?;
        Ok(())
    }

    pub async fn anvil_mine(&self, blocks: u64) -> Result<()> {
        self.call_raw("anvil_mine", (U64::from(blocks),)).await?;
        Ok(())
    }

    /// Mine one block iff the pool is non-empty; `Some(pending)` is the
    /// pool's depth when a block was mined, `None` when it was empty (V's
    /// producer rule: an idle chain stays static; T's prepend runs the same
    /// check with its timestamp step around it).
    pub async fn mine_pending(&self) -> Result<Option<u64>> {
        let status = self.txpool_status().await?;
        let pending = status.pending;
        if pending == 0 {
            return Ok(None);
        }
        self.anvil_mine(1).await?;
        Ok(Some(pending))
    }

    /// Set the next block's timestamp alone (the devnet prepend block: the
    /// diffs' own context alignment follows separately).
    pub async fn anvil_set_next_block_timestamp(&self, timestamp: u64) -> Result<()> {
        self.call_raw("anvil_setNextBlockTimestamp", (U64::from(timestamp),))
            .await?;
        Ok(())
    }

    /// Align the next block's context fields T owns (timestamp/coinbase/
    /// prevRandao/baseFee); the gas limit stays the devnet's own.
    pub async fn set_next_block_context(&self, timestamp: u64, header: &Header) -> Result<()> {
        self.apply_env_calls(Self::context_calls(timestamp, header)?).await
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
        let results = self.batch(&calls).await?;
        for (call, result) in calls.iter().zip(results) {
            if let Err(payload) = result {
                return Err(self.rpc(&call.meta.method, payload));
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
        let forking = Forking {
            json_rpc_url: Some(url.to_string()),
            block_number: None,
        };
        self.call_raw("anvil_reset", (ResetParams { forking },)).await?;
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
        self.call_raw("anvil_impersonateAccount", (addr,)).await?;
        Ok(())
    }

    /// `anvil_stopImpersonatingAccount`.
    pub async fn anvil_stop_impersonating_account(&self, addr: Address) -> Result<()> {
        self.call_raw("anvil_stopImpersonatingAccount", (addr,)).await?;
        Ok(())
    }

    /// `anvil_setBalance` — the impersonated senders (the bridge's system
    /// addresses) need gas on V.
    pub async fn anvil_set_balance(&self, addr: Address, balance: U256) -> Result<()> {
        self.call_raw("anvil_setBalance", (addr, balance)).await?;
        Ok(())
    }

    /// `eth_sendTransaction` from an arbitrary sender (V runs
    /// auto-impersonate): the bridge's system-address completion path.
    pub async fn send_transaction(&self, tx: TransactionRequest) -> Result<B256> {
        self.call("eth_sendTransaction", (tx,)).await
    }

    /// One storage word, silently (the venue driver's chain-instance canary;
    /// the same measured wire rules as [`crate::apply_writes`]' storage form).
    pub async fn anvil_set_storage_at(&self, addr: Address, slot: U256, value: B256) -> Result<()> {
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

    fn client() -> Rpc {
        Rpc::new("http://127.0.0.1:1", Duration::from_secs(1)).expect("client builds")
    }

    /// Measured: drpc answers a JSON-RPC `-32601` on HTTP 400. The body is
    /// the node's answer, so the typed predicate must see it — otherwise the
    /// BAL→trace fallback never engages and the engine retries forever.
    #[test]
    fn jsonrpc_error_on_non_2xx_is_the_nodes_answer() {
        let err = client()
            .answer(
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
        let err = client()
            .answer(
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
        let err = client()
            .answer("eth_call", reqwest::StatusCode::OK, r#"{"message":"hello"}"#)
            .expect_err("not an answer");
        assert!(matches!(err, Error::Decode { .. }), "{err}");
    }

    /// `result: null` is a legitimate empty answer (anvil cheatcodes return
    /// null on success), not corruption and not a method-not-found.
    #[test]
    fn null_result_is_an_empty_answer() {
        let value = client()
            .answer(
                "anvil_setAutomine",
                reqwest::StatusCode::OK,
                r#"{"id":1,"jsonrpc":"2.0","result":null}"#,
            )
            .expect("null result parses");
        assert_eq!(value.get(), "null");
        assert!(serde_json::from_str::<Option<()>>(value.get()).unwrap().is_none());
    }

    /// A batch body answering a single call is corrupt, not a transport fault.
    #[test]
    fn batch_body_for_single_call_is_corrupt() {
        let err = client()
            .answer(
                "eth_call",
                reqwest::StatusCode::OK,
                r#"[{"id":1,"jsonrpc":"2.0","result":"0x"}]"#,
            )
            .expect_err("a batch is not a single answer");
        assert!(matches!(err, Error::Decode { .. }), "{err}");
    }
}
