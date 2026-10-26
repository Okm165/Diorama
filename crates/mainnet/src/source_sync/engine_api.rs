//! Engine API client (JWT-authenticated authrpc) on the official wire model.
//!
//! Only what the top-up loop needs: `engine_newPayloadV4` (falling back to V3
//! for pre-Prague payloads) and `engine_forkchoiceUpdatedV3`. The JWT is the
//! same secret the consensus client uses (`/secrets/jwt.hex`); the envelope is
//! `alloy_json_rpc`'s and the params/results are `alloy_rpc_types_engine`'s,
//! so a spec change lands as a compile error, never a silent mismatch.
use alloy_eips::eip7685::Requests;
use alloy_json_rpc::{ErrorPayload, Id, Request, ResponsePacket, ResponsePayload};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    Claims, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, JwtSecret, PayloadStatus,
};
use serde::{de::Error as _, Serialize};
use serde_json::value::RawValue;
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

    /// A request that could not be encoded (a bug in our own types; named so
    /// it can never masquerade as a reth fault).
    #[error("engine {method}: encoding the request")]
    Encode {
        method: &'static str,
        #[source]
        source: serde_json::Error,
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

    /// reth's typed refusal. The field is named `payload`, not `source`:
    /// alloy's `ErrorPayload` implements `Display`, not `Error`, and thiserror
    /// treats a field named `source` as the error source.
    #[error("engine {method}: {payload}")]
    Rpc {
        method: &'static str,
        payload: ErrorPayload,
    },
}

#[derive(Clone)]
pub(crate) struct EngineClient {
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

    /// One JSON-RPC round trip; the raw `result` is returned so each typed
    /// method parses its own shape. The same boundary rule as the anvil
    /// client: a body that parses as a JSON-RPC answer *is* reth's answer,
    /// whatever the HTTP status.
    async fn call(
        &self,
        method: &'static str,
        params: &impl Serialize,
    ) -> Result<Box<RawValue>, EngineApiError> {
        let request = self.request_body(method, params)?;
        let resp = self
            .http
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, diorama_anvil::JSON_CONTENT_TYPE)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", request.token))
            .body(request.json)
            .send()
            .await
            .map_err(|source| EngineApiError::Transport { method, source })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|source| EngineApiError::Transport { method, source })?;
        parse_response(method, status, &text)
    }

    /// The envelope's pieces: the signed JWT and the serialized request.
    fn request_body(
        &self,
        method: &'static str,
        params: &impl Serialize,
    ) -> Result<EngineRequest, EngineApiError> {
        let token = self
            .secret
            .encode(&Claims::with_current_timestamp())
            .map_err(|source| EngineApiError::JwtSign { source })?;
        let params = serde_json::value::to_raw_value(params)
            .map_err(|source| EngineApiError::Encode { method, source })?;
        let body = Request::new(method.to_string(), Id::Number(1), params)
            .serialize()
            .map_err(|source| EngineApiError::Encode { method, source })?;
        Ok(EngineRequest {
            token,
            json: body.take_request().get().to_string(),
        })
    }

    /// Parse a raw `result` into its typed shape, naming the method on a
    /// corrupt answer.
    fn parse<T: serde::de::DeserializeOwned>(
        method: &'static str,
        result: &RawValue,
    ) -> Result<T, EngineApiError> {
        serde_json::from_str(result.get()).map_err(|source| EngineApiError::Decode {
            method,
            body: diorama_anvil::truncate(result.get(), 200).to_string(),
            source,
        })
    }

    /// Validate+execute a payload. `engine_newPayloadV4` when the EIP-7685
    /// execution requests are present (Prague+), `V3` otherwise. The returned
    /// status is the spec's VALID | INVALID | SYNCING | ACCEPTED.
    pub async fn new_payload(
        &self,
        payload: &ExecutionPayloadV3,
        versioned_hashes: &[B256],
        parent_beacon_block_root: B256,
        execution_requests: Option<&Requests>,
    ) -> Result<PayloadStatus, EngineApiError> {
        let (method, result) = match execution_requests {
            Some(requests) => (
                "engine_newPayloadV4",
                self.call(
                    "engine_newPayloadV4",
                    &(
                        payload,
                        versioned_hashes,
                        parent_beacon_block_root,
                        requests,
                    ),
                )
                .await?,
            ),
            None => (
                "engine_newPayloadV3",
                self.call(
                    "engine_newPayloadV3",
                    &(payload, versioned_hashes, parent_beacon_block_root),
                )
                .await?,
            ),
        };
        Self::parse(method, &result)
    }

    /// Update the forkchoice to `state` (no payload attributes). Returns the
    /// resulting payload status.
    pub async fn forkchoice_updated(
        &self,
        state: &ForkchoiceState,
    ) -> Result<PayloadStatus, EngineApiError> {
        let result = self
            .call(
                "engine_forkchoiceUpdatedV3",
                &(state, Option::<()>::None),
            )
            .await?;
        let updated: ForkchoiceUpdated = Self::parse("engine_forkchoiceUpdatedV3", &result)?;
        Ok(updated.payload_status)
    }
}

/// The envelope's pieces: the signed JWT and the serialized request body.
struct EngineRequest {
    token: String,
    json: String,
}

/// Parse one answer. The boundary rule: a body that parses as a JSON-RPC
/// answer *is* reth's answer whatever the HTTP status; a non-body error names
/// the status and the truncated body.
fn parse_response(
    method: &'static str,
    status: reqwest::StatusCode,
    text: &str,
) -> Result<Box<RawValue>, EngineApiError> {
    match serde_json::from_str::<ResponsePacket>(text) {
        Ok(ResponsePacket::Single(response)) => match response.payload {
            ResponsePayload::Success(result) => Ok(result),
            ResponsePayload::Failure(payload) => Err(EngineApiError::Rpc { method, payload }),
        },
        Ok(ResponsePacket::Batch(_)) => Err(EngineApiError::Decode {
            method,
            body: diorama_anvil::truncate(text, 200).to_string(),
            source: serde_json::Error::custom("a batch answer to a single call"),
        }),
        Err(_) if !status.is_success() => Err(EngineApiError::Status {
            method,
            status: status.as_u16(),
            body: diorama_anvil::truncate(text, 300).to_string(),
        }),
        Err(source) => Err(EngineApiError::Decode {
            method,
            body: diorama_anvil::truncate(text, 200).to_string(),
            source,
        }),
    }
}
