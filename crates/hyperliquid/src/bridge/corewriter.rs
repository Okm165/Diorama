//! The CoreWriter action stream decoder (docs-measured, plan §7.4).
//!
//! A call to the system contract `0x3333…3333` carries one action:
//! `byte 1 = encoding version (1)`, `bytes 2–4 = action id (u24 BE)`, then the
//! action's raw ABI payload. This module decodes the actions the devnet's
//! contracts send (their paths: deposit/transfer/recall/addApiWallet/orders/
//! cancels) into a typed enum; everything else is counted, never guessed at.
//!
//! Amounts are in *Core wei* (`weiDecimals` of the token, 8 for USDC); the
//! bridge converts to the human decimal the sim keeps when it applies one.
use alloy_primitives::Address;
use alloy_sol_types::{sol, SolCall, SolValue};
use serde::{Deserialize, Serialize};

/// The CoreWriter system contract.
pub const CORE_WRITER: Address = alloy_primitives::address!("3333333333333333333333333333333333333333");

sol! {
    /// The call wrapper (the docs' own example): a CoreWriter call's calldata
    /// is this selector plus an ABI-encoded `bytes` argument — never the raw
    /// stream alone. alloy owns the selector and the argument decoder.
    function sendRawAction(bytes data);

    // The action payloads the bridge models, in their documented ABI form —
    // alloy decodes them (strict lengths, range-checked integers), so a
    // malformed payload is a typed refusal, never a silently truncated value.
    struct LimitOrderPayload {
        uint32 asset;
        bool isBuy;
        uint64 limitPx;
        uint64 sz;
        bool reduceOnly;
        uint8 tif;
        uint128 cloid;
    }
    struct SpotSendPayload {
        address destination;
        uint64 token;
        uint64 wei;
    }
    struct UsdClassTransferPayload {
        uint64 ntl;
        bool toPerp;
    }
    struct AddApiWalletPayload {
        address apiWallet;
        string name;
    }
    struct CancelByOidPayload {
        uint32 asset;
        uint64 oid;
    }
    struct CancelByCloidPayload {
        uint32 asset;
        uint128 cloid;
    }
    struct SendAssetPayload {
        address destination;
        address subAccount;
        uint32 sourceDex;
        uint32 destinationDex;
        uint64 token;
        uint64 wei;
    }
    struct SetAbstractionPayload {
        address user;
        uint8 abstraction;
    }
}

/// The version byte this decoder understands.
const VERSION: u8 = 1;

/// `uint32::MAX` marks the spot dex in `sendAsset`'s dex fields (docs).
pub const SPOT_DEX: u32 = u32::MAX;

/// One decoded action. The payloads are decoded from their ABI words; a short
/// or malformed payload is [`CoreAction::Malformed`] (counted, never applied).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CoreAction {
    /// 1: `(uint32 asset, bool isBuy, uint64 limitPx, uint64 sz, bool reduceOnly, uint8 tif, uint128 cloid)`.
    LimitOrder {
        asset: u32,
        is_buy: bool,
        limit_px: u64,
        sz: u64,
        reduce_only: bool,
        tif: u8,
        cloid: u128,
    },
    /// 6: `(address destination, uint64 token, uint64 wei)` — Core spot send.
    SpotSend {
        destination: Address,
        token: u64,
        wei: u64,
    },
    /// 7: `(uint64 ntl, bool toPerp)` — USD class transfer.
    UsdClassTransfer { ntl: u64, to_perp: bool },
    /// 9: `(address apiWallet, string name)`.
    AddApiWallet { wallet: Address, name: String },
    /// 10: `(uint32 asset, uint64 oid)`.
    CancelByOid { asset: u32, oid: u64 },
    /// 11: `(uint32 asset, uint128 cloid)`.
    CancelByCloid { asset: u32, cloid: u128 },
    /// 13: `(address destination, address subAccount, uint32 sourceDex, uint32 destinationDex, uint64 token, uint64 wei)`.
    SendAsset {
        destination: Address,
        sub_account: Address,
        source_dex: u32,
        destination_dex: u32,
        token: u64,
        wei: u64,
    },
    /// 16: `(address user, uint8 abstraction)` — 1 disabled, 2 unified, 3 portfolio.
    SetAbstraction { user: Address, abstraction: u8 },
    /// A known id whose payload the sim does not model yet (orders, staking,
    /// vaults, …): decoded as far as the id, counted, never applied.
    Unmodeled { id: u32 },
    /// The bytes did not decode (short, wrong version, bad ABI).
    Malformed,
}

impl CoreAction {
    /// The action's wire id (for metrics/logs).
    pub fn id(&self) -> u32 {
        match self {
            CoreAction::LimitOrder { .. } => 1,
            CoreAction::SpotSend { .. } => 6,
            CoreAction::UsdClassTransfer { .. } => 7,
            CoreAction::AddApiWallet { .. } => 9,
            CoreAction::CancelByOid { .. } => 10,
            CoreAction::CancelByCloid { .. } => 11,
            CoreAction::SendAsset { .. } => 13,
            CoreAction::SetAbstraction { .. } => 16,
            CoreAction::Unmodeled { id } => *id,
            CoreAction::Malformed => 0,
        }
    }
}

/// Decode a CoreWriter *call*: the `sendRawAction(bytes)` wrapper (selector +
/// ABI bytes argument), or — for a caller that low-level-calls the contract —
/// the raw stream itself.
pub fn decode_calldata(input: &[u8]) -> CoreAction {
    if input.len() >= 4 && input[..4] == sendRawActionCall::SELECTOR {
        return match sendRawActionCall::abi_decode(input) {
            Ok(call) => decode(&call.data),
            Err(_) => CoreAction::Malformed,
        };
    }
    decode(input)
}

/// Decode one action stream (the raw bytes of a `sendRawAction` argument).
pub fn decode(input: &[u8]) -> CoreAction {
    if input.len() < 4 || input[0] != VERSION {
        return CoreAction::Malformed;
    }
    let id = u32::from_be_bytes([0, input[1], input[2], input[3]]);
    let payload = &input[4..];
    match id {
        1 => decode_limit_order(payload),
        6 => decode_spot_send(payload),
        7 => decode_usd_class_transfer(payload),
        9 => decode_add_api_wallet(payload).unwrap_or(CoreAction::Malformed),
        10 => decode_cancel_by_oid(payload),
        11 => decode_cancel_by_cloid(payload),
        13 => decode_send_asset(payload),
        16 => decode_set_abstraction(payload),
        other => CoreAction::Unmodeled { id: other },
    }
}

// The payloads are `abi.encode(...)` of their documented signatures, so every
// decoder uses alloy's *params* variant (`abi_decode_params`) — the one that
// reads the head/tail layout of a top-level parameter list.

/// Action 1: `limitOrder`.
fn decode_limit_order(payload: &[u8]) -> CoreAction {
    match LimitOrderPayload::abi_decode_params(payload) {
        Ok(v) => CoreAction::LimitOrder {
            asset: v.asset,
            is_buy: v.isBuy,
            limit_px: v.limitPx,
            sz: v.sz,
            reduce_only: v.reduceOnly,
            tif: v.tif,
            cloid: v.cloid,
        },
        Err(_) => CoreAction::Malformed,
    }
}

/// Action 6: `spotSend`.
fn decode_spot_send(payload: &[u8]) -> CoreAction {
    match SpotSendPayload::abi_decode_params(payload) {
        Ok(v) => CoreAction::SpotSend {
            destination: v.destination,
            token: v.token,
            wei: v.wei,
        },
        Err(_) => CoreAction::Malformed,
    }
}

/// Action 7: `usdClassTransfer`.
fn decode_usd_class_transfer(payload: &[u8]) -> CoreAction {
    match UsdClassTransferPayload::abi_decode_params(payload) {
        Ok(v) => CoreAction::UsdClassTransfer {
            ntl: v.ntl,
            to_perp: v.toPerp,
        },
        Err(_) => CoreAction::Malformed,
    }
}

/// Action 10: `cancelByOid`.
fn decode_cancel_by_oid(payload: &[u8]) -> CoreAction {
    match CancelByOidPayload::abi_decode_params(payload) {
        Ok(v) => CoreAction::CancelByOid {
            asset: v.asset,
            oid: v.oid,
        },
        Err(_) => CoreAction::Malformed,
    }
}

/// Action 11: `cancelByCloid`.
fn decode_cancel_by_cloid(payload: &[u8]) -> CoreAction {
    match CancelByCloidPayload::abi_decode_params(payload) {
        Ok(v) => CoreAction::CancelByCloid {
            asset: v.asset,
            cloid: v.cloid,
        },
        Err(_) => CoreAction::Malformed,
    }
}

/// Action 13: `sendAsset`.
fn decode_send_asset(payload: &[u8]) -> CoreAction {
    match SendAssetPayload::abi_decode_params(payload) {
        Ok(v) => CoreAction::SendAsset {
            destination: v.destination,
            sub_account: v.subAccount,
            source_dex: v.sourceDex,
            destination_dex: v.destinationDex,
            token: v.token,
            wei: v.wei,
        },
        Err(_) => CoreAction::Malformed,
    }
}

/// Action 16: `setAbstraction`.
fn decode_set_abstraction(payload: &[u8]) -> CoreAction {
    match SetAbstractionPayload::abi_decode_params(payload) {
        Ok(v) => CoreAction::SetAbstraction {
            user: v.user,
            abstraction: v.abstraction,
        },
        Err(_) => CoreAction::Malformed,
    }
}

/// Action 9: `addApiWallet` (the dynamic string included).
fn decode_add_api_wallet(payload: &[u8]) -> Option<CoreAction> {
    let v = AddApiWalletPayload::abi_decode_params(payload).ok()?;
    Some(CoreAction::AddApiWallet {
        wallet: v.apiWallet,
        name: v.name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    /// The docs' own example: `sendUsdClassTransfer(ntl, toPerp)` builds
    /// `[0x01, 0x00, 0x00, 0x07] ++ abi.encode(uint64, bool)`.
    #[test]
    fn usd_class_transfer_decodes() {
        let mut input = vec![0x01, 0x00, 0x00, 0x07];
        input.extend_from_slice(&U256::from(5_000_000u64).to_be_bytes::<32>());
        input.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
        let action = decode(&input);
        assert_eq!(
            action,
            CoreAction::UsdClassTransfer {
                ntl: 5_000_000,
                to_perp: true
            }
        );
        assert_eq!(action.id(), 7);
    }

    #[test]
    fn send_asset_decodes_with_spot_dex() {
        let mut input = vec![0x01, 0x00, 0x00, 0x0d];
        for word in [
            U256::from_be_bytes(Address::repeat_byte(0xaa).into_word().0),
            U256::ZERO,
            U256::from(SPOT_DEX),
            U256::from(SPOT_DEX),
            U256::from(0u64), // token 0 = USDC
            U256::from(1_000_000u64),
        ] {
            input.extend_from_slice(&word.to_be_bytes::<32>());
        }
        let action = decode(&input);
        assert_eq!(
            action,
            CoreAction::SendAsset {
                destination: Address::repeat_byte(0xaa),
                sub_account: Address::ZERO,
                source_dex: SPOT_DEX,
                destination_dex: SPOT_DEX,
                token: 0,
                wei: 1_000_000
            }
        );
    }

    #[test]
    fn add_api_wallet_decodes_the_dynamic_string() {
        let mut input = vec![0x01, 0x00, 0x00, 0x09];
        input.extend_from_slice(&U256::from_be_bytes(Address::repeat_byte(0xbb).into_word().0).to_be_bytes::<32>());
        input.extend_from_slice(&U256::from(64u64).to_be_bytes::<32>()); // offset
        input.extend_from_slice(&U256::from(5u64).to_be_bytes::<32>()); // len
        let mut name = b"agent".to_vec();
        name.resize(32, 0);
        input.extend_from_slice(&name);
        assert_eq!(
            decode(&input),
            CoreAction::AddApiWallet {
                wallet: Address::repeat_byte(0xbb),
                name: "agent".into()
            }
        );
    }

    /// The docs' wrapper: selector + ABI bytes argument (measured calldata
    /// from `cast calldata "sendRawAction(bytes)" 0x01000007…`).
    #[test]
    fn wrapped_calls_decode_through_the_bytes_argument() {
        let mut input = sendRawActionCall::SELECTOR.to_vec();
        input.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>()); // offset
        input.extend_from_slice(&U256::from(68u64).to_be_bytes::<32>()); // len
        let mut stream = vec![0x01, 0x00, 0x00, 0x07];
        stream.extend_from_slice(&U256::from(500_000_000u64).to_be_bytes::<32>());
        stream.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
        input.extend_from_slice(&stream);
        input.resize(input.len() + 28, 0); // ABI padding
        assert_eq!(
            decode_calldata(&input),
            CoreAction::UsdClassTransfer {
                ntl: 500_000_000,
                to_perp: true
            }
        );
        // A truncated wrapper is malformed, never a guess.
        assert_eq!(decode_calldata(&input[..40]), CoreAction::Malformed);
    }

    #[test]
    fn malformed_and_unmodeled_are_typed() {
        assert_eq!(decode(&[]), CoreAction::Malformed);
        assert_eq!(decode(&[0x02, 0, 0, 7]), CoreAction::Malformed);
        assert_eq!(decode(&[0x01, 0, 0, 7, 0, 0]), CoreAction::Malformed);
        assert_eq!(decode(&[0x01, 0, 0, 0x11]), CoreAction::Unmodeled { id: 17 });
    }
}
