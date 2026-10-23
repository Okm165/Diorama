//! CCTP V2 message types, the `depositForBurn` builder, and the transaction
//! plumbing both the relayer and the control API submit through.
//!
//! Every layout and signature here is the contract's own, re-verified against
//! `circlefin/evm-cctp-contracts` and the measured forks (§15.1/§15.2):
//!
//! - **Header, 148 B** (`MessageV2`): version 0 · sourceDomain 4 ·
//!   destinationDomain 8 · nonce 12 · sender 44 · recipient 76 ·
//!   destinationCaller 108 · minFinalityThreshold 140 ·
//!   finalityThresholdExecuted 144 · body 148.
//! - **BurnMessageV2 body** (`BurnMessageV2`): version 0 · burnToken 4 ·
//!   mintRecipient 36 · amount 68 · messageSender 100 · maxFee 132 ·
//!   feeExecuted 164 · expirationBlock 196 · hookData 228.
//! - `depositForBurn(uint256,uint32,bytes32,address,bytes32,uint256,uint32)`;
//!   `receiveMessage(bytes,bytes)`.
//!
//! The emitted `MessageSent` payload carries `EMPTY_NONCE` and
//! `EMPTY_FINALITY_THRESHOLD_EXECUTED` (both 0) plus zero `feeExecuted` and
//! `expirationBlock` — it is **not submittable** (the zero-nonce pre-block).
//! The attester fills all four fields on the *served* message (§15.1/§15.3);
//! [`Message::with_attester_fields`] is that substitution, and the signature
//! covers the served bytes.
use crate::chains::{Domain, MIN_FEE_MULTIPLIER};
use crate::error::{Error, Result};
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256, U64};
use alloy_rpc_types_eth::{Transaction, TransactionReceipt, TransactionRequest};
use alloy_sol_types::{sol, SolCall};
use k256::ecdsa::signature::hazmat::PrehashSigner;
use k256::ecdsa::{RecoveryId, SigningKey};
use mirror_anvil::Rpc;
use serde::{Deserialize, Serialize};
use std::time::Duration;

sol! {
    /// The TokenMessengerV2 surface (§15.1). `minFee`/`getMinFeeAmount` are
    /// the revision-dependent fee surface: absent on Ethereum's instance,
    /// present (`0`) on HyperEVM's — both measured, §15.2 — so the builder
    /// capability-probes them instead of assuming either.
    #[derive(Debug)]
    interface ITokenMessengerV2 {
        function depositForBurn(
            uint256 amount,
            uint32 destinationDomain,
            bytes32 mintRecipient,
            address burnToken,
            bytes32 destinationCaller,
            uint256 maxFee,
            uint32 minFinalityThreshold
        ) external;

        /// §15.1's hook variant: the contract reverts on empty `hookData`.
        function depositForBurnWithHook(
            uint256 amount,
            uint32 destinationDomain,
            bytes32 mintRecipient,
            address burnToken,
            bytes32 destinationCaller,
            uint256 maxFee,
            uint32 minFinalityThreshold,
            bytes hookData
        ) external;

        function minFee() external view returns (uint256);
        function getMinFeeAmount(uint256 amount) external view returns (uint256);
        function localMessageTransmitter() external view returns (address);
        /// The local minter this messenger burns/mints through. Measured
        /// 2026-10-23: the getter is `localMinter()`; `localTokenMinter()`
        /// reverts (the const's comment had guessed the wrong name).
        function localMinter() external view returns (address);
        function messageBodyVersion() external view returns (uint32);
        function remoteTokenMessengers(uint32 domain) external view returns (bytes32);
    }

    /// The MessageTransmitterV2 surface: `receiveMessage` is the real
    /// `Attestable`-verified entry point; the getters are the boot wiring
    /// asserts and the attester installation (§15.3).
    #[derive(Debug)]
    interface IMessageTransmitterV2 {
        function receiveMessage(bytes message, bytes attestation) external returns (bool);
        function localDomain() external view returns (uint32);
        function version() external view returns (uint32);
        function attesterManager() external view returns (address);
        function signatureThreshold() external view returns (uint256);
        function getNumEnabledAttesters() external view returns (uint256);
        function isEnabledAttester(address attester) external view returns (bool);
        function usedNonces(bytes32 nonce) external view returns (uint256);
        function enableAttester(address newAttester) external;
        function setSignatureThreshold(uint256 newSignatureThreshold) external;

        event MessageSent(bytes message);
    }

    /// The ERC-20 surface for balance/allowance checks and approvals. The
    /// approve spender is TokenMessengerV2 itself: `_depositAndBurn` calls
    /// `transferFrom(_from, minter, amount)` from the messenger's own address
    /// (verified in `BaseTokenMessenger.sol`), so the allowance lives there.
    #[derive(Debug)]
    interface IERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
    }
}

/// The V2 message header length (`MessageV2.MESSAGE_BODY_INDEX`).
pub(crate) const HEADER_LEN: usize = 148;
/// The fixed BurnMessageV2 body length up to (excluding) `hookData`.
pub(crate) const BURN_BODY_LEN: usize = 228;

// The contract's index table, one constant per field (MessageV2.sol /
// BurnMessageV2.sol; the offsets §15.1 records). Body offsets are relative to
// the start of the body (absolute offset = [`HEADER_LEN`] + these).
pub(crate) const HEADER_VERSION_AT: usize = 0;
pub(crate) const HEADER_SOURCE_DOMAIN_AT: usize = 4;
pub(crate) const HEADER_DESTINATION_DOMAIN_AT: usize = 8;
pub(crate) const HEADER_NONCE_AT: usize = 12;
pub(crate) const HEADER_SENDER_AT: usize = 44;
pub(crate) const HEADER_RECIPIENT_AT: usize = 76;
pub(crate) const HEADER_DESTINATION_CALLER_AT: usize = 108;
pub(crate) const HEADER_MIN_FINALITY_THRESHOLD_AT: usize = 140;
pub(crate) const HEADER_FINALITY_THRESHOLD_EXECUTED_AT: usize = 144;

pub(crate) const BODY_VERSION_AT: usize = 0;
pub(crate) const BODY_BURN_TOKEN_AT: usize = 4;
pub(crate) const BODY_MINT_RECIPIENT_AT: usize = 36;
pub(crate) const BODY_AMOUNT_AT: usize = 68;
pub(crate) const BODY_MESSAGE_SENDER_AT: usize = 100;
pub(crate) const BODY_MAX_FEE_AT: usize = 132;
pub(crate) const BODY_FEE_EXECUTED_AT: usize = 164;
pub(crate) const BODY_EXPIRATION_BLOCK_AT: usize = 196;
pub(crate) const BODY_HOOK_DATA_AT: usize = 228;

/// The `MessageV2` header as typed fields (offsets above).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MessageHeader {
    pub version: u32,
    pub source_domain: Domain,
    pub destination_domain: Domain,
    pub nonce: B256,
    pub sender: B256,
    pub recipient: B256,
    pub destination_caller: B256,
    pub min_finality_threshold: u32,
    pub finality_threshold_executed: u32,
}

/// The `BurnMessageV2` body as typed fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BurnMessageBody {
    pub version: u32,
    pub burn_token: B256,
    pub mint_recipient: B256,
    pub amount: U256,
    pub message_sender: B256,
    pub max_fee: U256,
    pub fee_executed: U256,
    pub expiration_block: U256,
    pub hook_data: Bytes,
}

/// A CCTP V2 burn message, header + body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub(crate) header: MessageHeader,
    pub(crate) body: BurnMessageBody,
}

impl Message {
    /// Decode a burn message. Anything shorter than the fixed part is
    /// [`Error::MessageTooShort`]: the contracts' own `_validateMessageFormat`
    /// / `_validateBurnMessageFormat` guards, applied before any field read.
    pub fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() < HEADER_LEN + BURN_BODY_LEN {
            return Err(Error::MessageTooShort {
                len: raw.len(),
                want: HEADER_LEN + BURN_BODY_LEN,
            });
        }
        let body = &raw[HEADER_LEN..];
        Ok(Self {
            header: MessageHeader {
                version: u32_at(raw, HEADER_VERSION_AT),
                source_domain: Domain(u32_at(raw, HEADER_SOURCE_DOMAIN_AT)),
                destination_domain: Domain(u32_at(raw, HEADER_DESTINATION_DOMAIN_AT)),
                nonce: word_at(raw, HEADER_NONCE_AT),
                sender: word_at(raw, HEADER_SENDER_AT),
                recipient: word_at(raw, HEADER_RECIPIENT_AT),
                destination_caller: word_at(raw, HEADER_DESTINATION_CALLER_AT),
                min_finality_threshold: u32_at(raw, HEADER_MIN_FINALITY_THRESHOLD_AT),
                finality_threshold_executed: u32_at(raw, HEADER_FINALITY_THRESHOLD_EXECUTED_AT),
            },
            body: BurnMessageBody {
                version: u32_at(body, BODY_VERSION_AT),
                burn_token: word_at(body, BODY_BURN_TOKEN_AT),
                mint_recipient: word_at(body, BODY_MINT_RECIPIENT_AT),
                amount: U256::from_be_bytes(word_at(body, BODY_AMOUNT_AT).0),
                message_sender: word_at(body, BODY_MESSAGE_SENDER_AT),
                max_fee: U256::from_be_bytes(word_at(body, BODY_MAX_FEE_AT).0),
                fee_executed: U256::from_be_bytes(word_at(body, BODY_FEE_EXECUTED_AT).0),
                expiration_block: U256::from_be_bytes(word_at(body, BODY_EXPIRATION_BLOCK_AT).0),
                hook_data: Bytes::copy_from_slice(&body[BODY_HOOK_DATA_AT..]),
            },
        })
    }

    /// `abi.encodePacked` of the same fields (the contracts' formatting).
    pub fn encode(&self) -> Bytes {
        let mut out = Vec::with_capacity(HEADER_LEN + BURN_BODY_LEN + self.body.hook_data.len());
        let h = &self.header;
        out.extend_from_slice(&h.version.to_be_bytes());
        out.extend_from_slice(&h.source_domain.0.to_be_bytes());
        out.extend_from_slice(&h.destination_domain.0.to_be_bytes());
        out.extend_from_slice(h.nonce.as_slice());
        out.extend_from_slice(h.sender.as_slice());
        out.extend_from_slice(h.recipient.as_slice());
        out.extend_from_slice(h.destination_caller.as_slice());
        out.extend_from_slice(&h.min_finality_threshold.to_be_bytes());
        out.extend_from_slice(&h.finality_threshold_executed.to_be_bytes());
        let b = &self.body;
        out.extend_from_slice(&b.version.to_be_bytes());
        out.extend_from_slice(b.burn_token.as_slice());
        out.extend_from_slice(b.mint_recipient.as_slice());
        out.extend_from_slice(&b.amount.to_be_bytes::<32>());
        out.extend_from_slice(b.message_sender.as_slice());
        out.extend_from_slice(&b.max_fee.to_be_bytes::<32>());
        out.extend_from_slice(&b.fee_executed.to_be_bytes::<32>());
        out.extend_from_slice(&b.expiration_block.to_be_bytes::<32>());
        out.extend_from_slice(&b.hook_data);
        Bytes::from(out)
    }

    /// `keccak256(message)` — the digest the attestation signs (raw, no
    /// EIP-191 prefix; §15.1).
    pub fn keccak(&self) -> B256 {
        keccak256(self.encode())
    }

    /// The attester's substitution: fill the four fields the emitted message
    /// leaves empty (nonce, `finalityThresholdExecuted`, `feeExecuted`,
    /// `expirationBlock`) — the served artifact the signature covers (§15.1).
    pub fn with_attester_fields(
        &self,
        nonce: B256,
        finality_threshold_executed: u32,
        fee_executed: U256,
        expiration_block: U256,
    ) -> Self {
        let mut out = self.clone();
        out.header.nonce = nonce;
        out.header.finality_threshold_executed = finality_threshold_executed;
        out.body.fee_executed = fee_executed;
        out.body.expiration_block = expiration_block;
        out
    }
}

fn u32_at(raw: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(raw[at..at + 4].try_into().expect("length checked"))
}

fn word_at(raw: &[u8], at: usize) -> B256 {
    B256::from_slice(&raw[at..at + 32])
}

/// The measured Iris fee table (§16.2): `0→19` fast burns cost 1 unit at
/// threshold 1000, standard is free; `19→0` is free on both thresholds
/// (HyperEVM has no fast lane — its standard attestation is already fast).
/// `None` = the pair is not served (the Iris 404 arm).
pub(crate) fn measured_fees(source: u32, destination: u32) -> Option<[(u32, u64); 2]> {
    match (source, destination) {
        (0, 19) => Some([(1000, 1), (2000, 0)]),
        (19, 0) => Some([(1000, 0), (2000, 0)]),
        _ => None,
    }
}

// ---- the source-side builder ---------------------------------------------

/// `receiveMessage(message, attestation)` calldata.
pub(crate) fn receive_message_calldata(message: &Bytes, attestation: &Bytes) -> Bytes {
    IMessageTransmitterV2::receiveMessageCall {
        message: message.clone(),
        attestation: attestation.clone(),
    }
    .abi_encode()
    .into()
}

/// `eth_call` a typed view function and decode its answer. A node refusal
/// stays a typed RPC fault (the capability probe matches on it); a decoded
/// answer that does not parse is [`Error::Abi`], never a guessed value.
/// Whether `address` carries code on its chain. A re-pin removes a devnet
/// deployment (the wrapper, the attester install): the address is left empty,
/// and a call to it *succeeds vacuously* — so both the burn path and the
/// relay path must check this rather than trust a status (measured
/// 2026-10-23: a stale wrapper made a hook relay land as a no-op).
pub(crate) async fn has_code(rpc: &Rpc, address: Address) -> Result<bool> {
    let code: Bytes = rpc.call("eth_getCode", (address, "latest")).await?;
    Ok(!code.is_empty())
}

pub async fn eth_call<C: SolCall>(rpc: &Rpc, to: Address, call: &C) -> Result<C::Return> {
    let ret = rpc.call_latest(to, &Bytes::from(call.abi_encode())).await?;
    C::abi_decode_returns(&ret).map_err(|source| Error::Abi {
        what: C::SIGNATURE,
        source,
    })
}

/// The revision-dependent min-fee surface, capability-probed at boot (§15.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FeeSurface {
    pub min_fee: Option<U256>,
}

impl FeeSurface {
    /// Probe `minFee()`. A JSON-RPC error means the node answered "no such
    /// method" (the Ethereum instance's older revision reverts) — that is the
    /// `None` arm. A transport or decode fault is **not** absence
    /// (boundary-discipline): it propagates, because a probe that could not
    /// run must not be read as "the surface is absent".
    pub async fn probe(rpc: &Rpc, token_messenger: Address) -> Result<Self> {
        match eth_call(rpc, token_messenger, &ITokenMessengerV2::minFeeCall {}).await {
            Ok(min_fee) => Ok(Self {
                min_fee: Some(min_fee),
            }),
            Err(Error::Rpc(_)) => Ok(Self { min_fee: None }),
            Err(e) => Err(e),
        }
    }

    /// The source-side validation `_depositForBurn` performs: `0 < maxFee <
    /// amount` always; when `minFee > 0`, `maxFee >= max(1,
    /// amount·minFee/10_000_000)`. Refusing here names the numbers instead of
    /// paying for a revert.
    pub fn validate_max_fee(&self, amount: U256, max_fee: U256) -> Result<()> {
        if max_fee >= amount {
            return Err(Error::BadRequest {
                reason: format!("maxFee {max_fee} must be less than amount {amount}"),
            });
        }
        let Some(min_fee) = self.min_fee else {
            return Ok(());
        };
        if min_fee.is_zero() {
            return Ok(());
        }
        let required = required_min_fee(amount, min_fee)?;
        if max_fee < required {
            return Err(Error::BadRequest {
                reason: format!(
                    "maxFee {max_fee} is below the contract's minimum fee {required} \
                     (minFee {min_fee}, amount {amount})"
                ),
            });
        }
        Ok(())
    }
}

/// The contract's `_calcMinFeeAmount`: `max(1, amount·minFee/MIN_FEE_MULTIPLIER)`,
/// defined for `amount > 1` (its `getMinFeeAmount` guard).
pub(crate) fn required_min_fee(amount: U256, min_fee: U256) -> Result<U256> {
    if amount <= U256::ONE {
        return Err(Error::BadRequest {
            reason: format!("amount {amount} must be greater than 1 when minFee {min_fee} is nonzero"),
        });
    }
    let fee = amount.saturating_mul(min_fee) / U256::from(MIN_FEE_MULTIPLIER);
    Ok(if fee.is_zero() { U256::ONE } else { fee })
}

// ---- signing and transaction submission -----------------------------------

/// A secp256k1 key with its address (the relayer / control-transfer wallet).
/// The secret never leaves this struct; only the address is ever logged (and
/// the manual `Debug` prints nothing else).
pub struct Wallet {
    key: SigningKey,
    address: Address,
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Wallet {
    /// Parse a 32-byte hex secret (with or without `0x`). A key that does not
    /// parse is a named boot refusal, never a panic.
    pub fn from_secret(secret: &str) -> Result<Self> {
        let trimmed = secret.trim();
        let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        let bytes = alloy_primitives::hex::decode_to_array::<_, 32>(hex).map_err(|source| {
            Error::InvalidKey {
                what: "relay_key",
                detail: source.to_string(),
            }
        })?;
        let key = SigningKey::from_slice(&bytes).map_err(|source| Error::InvalidKey {
            what: "relay_key",
            detail: source.to_string(),
        })?;
        let address = Address::from_public_key(key.verifying_key());
        Ok(Self { key, address })
    }

    pub fn address(&self) -> Address {
        self.address
    }

    /// The 65-byte uncompressed public key (`0x04 ‖ X ‖ Y`), the encoding the
    /// Iris `publicKeys` endpoint serves.
    pub fn public_key_uncompressed(&self) -> [u8; 65] {
        let point = self.key.verifying_key().to_encoded_point(false);
        let mut out = [0u8; 65];
        out.copy_from_slice(point.as_bytes());
        out
    }

    /// Sign a 32-byte digest with RFC6979 + low-s (k256), returning alloy's
    /// `Signature`: 65 bytes `r ‖ s ‖ v(27|28)` — byte-compatible with the
    /// `ECDSA.recover` the on-chain `Attestable` runs (§15.3's proven path).
    pub fn sign_digest(&self, digest: B256) -> Result<alloy_primitives::Signature> {
        let signature: k256::ecdsa::Signature =
            self.key
                .sign_prehash(digest.as_slice())
                .map_err(|source| Error::Sign {
                    what: "digest",
                    detail: source.to_string(),
                })?;
        let recovery = RecoveryId::trial_recovery_from_prehash(
            self.key.verifying_key(),
            digest.as_slice(),
            &signature,
        )
        .map_err(|source| Error::Sign {
            what: "digest",
            detail: source.to_string(),
        })?;
        Ok(alloy_primitives::Signature::from((signature, recovery)))
    }
}

const NO_PARAMS: [u8; 0] = [];
const RECEIPT_POLL: Duration = Duration::from_millis(500);
const GAS_MARGIN_NUM: u64 = 12;
const GAS_MARGIN_DEN: u64 = 10;

/// Builds, signs (EIP-1559) and submits calls. The devnet's block production
/// belongs to the deployment (T is mined by the engine's applier; V by the
/// venue deployment): this sender never mines, it waits.
///
/// One submission at a time ([`TxSender::send`] holds a lock): the relayer
/// and the control API share this sender, and two interleaved `pending`-nonce
/// reads would hand both the same nonce.
pub struct TxSender {
    wallet: Wallet,
    send_lock: tokio::sync::Mutex<()>,
}

impl TxSender {
    pub fn new(wallet: Wallet) -> Self {
        Self {
            wallet,
            send_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn address(&self) -> Address {
        self.wallet.address()
    }

    /// Submit a signed call. Nonce from `pending` (serialized against the
    /// other submitter); gas estimated with a margin; fees from
    /// `eth_gasPrice`.
    pub async fn send(&self, rpc: &Rpc, to: Address, input: &[u8], value: U256) -> Result<B256> {
        self.submit(rpc, TxKind::Call(to), input, value).await
    }

    /// Submit a signed contract creation (the hook wrapper, §15.4). Same
    /// discipline as [`TxSender::send`]: nonce from `pending`, estimated gas,
    /// one submission at a time.
    pub async fn deploy(&self, rpc: &Rpc, code: &[u8], value: U256) -> Result<B256> {
        self.submit(rpc, TxKind::Create, code, value).await
    }

    async fn submit(&self, rpc: &Rpc, to: TxKind, input: &[u8], value: U256) -> Result<B256> {
        let _submitting = self.send_lock.lock().await;
        let chain_id = rpc.chain_id().await?;
        let nonce: U64 = rpc
            .call("eth_getTransactionCount", (self.wallet.address(), "pending"))
            .await?;
        let gas_limit = self.estimate_gas(rpc, to, input, value).await?;
        let gas_price: U256 = rpc.call("eth_gasPrice", NO_PARAMS).await?;
        let tx = TxEip1559 {
            chain_id,
            nonce: nonce.to::<u64>(),
            gas_limit,
            max_fee_per_gas: gas_price.saturating_mul(U256::from(2)).to::<u128>(),
            max_priority_fee_per_gas: gas_price.min(U256::from(1_000_000_000u64)).to::<u128>(),
            to,
            value,
            input: Bytes::copy_from_slice(input),
            access_list: Default::default(),
        };
        let signature = self.wallet.sign_digest(tx.signature_hash())?;
        let raw = tx.into_signed(signature).encoded_2718();
        let hash = rpc
            .call("eth_sendRawTransaction", (Bytes::from(raw),))
            .await?;
        Ok(hash)
    }

    async fn estimate_gas(&self, rpc: &Rpc, to: TxKind, input: &[u8], value: U256) -> Result<u64> {
        let mut request = TransactionRequest::default()
            .from(self.wallet.address())
            .input(input.to_vec().into())
            .value(value);
        if let TxKind::Call(to) = to {
            request = request.to(to);
        }
        let estimate: U64 = rpc.call("eth_estimateGas", (request,)).await?;
        let gas = estimate.to::<u64>();
        Ok(gas.saturating_mul(GAS_MARGIN_NUM) / GAS_MARGIN_DEN + 21_000)
    }

    /// Whether the tx is still in the pool. `false` + no receipt = the tx was
    /// dropped (an anvil restart loses its pool): the caller may re-send.
    pub async fn in_pool(&self, rpc: &Rpc, tx: B256) -> Result<bool> {
        let found: Option<Transaction> = rpc.call("eth_getTransactionByHash", (tx,)).await?;
        Ok(found.is_some())
    }
}

/// One receipt read (`null` while the tx is pending).
pub async fn receipt(rpc: &Rpc, tx: B256) -> Result<Option<TransactionReceipt>> {
    Ok(rpc.call("eth_getTransactionReceipt", (tx,)).await?)
}

/// Wait for a receipt up to `timeout`. `None` = not mined yet (the caller
/// re-checks next tick; it never re-sends blindly).
pub async fn wait_for_receipt(
    rpc: &Rpc,
    tx: B256,
    timeout: Duration,
) -> Result<Option<TransactionReceipt>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(receipt) = receipt(rpc, tx).await? {
            return Ok(Some(receipt));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(RECEIPT_POLL).await;
    }
}

/// Test fixtures shared by this crate's unit tests (a raw 376-byte burn
/// message shaped exactly like the measured `MessageSent` payloads, §15.3).
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use crate::chains::{USDC_ETHEREUM, USDC_HYPER_EVM};

    /// A raw 0 → 19 message with the emitted (zeroed) attester fields.
    pub fn sample_raw_message() -> Bytes {
        let message = Message {
            header: MessageHeader {
                version: 1,
                source_domain: Domain(0),
                destination_domain: Domain(19),
                nonce: B256::ZERO,
                sender: Address::repeat_byte(0x11).into_word(),
                recipient: crate::chains::TOKEN_MESSENGER_V2.into_word(),
                destination_caller: B256::ZERO,
                min_finality_threshold: 2000,
                finality_threshold_executed: 0,
            },
            body: BurnMessageBody {
                version: 1,
                burn_token: USDC_ETHEREUM.into_word(),
                mint_recipient: Address::repeat_byte(0x22).into_word(),
                amount: U256::from(100_000_000u64),
                message_sender: Address::repeat_byte(0x11).into_word(),
                max_fee: U256::ZERO,
                fee_executed: U256::ZERO,
                expiration_block: U256::ZERO,
                hook_data: Bytes::new(),
            },
        };
        message.encode()
    }

    /// A 19 → 0 message (the reverse direction, §15.3).
    pub fn sample_reverse_message() -> Bytes {
        let mut message = Message::decode(&sample_raw_message()).unwrap();
        message.header.source_domain = Domain(19);
        message.header.destination_domain = Domain(0);
        message.body.burn_token = USDC_HYPER_EVM.into_word();
        message.encode()
    }

    /// A deterministic wallet for signing tests.
    pub fn sample_wallet() -> Wallet {
        Wallet::from_secret("0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d")
            .expect("fixture key parses")
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{sample_raw_message, sample_reverse_message, sample_wallet};
    use super::*;
    use crate::chains::{USDC_ETHEREUM, USDC_HYPER_EVM};
    use alloy_sol_types::SolEvent;

    /// The selectors/topic measured with `cast sig` / `cast sig-event` against
    /// the exact signatures §15.1 pins — an independent check that the `sol!`
    /// bindings encode what the contracts expose.
    #[test]
    fn selectors_match_the_measured_contract_signatures() {
        assert_eq!(
            ITokenMessengerV2::depositForBurnCall::SELECTOR,
            [0x8e, 0x02, 0x50, 0xee]
        );
        assert_eq!(
            IMessageTransmitterV2::receiveMessageCall::SELECTOR,
            [0x57, 0xec, 0xfd, 0x28]
        );
        assert_eq!(ITokenMessengerV2::minFeeCall::SELECTOR, [0x24, 0xec, 0x75, 0x90]);
        assert_eq!(
            ITokenMessengerV2::getMinFeeAmountCall::SELECTOR,
            [0x51, 0x69, 0x90, 0xe3]
        );
        assert_eq!(
            IMessageTransmitterV2::attesterManagerCall::SELECTOR,
            [0x9b, 0x0d, 0x94, 0xb7]
        );
        assert_eq!(
            IMessageTransmitterV2::signatureThresholdCall::SELECTOR,
            [0xa8, 0x2f, 0x2e, 0x26]
        );
        assert_eq!(
            IMessageTransmitterV2::isEnabledAttesterCall::SELECTOR,
            [0x7a, 0xf8, 0x2f, 0x60]
        );
        assert_eq!(
            IMessageTransmitterV2::enableAttesterCall::SELECTOR,
            [0xfa, 0xe3, 0x68, 0x79]
        );
        assert_eq!(
            IMessageTransmitterV2::setSignatureThresholdCall::SELECTOR,
            [0xbb, 0xde, 0x53, 0x74]
        );
        assert_eq!(
            IMessageTransmitterV2::MessageSent::SIGNATURE_HASH,
            "0x8c5261668696ce22758910d05bab8f186d6eb247ceac2af2e82c7dc17669b036"
                .parse::<B256>()
                .unwrap()
        );
    }

    /// The header and body fields land on the contract's own indices (§15.1).
    #[test]
    fn encode_decode_round_trip_lands_on_the_contracts_index_table() {
        let raw = sample_raw_message();
        assert_eq!(raw.len(), HEADER_LEN + BURN_BODY_LEN, "376 bytes, measured");
        let message = Message::decode(&raw).expect("sample decodes");
        assert_eq!(message.encode(), raw, "encode ∘ decode = identity");
        assert_eq!(message.header.version, 1);
        assert_eq!(message.header.source_domain, Domain(0));
        assert_eq!(message.header.destination_domain, Domain(19));
        assert_eq!(message.header.min_finality_threshold, 2000);
        assert_eq!(message.header.finality_threshold_executed, 0);
        assert_eq!(message.body.burn_token, USDC_ETHEREUM.into_word());
        assert_eq!(message.body.amount, U256::from(100_000_000u64));
        assert_eq!(message.body.hook_data.len(), 0);
        // The raw offsets, byte for byte.
        assert_eq!(&raw[HEADER_VERSION_AT..HEADER_VERSION_AT + 4], &[0, 0, 0, 1]);
        assert_eq!(&raw[HEADER_SOURCE_DOMAIN_AT..HEADER_SOURCE_DOMAIN_AT + 4], &[0, 0, 0, 0]);
        assert_eq!(&raw[HEADER_DESTINATION_DOMAIN_AT..HEADER_DESTINATION_DOMAIN_AT + 4], &[0, 0, 0, 19]);
        assert_eq!(word_at(&raw, HEADER_NONCE_AT), B256::ZERO);
        assert_eq!(word_at(&raw, HEADER_SENDER_AT), Address::repeat_byte(0x11).into_word());
        assert_eq!(word_at(&raw, HEADER_DESTINATION_CALLER_AT), B256::ZERO);
        assert_eq!(&raw[HEADER_MIN_FINALITY_THRESHOLD_AT..HEADER_MIN_FINALITY_THRESHOLD_AT + 4], &[0, 0, 0x07, 0xd0]);
        assert_eq!(word_at(&raw[HEADER_LEN..], BODY_BURN_TOKEN_AT), USDC_ETHEREUM.into_word());
        assert_eq!(word_at(&raw[HEADER_LEN..], BODY_MINT_RECIPIENT_AT), Address::repeat_byte(0x22).into_word());
        assert_eq!(word_at(&raw[HEADER_LEN..], BODY_MESSAGE_SENDER_AT), Address::repeat_byte(0x11).into_word());
        assert_eq!(word_at(&raw[HEADER_LEN..], BODY_FEE_EXECUTED_AT), B256::ZERO);
        assert_eq!(word_at(&raw[HEADER_LEN..], BODY_EXPIRATION_BLOCK_AT), B256::ZERO);
    }

    /// The reverse direction's body carries the HyperEVM burn token (§15.3).
    #[test]
    fn reverse_message_carries_the_hyperevm_token() {
        let raw = sample_reverse_message();
        let message = Message::decode(&raw).unwrap();
        assert_eq!(message.header.source_domain, Domain(19));
        assert_eq!(message.header.destination_domain, Domain(0));
        assert_eq!(message.body.burn_token, USDC_HYPER_EVM.into_word());
    }

    /// Patching fills exactly the four attester fields; every other byte is
    /// untouched (the digest changes only through them).
    #[test]
    fn attester_fields_are_the_only_patched_bytes() {
        let raw = sample_raw_message();
        let parsed = Message::decode(&raw).unwrap();
        let nonce = B256::repeat_byte(0xab);
        let served = parsed.with_attester_fields(nonce, 2000, U256::ZERO, U256::from(123u64));
        assert_ne!(served.encode(), raw);
        let bytes = served.encode();
        assert_eq!(word_at(&bytes, HEADER_NONCE_AT), nonce);
        assert_eq!(&bytes[HEADER_FINALITY_THRESHOLD_EXECUTED_AT..HEADER_FINALITY_THRESHOLD_EXECUTED_AT + 4], &[0, 0, 0x07, 0xd0]);
        assert_eq!(
            word_at(&bytes[HEADER_LEN..], BODY_EXPIRATION_BLOCK_AT),
            B256::from(U256::from(123u64))
        );
        // Re-decoding the served message reproduces the patched fields.
        let decoded = Message::decode(&bytes).unwrap();
        assert_eq!(decoded.header.nonce, nonce);
        assert_eq!(decoded.header.finality_threshold_executed, 2000);
    }

    /// The attestation encoding: 65 bytes, r ‖ s ‖ v ∈ {27, 28}, recovering
    /// the signer over `keccak256(message)` — the exact shape the on-chain
    /// `Attestable._recoverAttesterSignature` accepts (§15.3).
    #[test]
    fn attestation_signs_keccak_of_the_served_message() {
        let wallet = sample_wallet();
        let served = Message::decode(&sample_raw_message())
            .unwrap()
            .with_attester_fields(B256::repeat_byte(0xab), 2000, U256::ZERO, U256::ZERO);
        let digest = served.keccak();
        let signature = wallet.sign_digest(digest).expect("signs");
        let bytes = signature.as_bytes();
        assert_eq!(bytes.len(), 65);
        assert!(bytes[64] == 27 || bytes[64] == 28, "v = {}", bytes[64]);
        let recovered = signature
            .recover_address_from_prehash(&digest)
            .expect("recovers");
        assert_eq!(recovered, wallet.address());
        // And a different message does not recover the signer.
        let other = Message::decode(&sample_reverse_message()).unwrap().keccak();
        assert_ne!(
            signature.recover_address_from_prehash(&other).unwrap(),
            wallet.address()
        );
    }

    #[test]
    fn wallet_rejects_a_malformed_secret() {
        let err = Wallet::from_secret("not-a-key").unwrap_err();
        assert!(matches!(err, Error::InvalidKey { .. }), "{err}");
        let err = Wallet::from_secret("0x1234").unwrap_err();
        assert!(matches!(err, Error::InvalidKey { .. }), "{err}");
    }

    /// The source-side fee rules: `maxFee < amount` always; the contract's
    /// `max(1, amount·minFee/10_000_000)` when the probed surface says so.
    #[test]
    fn max_fee_validation_follows_the_contract_rules() {
        let absent = FeeSurface { min_fee: None };
        absent.validate_max_fee(U256::from(1000u64), U256::ZERO).unwrap();
        let err = absent.validate_max_fee(U256::from(1000u64), U256::from(1000u64)).unwrap_err();
        assert!(matches!(err, Error::BadRequest { .. }), "{err}");

        let zero = FeeSurface { min_fee: Some(U256::ZERO) };
        zero.validate_max_fee(U256::from(1000u64), U256::ZERO).unwrap();

        let some = FeeSurface { min_fee: Some(U256::from(1_000_000u64)) };
        // amount 10_000_000 → min fee 1_000_000 (10 %).
        let err = some.validate_max_fee(U256::from(10_000_000u64), U256::ZERO).unwrap_err();
        assert!(format!("{err}").contains("1000000"), "{err}");
        some.validate_max_fee(U256::from(10_000_000u64), U256::from(1_000_000u64)).unwrap();
        // The floor: a tiny fee still requires 1.
        assert_eq!(
            required_min_fee(U256::from(2u64), U256::from(1u64)).unwrap(),
            U256::ONE
        );
        assert!(required_min_fee(U256::ONE, U256::ONE).is_err(), "amount > 1 required");
    }

    /// The decode guard names the numbers instead of misreading fields.
    #[test]
    fn short_messages_are_refused_with_their_length() {
        let err = Message::decode(&[0u8; 147]).unwrap_err();
        match err {
            Error::MessageTooShort { len, want } => {
                assert_eq!(len, 147);
                assert_eq!(want, HEADER_LEN + BURN_BODY_LEN);
            }
            other => panic!("expected MessageTooShort, got {other}"),
        }
        // A 148-byte header with no body is still refused (the fixed body is
        // part of the V2 burn format).
        assert!(Message::decode(&[0u8; HEADER_LEN]).is_err());
    }

    /// The calldata the `sol!` call type encodes. The expected bytes were
    /// produced independently with
    /// `cast calldata "depositForBurn(uint256,uint32,bytes32,address,bytes32,uint256,uint32)"`.
    #[test]
    fn deposit_for_burn_calldata_carries_the_typed_arguments() {
        let calldata: Bytes = ITokenMessengerV2::depositForBurnCall {
            amount: U256::from(100_000_000u64),
            destinationDomain: 19,
            mintRecipient: Address::repeat_byte(0x22).into_word(),
            burnToken: USDC_ETHEREUM,
            destinationCaller: B256::ZERO,
            maxFee: U256::ZERO,
            minFinalityThreshold: 2000,
        }
        .abi_encode()
        .into();
        assert_eq!(
            format!("0x{}", alloy_primitives::hex::encode(&calldata)),
            concat!(
                "0x8e0250ee",
                "0000000000000000000000000000000000000000000000000000000005f5e100",
                "0000000000000000000000000000000000000000000000000000000000000013",
                "0000000000000000000000002222222222222222222222222222222222222222",
                "000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "0000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000000000000000000000",
                "00000000000000000000000000000000000000000000000000000000000007d0",
            ),
            "cast-verified calldata"
        );
        let decoded = ITokenMessengerV2::depositForBurnCall::abi_decode(&calldata).unwrap();
        assert_eq!(decoded.amount, U256::from(100_000_000u64));
        assert_eq!(decoded.destinationDomain, 19);
        assert_eq!(decoded.mintRecipient, Address::repeat_byte(0x22).into_word());
        assert_eq!(decoded.burnToken, USDC_ETHEREUM);
        assert_eq!(decoded.maxFee, U256::ZERO);
        assert_eq!(decoded.minFinalityThreshold, 2000);

        let receive = receive_message_calldata(&sample_raw_message(), &Bytes::from(vec![0u8; 65]));
        let decoded = IMessageTransmitterV2::receiveMessageCall::abi_decode(&receive).unwrap();
        assert_eq!(decoded.message, sample_raw_message());
        assert_eq!(decoded.attestation.len(), 65);
    }

    /// The measured fee table (§16.2).
    #[test]
    fn fee_table_is_the_measured_one() {
        assert_eq!(measured_fees(0, 19), Some([(1000, 1), (2000, 0)]));
        assert_eq!(measured_fees(19, 0), Some([(1000, 0), (2000, 0)]));
        assert_eq!(measured_fees(0, 0), None);
        assert_eq!(measured_fees(19, 19), None);
    }
}
