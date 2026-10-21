//! Minimal Engine API client (JWT-authenticated authrpc).
//!
//! Only what the top-up loop needs: `engine_newPayloadV4` (falling back to V3
//! for pre-Prague payloads) and `engine_forkchoiceUpdatedV3`. The JWT is the
//! same secret the consensus client uses (`/secrets/jwt.hex`).
use alloy_rpc_types_engine::{Claims, JwtSecret};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Clone)]
pub struct EngineClient {
    http: reqwest::Client,
    url: String,
    secret: JwtSecret,
}

impl EngineClient {
    pub fn new(url: &str, jwt_path: &Path) -> Result<Self> {
        let secret = JwtSecret::from_file(jwt_path)
            .with_context(|| format!("reading engine JWT secret {}", jwt_path.display()))?;
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(180))
                .build()?,
            url: url.to_string(),
            secret,
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let token = self
            .secret
            .encode(&Claims::with_current_timestamp())
            .context("signing engine JWT")?;
        let resp = self
            .http
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .with_context(|| format!("engine {method} request"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("engine {method}: HTTP {status}: {}", truncate(&text, 300));
        }
        let v: Value = serde_json::from_str(&text)
            .with_context(|| format!("engine {method}: bad response {}", truncate(&text, 200)))?;
        if let Some(err) = v.get("error") {
            bail!("engine {method}: {err}");
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Validate+execute a payload. Returns (status, message); status is one of
    /// VALID | INVALID | SYNCING | ACCEPTED per the Engine API spec.
    pub async fn new_payload(
        &self,
        payload: Value,
        versioned_hashes: Vec<String>,
        parent_beacon_block_root: String,
        execution_requests: Option<Vec<String>>,
    ) -> Result<(String, String)> {
        let mut params = json!([payload, versioned_hashes, parent_beacon_block_root]);
        let method = match &execution_requests {
            Some(requests) => {
                params
                    .as_array_mut()
                    .expect("array")
                    .push(json!(requests));
                "engine_newPayloadV4"
            }
            None => "engine_newPayloadV3",
        };
        let out = self.call(method, params).await?;
        let status = out
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string();
        let message = out
            .get("validationError")
            .or_else(|| out.get("message"))
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        Ok((status, message))
    }

    /// Update the forkchoice to `head`. Returns the resulting payload status.
    pub async fn forkchoice_updated(&self, head: &str, safe: &str, finalized: &str) -> Result<String> {
        let out = self
            .call(
                "engine_forkchoiceUpdatedV3",
                json!([{
                    "headBlockHash": head,
                    "safeBlockHash": safe,
                    "finalizedBlockHash": finalized
                }, null]),
            )
            .await?;
        Ok(out
            .get("payloadStatus")
            .and_then(|p| p.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string())
    }
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        &s[..n]
    }
}
