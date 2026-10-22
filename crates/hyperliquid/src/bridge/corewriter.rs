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
use alloy_primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

/// The CoreWriter system contract.
pub const CORE_WRITER: Address = alloy_primitives::address!("3333333333333333333333333333333333333333");

/// `sendRawAction(bytes)` selector: the docs' own example wraps the action
/// stream in this call, so a CoreWriter call's calldata is the selector plus
/// an ABI-encoded `bytes` argument — never the raw stream alone.
pub const SEND_RAW_ACTION_SELECTOR: [u8; 4] = [0x17, 0x93, 0x8e, 0x13];

/// The version byte this decoder understands.
const VERSION: u8 = 1;

/// `uint32::MAX` marks the spot dex in `sendAsset`'s dex fields (docs).
pub const SPOT_DEX: u32 = u32::MAX;

/// One decoded action. The payloads are decoded from their ABI words; a short
/// or malformed payload is [`CoreAction::Malformed`] (counted, never applied).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    if input.len() >= 4 && input[..4] == SEND_RAW_ACTION_SELECTOR {
        return match bytes_argument(&input[4..]) {
            Some(stream) => decode(stream),
            None => CoreAction::Malformed,
        };
    }
    decode(input)
}

/// The single ABI `bytes` argument (head offset word + length word + data).
fn bytes_argument(args: &[u8]) -> Option<&[u8]> {
    let offset = usize::try_from(U256::from_be_slice(args.get(..32)?)).ok()?;
    let len_word = args.get(offset..offset.checked_add(32)?)?;
    let len = usize::try_from(U256::from_be_slice(len_word)).ok()?;
    args.get(offset.checked_add(32)?..offset.checked_add(32)?.checked_add(len)?)
}

/// Decode one action stream (the raw bytes of a `sendRawAction` argument).
pub fn decode(input: &[u8]) -> CoreAction {
    if input.len() < 4 || input[0] != VERSION {
        return CoreAction::Malformed;
    }
    let id = u32::from_be_bytes([0, input[1], input[2], input[3]]);
    let payload = &input[4..];
    match id {
        1 => match words(payload, 7) {
            Some(w) => CoreAction::LimitOrder {
                asset: u32_of(&w[0]),
                is_buy: !w[1].is_zero(),
                limit_px: u64_of(&w[2]),
                sz: u64_of(&w[3]),
                reduce_only: !w[4].is_zero(),
                tif: u8_of(&w[5]),
                cloid: u128_of(&w[6]),
            },
            None => CoreAction::Malformed,
        },
        6 => match words(payload, 3) {
            Some(w) => CoreAction::SpotSend {
                destination: address_of(&w[0]),
                token: u64_of(&w[1]),
                wei: u64_of(&w[2]),
            },
            None => CoreAction::Malformed,
        },
        7 => match words(payload, 2) {
            Some(w) => CoreAction::UsdClassTransfer {
                ntl: u64_of(&w[0]),
                to_perp: !w[1].is_zero(),
            },
            None => CoreAction::Malformed,
        },
        9 => decode_add_api_wallet(payload).unwrap_or(CoreAction::Malformed),
        10 => match words(payload, 2) {
            Some(w) => CoreAction::CancelByOid {
                asset: u32_of(&w[0]),
                oid: u64_of(&w[1]),
            },
            None => CoreAction::Malformed,
        },
        11 => match words(payload, 2) {
            Some(w) => CoreAction::CancelByCloid {
                asset: u32_of(&w[0]),
                cloid: u128_of(&w[1]),
            },
            None => CoreAction::Malformed,
        },
        13 => match words(payload, 6) {
            Some(w) => CoreAction::SendAsset {
                destination: address_of(&w[0]),
                sub_account: address_of(&w[1]),
                source_dex: u32_of(&w[2]),
                destination_dex: u32_of(&w[3]),
                token: u64_of(&w[4]),
                wei: u64_of(&w[5]),
            },
            None => CoreAction::Malformed,
        },
        16 => match words(payload, 2) {
            Some(w) => CoreAction::SetAbstraction {
                user: address_of(&w[0]),
                abstraction: u8_of(&w[1]),
            },
            None => CoreAction::Malformed,
        },
        other => CoreAction::Unmodeled { id: other },
    }
}

/// Decode `(address, string)`: the address word + an ABI dynamic string.
fn decode_add_api_wallet(payload: &[u8]) -> Option<CoreAction> {
    let w = words(payload, 2)?;
    let offset = usize::try_from(u64_of(&w[1])).ok()?;
    // The string's offset points into the payload (its head words included).
    let len_start = offset.checked_add(32)?;
    let bytes = payload.get(len_start..)?;
    let len_word = payload.get(offset..offset.checked_add(32)?)?;
    let len = U256::from_be_slice(len_word).try_into().ok()?;
    let name = String::from_utf8(bytes.get(..len)?.to_vec()).ok()?;
    Some(CoreAction::AddApiWallet {
        wallet: address_of(&w[0]),
        name,
    })
}

/// Read `n` 32-byte ABI words (a short payload is `None`).
fn words(payload: &[u8], n: usize) -> Option<Vec<U256>> {
    if payload.len() < n * 32 {
        return None;
    }
    let (words, _rest) = payload.as_chunks::<32>();
    Some(words.iter().take(n).map(|w| U256::from_be_slice(w)).collect())
}

fn u64_of(word: &U256) -> u64 {
    word.try_into().unwrap_or(u64::MAX)
}

fn u32_of(word: &U256) -> u32 {
    word.try_into().unwrap_or(u32::MAX)
}

fn u8_of(word: &U256) -> u8 {
    word.try_into().unwrap_or(u8::MAX)
}

fn u128_of(word: &U256) -> u128 {
    word.try_into().unwrap_or(u128::MAX)
}

fn address_of(word: &U256) -> Address {
    Address::from_word(B256::from(word.to_be_bytes::<32>()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut input = SEND_RAW_ACTION_SELECTOR.to_vec();
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
