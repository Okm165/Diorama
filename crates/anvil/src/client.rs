//! Thin JSON-RPC client: typed helpers + chunked batching with per-item errors.
//!
//! Boundary rule (`classify`): a body that parses as a JSON-RPC answer *is*
//! the node's answer, whatever the HTTP status; the status only classifies
//! bodies that are not answers (gateway pages, auth walls, empty bodies).
//! Measured: drpc answers `-32601` with HTTP 400 — status-first classification
//! turned the node's typed refusal into a transport fault and the BAL→trace
//! fallback never engaged.
//!
//! Batching matters for two measured reasons: writes are 9.6-18 ms each when
//! issued one at a time, and public endpoints reject large batches with HTTP 413
//! (local reth tolerates the sizes we use here, but callers chunk regardless).
use alloy_eips::eip7928::BlockAccessList;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_eth::{Block, Log, TransactionReceipt};
use crate::error::{Error, Result, RpcError};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// One JSON-RPC answer: exactly one of `result` or `error`. A body with
/// neither is not an answer at all (a gateway page dressed as JSON), and the
/// HTTP status classifies it instead — many endpoints answer a JSON-RPC
/// error on a non-2xx status (measured: drpc answers `-32601` with HTTP 400).
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum RpcAnswer {
    Result {
        #[serde(default)]
        id: Option<u64>,
        result: Value,
    },
    Error {
        #[serde(default)]
        id: Option<u64>,
        error: RpcError,
    },
}

impl RpcAnswer {
    fn id(&self) -> Option<u64> {
        match self {
            RpcAnswer::Result { id, .. } | RpcAnswer::Error { id, .. } => *id,
        }
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

    fn rpc(&self, method: &str, source: RpcError) -> Error {
        Error::Rpc {
            endpoint: self.url.clone(),
            method: method.to_string(),
            source,
        }
    }

    fn decode(&self, method: &str, source: serde_json::Error) -> Error {
        Error::Decode {
            endpoint: self.url.clone(),
            method: method.to_string(),
            source,
        }
    }

    fn bad_quantity(&self, method: &str, value: &str) -> Error {
        Error::bad_quantity(&self.url, method, value)
    }

    /// Single call, raw JSON result.
    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc":"2.0","id":self.id(),"method":method,"params":params});
        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|source| self.transport(method, source))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|source| self.transport(method, source))?;
        self.classify(method, status, &text)
    }

    /// The JSON-RPC-over-HTTP boundary rule: the body is the node's answer
    /// whenever it parses as one, whatever the HTTP status; the status only
    /// classifies bodies that are not answers (gateway pages, auth walls,
    /// empty bodies). A JSON-RPC error on a non-2xx status is the node's
    /// typed refusal, not a transport fault.
    fn classify(&self, method: &str, status: reqwest::StatusCode, text: &str) -> Result<Value> {
        match serde_json::from_str::<RpcAnswer>(text) {
            Ok(RpcAnswer::Result { result, .. }) => Ok(result),
            Ok(RpcAnswer::Error { error, .. }) => Err(self.rpc(method, error)),
            Err(_) if !status.is_success() => Err(self.status(method, status.as_u16(), text)),
            Err(source) => Err(self.decode(method, source)),
        }
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let v = self.call_raw(method, params).await?;
        serde_json::from_value(v).map_err(|source| self.decode(method, source))
    }

    /// Result may legitimately be null (e.g. `eth_getBlockByNumber` of a
    /// pruned/future block, `eth_getBlockAccessList` before activation).
    pub async fn call_opt<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<Option<T>> {
        let v = self.call_raw(method, params).await?;
        match v {
            Value::Null => Ok(None),
            v => serde_json::from_value(v)
                .map(Some)
                .map_err(|source| self.decode(method, source)),
        }
    }

    /// Batched call. Returns per-item results in request order.
    ///
    /// The batch is split so that no single HTTP body exceeds `max_bytes`
    /// (anvil rejects bodies >= 2 MB with `-32600 Invalid request`; public
    /// endpoints answer 413) and no sub-batch exceeds `max_items`.
    pub async fn batch_raw(&self, calls: &[(String, Value)]) -> Result<Vec<Result<Value, RpcError>>> {
        self.batch_raw_limited(calls, 500, 1 << 20).await
    }

    pub async fn batch_raw_limited(
        &self,
        calls: &[(String, Value)],
        max_items: usize,
        max_bytes: usize,
    ) -> Result<Vec<Result<Value, RpcError>>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(calls.len());
        let mut start = 0usize;
        while start < calls.len() {
            let mut end = start;
            let mut bytes = 2usize; // brackets
            while end < calls.len() && (end - start) < max_items {
                // `Value`'s Display is compact JSON and cannot fail (a Value
                // serializes by construction) - no error path to invent here.
                let estimate = calls[end].0.len() + calls[end].1.to_string().len() + 64;
                if end > start && bytes + estimate > max_bytes {
                    break;
                }
                bytes += estimate + 1;
                end += 1;
            }
            let slice = &calls[start..end];
            out.extend(self.batch_once(slice).await?);
            start = end;
        }
        Ok(out)
    }

    async fn batch_once(&self, calls: &[(String, Value)]) -> Result<Vec<Result<Value, RpcError>>> {
        let ids: Vec<u64> = calls.iter().map(|_| self.id()).collect();
        let body: Vec<Value> = calls
            .iter()
            .zip(&ids)
            .map(|((method, params), id)| {
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            })
            .collect();
        let body_str = Value::Array(body).to_string();
        let resp = self
            .http
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body_str.clone())
            .send()
            .await
            .map_err(|source| self.transport("batch", source))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|source| self.transport("batch", source))?;
        // Same boundary rule as `classify`: a body that parses as a batch of
        // JSON-RPC answers is the answers, whatever the HTTP status.
        let parsed: Vec<Value> = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(_) if !status.is_success() => {
                return Err(self.status("batch", status.as_u16(), &text));
            }
            Err(source) => {
                return Err(Error::BatchDecode {
                    endpoint: self.url.clone(),
                    request: truncate(&body_str, 600).to_string(),
                    source,
                });
            }
        };
        // Items that do not parse as an answer (or carry no id) are dropped:
        // their ids surface as `missing_from_batch` below, per item.
        let mut by_id: std::collections::HashMap<u64, RpcAnswer> = parsed
            .into_iter()
            .filter_map(|item| serde_json::from_value::<RpcAnswer>(item).ok())
            .filter_map(|a| a.id().map(|id| (id, a)))
            .collect();
        let mut out = Vec::with_capacity(calls.len());
        for id in ids {
            match by_id.remove(&id) {
                Some(RpcAnswer::Result { result, .. }) => out.push(Ok(result)),
                Some(RpcAnswer::Error { error, .. }) => out.push(Err(error)),
                None => out.push(Err(RpcError::missing_from_batch(id))),
            }
        }
        Ok(out)
    }

    // ---- eth namespace -------------------------------------------------

    pub async fn block_number(&self) -> Result<u64> {
        let hex: String = self.call("eth_blockNumber", json!([])).await?;
        u64::from_str_radix(hex.trim_start_matches("0x"), 16)
            .map_err(|_| self.bad_quantity("eth_blockNumber", &hex))
    }

    pub async fn block_by_number(&self, n: u64, full: bool) -> Result<Block> {
        self.call_opt("eth_getBlockByNumber", json!([hex_u64(n), full]))
            .await?
            .ok_or_else(|| Error::BlockMissing {
                endpoint: self.url.clone(),
                n,
            })
    }

    pub async fn block_by_number_opt(&self, n: u64, full: bool) -> Result<Option<Block>> {
        self.call_opt("eth_getBlockByNumber", json!([hex_u64(n), full])).await
    }

    pub async fn block_by_tag(&self, tag: &str, full: bool) -> Result<Option<Block>> {
        self.call_opt("eth_getBlockByNumber", json!([tag, full])).await
    }

    pub async fn block_access_list(&self, n: u64) -> Result<Option<BlockAccessList>> {
        match self
            .call_opt::<BlockAccessList>("eth_getBlockAccessList", json!([hex_u64(n)]))
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
        self.call("eth_getBlockReceipts", json!([hex_u64(n)])).await
    }

    pub async fn logs(&self, n: u64) -> Result<Vec<Log>> {
        self.call(
            "eth_getLogs",
            json!([{"fromBlock": hex_u64(n), "toBlock": hex_u64(n)}]),
        )
        .await
    }

    pub async fn call_at(&self, to: Address, data: &Bytes, block: u64) -> Result<Bytes> {
        self.call(
            "eth_call",
            json!([{"to": to, "data": data}, hex_u64(block)]),
        )
        .await
    }

    /// eth_call against the current state, including unmined cheatcode writes
    /// (used to read M right after the BAL convergence repair).
    pub async fn call_latest(&self, to: Address, data: &Bytes) -> Result<Bytes> {
        self.call("eth_call", json!([{"to": to, "data": data}, "latest"]))
            .await
    }

    /// eth_call with a `stateDiff` override, returned as the raw hex string.
    /// Used to discover ERC20 balance slots without mining anything (anvil's
    /// own dealERC20 probe needs mined writes and fails on a no-mining fork).
    ///
    /// A transport/RPC fault is a typed error, never collapsed into `None`: a
    /// probe that could not run must not masquerade as "this slot is not the
    /// balance slot" (boundary-discipline: a fault is not an empty answer).
    pub async fn call_with_override(
        &self,
        to: Address,
        data: &Bytes,
        overrides: &serde_json::Value,
    ) -> Result<String> {
        self.call::<String>(
            "eth_call",
            json!([{"to": to, "data": data}, "latest", overrides]),
        )
        .await
    }

    pub async fn balance_at(&self, addr: Address, block: u64) -> Result<U256> {
        let hex: String = self
            .call("eth_getBalance", json!([addr, hex_u64(block)]))
            .await?;
        U256::from_str_radix(hex.trim_start_matches("0x"), 16)
            .map_err(|_| self.bad_quantity("eth_getBalance", &hex))
    }

    pub async fn storage_at(&self, addr: Address, slot: U256, block: u64) -> Result<B256> {
        self.call("eth_getStorageAt", json!([addr, hex_u256(slot), hex_u64(block)]))
            .await
    }

    pub async fn code_at(&self, addr: Address, block: u64) -> Result<Bytes> {
        self.call("eth_getCode", json!([addr, hex_u64(block)])).await
    }

    pub async fn nonce_at(&self, addr: Address, block: u64) -> Result<u64> {
        let hex: String = self
            .call("eth_getTransactionCount", json!([addr, hex_u64(block)]))
            .await?;
        u64::from_str_radix(hex.trim_start_matches("0x"), 16)
            .map_err(|_| self.bad_quantity("eth_getTransactionCount", &hex))
    }

    /// Batch raw-tx submission. Per-item errors are returned, not thrown.
    /// Chunked by item count and body size (mainnet blocks can exceed anvil's
    /// 2 MB request limit at 500 txs).
    pub async fn send_raw_batch(&self, raws: &[Bytes]) -> Result<Vec<Result<B256, RpcError>>> {
        let calls: Vec<(String, Value)> = raws
            .iter()
            .map(|raw| ("eth_sendRawTransaction".to_string(), json!([raw])))
            .collect();
        let results = self.batch_raw_limited(&calls, 500, 1 << 20).await?;
        Ok(results
            .into_iter()
            .map(|r| match r {
                Ok(v) => serde_json::from_value::<B256>(v)
                    .map_err(|e| RpcError::bad_result("eth_sendRawTransaction", &e.to_string())),
                Err(e) => Err(e),
            })
            .collect())
    }

    /// debug_traceBlockByNumber with prestateTracer diffMode (BAL fallback and
    /// write-tracking). Returns the raw tracer output.
    pub async fn trace_block_prestate(&self, n: u64) -> Result<Value> {
        self.call_raw(
            "debug_traceBlockByNumber",
            json!([hex_u64(n), {"tracer": "prestateTracer", "tracerConfig": {"diffMode": true}}]),
        )
        .await
    }

    pub async fn trace_tx_prestate(&self, hash: B256) -> Result<Value> {
        self.call_raw(
            "debug_traceTransaction",
            json!([hash, {"tracer": "prestateTracer", "tracerConfig": {"diffMode": true}}]),
        )
        .await
    }

    // ---- anvil namespace -----------------------------------------------

    pub async fn anvil_set_automine(&self, on: bool) -> Result<()> {
        self.call_raw("anvil_setAutomine", json!([on])).await?;
        Ok(())
    }

    pub async fn anvil_mine(&self, blocks: u64) -> Result<()> {
        self.call_raw("anvil_mine", json!([hex_u64(blocks)])).await?;
        Ok(())
    }

    /// The chain's fork source + block from anvil_nodeInfo (None when the
    /// chain was not forked).
    pub async fn fork_config(&self) -> Result<Option<(String, u64)>> {
        let v: Value = self.call("anvil_nodeInfo", json!([])).await?;
        let url = v
            .pointer("/forkConfig/forkUrl")
            .and_then(|x| x.as_str())
            .map(str::to_owned);
        let block = v.pointer("/forkConfig/forkBlockNumber").and_then(serde_json::Value::as_u64);
        Ok(url.zip(block))
    }

    /// anvil_reset with a fresh fork: re-bases the chain at `block` from `url`.
    /// Used to recover M when the follow head moves backward past its
    /// irreversible replay (the cursor hash check rebases the replay after).
    pub async fn anvil_reset_fork(&self, url: &str, block: u64) -> Result<()> {
        self.call_raw(
            "anvil_reset",
            json!([{ "forking": { "jsonRpcUrl": url, "blockNumber": format!("0x{block:x}") } }]),
        )
        .await?;
        Ok(())
    }

    /// (pending, queued) counts from txpool_status.
    pub async fn txpool_status(&self) -> Result<(u64, u64)> {
        let v: Value = self.call("txpool_status", json!([])).await?;
        let get = |k: &str| -> u64 {
            v.get(k)
                .and_then(|x| x.as_str())
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0)
        };
        Ok((get("pending"), get("queued")))
    }

    pub async fn anvil_set_nonce(&self, addr: Address, nonce: u64) -> Result<()> {
        self.call_raw("anvil_setNonce", json!([addr, hex_u64(nonce)])).await?;
        Ok(())
    }

    pub async fn anvil_drop_transaction(&self, hash: B256) -> Result<()> {
        self.call_raw("anvil_dropTransaction", json!([hash])).await?;
        Ok(())
    }

    pub async fn anvil_node_info(&self) -> Result<Value> {
        self.call_raw("anvil_nodeInfo", json!([])).await
    }
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

pub fn hex_u64(v: u64) -> String {
    format!("0x{v:x}")
}

pub fn hex_u256(v: U256) -> String {
    format!("0x{v:x}")
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
            .classify(
                "eth_getBlockAccessList",
                reqwest::StatusCode::BAD_REQUEST,
                r#"{"id":7,"jsonrpc":"2.0","error":{"code":-32601,"message":"method is not available"}}"#,
            )
            .expect_err("an error answer is an error");
        assert!(err.is_method_not_found(), "{err}");
        match err {
            Error::Rpc { source, .. } => assert_eq!(source.code, -32601),
            other => panic!("expected the Rpc class, got {other:?}"),
        }
    }

    /// A gateway page on a 5xx is not an answer: the HTTP status classifies it.
    #[test]
    fn non_answer_body_on_non_2xx_keeps_the_status_class() {
        let err = client()
            .classify(
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
            .classify("eth_call", reqwest::StatusCode::OK, r#"{"message":"hello"}"#)
            .expect_err("not an answer");
        assert!(matches!(err, Error::Decode { .. }), "{err}");
        assert!(!err.is_retryable());
    }

    /// `result: null` is a legitimate empty answer (anvil cheatcodes return
    /// null on success), not corruption and not a method-not-found.
    #[test]
    fn null_result_is_an_empty_answer() {
        let v = client()
            .classify(
                "anvil_setAutomine",
                reqwest::StatusCode::OK,
                r#"{"id":1,"jsonrpc":"2.0","result":null}"#,
            )
            .expect("null result parses");
        assert!(v.is_null());
    }
}
