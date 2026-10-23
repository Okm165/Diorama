//! Blob sidecar acquisition + network-form encoding for blob-tx replay.
//!
//! The wire shapes are `alloy_rpc_types_beacon`'s (the Beacon REST API's own
//! types): `BlobData` for sidecar items, `HeaderResponse` for the head,
//! `BlockResponse` for blocks, and the official beacon↔engine payload
//! converters (`beacon_payload_v3`). Parsing happens once, at decode; the
//! engine works on typed values afterwards.
//!
//! Measured/verified facts:
//!   - The beacon root of payload N is `parentBeaconBlockRoot` of EL block N+1
//!     (verified against publicnode + beacon API: EL block 26242175's root
//!     resolves to the beacon block whose payload is 26242174).
//!   - `/eth/v1/beacon/blob_sidecars/{root}` serves items whose fields are
//!     exactly alloy's `BlobData` (blob, index, kzg_commitment,
//!     kzg_commitment_inclusion_proof, kzg_proof, signed_block_header);
//!     some servers also add `proofs` (128 cell proofs per blob, EIP-7594
//!     form), which alloy's type does not model — the flatten below keeps it.
//!   - The pool expects the *network* form: `0x03 || rlp([payload, <sidecar>])`,
//!     with the 7594 variant carrying a wrapper version byte (plan A6).
use crate::error::Result;
use alloy_consensus::{EthereumTxEnvelope, TxEip4844, TxEip4844Variant};
use alloy_eips::{
    eip2718::Encodable2718,
    eip4844::{c_kzg, BlobTransactionSidecar, Bytes48},
    eip7594::{BlobTransactionSidecarEip7594, BlobTransactionSidecarVariant},
    eip7685::Requests,
};
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types_beacon::{
    block::BlockResponse,
    header::HeaderResponse,
    requests::ExecutionRequestsV4,
    sidecar::BlobData,
};
use alloy_rpc_types_engine::ExecutionPayloadV3;
use mirror_anvil::truncate;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The beacon-API boundary's error taxonomy (boundary-discipline).
///
/// A read has three answers: the value; **absent** — `Ok(None)` for a missed
/// slot (404: a fact) or an empty sidecar list; and a **fault** — transport,
/// HTTP status, bad JSON, or a head response with no slot. Every source failing
/// is a fault, never a silent `None`: the top-up loop must not read "the beacon
/// is down" as "there is nothing to top up".
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

    #[error("beacon {url}: no slot in the head response")]
    NoHead { url: String },

    #[error("no beacon sources are configured")]
    NotConfigured,
}

/// The blob/KZG boundary's error taxonomy: every variant names the field or
/// sidecar it refuses, so a source serving corrupt data is identifiable from
/// the error alone (the replay path tries the next source on any of these).
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// The source does not hold this tx's sidecar (an empty answer, not a
    /// fault; the caller fails over to the next source).
    #[error("no sidecar for versioned hash {vh}")]
    NoSidecar { vh: B256 },

    /// A field the requested form requires is missing.
    #[error("{what} is missing")]
    Missing { what: &'static str },

    /// A c-kzg operation failed on a field that parsed (corrupt value).
    #[error("{what}: KZG")]
    Kzg {
        what: &'static str,
        #[source]
        source: c_kzg::Error,
    },

    /// The sidecar's proofs do not verify against its commitments and the
    /// tx's versioned hashes.
    #[error("sidecar validation failed")]
    Validation {
        #[source]
        source: alloy_eips::eip4844::BlobTransactionValidationError,
    },

    /// A recomputed proof still does not verify: the blob does not match its
    /// commitment (a corrupt source).
    #[error("recomputed KZG proof does not verify (blob does not match its commitment)")]
    ProofMismatch,

    /// A sidecar index outside the tx's versioned-hash range.
    #[error("sidecar index {index} out of range")]
    SidecarIndex { index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BlobForm {
    /// 0x03 || rlp([payload, blobs, commitments, proofs])   (EIP-4844)
    #[default]
    Legacy,
    /// 0x03 || rlp([payload, 0x01, blobs, commitments, cell_proofs])  (EIP-7594)
    Eip7594,
    /// Bare consensus form, no sidecar (usually rejected by the pool).
    Bare,
}

impl BlobForm {
    /// The persisted name (the state file's `blob_wrapper` field).
    pub fn as_str(self) -> &'static str {
        match self {
            BlobForm::Legacy => "legacy",
            BlobForm::Eip7594 => "eip7594",
            BlobForm::Bare => "bare",
        }
    }

    /// Parse a persisted name. Unknown values are `None`, never a load
    /// failure: the form is *learned* (the next replay re-probes the pool), so
    /// a state file written by a newer engine must not brick an older one —
    /// the engine falls back to probing and learns again.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "legacy" => Some(BlobForm::Legacy),
            "eip7594" => Some(BlobForm::Eip7594),
            "bare" => Some(BlobForm::Bare),
            _ => None,
        }
    }
}

/// One blob sidecar item as the beacon API serves it: alloy's official
/// `BlobData` (parsed once, at decode: a corrupt blob/commitment/proof is
/// refused with the source's URL) plus the EIP-7594 cell proofs when the
/// source includes them (Lighthouse on Fulu). Unknown extra fields are
/// ignored by serde, so a newer API revision cannot break the decode.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SidecarItem {
    #[serde(flatten)]
    pub data: BlobData,
    #[serde(default)]
    pub proofs: Option<Vec<Bytes48>>,
}

/// `/eth/v1/beacon/blob_sidecars/{root}`: the `data` envelope around items.
#[derive(Debug, Deserialize)]
struct SidecarResponse {
    data: Vec<SidecarItem>,
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

/// Decode one beacon response, naming the source in the error — the three
/// readers (head slot, block by slot, sidecars) share the mapping, and a
/// decode fault must never read as a missing object.
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

    /// Fetch sidecar lists from every configured source, in configured order.
    /// Unlike `blob_sidecars` this does not stop at the first non-empty answer:
    /// the caller validates each candidate with KZG and picks a good one, so a
    /// source serving stale/corrupt data cannot poison the block.
    ///
    /// Collect-from-all is the contract here: per-source failures are named
    /// degradations (warn at the site), and the caller treats an empty result
    /// as "no source could serve this root" — which the replay path reports as
    /// an unavailable-sidecar fault for the block, never a silent skip.
    pub async fn blob_sidecar_sources(
        &self,
        beacon_root: B256,
    ) -> Vec<(String, Vec<SidecarItem>)> {
        let mut out = Vec::new();
        for base in &self.urls {
            match self.sidecars_from(base, beacon_root).await {
                Ok(Some(items)) => out.push((base.clone(), items)),
                Ok(None) => {
                    tracing::warn!(source = %base, "blob sidecar source returned an empty list");
                }
                Err(e) => {
                    tracing::warn!(source = %base, error = %e, "blob sidecar source failed");
                }
            }
        }
        out
    }

    async fn sidecars_from(
        &self,
        base: &str,
        beacon_root: B256,
    ) -> Result<Option<Vec<SidecarItem>>, BeaconError> {
        let url = format!(
            "{}/eth/v1/beacon/blob_sidecars/{}",
            base.trim_end_matches('/'),
            beacon_root
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
        let text = self.body_text(&url, resp).await?;
        let parsed: SidecarResponse = decode_json(&url, &text)?;
        if parsed.data.is_empty() {
            Ok(None)
        } else {
            Ok(Some(parsed.data))
        }
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

/// Select the sidecar items matching a tx's versioned hashes, in tx order.
pub(crate) fn select_sidecars<'a>(
    items: &'a [SidecarItem],
    versioned_hashes: &[B256],
) -> Result<Vec<&'a SidecarItem>, BlobError> {
    let mut out = Vec::with_capacity(versioned_hashes.len());
    for vh in versioned_hashes {
        let found = items
            .iter()
            .find(|item| commitment_to_versioned_hash(&item.data.kzg_commitment) == *vh);
        match found {
            Some(item) => out.push(item),
            None => return Err(BlobError::NoSidecar { vh: *vh }),
        }
    }
    Ok(out)
}

/// Validate a tx's selected sidecars: commitments must match the tx's versioned
/// hashes (checked by `select_sidecars`) and every KZG proof must verify against
/// the bundled mainnet trusted setup. A beacon source that cannot reconstruct
/// blobs (e.g. a non-supernode after PeerDAS) fails here instead of poisoning
/// the pool with a tx anvil rejects with `-32003 invalid KZG proof`.
pub fn validate_sidecars(
    items: &[&SidecarItem],
    versioned_hashes: &[B256],
) -> Result<(), BlobError> {
    let sidecar = BlobTransactionSidecar {
        blobs: items.iter().map(|i| *i.data.blob.clone()).collect(),
        commitments: items.iter().map(|i| i.data.kzg_commitment).collect(),
        proofs: items.iter().map(|i| i.data.kzg_proof).collect(),
    };
    let settings = c_kzg::ethereum_kzg_settings(0);
    sidecar
        .validate(versioned_hashes, settings)
        .map_err(|source| BlobError::Validation { source })
}

/// Return the tx's sidecars with a blob-level KZG proof that actually verifies.
///
/// A server that stores PeerDAS cells (Lighthouse on Fulu) still serves the
/// full blob and its commitment, but the blob-level proof is not part of the
/// stored container, so it serves a placeholder (observed: `0xc0…`, the point
/// at infinity) and validation fails with "invalid KZG proof". Recomputing the
/// proof from the blob is not a bypass: a recomputed proof verifies if and only
/// if the blob is the one the commitment commits to, so the versioned hash in
/// the tx (checked against the commitment) anchors the whole triple.
pub fn repair_proofs(
    items: &[&SidecarItem],
    versioned_hashes: &[B256],
) -> Result<Vec<SidecarItem>, BlobError> {
    let settings = c_kzg::ethereum_kzg_settings(0);
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let vh = versioned_hashes
            .get(i)
            .copied()
            .ok_or(BlobError::SidecarIndex { index: i })?;
        let mut fixed = (*item).clone();
        if validate_sidecars(&[item], &[vh]).is_ok() {
            out.push(fixed);
            continue;
        }
        let blob_kzg = c_kzg::Blob::from_bytes(fixed.data.blob.as_slice()).map_err(|source| {
            BlobError::Kzg {
                what: "blob bytes",
                source,
            }
        })?;
        let commitment_kzg = c_kzg::Bytes48::from_bytes(fixed.data.kzg_commitment.as_slice())
            .map_err(|source| BlobError::Kzg {
                what: "commitment bytes",
                source,
            })?;
        let proof = settings
            .compute_blob_kzg_proof(&blob_kzg, &commitment_kzg)
            .map_err(|source| BlobError::Kzg {
                what: "computing the blob proof",
                source,
            })?;
        fixed.data.kzg_proof = Bytes48::from(proof.to_bytes().into_inner());
        // A recomputed proof verifies if and only if the blob is the one the
        // commitment commits to; a mismatch means the source served a blob that
        // does not match its commitment.
        validate_sidecars(&[&fixed], &[vh]).map_err(|_| BlobError::ProofMismatch)?;
        tracing::info!(index = i, "recomputed blob KZG proof (source served a placeholder)");
        out.push(fixed);
    }
    Ok(out)
}

/// Which forms are actually encodable from the fetched sidecars.
pub(crate) fn available_forms(items: &[&SidecarItem], blobs_per_tx: usize) -> Vec<BlobForm> {
    let mut forms = Vec::new();
    let all_cells = items.iter().all(|i| {
        i.proofs
            .as_ref()
            .map(|p| p.len() == 128 * blobs_per_tx.max(1))
            .unwrap_or(false)
    });
    if all_cells && !items.is_empty() {
        forms.push(BlobForm::Eip7594);
    }
    // `BlobData.kzg_proof` is non-optional: the legacy form is always
    // encodable once the proofs have been repaired (a placeholder proof fails
    // `validate_sidecars` before this point).
    forms.push(BlobForm::Legacy);
    forms.push(BlobForm::Bare);
    forms
}

/// Encode a blob tx in the requested network form.
///
/// The signed envelope must be rebuilt around the sidecar; the original
/// signature and tx hash are preserved.
pub fn encode_blob_tx(
    tx: &TxEip4844,
    signature: alloy_primitives::Signature,
    hash: B256,
    items: &[&SidecarItem],
    form: BlobForm,
) -> Result<Bytes, BlobError> {
    let variant: TxEip4844Variant = match form {
        BlobForm::Bare => TxEip4844Variant::TxEip4844(tx.clone()),
        BlobForm::Legacy => {
            let sidecar = BlobTransactionSidecarVariant::Eip4844(BlobTransactionSidecar {
                blobs: items.iter().map(|i| *i.data.blob.clone()).collect(),
                commitments: items.iter().map(|i| i.data.kzg_commitment).collect(),
                proofs: items.iter().map(|i| i.data.kzg_proof).collect(),
            });
            TxEip4844Variant::TxEip4844WithSidecar(tx.clone().with_sidecar(sidecar))
        }
        BlobForm::Eip7594 => {
            let mut cell_proofs = Vec::with_capacity(items.len() * 128);
            for item in items {
                let proofs = item
                    .proofs
                    .as_ref()
                    .ok_or(BlobError::Missing { what: "cell proofs" })?;
                cell_proofs.extend_from_slice(proofs);
            }
            let sidecar = BlobTransactionSidecarVariant::Eip7594(BlobTransactionSidecarEip7594 {
                blobs: items.iter().map(|i| *i.data.blob.clone()).collect(),
                commitments: items.iter().map(|i| i.data.kzg_commitment).collect(),
                cell_proofs,
            });
            TxEip4844Variant::TxEip4844WithSidecar(tx.clone().with_sidecar(sidecar))
        }
    };
    let signed = alloy_consensus::Signed::new_unchecked(variant, signature, hash);
    let envelope: EthereumTxEnvelope<TxEip4844Variant> = EthereumTxEnvelope::Eip4844(signed);
    Ok(envelope.encoded_2718().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::eip4844::Blob;

    fn item_with(blob: Blob, commitment: Bytes48, proof: Bytes48) -> SidecarItem {
        SidecarItem {
            data: BlobData {
                index: 0,
                blob: Box::new(blob),
                kzg_commitment: commitment,
                kzg_proof: proof,
                signed_block_header: Default::default(),
                kzg_commitment_inclusion_proof: Vec::new(),
            },
            proofs: None,
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one fixture through every blob call on a measured stack budget: splitting it would split the measurement"
    )]
    fn blob_path_validates_repairs_and_encodes_on_a_bounded_stack() {
        // One fixture through every call the replay path makes on a blob tx,
        // on a deliberately small stack budget: 4 MiB, a quarter of the
        // engine's worker budget (bins/mainnet-engine). Debug builds give every
        // by-value 128 KiB `Blob` its own slot, so the chain measures ~2 MiB
        // (repair 1.28 + validate 0.64; encode 1.29). If a future frame
        // outgrows the budget, this test aborts loudly instead of the engine
        // crashing in the field.
        std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                use alloy_eips::eip4844::c_kzg;
                let settings = c_kzg::ethereum_kzg_settings(0);
                let mut bytes = vec![0u8; 131_072];
                bytes[31] = 42; // one field element = 42
                let blob = c_kzg::Blob::from_bytes(&bytes).expect("blob");
                let commitment = settings.blob_to_kzg_commitment(&blob).expect("commitment");
                let commitment_bytes = commitment.to_bytes();
                let proof = settings
                    .compute_blob_kzg_proof(&blob, &commitment_bytes)
                    .expect("proof");
                let vh = commitment_to_versioned_hash(&Bytes48::from(commitment_bytes.into_inner()));
                let item = item_with(
                    Blob::from_slice(&bytes),
                    Bytes48::from(commitment_bytes.into_inner()),
                    Bytes48::from(proof.to_bytes().into_inner()),
                );

                // The validator the replay path uses accepts a good triple...
                validate_sidecars(&[&item], &[vh]).expect("valid sidecar must validate");

                // ...and rejects a blob that does not match its commitment.
                let mut bad = item.clone();
                *bad.data.blob = Blob::from_slice(&vec![7u8; 131_072]);
                assert!(validate_sidecars(&[&bad], &[vh]).is_err());

                // A Lighthouse-on-Fulu source serves the point-at-infinity
                // proof placeholder; repair recomputes and re-validates it.
                let mut placeholder = item.clone();
                placeholder.data.kzg_proof =
                    Bytes48::from_slice(&[&[0xc0u8][..], &[0u8; 47][..]].concat());
                let repaired = repair_proofs(&[&placeholder], &[vh]).expect("repaired");
                validate_sidecars(&[&repaired[0]], &[vh]).expect("repaired proof validates");

                // Both network forms the encoder can build from the item.
                let tx = TxEip4844 {
                    chain_id: 1,
                    nonce: 0,
                    gas_limit: 21_000,
                    max_fee_per_gas: 1,
                    max_priority_fee_per_gas: 1,
                    max_fee_per_blob_gas: 1,
                    to: alloy_primitives::Address::ZERO,
                    value: alloy_primitives::U256::ZERO,
                    input: Bytes::new(),
                    access_list: Default::default(),
                    blob_versioned_hashes: vec![vh],
                };
                let signature = alloy_primitives::Signature::new(
                    alloy_primitives::U256::ONE,
                    alloy_primitives::U256::ONE,
                    false,
                );
                let items: Vec<&SidecarItem> = repaired.iter().collect();
                let legacy = encode_blob_tx(&tx, signature, B256::ZERO, &items, BlobForm::Legacy)
                    .expect("legacy form");
                let bare = encode_blob_tx(&tx, signature, B256::ZERO, &items, BlobForm::Bare)
                    .expect("bare form");
                assert!(legacy.len() > bare.len(), "the sidecar form carries the blobs");
            })
            .expect("spawn")
            .join()
            .expect("blob path");
    }

    /// A minimal Electra block response around the given body (the envelope
    /// fields the official `BlockResponse` requires).
    fn block_response(body: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "version": "electra",
            "execution_optimistic": false,
            "finalized": false,
            "data": {
                "message": {
                    "slot": "1",
                    "proposer_index": "0",
                    "parent_root": format!("0x{}", "00".repeat(32)),
                    "state_root": format!("0x{}", "00".repeat(32)),
                    "body": body
                },
                "signature": format!("0x{}", "00".repeat(96))
            }
        })
    }

    #[test]
    fn execution_requests_follow_engine_api_encoding() {
        // engine/prague.md: elements are `request_type || SSZ(request_data)`,
        // ordered by type, and empty request types are excluded entirely.
        // The conversion is alloy's official beacon→engine path.
        let block: BeaconBlock = serde_json::from_value(block_response(&serde_json::json!({
            "execution_requests": {
                "deposits": [{
                    "pubkey": format!("0x{}", "11".repeat(48)),
                    "withdrawal_credentials": format!("0x{}", "22".repeat(32)),
                    "amount": "32000000000",
                    "signature": format!("0x{}", "33".repeat(96)),
                    "index": "7"
                }],
                "withdrawals": [],
                "consolidations": []
            }
        })))
        .expect("beacon block");

        let reqs = block.data.message.body.requests().expect("requests");
        assert_eq!(reqs.len(), 1, "only non-empty request types are included");
        let bytes = reqs[0].clone();
        assert_eq!(bytes.len(), 1 + 192, "type byte + one deposit request");
        assert_eq!(bytes[0], 0x00, "deposit request type");
        let amount_off = 1 + 48 + 32;
        let amount = u64::from_le_bytes(bytes[amount_off..amount_off + 8].try_into().unwrap());
        assert_eq!(amount, 32_000_000_000, "SSZ quantities are little-endian");
        let index_off = 1 + 48 + 32 + 8 + 96;
        let index = u64::from_le_bytes(bytes[index_off..index_off + 8].try_into().unwrap());
        assert_eq!(index, 7);

        // All-empty requests must encode as `[]`, not as empty hex strings.
        let empty: BeaconBlock = serde_json::from_value(block_response(&serde_json::json!({
            "execution_requests": { "deposits": [], "withdrawals": [], "consolidations": [] }
        })))
        .expect("beacon block");
        assert!(empty
            .data
            .message
            .body
            .requests()
            .expect("requests")
            .is_empty());
    }

    #[test]
    fn beacon_payload_parses_through_the_official_converter() {
        // The measured beacon shape: snake_case fields, decimal-string
        // quantities — alloy's beacon↔engine converter is the only parser.
        let block: BeaconBlock = serde_json::from_value(block_response(&serde_json::json!({
            "execution_payload": {
                "parent_hash": format!("0x{}", "aa".repeat(32)),
                "fee_recipient": format!("0x{}", "bb".repeat(20)),
                "state_root": format!("0x{}", "cc".repeat(32)),
                "receipts_root": format!("0x{}", "dd".repeat(32)),
                "logs_bloom": format!("0x{}", "00".repeat(256)),
                "prev_randao": format!("0x{}", "ee".repeat(32)),
                "block_number": "26250104",
                "gas_limit": "60000000",
                "gas_used": "21000",
                "timestamp": "1750000000",
                "extra_data": "0x",
                "base_fee_per_gas": "1000000000",
                "block_hash": format!("0x{}", "ff".repeat(32)),
                "transactions": [],
                "withdrawals": [],
                "blob_gas_used": "131072",
                "excess_blob_gas": "0"
            }
        })))
        .expect("beacon block");

        let payload = block.data.message.body.payload().expect("payload");
        assert_eq!(
            payload.payload_inner.payload_inner.block_number,
            26_250_104
        );
        assert_eq!(payload.blob_gas_used, 131_072);
        assert!(block.data.message.body.versioned_hashes().is_empty());
    }

    #[test]
    fn versioned_hash_mapping() {
        // A known mainnet blob: commitment -> versioned hash mapping is
        // 0x01 || sha256(commitment)[1..]; just verify shape/consistency here.
        let commitment: Bytes48 = Bytes48::repeat_byte(7);
        let vh = commitment_to_versioned_hash(&commitment);
        assert_eq!(vh[0], 0x01);
        assert_ne!(vh, B256::ZERO);
    }
}
