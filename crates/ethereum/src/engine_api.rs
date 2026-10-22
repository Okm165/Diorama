//! Minimal Engine API client (JWT-authenticated authrpc).
//!
//! Only what the top-up loop needs: `engine_newPayloadV4` (falling back to V3
//! for pre-Prague payloads) and `engine_forkchoiceUpdatedV3`. The JWT is the
//! same secret the consensus client uses (`/secrets/jwt.hex`).
use alloy_rpc_types_engine::{Claims, JwtSecret};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The engine-API boundary's error taxonomy (boundary-discipline): a JWT that
/// cannot be read or signed is a boot/config fault; a transport failure is a
/// fault; an HTTP refusal or a bad body is named with the method and a
/// truncated body, so the operator sees what reth actually said.
#[derive(Debug, thiserror::Error)]
pub enum EngineApiError {
    #[error("reading the engine JWT secret {path}")]
    JwtRead {
        path: PathBuf,
        #[source]
        source: alloy_rpc_types_engine::JwtError,
    },

    #[error("signing the engine JWT")]
    JwtSign {
        #[source]
        source: jsonwebtoken::errors::Error,
    },

    #[error("building the engine api client")]
    ClientBuild {
        #[source]
        source: reqwest::Error,
    },

    #[error("engine {method} transport")]
    Transport {
        method: &'static str,
        #[source]
        source: reqwest::Error,
    },

    #[error("engine {method}: HTTP {status}: {body}")]
    Status {
        method: &'static str,
        status: u16,
        body: String,
    },

    #[error("engine {method}: bad response: {body}")]
    Decode {
        method: &'static str,
        body: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("engine {method}: {message}")]
    Rpc {
        method: &'static str,
        message: String,
    },
}

#[derive(Clone)]
pub struct EngineClient {
    http: reqwest::Client,
    url: String,
    secret: JwtSecret,
}

impl EngineClient {
    pub fn new(url: &str, jwt_path: &Path) -> Result<Self, EngineApiError> {
        let secret = JwtSecret::from_file(jwt_path).map_err(|source| EngineApiError::JwtRead {
            path: jwt_path.to_path_buf(),
            source,
        })?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .map_err(|source| EngineApiError::ClientBuild { source })?;
        Ok(Self {
            http,
            url: url.to_string(),
            secret,
        })
    }

    async fn call(&self, method: &'static str, params: Value) -> Result<Value, EngineApiError> {
        let token = self
            .secret
            .encode(&Claims::with_current_timestamp())
            .map_err(|source| EngineApiError::JwtSign { source })?;
        let resp = self
            .http
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|source| EngineApiError::Transport { method, source })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|source| EngineApiError::Transport { method, source })?;
        if !status.is_success() {
            return Err(EngineApiError::Status {
                method,
                status: status.as_u16(),
                body: truncate(&text, 300).to_string(),
            });
        }
        let v: Value = serde_json::from_str(&text).map_err(|source| EngineApiError::Decode {
            method,
            body: truncate(&text, 200).to_string(),
            source,
        })?;
        if let Some(err) = v.get("error") {
            return Err(EngineApiError::Rpc {
                method,
                message: err.to_string(),
            });
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
    ) -> Result<(String, String), EngineApiError> {
        let mut params = vec![
            payload,
            json!(versioned_hashes),
            json!(parent_beacon_block_root),
        ];
        let method = match execution_requests {
            Some(requests) => {
                params.push(json!(requests));
                "engine_newPayloadV4"
            }
            None => "engine_newPayloadV3",
        };
        let out = self.call(method, Value::Array(params)).await?;
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
    pub async fn forkchoice_updated(
        &self,
        head: &str,
        safe: &str,
        finalized: &str,
    ) -> Result<String, EngineApiError> {
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
