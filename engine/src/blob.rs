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
    eip4844::{Blob, Bytes48},
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
    pub fn as_str(&self) -> &'static str {
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

    /// Fetch the block's blob sidecars. Sources are tried in order, twice
    /// around, with short backoff; an empty list is a miss (we only ask for
    /// blocks that contain blob txs) so the next source gets a chance.
    pub async fn blob_sidecars(&self, beacon_root: B256) -> Result<Vec<SidecarItem>> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..2u64 {
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
                            last_err =
                                Some(anyhow!("{url}: HTTP {status}: {}", truncate(&text, 200)));
                            continue;
                        }
                        match serde_json::from_str::<BeaconResponse<Vec<SidecarItem>>>(&text) {
                            Ok(parsed) if !parsed.data.is_empty() => return Ok(parsed.data),
                            Ok(_) => {
                                last_err = Some(anyhow!("{url}: empty sidecar list"));
                                continue;
                            }
                            Err(e) => {
                                last_err = Some(anyhow!("{url}: bad sidecar json: {e}"));
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        last_err = Some(anyhow!("{url}: {e}"));
                        continue;
                    }
                }
            }
            if attempt == 0 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("no beacon urls configured")))
    }
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
    fn versioned_hash_mapping() {
        // A known mainnet blob: commitment -> versioned hash mapping is
        // 0x01 || sha256(commitment)[1..]; just verify shape/consistency here.
        let commitment: Bytes48 = Bytes48::repeat_byte(7);
        let vh = commitment_to_versioned_hash(&commitment);
        assert_eq!(vh[0], 0x01);
        assert_ne!(vh, B256::ZERO);
    }
}
