//! The beacon-API boundary: the consensus client's REST surface, narrowed to
//! what the top-up loop needs (head slot, block by slot).
//!
//! The wire shapes are `alloy_rpc_types_beacon`'s official types; parsing
//! happens once, at decode, and the engine works on typed values afterwards.
//! Blob-sidecar acquisition lived here when M replayed blob txs; with a
//! single Anvil (the BAL carries every effect), only the top-up loop's reads
//! remain.
use alloy_eips::eip4844::Bytes48;
use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use alloy_rpc_types_beacon::{
    block::BlockResponse, header::HeaderResponse, requests::ExecutionRequestsV4,
};
use alloy_rpc_types_engine::ExecutionPayloadV3;
use mirror_anvil::truncate;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The beacon-API boundary's error taxonomy (boundary-discipline).
///
/// A read has three answers: the value; **absent** — `Ok(None)` for a missed
/// slot (404: a fact); and a **fault** — transport, HTTP status, bad JSON, or
/// a head response with no slot. Every source failing is a fault, never a
/// silent `None`: the top-up loop must not read "the beacon is down" as
/// "there is nothing to top up".
#[derive(Debug, thiserror::Error)]
pub enum BeaconError {
    #[error("building the beacon http client")]
    ClientBuild {
        #[source]
        source: reqwest::Error,
    },

    #[error("beacon {url} transport")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("beacon {url}: HTTP {status}: {body}")]
    Status {
        url: String,
        status: u16,
        body: String,
    },

    #[error("beacon {url}: bad json")]
    Decode {
        url: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("no beacon sources are configured")]
    NotConfigured,
}

/// The beacon block fields the top-up loop needs. Everything else a fork
/// carries (attestations, slashings, deposits) is ignored by serde, so one
/// narrow body serves every fork version; the execution payload is typed
/// through alloy's official beacon↔engine converter (`beacon_payload_v3`),
/// so the engine never rewrites payload JSON by hand.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TopUpBody {
    #[serde(default, deserialize_with = "deserialize_opt_payload")]
    pub execution_payload: Option<ExecutionPayloadV3>,
    #[serde(default)]
    pub blob_kzg_commitments: Vec<Bytes48>,
    /// Prague+ (Electra): the EIP-7685 execution requests as the beacon API
    /// serves them (structured objects, decimal quantities).
    #[serde(default)]
    pub execution_requests: Option<ExecutionRequestsV4>,
}

/// Decode one beacon response, naming the source in the error — the readers
/// (head slot, block by slot) share the mapping, and a decode fault must never
/// read as a missing object.
fn decode_json<T: serde::de::DeserializeOwned>(url: &str, text: &str) -> Result<T, BeaconError> {
    serde_json::from_str(text).map_err(|source| BeaconError::Decode {
        url: url.to_string(),
        source,
    })
}

/// `beacon_payload_v3::deserialize` produces a `ExecutionPayloadV3`; the
/// field is optional because pre-Bellatrix bodies carry no payload at all.
fn deserialize_opt_payload<'de, D>(deserializer: D) -> Result<Option<ExecutionPayloadV3>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    alloy_rpc_types_beacon::payload::beacon_payload_v3::deserialize(deserializer).map(Some)
}

impl TopUpBody {
    pub fn payload(&self) -> Option<&ExecutionPayloadV3> {
        self.execution_payload.as_ref()
    }

    /// Versioned hashes of the block's blobs, in order (engine_newPayload arg).
    pub fn versioned_hashes(&self) -> Vec<B256> {
        self.blob_kzg_commitments
            .iter()
            .map(commitment_to_versioned_hash)
            .collect()
    }

    /// The EIP-7685 requests in the engine API's wire form: each element is
    /// `request_type || SSZ(request_data)`, ordered by type, empty request
    /// types excluded (alloy's `to_requests` implements exactly the spec).
    pub fn requests(&self) -> Option<Requests> {
        self.execution_requests
            .as_ref()
            .map(ExecutionRequestsV4::to_requests)
    }
}

/// A beacon block response, narrowed to the top-up fields: the official
/// `BlockResponse` envelope (version, flags, signed block) with the narrow
/// body above.
pub(crate) type BeaconBlock = BlockResponse<TopUpBody>;

pub(crate) struct BeaconClient {
    http: reqwest::Client,
    urls: Vec<String>,
}

impl BeaconClient {
    /// Build the client. Fallible: the TLS backend's initialization can fail,
    /// and a boundary that cannot be built is a boot error naming itself.
    pub fn new(urls: Vec<String>) -> Result<Self, BeaconError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            // Some public beacon APIs reject unknown user agents.
            .user_agent("curl/8.5.0")
            .build()
            .map_err(|source| BeaconError::ClientBuild { source })?;
        Ok(Self { http, urls })
    }

    pub fn enabled(&self) -> bool {
        !self.urls.is_empty()
    }

    /// Consensus head slot (`/eth/v1/beacon/headers/head`).
    ///
    /// A head always exists, so the only failure is a **fault**: every source
    /// failed (the last failure is returned; the per-source causes are logged
    /// at debug). The top-up loop must not read "the beacon is down" as "there
    /// is nothing to top up".
    pub async fn head_slot(&self) -> Result<u64, BeaconError> {
        let mut last: Option<BeaconError> = None;
        for base in &self.urls {
            match self.head_slot_from(base).await {
                Ok(slot) => return Ok(slot),
                Err(e) => {
                    tracing::debug!(source = %base, error = %e, "head slot unavailable");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or(BeaconError::NotConfigured))
    }

    async fn head_slot_from(&self, base: &str) -> Result<u64, BeaconError> {
        let url = format!("{}/eth/v1/beacon/headers/head", base.trim_end_matches('/'));
        let text = self.get_text(&url).await?;
        // The official response type: `data.header.message.slot` is a u64 with
        // the API's decimal-string serde. Parsing it as hex silently yielded a
        // bogus slot, which made the top-up loop walk far past the CL head.
        let parsed: HeaderResponse = decode_json(&url, &text)?;
        Ok(parsed.data.header.message.slot)
    }

    /// Beacon block at a slot. `Ok(None)` is a missed slot (404: a fact, the
    /// same on every source); a fault fails over to the next source and, if
    /// all fail, is returned — never collapsed into "missed slot", which would
    /// make the top-up walk skip a live slot during an outage.
    pub async fn block_by_slot(&self, slot: u64) -> Result<Option<BeaconBlock>, BeaconError> {
        let mut last: Option<BeaconError> = None;
        for base in &self.urls {
            match self.block_by_slot_from(base, slot).await {
                Ok(found) => return Ok(found),
                Err(e) => {
                    tracing::debug!(slot, source = %base, error = %e, "beacon block unavailable");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or(BeaconError::NotConfigured))
    }

    async fn block_by_slot_from(
        &self,
        base: &str,
        slot: u64,
    ) -> Result<Option<BeaconBlock>, BeaconError> {
        let url = format!(
            "{}/eth/v2/beacon/blocks/{}",
            base.trim_end_matches('/'),
            slot
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|source| BeaconError::Transport {
                url: url.clone(),
                source,
            })?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let text = self.body_text(&url, resp).await?;
        let parsed: BeaconBlock = decode_json(&url, &text)?;
        Ok(Some(parsed))
    }

    /// GET a URL; a non-2xx is a named status fault.
    async fn get_text(&self, url: &str) -> Result<String, BeaconError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|source| BeaconError::Transport {
                url: url.to_string(),
                source,
            })?;
        self.body_text(url, resp).await
    }

    /// The body of a successful answer; a non-2xx is a status fault with the
    /// first bytes of the body (the operator sees what the source said).
    async fn body_text(
        &self,
        url: &str,
        resp: reqwest::Response,
    ) -> Result<String, BeaconError> {
        let status = resp.status();
        let text = resp.text().await.map_err(|source| BeaconError::Transport {
            url: url.to_string(),
            source,
        })?;
        if !status.is_success() {
            return Err(BeaconError::Status {
                url: url.to_string(),
                status: status.as_u16(),
                body: truncate(&text, 160).to_string(),
            });
        }
        Ok(text)
    }
}

/// Map a KZG commitment to its versioned hash: 0x01 || sha256(commitment)[1..].
pub(crate) fn commitment_to_versioned_hash(commitment: &Bytes48) -> B256 {
    let mut hasher = Sha256::new();
    hasher.update(commitment.as_slice());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out[0] = 0x01;
    out[1..].copy_from_slice(&digest[1..]);
    B256::from(out)
}
