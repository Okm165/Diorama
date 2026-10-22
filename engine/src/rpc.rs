//! Thin JSON-RPC client: typed helpers + chunked batching with per-item errors.
//!
//! Batching matters for two measured reasons: writes are 9.6-18 ms each when
//! issued one at a time, and public endpoints reject large batches with HTTP 413
//! (local reth tolerates the sizes we use here, but callers chunk regardless).
use alloy_eips::eip7928::BlockAccessList;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_eth::{Block, Log, TransactionReceipt};
use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
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

#[derive(Debug, serde::Deserialize)]
struct RpcResponse {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

pub struct Rpc {
    http: reqwest::Client,
    url: String,
    next_id: AtomicU64,
}

impl Rpc {
    pub fn new(url: &str, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("http client");
        Self {
            http,
            url: url.trim_end_matches('/').to_string(),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
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
            .with_context(|| format!("{method} -> {}", self.url))?;
        let status = resp.status();
        let text = resp.text().await.context("reading rpc response")?;
        if !status.is_success() {
            bail!("{method}: HTTP {status}: {}", truncate(&text, 300));
        }
        let parsed: RpcResponse = serde_json::from_str(&text)
            .with_context(|| format!("{method}: bad rpc response: {}", truncate(&text, 300)))?;
        if let Some(err) = parsed.error {
            bail!("{method}: {err}");
        }
        // JSON-RPC allows `result: null` (anvil cheatcodes return null on success).
        Ok(parsed.result.unwrap_or(Value::Null))
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let v = self.call_raw(method, params).await?;
        serde_json::from_value(v).with_context(|| format!("{method}: decoding result"))
    }

    /// Result may legitimately be null (e.g. `eth_getBlockByNumber` of a
    /// pruned/future block, `eth_getBlockAccessList` before activation).
    pub async fn call_opt<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<Option<T>> {
        let body = json!({"jsonrpc":"2.0","id":self.id(),"method":method,"params":params});
        let resp = self.http.post(&self.url).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await.context("reading rpc response")?;
        if !status.is_success() {
            bail!("{method}: HTTP {status}: {}", truncate(&text, 300));
        }
        let parsed: RpcResponse = serde_json::from_str(&text)
            .with_context(|| format!("{method}: bad rpc response: {}", truncate(&text, 300)))?;
        if let Some(err) = parsed.error {
            bail!("{method}: {err}");
        }
        match parsed.result {
            None | Some(Value::Null) => Ok(None),
            Some(v) => Ok(Some(
                serde_json::from_value(v).with_context(|| format!("{method}: decoding result"))?,
            )),
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
                let estimate = calls[end].0.len() + serde_json::to_string(&calls[end].1)?.len() + 64;
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
        let body_str = serde_json::to_string(&body)?;
        let resp = self
            .http
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body_str.clone())
            .send()
            .await
            .context("batch request")?;
        let status = resp.status();
        let text = resp.text().await.context("reading batch response")?;
        if !status.is_success() {
            bail!("batch: HTTP {status}: {}", truncate(&text, 300));
        }
        let parsed: Vec<RpcResponse> = serde_json::from_str(&text).with_context(|| {
            format!(
                "batch: bad response ({}), request was: {}",
                truncate(&text, 200),
                truncate(&body_str, 600)
            )
        })?;
        let mut by_id: std::collections::HashMap<u64, RpcResponse> = parsed
            .into_iter()
            .filter_map(|r| r.id.map(|id| (id, r)))
            .collect();
        let mut out = Vec::with_capacity(calls.len());
        for id in ids {
            match by_id.remove(&id) {
                Some(r) => {
                    if let Some(err) = r.error {
                        out.push(Err(err));
                    } else {
                        out.push(Ok(r.result.unwrap_or(Value::Null)));
                    }
                }
                None => out.push(Err(RpcError {
                    code: -1,
                    message: "missing batch response".into(),
                    data: None,
                })),
            }
        }
        Ok(out)
    }

    // ---- eth namespace -------------------------------------------------

    pub async fn block_number(&self) -> Result<u64> {
        let hex: String = self.call("eth_blockNumber", json!([])).await?;
        Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }

    pub async fn block_by_number(&self, n: u64, full: bool) -> Result<Block> {
        self.call_opt("eth_getBlockByNumber", json!([hex_u64(n), full]))
            .await?
            .ok_or_else(|| anyhow!("block {n} not available on {}", self.url))
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
            // Older builds / non-reth endpoints may not know the method.
            Err(e) if e.to_string().contains("-32601") || e.to_string().contains("not found") => Ok(None),
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

    pub async fn balance_at(&self, addr: Address, block: u64) -> Result<U256> {
        let hex: String = self
            .call("eth_getBalance", json!([addr, hex_u64(block)]))
            .await?;
        Ok(U256::from_str_radix(hex.trim_start_matches("0x"), 16)?)
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
        Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
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
        results
            .into_iter()
            .map(|r| match r {
                Ok(v) => serde_json::from_value::<B256>(v).map_err(|e| RpcError {
                    code: -1,
                    message: format!("bad tx hash: {e}"),
                    data: None,
                }),
                Err(e) => Err(e),
            })
            .collect::<Vec<_>>()
            .pipe(Ok)
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

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        &s[..n]
    }
}

pub fn hex_u64(v: u64) -> String {
    format!("0x{v:x}")
}

pub fn hex_u256(v: U256) -> String {
    format!("0x{v:x}")
}

// Tiny pipeline helper (avoids a `.map(...).collect()` dance above).
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}
