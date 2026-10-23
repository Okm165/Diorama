//! The devnet attester (§15.3): the substituted off-chain authority, real
//! on-chain verification.
//!
//! Circle's attestation is an m-of-n threshold ECDSA multisig over
//! `keccak256(message)` (raw, no EIP-191 prefix), concatenated in increasing
//! order of the recovered attester address. This crate substitutes the
//! *authority*, never the *check*: a deterministic devnet key is installed
//! through the contracts' own setters — read `attesterManager()` live (it
//! differs per chain), impersonate it, `enableAttester(devnet)` then
//! `setSignatureThreshold(1)` — and the destination's real `Attestable` code
//! still verifies recovery, ordering, the enabled-attester check, and replay
//! (§15.3's proven path: a signed 376-byte message completed the full mint;
//! replay, tampered-signature and empty-attestation are real reverts).
//!
//! The key is deterministic from the configured seed (a 32-byte hex secret is
//! used verbatim; any other string is hashed) so restarts re-derive the same
//! attester and the boot installation stays idempotent.
//!
//! §15.3's storage-write fallback (if impersonation were ever blocked) is
//! deliberately not implemented: `--auto-impersonate` is a boot flag both
//! chains carry, and the real setters are the measured, working path.
use crate::chains::MESSAGE_TRANSMITTER_V2;
use crate::error::{Error, Result};
use crate::transfer::{
    eth_call, wait_for_receipt, Wallet, IMessageTransmitterV2,
};
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::SolCall;
use mirror_anvil::Rpc;
use std::time::Duration;

/// One gas stipend for an impersonated attester-manager tx (a devnet
/// convenience; the real managers carry their own gas).
const MANAGER_GAS_STIPEND: u64 = 1_000_000_000_000_000_000;

// TRUST: the devnet attester is the one substituted authority (Circle's
// off-chain signer). The guards: it is installed through the contracts' own
// setters (enableAttester/setSignatureThreshold, impersonating the live
// attesterManager), and every verification stays real — recovery, ordering,
// threshold, nonce replay, expiration, fee caps.

/// The devnet attester: a real secp256k1 key that signs served messages.
pub struct Attester {
    wallet: Wallet,
}

impl Attester {
    /// Derive the attester from a seed. A 32-byte hex seed (with or without
    /// `0x`) is the key itself; anything else is `keccak256(seed)`. Either
    /// way the key is reproducible across restarts.
    pub fn from_seed(seed: &str) -> Result<Self> {
        let trimmed = seed.trim();
        if trimmed.is_empty() {
            return Err(Error::InvalidKey {
                what: "attester_seed",
                detail: "empty".into(),
            });
        }
        let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        let secret = match alloy_primitives::hex::decode_to_array::<_, 32>(hex) {
            Ok(bytes) => bytes,
            Err(_) => alloy_primitives::keccak256(trimmed.as_bytes()).0,
        };
        Ok(Self {
            wallet: Wallet::from_secret(&alloy_primitives::hex::encode(secret))?,
        })
    }

    pub fn address(&self) -> Address {
        self.wallet.address()
    }

    /// The 66-byte uncompressed public key (`0x04…`), the Iris `publicKeys`
    /// encoding.
    pub fn public_key_hex(&self) -> String {
        format!("0x{}", alloy_primitives::hex::encode(self.wallet.public_key_uncompressed()))
    }

    /// Sign a served message: `keccak256(message)` under the devnet key,
    /// returned as the 65-byte `r ‖ s ‖ v` attestation.
    pub fn sign(&self, message: &[u8]) -> Result<Bytes> {
        let digest = alloy_primitives::keccak256(message);
        let signature = self.wallet.sign_digest(digest)?;
        Ok(Bytes::copy_from_slice(&signature.as_bytes()))
    }
}

/// What the boot installation found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The attester was already enabled and the threshold already 1.
    Already,
    /// The setters ran this boot.
    Installed,
}

/// Install the devnet attester on one chain through the real setters,
/// idempotently. Refuses loudly if the post-state does not verify: serving
/// attestations the destination would reject is worse than not serving.
pub async fn install(
    rpc: &Rpc,
    chain: &'static str,
    attester: Address,
    timeout: Duration,
) -> Result<InstallOutcome> {
    let manager = eth_call(
        rpc,
        MESSAGE_TRANSMITTER_V2,
        &IMessageTransmitterV2::attesterManagerCall {},
    )
    .await?;
    let enabled = eth_call(
        rpc,
        MESSAGE_TRANSMITTER_V2,
        &IMessageTransmitterV2::isEnabledAttesterCall { attester },
    )
    .await?;
    let threshold = eth_call(
        rpc,
        MESSAGE_TRANSMITTER_V2,
        &IMessageTransmitterV2::signatureThresholdCall {},
    )
    .await?;
    if enabled && threshold == U256::ONE {
        tracing::info!(
            chain,
            %attester,
            %manager,
            "devnet attester already installed (threshold 1)"
        );
        return Ok(InstallOutcome::Already);
    }

    // Impersonate the live attester manager: the same substitution §15.3
    // proved through the real setters. A manager with no gas on the fork gets
    // a devnet stipend (the pool refuses unfunded senders).
    let balance = rpc
        .call::<U256>("eth_getBalance", (manager, "latest"))
        .await?;
    if balance.is_zero() {
        tracing::warn!(chain, %manager, "attester manager has no gas on the fork; setting a devnet stipend");
        rpc.anvil_set_balance(manager, U256::from(MANAGER_GAS_STIPEND))
            .await?;
    }
    rpc.call_raw("anvil_impersonateAccount", (manager,)).await?;
    if !enabled {
        let input = IMessageTransmitterV2::enableAttesterCall { newAttester: attester }.abi_encode();
        send_impersonated(rpc, chain, manager, input, timeout).await?;
    }
    if threshold != U256::ONE {
        let input =
            IMessageTransmitterV2::setSignatureThresholdCall { newSignatureThreshold: U256::ONE }
                .abi_encode();
        send_impersonated(rpc, chain, manager, input, timeout).await?;
    }
    rpc.call_raw("anvil_stopImpersonatingAccount", (manager,))
        .await?;

    // Verify what the setters claim: a failed installation must not serve.
    let enabled = eth_call(
        rpc,
        MESSAGE_TRANSMITTER_V2,
        &IMessageTransmitterV2::isEnabledAttesterCall { attester },
    )
    .await?;
    let threshold = eth_call(
        rpc,
        MESSAGE_TRANSMITTER_V2,
        &IMessageTransmitterV2::signatureThresholdCall {},
    )
    .await?;
    if !enabled || threshold != U256::ONE {
        return Err(Error::AttesterInstall {
            chain,
            reason: format!(
                "post-state not verified: isEnabledAttester={enabled}, signatureThreshold={threshold}"
            ),
        });
    }
    tracing::info!(chain, %attester, %manager, "devnet attester installed (threshold 1)");
    Ok(InstallOutcome::Installed)
}

/// One impersonated `eth_sendTransaction`, waited to a mined receipt.
async fn send_impersonated(
    rpc: &Rpc,
    chain: &'static str,
    from: Address,
    input: Vec<u8>,
    timeout: Duration,
) -> Result<()> {
    let tx = TransactionRequest::default()
        .from(from)
        .to(MESSAGE_TRANSMITTER_V2)
        .input(input.into());
    let hash = rpc.send_transaction(tx).await?;
    match wait_for_receipt(rpc, hash, timeout).await? {
        Some(receipt) if receipt.status() => Ok(()),
        Some(_) => Err(Error::TxReverted { chain, tx: hash }),
        None => Err(Error::AttesterInstall {
            chain,
            reason: format!(
                "tx {hash} was not mined within {}s (the chain's block producer owns mining)",
                timeout.as_secs()
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Determinism: the same seed always derives the same key/address, and a
    /// hex seed is used verbatim.
    #[test]
    fn seed_derivation_is_deterministic() {
        let a = Attester::from_seed("mirror-cctp-devnet-attester").unwrap();
        let b = Attester::from_seed("mirror-cctp-devnet-attester").unwrap();
        assert_eq!(a.address(), b.address());
        assert_ne!(a.address(), Attester::from_seed("other").unwrap().address());

        let hex_seed = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
        let wallet = Wallet::from_secret(hex_seed).unwrap();
        assert_eq!(Attester::from_seed(hex_seed).unwrap().address(), wallet.address());
        assert!(Attester::from_seed("  ").is_err());
    }

    /// The attestation is the 65-byte signature over `keccak256(message)`
    /// (raw — no EIP-191 prefix), recovering the attester address.
    #[test]
    fn sign_is_keccak_recoverable_and_verifiable() {
        let attester = Attester::from_seed("mirror-cctp-devnet-attester").unwrap();
        let message = b"a served CCTP message";
        let attestation = attester.sign(message).unwrap();
        assert_eq!(attestation.len(), 65);
        let digest = alloy_primitives::keccak256(message);
        let signature = alloy_primitives::Signature::from_raw(&attestation).unwrap();
        assert_eq!(
            signature.recover_address_from_prehash(&digest).unwrap(),
            attester.address()
        );
        // The EIP-191 variant is a different digest: the raw form is the one
        // the contracts verify.
        assert_ne!(
            signature.recover_address_from_msg(message).unwrap(),
            attester.address()
        );
        // The public key hex is the 66-byte uncompressed encoding.
        let public = attester.public_key_hex();
        assert_eq!(public.len(), 2 + 130);
        assert!(public.starts_with("0x04"));
    }
}
