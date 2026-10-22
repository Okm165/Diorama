//! System addresses: the Core↔EVM boundary's addresses, log decoding, and
//! the Core↔EVM amount conversion (docs-measured, plan §16.3).
//!
//! - a token's system address is `0x20` + the token index big-endian in the
//!   remaining 19 bytes (token 200 → `0x20…c8`); HYPE's is `0x2222…2222`
//!   (native, not an ERC20);
//! - EVM→Core is an ERC20 transfer to the system address, credited from the
//!   `Transfer` log;
//! - Core→EVM is completed by a `transfer(recipient, amount)` call **from**
//!   the system address on the linked ERC20;
//! - Core amounts are in the token's Core wei (`weiDecimals`), EVM amounts in
//!   `weiDecimals + evmExtraWeiDecimals` (USDC: 8 and 6).
use crate::error::Error;
use crate::wire::SpotMeta;
use alloy_primitives::{address, b256, Address, Bytes, B256, U256};
use rust_decimal::Decimal;

/// The native HYPE system address (transfers as tx value; `Received` event).
pub const HYPE_SYSTEM: Address = address!("2222222222222222222222222222222222222222");

/// The ERC20 `Transfer(address,address,uint256)` topic0.
pub const TRANSFER_TOPIC: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// The ERC20 `transfer(address,uint256)` selector.
pub const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// The system address for a token index.
pub fn system_address(token_index: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = 0x20;
    bytes[12..20].copy_from_slice(&token_index.to_be_bytes());
    Address::from(bytes)
}

/// The token index a system address encodes (`None` when it is not one).
pub fn token_index_of(address: Address) -> Option<u64> {
    if address.0[0] != 0x20 || address == HYPE_SYSTEM {
        return None;
    }
    let mut idx = [0u8; 8];
    idx.copy_from_slice(&address.0[12..20]);
    Some(u64::from_be_bytes(idx))
}

/// One spot token as the bridge needs it: the Core wei scale and the EVM link.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenInfo {
    pub index: u32,
    pub name: String,
    pub wei_decimals: u32,
    /// The linked ERC20 + its extra wei decimals (`None` = no EVM link).
    pub evm: Option<(Address, i32)>,
}

impl TokenInfo {
    /// The EVM-side decimals (`weiDecimals + evmExtraWeiDecimals`).
    pub fn evm_decimals(&self) -> Option<i32> {
        self.evm.map(|(_, extra)| self.wei_decimals as i32 + extra)
    }
}

/// The token registry view (spotMeta → the bridge's lookup).
pub fn token_registry(spot: &SpotMeta) -> Vec<TokenInfo> {
    spot.tokens
        .iter()
        .map(|t| TokenInfo {
            index: t.index,
            name: t.name.clone(),
            wei_decimals: t.wei_decimals as u32,
            evm: t
                .evm_contract
                .as_ref()
                .map(|c| (c.address, c.evm_extra_wei_decimals)),
        })
        .collect()
}

/// Find a token by index.
pub fn by_index(tokens: &[TokenInfo], index: u32) -> Option<&TokenInfo> {
    tokens.iter().find(|t| t.index == index)
}

/// Find a token by name.
pub fn by_name<'a>(tokens: &'a [TokenInfo], name: &str) -> Option<&'a TokenInfo> {
    tokens.iter().find(|t| t.name == name)
}

/// An EVM-unit amount as the human decimal the sim keeps (`value / 10^d`).
pub fn evm_to_human(value: U256, evm_decimals: i32) -> Decimal {
    let amount = decimal_of(value);
    amount / pow10(evm_decimals)
}

/// A Core wei amount as the human decimal (`wei / 10^d`).
pub fn core_to_human(wei: u64, wei_decimals: u32) -> Decimal {
    Decimal::from(wei) / pow10(wei_decimals as i32)
}

/// A human decimal as an EVM-unit amount (exact; `None` on overflow or a
/// fractional wei, which the chain would burn — the caller refuses instead).
pub fn human_to_evm(amount: Decimal, evm_decimals: i32) -> Option<U256> {
    let scaled = amount * pow10(evm_decimals);
    if scaled.fract() != Decimal::ZERO || scaled.is_sign_negative() {
        return None;
    }
    scaled.trunc().to_string().parse().ok()
}

/// `transfer(address,uint256)` calldata.
pub fn transfer_calldata(to: Address, amount: U256) -> Bytes {
    let mut out = Vec::with_capacity(68);
    out.extend_from_slice(&TRANSFER_SELECTOR);
    out.extend_from_slice(&to.into_word().0);
    out.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(out)
}

/// One decoded ERC20 Transfer log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    pub from: Address,
    pub to: Address,
    pub value: U256,
}

/// Decode a log as an ERC20 `Transfer` (topic0 + two indexed address topics).
pub fn transfer_of(topics: &[B256], data: &[u8]) -> Option<Transfer> {
    if topics.len() != 3 || topics[0] != TRANSFER_TOPIC {
        return None;
    }
    let from = Address::from_word(topics[1]);
    let to = Address::from_word(topics[2]);
    if data.len() != 32 {
        return None;
    }
    Some(Transfer {
        from,
        to,
        value: U256::from_be_slice(data),
    })
}

/// The EVM→Core credit an observed transfer represents: the sender, the
/// token, and the human amount. A transfer to a system address of an
/// unlinked token (or a Core→EVM completion from a system address) is not a
/// credit.
pub fn credit_of<'a>(transfer: &Transfer, tokens: &'a [TokenInfo]) -> Option<(Address, &'a TokenInfo, Decimal)> {
    if transfer.from.0[0] == 0x20 {
        // The completion path: the system sends out; never re-credit.
        return None;
    }
    let index = token_index_of(transfer.to)?;
    let token = by_index(tokens, u32::try_from(index).ok()?)?;
    let evm_decimals = token.evm_decimals()?;
    Some((transfer.from, token, evm_to_human(transfer.value, evm_decimals)))
}

fn decimal_of(value: U256) -> Decimal {
    value.to_string().parse().unwrap_or(Decimal::ZERO)
}

fn pow10(decimals: i32) -> Decimal {
    let mut out = Decimal::ONE;
    let n = decimals.unsigned_abs();
    for _ in 0..n {
        out *= Decimal::TEN;
    }
    if decimals < 0 {
        Decimal::ONE / out
    } else {
        out
    }
}

/// The system address's completion send cannot be built: a typed fault the
/// bridge counts rather than a silent drop.
pub fn no_evm_link(token: &TokenInfo) -> Error {
    Error::BadRequest {
        reason: format!(
            "token `{}` (index {}) has no EVM link; cannot complete Core→EVM",
            token.name, token.index
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_addresses_round_trip() {
        assert_eq!(
            system_address(0),
            address!("2000000000000000000000000000000000000000")
        );
        assert_eq!(
            system_address(200),
            address!("20000000000000000000000000000000000000c8")
        );
        assert_eq!(token_index_of(system_address(0)), Some(0));
        assert_eq!(token_index_of(system_address(200)), Some(200));
        assert_eq!(token_index_of(HYPE_SYSTEM), None);
        assert_eq!(token_index_of(Address::ZERO), None);
    }

    /// The measured USDC row: Core wei 8, EVM 6 (`evm_extra_wei_decimals -2`).
    #[test]
    fn usdc_conversions_are_exact() {
        let usdc = TokenInfo {
            index: 0,
            name: "USDC".into(),
            wei_decimals: 8,
            evm: Some((Address::repeat_byte(0x88), -2)),
        };
        assert_eq!(usdc.evm_decimals(), Some(6));
        // 1.0 USDC EVM (1e6) → 1 human.
        assert_eq!(evm_to_human(U256::from(1_000_000u64), 6), Decimal::ONE);
        // 1.0 USDC Core wei (1e8) → 1 human.
        assert_eq!(core_to_human(100_000_000, 8), Decimal::ONE);
        // 1 human → 1e6 EVM units.
        assert_eq!(human_to_evm(Decimal::ONE, 6), Some(U256::from(1_000_000u64)));
        assert_eq!(human_to_evm(Decimal::new(15, 1), 6), Some(U256::from(1_500_000u64)));
        // A fractional wei cannot be represented: refuse, never truncate.
        assert_eq!(human_to_evm(Decimal::new(1, 7), 6), None);
    }

    #[test]
    fn transfer_logs_decode_and_credit_only_evm_to_core() {
        let usdc = TokenInfo {
            index: 0,
            name: "USDC".into(),
            wei_decimals: 8,
            evm: Some((Address::repeat_byte(0x88), -2)),
        };
        let tokens = vec![usdc];
        let from = Address::repeat_byte(0xdd);
        let mut topics = vec![TRANSFER_TOPIC];
        topics.push(from.into_word());
        topics.push(system_address(0).into_word());
        let data = U256::from(5_000_000u64).to_be_bytes::<32>();
        let t = transfer_of(&topics, &data).unwrap();
        let (sender, token, amount) = credit_of(&t, &tokens).unwrap();
        assert_eq!(sender, from);
        assert_eq!(token.name, "USDC");
        assert_eq!(amount, Decimal::from(5));
        // The completion direction (from the system) is never a credit.
        let mut reverse = topics.clone();
        reverse[1] = system_address(0).into_word();
        reverse[2] = from.into_word();
        let t = transfer_of(&reverse, &data).unwrap();
        assert!(credit_of(&t, &tokens).is_none());
        // An unlinked token's transfer is not ours to credit.
        let mut topics2 = topics;
        topics2[2] = system_address(5).into_word();
        let t = transfer_of(&topics2, &data).unwrap();
        assert!(credit_of(&t, &tokens).is_none());
    }
}
