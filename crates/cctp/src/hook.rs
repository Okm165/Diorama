//! The CCTP V2 hook wrapper: Circle's official sample
//! (`src/examples/CCTPHookWrapper.sol`, Apache-2.0), deployed by this bridge
//! and owned by its relay key. It is the protocol-native way to carry a
//! credit with a transfer: the burn message carries
//! `hookData = target(20) ‖ calldata` and pins `destinationCaller` to the
//! wrapper, whose owner-gated `relay` calls `receiveMessage` (the mint) and
//! then the hook. **Non-atomic by design**: a failed hook cannot lose the
//! mint, and `(relaySuccess, hookSuccess, hookReturnData)` reports both.
//!
//! The bytecode is the official source compiled by the mirrors repo's
//! provenance step — commit `a92a2b4e` of `circlefin/evm-cctp-contracts`,
//! solc 0.7.6, optimizer 100 000 runs — and the artifact records the runtime
//! bytecode's sha256 so a rebuild is comparable byte for byte (the test below
//! pins it).
use crate::error::{Error, Result};
use crate::transfer::TxSender;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::{sol, SolCall};
use serde::Deserialize;

/// The compiled official wrapper (creation + runtime bytecode + provenance).
const ARTIFACT: &str = include_str!("../assets/CCTPHookWrapper.json");

sol! {
    /// The official wrapper's surface: the owner-gated `relay` plus the two
    /// views the boot verification reads back.
    interface ICCTPHookWrapper {
        function relay(bytes message, bytes attestation)
            external
            returns (bool relaySuccess, bool hookSuccess, bytes hookReturnData);
        function owner() external view returns (address);
        function messageTransmitter() external view returns (address);
    }
}

#[derive(Deserialize)]
struct Artifact {
    creation_bytecode: String,
    /// The provenance test's data (deploy time only needs the creation code;
    /// the runtime is the artifact's identity). Test-only: the artifact JSON's
    /// extra keys are ignored in production builds.
    #[cfg(test)]
    runtime_bytecode: String,
    #[cfg(test)]
    runtime_sha256: String,
}

fn artifact() -> Result<Artifact> {
    serde_json::from_str(ARTIFACT).map_err(|source| Error::Corrupt {
        what: "the CCTPHookWrapper artifact",
        path: std::path::PathBuf::from("crates/cctp/assets/CCTPHookWrapper.json"),
        source,
    })
}

fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    // const-hex strips an optional `0x`/`0X` prefix itself; the wrapper stays
    // for its named error mapping.
    alloy_primitives::hex::decode(hex).map_err(|source| Error::Hex {
        what: "the CCTPHookWrapper artifact bytecode",
        source,
    })
}

/// The deployment code: the creation bytecode ‖ the ABI-encoded constructor
/// argument (the local `MessageTransmitterV2` — the wrapper calls it for
/// `receiveMessage`; the deployer becomes the owner through `Ownable2Step`).
pub(crate) fn creation_code(message_transmitter: Address) -> Result<Bytes> {
    let artifact = artifact()?;
    let mut code = decode_hex(&artifact.creation_bytecode)?;
    code.extend_from_slice(message_transmitter.into_word().as_slice());
    Ok(Bytes::from(code))
}

/// `hookData` in the wrapper's own format: `target(20) ‖ calldata`.
pub(crate) fn hook_data(target: Address, calldata: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(20 + calldata.len());
    out.extend_from_slice(target.as_slice());
    out.extend_from_slice(calldata);
    Bytes::from(out)
}

/// `relay(message, attestation)` calldata — the wrapper calls
/// `receiveMessage` itself, so a hook message is never submitted to the
/// transmitter directly (its `destinationCaller` pins consumption here).
pub(crate) fn relay_calldata(message: &[u8], attestation: &[u8]) -> Bytes {
    ICCTPHookWrapper::relayCall {
        message: Bytes::copy_from_slice(message),
        attestation: Bytes::copy_from_slice(attestation),
    }
    .abi_encode()
    .into()
}

/// Deploy the wrapper on `chain` with `owner = sender`; returns the tx hash.
/// The caller waits for the receipt and records `contractAddress` (the bin's
/// boot step, [`verify`] then confirms the wiring).
pub async fn deploy(
    rpc: &diorama_anvil::Rpc,
    sender: &TxSender,
    message_transmitter: Address,
) -> Result<B256> {
    let code = creation_code(message_transmitter)?;
    sender.deploy(rpc, &code, U256::ZERO).await
}

/// Read back the deployed wrapper's wiring: owner and transmitter must be
/// exactly what the boot intended (a mismatch is a named fault, never a
/// silent substitution).
pub async fn verify(
    rpc: &diorama_anvil::Rpc,
    wrapper: Address,
    expected_owner: Address,
    expected_transmitter: Address,
) -> Result<()> {
    let owner = rpc.call_decode(wrapper, &ICCTPHookWrapper::ownerCall {}).await?;
    let transmitter = rpc
        .call_decode(wrapper, &ICCTPHookWrapper::messageTransmitterCall {})
        .await?;
    if owner != expected_owner || transmitter != expected_transmitter {
        return Err(Error::bad_request(format!(
                "hook wrapper {wrapper} wiring mismatch: owner {owner} (want {expected_owner}), transmitter {transmitter} (want {expected_transmitter})"
            ),));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_is_the_pinned_official_build() {
        let artifact = artifact().unwrap();
        let runtime = decode_hex(&artifact.runtime_bytecode).unwrap();
        let creation = decode_hex(&artifact.creation_bytecode).unwrap();
        assert!(runtime.len() > 1000, "real bytecode, not a stub");
        assert!(creation.len() > runtime.len(), "creation wraps the runtime");
        let digest = {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(&runtime);
            format!("{:x}", hasher.finalize())
        };
        assert_eq!(
            digest, artifact.runtime_sha256,
            "the embedded bytecode matches its recorded provenance hash"
        );
    }

    #[test]
    fn hook_data_is_target_then_calldata() {
        let target = Address::repeat_byte(0xab);
        let data = hook_data(target, &[1, 2, 3]);
        assert_eq!(&data[..20], target.as_slice());
        assert_eq!(&data[20..], &[1, 2, 3]);
        // The wrapper's own rule: a hook shorter than an address is ignored
        // (no call), so `hook_data` always emits at least the target.
        assert_eq!(hook_data(target, &[]).len(), 20);
    }

    #[test]
    fn creation_code_appends_the_transmitter_word() {
        let transmitter = Address::repeat_byte(0x11);
        let code = creation_code(transmitter).unwrap();
        let tail = &code[code.len() - 32..];
        assert_eq!(tail, transmitter.into_word().as_slice());
    }
}
