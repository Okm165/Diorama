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
use crate::error::{bad_result, missing_from_batch, Error, Result, RpcError};
use alloy_eips::eip7928::BlockAccessList;
use alloy_json_rpc::{
    Id, Request, RequestPacket, Response, ResponsePacket, ResponsePayload, SerializedRequest,
};
use alloy_primitives::{Address, Bytes, B256, TxKind, U256, U64};
use alloy_rpc_types_anvil::{Forking, NodeInfo};
use alloy_rpc_types_eth::{
    state::StateOverride, Block, BlockNumberOrTag, Filter, Header, Log, TransactionReceipt,
    TransactionRequest,
};
use alloy_rpc_types_trace::geth::{
    GethDebugTracingOptions, GethTrace, PreStateConfig, TraceResult,
};
use alloy_rpc_types_txpool::{TxpoolContent, TxpoolStatus};
use serde::{de::DeserializeOwned, de::Error as _, Serialize};
use serde_json::value::RawValue;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Positional params for methods that take none: the spec's `[]` (the
/// discouraged alternative is `null`, which not every node accepts).
const NO_PARAMS: [u8; 0] = [];

/// The JSON-RPC code a node answers when it does not implement a method
/// (measured: drpc answers `-32601` on HTTP 400 — see the module docs).
const METHOD_NOT_FOUND: i64 = -32601;

/// One call to send: a method and its typed positional params, serialized
/// once. Params are method-specific, so a batch erases them to the official
/// `RawValue` — exactly `alloy_json_rpc::Request::box_params`' contract.
#[derive(Debug, Clone)]
pub struct Call {
    method: &'static str,
    params: Box<RawValue>,
}

impl Call {
    /// Build a call. Params serialize to the JSON-RPC positional form: a
    /// tuple for several (`(U64::from(n), true)`), a 1-tuple for one
    /// (`(raw,)`), [`NO_PARAMS`] for none.
    pub fn new(method: &'static str, params: impl Serialize) -> Result<Self> {
        let params = serde_json::value::to_raw_value(&params).map_err(|source| Error::Encode {
            method: method.to_string(),
            source,
        })?;
        Ok(Self { method, params })
    }
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
            .header(reqwest::header::CONTENT_TYPE, "application/json")
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
        let (status, text) = self.post_raw(method, &request.take_request()).await?;
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
    pub async fn batch(&self, calls: &[Call]) -> Result<Vec<std::result::Result<Box<RawValue>, RpcError>>> {
        self.batch_limited(calls, 500, 1 << 20).await
    }

    pub async fn batch_limited(
        &self,
        calls: &[Call],
        max_items: usize,
        max_bytes: usize,
    ) -> Result<Vec<std::result::Result<Box<RawValue>, RpcError>>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(calls.len());
        let mut start = 0usize;
        while start < calls.len() {
            let mut end = start;
            let mut bytes = 2usize; // brackets
            while end < calls.len() && (end - start) < max_items {
                // The params are already serialized JSON, so the size estimate
                // is exact enough for the chunk decision.
                let estimate = calls[end].method.len() + calls[end].params.get().len() + 64;
                if end > start && bytes + estimate > max_bytes {
                    break;
                }
                bytes += estimate + 1;
                end += 1;
            }
            out.extend(self.batch_once(&calls[start..end]).await?);
            start = end;
        }
        Ok(out)
    }

    async fn batch_once(
        &self,
        calls: &[Call],
    ) -> Result<Vec<std::result::Result<Box<RawValue>, RpcError>>> {
        let ids: Vec<u64> = calls.iter().map(|_| self.id()).collect();
        let requests = calls
            .iter()
            .zip(&ids)
            .map(|(call, id)| {
                Request::new(call.method, Id::Number(*id), call.params.clone())
                    .box_params()
                    .serialize()
            })
            .collect::<serde_json::Result<Vec<SerializedRequest>>>()
            .map_err(|source| self.encode("batch", source))?;
        let body = RequestPacket::Batch(requests)
            .serialize()
            .map_err(|source| self.encode("batch", source))?;
        let (status, text) = self.post_raw("batch", &body).await?;
        // Same boundary rule as `answer`: a body that parses as a batch of
        // JSON-RPC answers is the answers, whatever the HTTP status.
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
        let responses = match packet {
            ResponsePacket::Batch(responses) => responses,
            ResponsePacket::Single(_) => {
                return Err(self.decode(
                    "batch",
                    serde_json::Error::custom("a single answer to a batch call"),
                ))
            }
        };
        // Items without a numeric id are not ours (subscription pushes, a
        // different client's response); our ids that the node did not answer
        // surface as `missing_from_batch`, never as a silent skip.
        let mut by_id: std::collections::HashMap<u64, Response> = responses
            .into_iter()
            .filter_map(|response| match response.id {
                Id::Number(id) => Some((id, response)),
                Id::String(_) | Id::None => None,
            })
            .collect();
        let mut out = Vec::with_capacity(calls.len());
        for id in ids {
            match by_id.remove(&id) {
                Some(response) => out.push(Self::response_result(response)),
                None => out.push(Err(missing_from_batch(id))),
            }
        }
        Ok(out)
    }

    // ---- eth namespace -------------------------------------------------

    pub async fn block_number(&self) -> Result<u64> {
        let quantity: U64 = self.call("eth_blockNumber", NO_PARAMS).await?;
        Ok(quantity.to::<u64>())
    }

    /// The chain's id (`eth_chainId`): the venue's boot assert reads it.
    pub async fn chain_id(&self) -> Result<u64> {
        let quantity: U64 = self.call("eth_chainId", NO_PARAMS).await?;
        Ok(quantity.to::<u64>())
    }

    pub async fn block_by_number(&self, n: u64, full: bool) -> Result<Block> {
        self.call_opt("eth_getBlockByNumber", (U64::from(n), full))
            .await?
            .ok_or_else(|| Error::BlockMissing {
                endpoint: self.url.clone(),
                n,
            })
    }

    pub async fn block_by_number_opt(&self, n: u64, full: bool) -> Result<Option<Block>> {
        self.call_opt("eth_getBlockByNumber", (U64::from(n), full))
            .await
    }

    /// `tag` is the protocol's own block-parameter string: `latest`,
    /// `finalized`, `safe`, `pending`, `earliest`, or a hex height.
    pub async fn block_by_tag(&self, tag: &str, full: bool) -> Result<Option<Block>> {
        self.call_opt("eth_getBlockByNumber", (tag, full)).await
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
        self.call("eth_call", params).await
    }

    /// eth_call against the current state, including unmined cheatcode writes
    /// (used to read M right after the BAL convergence repair).
    pub async fn call_latest(&self, to: Address, data: &Bytes) -> Result<Bytes> {
        self.call("eth_call", (Self::call_tx(to, data), BlockNumberOrTag::Latest))
            .await
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
        self.call("eth_call", params).await
    }

    pub async fn balance_at(&self, addr: Address, block: u64) -> Result<U256> {
        self.call("eth_getBalance", (addr, U64::from(block))).await
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

    /// Batch raw-tx submission. Per-item errors are returned, not thrown.
    /// Chunked by item count and body size (mainnet blocks can exceed anvil's
    /// 2 MB request limit at 500 txs).
    pub async fn send_raw_batch(&self, raws: &[Bytes]) -> Result<Vec<std::result::Result<B256, RpcError>>> {
        let calls: Vec<Call> = raws
            .iter()
            .map(|raw| Call::new("eth_sendRawTransaction", (raw,)))
            .collect::<Result<_>>()?;
        let results = self.batch_limited(&calls, 500, 1 << 20).await?;
        Ok(results
            .into_iter()
            .map(|result| match result {
                Ok(value) => serde_json::from_str::<B256>(value.get())
                    .map_err(|e| bad_result("eth_sendRawTransaction", &e.to_string())),
                Err(error) => Err(error),
            })
            .collect())
    }

    /// `debug_traceBlockByNumber` with prestateTracer diffMode (BAL fallback
    /// and write-tracking): one alloy `TraceResult` per transaction.
    pub async fn trace_block_prestate(&self, n: u64) -> Result<Vec<TraceResult>> {
        self.call(
            "debug_traceBlockByNumber",
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
        self.call("debug_traceBlockByNumber", (U64::from(n), options)).await
    }

    // ---- anvil namespace -----------------------------------------------

    pub async fn anvil_set_automine(&self, on: bool) -> Result<()> {
        self.call_raw("anvil_setAutomine", (on,)).await?;
        Ok(())
    }

    pub async fn anvil_mine(&self, blocks: u64) -> Result<()> {
        self.call_raw("anvil_mine", (U64::from(blocks),)).await?;
        Ok(())
    }

    /// Set the next block's timestamp alone (the devnet prepend block: the
    /// diffs' own context alignment follows separately).
    pub async fn anvil_set_next_block_timestamp(&self, timestamp: u64) -> Result<()> {
        self.call_raw("anvil_setNextBlockTimestamp", (U64::from(timestamp),))
            .await?;
        Ok(())
    }

    /// Align the next block's **execution context** to a parent header:
    /// timestamp (the caller adjusts it for the devnet offset), coinbase,
    /// prevRandao, and — when the parent has one — base fee. M's replay uses
    /// this; T's applier uses [`Self::set_next_block_context`] because T keeps
    /// its own gas limit.
    pub async fn set_next_block_env(&self, timestamp: u64, header: &Header) -> Result<()> {
        let mut calls = Self::context_calls(timestamp, header)?;
        calls.push(Call::new(
            "anvil_setBlockGasLimit",
            (U64::from(header.gas_limit),),
        )?);
        self.apply_env_calls(calls).await
    }

    /// Align the next block's context fields T owns (timestamp/coinbase/
    /// prevRandao/baseFee); the gas limit stays the devnet's own.
    pub async fn set_next_block_context(&self, timestamp: u64, header: &Header) -> Result<()> {
        self.apply_env_calls(Self::context_calls(timestamp, header)?).await
    }

    /// The context fields shared by both chains (the parent header verbatim).
    fn context_calls(timestamp: u64, header: &Header) -> Result<Vec<Call>> {
        let mut calls = vec![
            Call::new("anvil_setNextBlockTimestamp", (U64::from(timestamp),))?,
            Call::new("anvil_setCoinbase", (header.beneficiary,))?,
            Call::new("anvil_setNextBlockPrevRandao", (header.mix_hash,))?,
        ];
        if let Some(base_fee) = header.base_fee_per_gas {
            calls.push(Call::new(
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
                return Err(self.rpc(call.method, payload));
            }
        }
        Ok(())
    }

    pub async fn anvil_node_info(&self) -> Result<NodeInfo> {
        self.call("anvil_nodeInfo", NO_PARAMS).await
    }

    /// The chain's fork source + block from `anvil_nodeInfo` (None when the
    /// chain was not forked).
    pub async fn fork_config(&self) -> Result<Option<(String, u64)>> {
        let info = self.anvil_node_info().await?;
        Ok(info
            .fork_config
            .fork_url
            .zip(info.fork_config.fork_block_number))
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

    /// `anvil_reset` with a fresh fork: re-bases the chain at `block` from `url`.
    /// Used to recover M when the follow head moves backward past its
    /// irreversible replay (the cursor hash check rebases the replay after).
    /// Always explicit: the measured no-params form drops the fork entirely.
    pub async fn anvil_reset_fork(&self, url: &str, block: u64) -> Result<()> {
        let forking = Forking {
            json_rpc_url: Some(url.to_string()),
            block_number: Some(block),
        };
        self.call_raw("anvil_reset", (ResetParams { forking },)).await?;
        Ok(())
    }

    /// `anvil_reset` to a fresh fork at the source's *latest* block (the
    /// venue's `/admin/epoch`: the driver's canary detects the reset and runs
    /// the epoch rule — new base = new epoch, same base = restore).
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
    pub async fn txpool_content(&self) -> Result<TxpoolContent> {
        self.call("txpool_content", NO_PARAMS).await
    }

    pub async fn anvil_set_nonce(&self, addr: Address, nonce: u64) -> Result<()> {
        self.call_raw("anvil_setNonce", (addr, U64::from(nonce))).await?;
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

    pub async fn anvil_drop_transaction(&self, hash: B256) -> Result<()> {
        self.call_raw("anvil_dropTransaction", (hash,)).await?;
        Ok(())
    }
}

/// The `anvil_reset` params envelope: `alloy-rpc-types-anvil` models the
/// `Forking` object; the `{ "forking": … }` wrapper is the method's own shape
/// (the same one the official client sends).
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
        assert!(err.is_retryable());
    }

    /// A 2xx body with neither `result` nor `error` is not a JSON-RPC answer.
    #[test]
    fn non_answer_body_on_2xx_is_corrupt() {
        let err = client()
            .answer("eth_call", reqwest::StatusCode::OK, r#"{"message":"hello"}"#)
            .expect_err("not an answer");
        assert!(matches!(err, Error::Decode { .. }), "{err}");
        assert!(!err.is_retryable());
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
