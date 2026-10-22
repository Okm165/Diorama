//! Blob sidecar acquisition + network-form encoding for blob-tx replay.
//!
//! Measured/verified facts:
//!   - The beacon root of payload N is `parentBeaconBlockRoot` of EL block N+1
//!     (verified against publicnode + beacon API: EL block 26242175's root
//!     resolves to the beacon block whose payload is 26242174).
//!   - `/eth/v1/beacon/blob_sidecars/{root}` serves `{index, blob,
//!     kzg_commitment, kzg_proof}` (legacy 4844 form); some servers also return
//!     `proofs` (128 cell proofs per blob, EIP-7594 form).
//!   - The pool expects the *network* form: `0x03 || rlp([payload, <sidecar>])`,
//!     with the 7594 variant carrying a wrapper version byte (plan A6).
use alloy_consensus::{EthereumTxEnvelope, TxEip4844, TxEip4844Variant};
use alloy_eips::{
    eip2718::Encodable2718,
    eip4844::{c_kzg, Blob, Bytes48},
    eip7594::{BlobTransactionSidecarEip7594, BlobTransactionSidecarVariant},
};
use alloy_primitives::{Bytes, B256};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlobForm {
    /// 0x03 || rlp([payload, blobs, commitments, proofs])   (EIP-4844)
    #[default]
    Legacy,
    /// 0x03 || rlp([payload, 0x01, blobs, commitments, cell_proofs])  (EIP-7594)
    Eip7594,
    /// Bare consensus form, no sidecar (usually rejected by the pool).
    Bare,
}

impl BlobForm {
    pub fn as_str(self) -> &'static str {
        match self {
            BlobForm::Legacy => "legacy",
            BlobForm::Eip7594 => "eip7594",
            BlobForm::Bare => "bare",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "legacy" => Some(BlobForm::Legacy),
            "eip7594" => Some(BlobForm::Eip7594),
            "bare" => Some(BlobForm::Bare),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SidecarItem {
    #[serde(default)]
    #[allow(dead_code)]
    pub index: Option<serde_json::Value>,
    pub blob: String,
    pub kzg_commitment: String,
    #[serde(default)]
    pub kzg_proof: Option<String>,
    /// EIP-7594 cell proofs, when the server serves them.
    #[serde(default)]
    pub proofs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct BeaconResponse<T> {
    data: T,
}

pub struct BeaconClient {
    http: reqwest::Client,
    urls: Vec<String>,
}

impl BeaconClient {
    pub fn new(urls: Vec<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            // Some public beacon APIs reject unknown user agents.
            .user_agent("curl/8.5.0")
            .build()
            .expect("http client");
        Self { http, urls }
    }

    pub fn enabled(&self) -> bool {
        !self.urls.is_empty()
    }

    /// Consensus head slot (`/eth/v1/beacon/headers/head`); None when the
    /// beacon API is unreachable.
    pub async fn head_slot(&self) -> Result<Option<u64>> {
        for base in &self.urls {
            let url = format!("{}/eth/v1/beacon/headers/head", base.trim_end_matches('/'));
            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.unwrap_or_default();
                    let v: serde_json::Value = serde_json::from_str(&text)?;
                    // Beacon API quantities are decimal strings (e.g. "15482511");
                    // parsing them as hex silently yields a bogus slot, which
                    // made the top-up loop walk far past the CL head.
                    let slot = v
                        .pointer("/data/header/message/slot")
                        .and_then(|s| s.as_str())
                        .and_then(parse_quantity);
                    return Ok(slot);
                }
                Ok(resp) => {
                    tracing::debug!(url = %url, status = %resp.status(), "head slot unavailable");
                }
                Err(e) => tracing::debug!(url = %url, error = %e, "head slot request failed"),
            }
        }
        Ok(None)
    }

    /// Beacon block at a slot; None for missed slots (404) or unreachable API.
    pub async fn block_by_slot(&self, slot: u64) -> Result<Option<BeaconBlock>> {
        for base in &self.urls {
            let url = format!(
                "{}/eth/v2/beacon/blocks/{}",
                base.trim_end_matches('/'),
                slot
            );
            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.unwrap_or_default();
                    let parsed: BeaconResponse<BeaconBlock> = serde_json::from_str(&text)
                        .with_context(|| format!("beacon block {slot}: bad json"))?;
                    return Ok(Some(parsed.data));
                }
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => return Ok(None),
                Ok(resp) => {
                    tracing::debug!(slot, status = %resp.status(), "beacon block unavailable");
                }
                Err(e) => tracing::debug!(slot, error = %e, "beacon block request failed"),
            }
        }
        Ok(None)
    }

    /// Fetch sidecar lists from every configured source, in configured order.
    /// Unlike `blob_sidecars` this does not stop at the first non-empty answer:
    /// the caller validates each candidate with KZG and picks a good one, so a
    /// source serving stale/corrupt data cannot poison the block.
    pub async fn blob_sidecar_sources(
        &self,
        beacon_root: B256,
    ) -> Vec<(String, Vec<SidecarItem>)> {
        let mut out = Vec::new();
        for base in &self.urls {
            let url = format!(
                "{}/eth/v1/beacon/blob_sidecars/{}",
                base.trim_end_matches('/'),
                beacon_root
            );
            match self.http.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if !status.is_success() {
                        tracing::warn!(
                            source = %base,
                            status = %status,
                            body = %truncate(&text, 160),
                            "blob sidecar source failed"
                        );
                        continue;
                    }
                    match serde_json::from_str::<BeaconResponse<Vec<SidecarItem>>>(&text) {
                        Ok(parsed) if !parsed.data.is_empty() => out.push((base.clone(), parsed.data)),
                        Ok(_) => tracing::warn!(source = %base, "blob sidecar source returned an empty list"),
                        Err(e) => tracing::warn!(source = %base, error = %e, "blob sidecar source returned bad json"),
                    }
                }
                Err(e) => tracing::warn!(source = %base, error = %e, "blob sidecar source unreachable"),
            }
        }
        out
    }
}

/// Beacon block (only the parts the top-up loop needs). The beacon API is
/// snake_case; the Engine API wants camelCase, so the payload is forwarded as
/// JSON and rewritten in `source_sync::to_engine_payload`.
#[derive(Debug, Clone, Deserialize)]
pub struct BeaconBlock {
    pub message: BeaconBlockMessage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BeaconBlockMessage {
    #[serde(default)]
    #[allow(dead_code)]
    pub slot: String,
    pub parent_root: String,
    pub body: BeaconBlockBody,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BeaconBlockBody {
    #[serde(default)]
    pub execution_payload: Option<serde_json::Value>,
    #[serde(default)]
    pub blob_kzg_commitments: Vec<String>,
    #[serde(default)]
    pub execution_requests: Option<serde_json::Value>,
}

impl BeaconBlock {
    pub fn execution_payload(&self) -> Option<&serde_json::Value> {
        self.message.body.execution_payload.as_ref()
    }

    pub fn parent_root(&self) -> String {
        self.message.parent_root.clone()
    }

    /// Versioned hashes of the block's blobs, in order (engine_newPayload arg).
    pub fn blob_versioned_hashes(&self) -> Vec<B256> {
        self.message
            .body
            .blob_kzg_commitments
            .iter()
            .filter_map(|c| {
                c.parse::<Bytes48>()
                    .ok()
                    .map(|c| commitment_to_versioned_hash(&c))
            })
            .collect()
    }

    /// Prague+: `engine_newPayloadV4` takes the execution-layer requests as
    /// defined by EIP-7685 / execution-apis `engine/prague.md`: each element is
    /// `request_type || SSZ(request_data)`, ordered by type, and **elements
    /// with empty request_data MUST be excluded** (all empty => `[]`). The
    /// beacon API serves them as structured objects (decimal quantities, 0x
    /// byte fields), so they are re-encoded here:
    ///   type 0x00 EIP-6110 DepositRequest       = pubkey(48) + wc(32) + amount(8) + signature(96) + index(8) = 192 B
    ///   type 0x01 EIP-7002 WithdrawalRequest    = source_address(20) + validator_pubkey(48) + amount(8)       =  76 B
    ///   type 0x02 EIP-7251 ConsolidationRequest = source_address(20) + source_pubkey(48) + target_pubkey(48)  = 116 B
    /// Each request_data is the plain concatenation of its fixed-size elements
    /// (SSZ `List[fixed-size]` carries no length prefix).
    pub fn execution_requests(&self) -> Option<Vec<String>> {
        let req = self.message.body.execution_requests.as_ref()?.as_object()?;
        let mut out = Vec::new();
        for (ty, data) in [
            (
                0x00u8,
                encode_requests(req.get("deposits"), 192, |item, buf| {
                    push_bytes(buf, item.get("pubkey"), 48)?;
                    push_bytes(buf, item.get("withdrawal_credentials"), 32)?;
                    push_u64(buf, item.get("amount"))?;
                    push_bytes(buf, item.get("signature"), 96)?;
                    push_u64(buf, item.get("index"))
                }),
            ),
            (
                0x01,
                encode_requests(req.get("withdrawals"), 76, |item, buf| {
                    push_bytes(buf, item.get("source_address"), 20)?;
                    push_bytes(buf, item.get("validator_pubkey"), 48)?;
                    push_u64(buf, item.get("amount"))
                }),
            ),
            (
                0x02,
                encode_requests(req.get("consolidations"), 116, |item, buf| {
                    push_bytes(buf, item.get("source_address"), 20)?;
                    push_bytes(buf, item.get("source_pubkey"), 48)?;
                    push_bytes(buf, item.get("target_pubkey"), 48)
                }),
            ),
        ] {
            if data.is_empty() {
                continue;
            }
            let mut buf = Vec::with_capacity(data.len() + 1);
            buf.push(ty);
            buf.extend_from_slice(&data);
            out.push(format!("0x{}", hex::encode(buf)));
        }
        Some(out)
    }
}

/// SSZ-encode a list of fixed-size requests (concatenation, no prefix).
fn encode_requests(
    items: Option<&serde_json::Value>,
    expected_len: usize,
    encode: impl Fn(&serde_json::Value, &mut Vec<u8>) -> Result<()>,
) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(serde_json::Value::Array(list)) = items {
        for item in list {
            let before = out.len();
            if let Err(e) = encode(item, &mut out) {
                tracing::warn!(error = %e, "execution request item skipped (bad encoding)");
                out.truncate(before);
                continue;
            }
            if out.len() - before != expected_len {
                tracing::warn!(
                    got = out.len() - before,
                    want = expected_len,
                    "execution request item has unexpected size"
                );
                out.truncate(before);
            }
        }
    }
    out
}

/// Parse a beacon-API quantity. The API standard is a **decimal string**
/// (`"15482511"`); hex is tolerated because some tooling emits it. Getting this
/// wrong is silent: `"26243147"` parsed as hex is a plausible-looking block
/// number, and a slot parsed as hex bounds the top-up walk far past the CL head.
pub fn parse_quantity(s: &str) -> Option<u64> {
    match s.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// Byte field with an exact length (beacon API: 0x hex).
fn push_bytes(out: &mut Vec<u8>, v: Option<&serde_json::Value>, len: usize) -> Result<()> {
    let s = v.and_then(|v| v.as_str()).context("missing byte field")?;
    let b = hex::decode(s.trim_start_matches("0x")).context("bad hex byte field")?;
    if b.len() != len {
        bail!("byte field is {} bytes, expected {len}", b.len());
    }
    out.extend_from_slice(&b);
    Ok(())
}

/// Little-endian u64 quantity (beacon API: decimal string, hex tolerated).
fn push_u64(out: &mut Vec<u8>, v: Option<&serde_json::Value>) -> Result<()> {
    let s = v.and_then(|v| v.as_str()).context("missing quantity")?;
    let n: u64 = match s.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).context("bad hex quantity")?,
        None => s.parse().context("bad decimal quantity")?,
    };
    out.extend_from_slice(&n.to_le_bytes());
    Ok(())
}

/// Map a KZG commitment to its versioned hash: 0x01 || sha256(commitment)[1..].
pub fn commitment_to_versioned_hash(commitment: &Bytes48) -> B256 {
    let mut hasher = Sha256::new();
    hasher.update(commitment.as_slice());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out[0] = 0x01;
    out[1..].copy_from_slice(&digest[1..]);
    B256::from(out)
}

/// Select the sidecar items matching a tx's versioned hashes, in tx order.
pub fn select_sidecars<'a>(
    items: &'a [SidecarItem],
    versioned_hashes: &[B256],
) -> Result<Vec<&'a SidecarItem>> {
    let mut out = Vec::with_capacity(versioned_hashes.len());
    for vh in versioned_hashes {
        let found = items.iter().find(|item| {
            item.kzg_commitment
                .parse::<Bytes48>()
                .map(|c| commitment_to_versioned_hash(&c) == *vh)
                .unwrap_or(false)
        });
        match found {
            Some(item) => out.push(item),
            None => bail!("no sidecar for versioned hash {vh}"),
        }
    }
    Ok(out)
}

/// Validate a tx's selected sidecars: commitments must match the tx's versioned
/// hashes (checked by `select_sidecars`) and every KZG proof must verify against
/// the bundled mainnet trusted setup. A beacon source that cannot reconstruct
/// blobs (e.g. a non-supernode after PeerDAS) fails here instead of poisoning
/// the pool with a tx anvil rejects with `-32003 invalid KZG proof`.
pub fn validate_sidecars(items: &[&SidecarItem], versioned_hashes: &[B256]) -> Result<()> {    let mut blobs = Vec::with_capacity(items.len());
    let mut commitments = Vec::with_capacity(items.len());
    let mut proofs = Vec::with_capacity(items.len());
    for item in items {
        blobs.push(item.blob.parse::<Blob>().context("blob hex")?);
        commitments.push(
            item.kzg_commitment
                .parse::<Bytes48>()
                .context("commitment hex")?,
        );
        proofs.push(
            item.kzg_proof
                .as_ref()
                .ok_or_else(|| anyhow!("missing kzg_proof (source cannot serve blobs)"))?
                .parse::<Bytes48>()
                .context("proof hex")?,
        );
    }
    let sidecar = alloy_eips::eip4844::BlobTransactionSidecar {
        blobs,
        commitments,
        proofs,
    };
    let settings = alloy_eips::eip4844::c_kzg::ethereum_kzg_settings(0);
    sidecar
        .validate(versioned_hashes, settings)
        .map_err(|e| anyhow!("{e}"))
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
pub fn repair_proofs(items: &[&SidecarItem], versioned_hashes: &[B256]) -> Result<Vec<SidecarItem>> {
    let settings = alloy_eips::eip4844::c_kzg::ethereum_kzg_settings(0);
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let vh = versioned_hashes
            .get(i)
            .copied()
            .ok_or_else(|| anyhow!("sidecar index {i} out of range"))?;
        let mut fixed = (*item).clone();
        if validate_sidecars(&[item], &[vh]).is_ok() {
            out.push(fixed);
            continue;
        }
        let blob = fixed.blob.parse::<Blob>().context("blob hex")?;
        let commitment = fixed.kzg_commitment.parse::<Bytes48>().context("commitment hex")?;
        let blob_kzg = c_kzg::Blob::from_bytes(blob.as_slice()).context("blob bytes")?;
        let commitment_kzg =
            c_kzg::Bytes48::from_bytes(commitment.as_slice()).context("commitment bytes")?;
        let proof = settings
            .compute_blob_kzg_proof(&blob_kzg, &commitment_kzg)
            .map_err(|e| anyhow!("computing blob KZG proof: {e}"))?;
        fixed.kzg_proof = Some(format!("0x{}", hex::encode(proof.to_bytes().into_inner())));
        validate_sidecars(&[&fixed], &[vh]).context(
            "recomputed blob KZG proof does not verify (blob does not match its commitment)",
        )?;
        tracing::info!(index = i, "recomputed blob KZG proof (source served a placeholder)");
        out.push(fixed);
    }
    Ok(out)
}

/// Which forms are actually encodable from the fetched sidecars.
pub fn available_forms(items: &[&SidecarItem], blobs_per_tx: usize) -> Vec<BlobForm> {
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
    if items.iter().all(|i| i.kzg_proof.is_some()) {
        forms.push(BlobForm::Legacy);
    }
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
) -> Result<Bytes> {
    let variant: TxEip4844Variant = match form {
        BlobForm::Bare => TxEip4844Variant::TxEip4844(tx.clone()),
        BlobForm::Legacy => {
            let mut blobs = Vec::with_capacity(items.len());
            let mut commitments = Vec::with_capacity(items.len());
            let mut proofs = Vec::with_capacity(items.len());
            for item in items {
                blobs.push(item.blob.parse::<Blob>().context("blob hex")?);
                commitments.push(item.kzg_commitment.parse::<Bytes48>().context("commitment hex")?);
                proofs.push(
                    item.kzg_proof
                        .as_ref()
                        .ok_or_else(|| anyhow!("missing kzg_proof"))?
                        .parse::<Bytes48>()
                        .context("proof hex")?,
                );
            }
            let sidecar = BlobTransactionSidecarVariant::Eip4844(
                alloy_eips::eip4844::BlobTransactionSidecar {
                    blobs,
                    commitments,
                    proofs,
                },
            );
            TxEip4844Variant::TxEip4844WithSidecar(tx.clone().with_sidecar(sidecar))
        }
        BlobForm::Eip7594 => {
            let mut blobs = Vec::with_capacity(items.len());
            let mut commitments = Vec::with_capacity(items.len());
            let mut cell_proofs = Vec::with_capacity(items.len() * 128);
            for item in items {
                blobs.push(item.blob.parse::<Blob>().context("blob hex")?);
                commitments.push(item.kzg_commitment.parse::<Bytes48>().context("commitment hex")?);
                for p in item.proofs.as_ref().ok_or_else(|| anyhow!("missing cell proofs"))? {
                    cell_proofs.push(p.parse::<Bytes48>().context("cell proof hex")?);
                }
            }
            let sidecar = BlobTransactionSidecarVariant::Eip7594(BlobTransactionSidecarEip7594 {
                blobs,
                commitments,
                cell_proofs,
            });
            TxEip4844Variant::TxEip4844WithSidecar(tx.clone().with_sidecar(sidecar))
        }
    };
    let signed = alloy_consensus::Signed::new_unchecked(variant, signature, hash);
    let envelope: EthereumTxEnvelope<TxEip4844Variant> = EthereumTxEnvelope::Eip4844(signed);
    Ok(envelope.encoded_2718().into())
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        &s[..n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kzg_validation_accepts_a_valid_sidecar() {
        // Build a blob, its commitment and proof with c-kzg (the same library
        // anvil uses), then run it through the validator the replay path uses.
        // If this passes, "invalid KZG proof" from a beacon source means the
        // source's data is genuinely bad. Blobs are 128 KiB values, so this
        // needs more stack than a debug test thread has by default.
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
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
                let item = SidecarItem {
                    index: None,
                    blob: format!("0x{}", hex::encode(&bytes)),
                    kzg_commitment: format!("0x{}", hex::encode(commitment_bytes.into_inner())),
                    kzg_proof: Some(format!("0x{}", hex::encode(proof.to_bytes().into_inner()))),
                    proofs: None,
                };
                validate_sidecars(&[&item], &[vh]).expect("valid sidecar must validate");

                // And a corrupted blob must be rejected.
                let mut bad = item.clone();
                bad.blob = format!("0x{}", hex::encode(vec![7u8; 131_072]));
                assert!(validate_sidecars(&[&bad], &[vh]).is_err());
            })
            .expect("spawn")
            .join()
            .expect("kzg test");
    }

    #[test]
    fn execution_requests_follow_engine_api_encoding() {
        // engine/prague.md: elements are `request_type || SSZ(request_data)`,
        // ordered by type, and empty request types are excluded entirely.
        let block: BeaconBlock = serde_json::from_value(serde_json::json!({
            "message": {
                "slot": "1",
                "parent_root": format!("0x{}", "00".repeat(32)),
                "body": {
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
                }
            }
        }))
        .expect("beacon block");

        let reqs = block.execution_requests().expect("requests");
        assert_eq!(reqs.len(), 1, "only non-empty request types are included");
        let bytes = hex::decode(reqs[0].trim_start_matches("0x")).expect("hex");
        assert_eq!(bytes.len(), 1 + 192, "type byte + one deposit request");
        assert_eq!(bytes[0], 0x00, "deposit request type");
        let amount_off = 1 + 48 + 32;
        let amount = u64::from_le_bytes(bytes[amount_off..amount_off + 8].try_into().unwrap());
        assert_eq!(amount, 32_000_000_000, "SSZ quantities are little-endian");
        let index_off = 1 + 48 + 32 + 8 + 96;
        let index = u64::from_le_bytes(bytes[index_off..index_off + 8].try_into().unwrap());
        assert_eq!(index, 7);

        // All-empty requests must encode as `[]`, not as empty hex strings.
        let empty: BeaconBlock = serde_json::from_value(serde_json::json!({
            "message": {
                "slot": "1",
                "parent_root": format!("0x{}", "00".repeat(32)),
                "body": {
                    "execution_requests": {
                        "deposits": [],
                        "withdrawals": [],
                        "consolidations": []
                    }
                }
            }
        }))
        .expect("beacon block");
        assert!(empty.execution_requests().expect("requests").is_empty());
    }

    #[test]
    fn beacon_quantities_are_decimal_or_hex() {
        // Beacon API standard: decimal strings; hex is tolerated.
        assert_eq!(parse_quantity("15482511"), Some(15_482_511));
        assert_eq!(
            parse_quantity(&format!("0x{:x}", 15_482_511u64)),
            Some(15_482_511)
        );
        // The bug this guards against: a decimal slot parsed as hex is a
        // plausible-looking number, and it silently un-bounds the top-up walk.
        assert_eq!(parse_quantity("26243147"), Some(26_243_147));
        assert_eq!(parse_quantity(""), None);
        assert_eq!(parse_quantity("0x"), None);
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
