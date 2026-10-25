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
//!
//! Measured caveat: the *real* HyperEVM USDC blacklists token 0's system
//! address (`isBlacklisted(0x2000…0) == true` on the live chain), so
//! EVM→Core USDC cannot be a plain transfer with the real token. Daloc's
//! deploy replaces the canonical ERC20s with its own mocks (the fixture
//! set), whose transfers to system addresses succeed — that is the path the
//! bridge credits.
use crate::error::Error;
use crate::wire::SpotMeta;
use alloy_primitives::{address, Address, Bytes, LogData, U256};
use alloy_sol_types::{sol, SolCall, SolEvent};
use rust_decimal::Decimal;

sol! {
    /// The canonical ERC-20 surface the bridge speaks: the `Transfer` event
    /// (the EVM→Core credit's only admissible source) and `transfer` (the
    /// Core→EVM completion). alloy generates the topic hash, the validating
    /// decoder and the selector — the wire format is never hand-rolled.
    event Transfer(address indexed from, address indexed to, uint256 value);
    function transfer(address to, uint256 amount) returns (bool);
    /// HYPE's boundary (docs): the system contract's payable `receive()`
    /// emits `Received(user, amount)` with `user = msg.sender`, so a tx value
    /// sent to `0x2222…2` is the EVM→Core HYPE transfer.
    event Received(address indexed user, uint256 amount);
}

/// The native HYPE system address (transfers as tx value; `Received` event).
pub(crate) const HYPE_SYSTEM: Address = address!("2222222222222222222222222222222222222222");

/// HYPE's EVM-side decimals: native, 18 (the docs' special case — HYPE is
/// received as the gas token, not an ERC20, so the token registry's
/// `evm_decimals` cannot answer for it).
pub(crate) const HYPE_EVM_DECIMALS: i32 = 18;

/// The system address for a token index.
pub(crate) fn system_address(token_index: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = 0x20;
    bytes[12..20].copy_from_slice(&token_index.to_be_bytes());
    Address::from(bytes)
}

/// The token index a system address encodes (`None` when it is not one).
pub(crate) fn token_index_of(address: Address) -> Option<u64> {
    if address.0[0] != 0x20 || address == HYPE_SYSTEM {
        return None;
    }
    let mut idx = [0u8; 8];
    idx.copy_from_slice(&address.0[12..20]);
    Some(u64::from_be_bytes(idx))
}

/// One spot token as the bridge needs it: the Core wei scale and the EVM link.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TokenInfo {
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
pub(crate) fn token_registry(spot: &SpotMeta) -> Vec<TokenInfo> {
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
pub(crate) fn by_index(tokens: &[TokenInfo], index: u32) -> Option<&TokenInfo> {
    tokens.iter().find(|t| t.index == index)
}

/// Find a token by name.
pub(crate) fn by_name<'a>(tokens: &'a [TokenInfo], name: &str) -> Option<&'a TokenInfo> {
    tokens.iter().find(|t| t.name == name)
}

/// An EVM-unit amount as the human decimal the sim keeps (`value / 10^d`).
/// `None` when the value or the token's decimals cannot be represented — the
/// caller refuses the amount rather than panicking on a corrupt answer.
pub(crate) fn evm_to_human(value: U256, evm_decimals: i32) -> Option<Decimal> {
    Some(decimal_of(value)? / pow10(evm_decimals)?)
}

/// A Core wei amount as the human decimal (`wei / 10^d`).
pub(crate) fn core_to_human(wei: u64, wei_decimals: u32) -> Option<Decimal> {
    Some(Decimal::from(wei) / pow10(wei_decimals as i32)?)
}

/// A human decimal as an EVM-unit amount, floored at the EVM wei: the real
/// chain burns the sub-wei remainder of a non-round amount (plan §16.3), so
/// the bridge does the same rather than refusing a transfer the chain would
/// complete. `None` for a negative amount or one the decimal range cannot
/// carry (the multiply is checked: `Decimal`'s `*` panics on overflow).
pub(crate) fn human_to_evm(amount: Decimal, evm_decimals: i32) -> Option<U256> {
    if amount.is_sign_negative() {
        return None;
    }
    (amount.checked_mul(pow10(evm_decimals)?)?)
        .trunc()
        .to_string()
        .parse()
        .ok()
}

/// Floor a human amount at the Core wei (`wei_decimals`): EVM→Core credits
/// are floored the same way (the sub-wei remainder is burned on the real
/// chain).
pub(crate) fn floor_to_core_wei(amount: Decimal, wei_decimals: u32) -> Option<Decimal> {
    let p = pow10(wei_decimals as i32)?;
    Some(amount.checked_mul(p)?.trunc() / p)
}

/// `transfer(address,uint256)` calldata — alloy's own encoder.
pub(crate) fn transfer_calldata(to: Address, amount: U256) -> Bytes {
    transferCall { to, amount }.abi_encode().into()
}

/// Decode a log as an ERC-20 `Transfer`: alloy's validating decoder (the
/// topic0 hash, the indexed-address count and the data length), so a
/// look-alike or malformed event decodes to `None` instead of a credit.
pub(crate) fn transfer_of(log: &LogData) -> Option<Transfer> {
    Transfer::decode_log_data_validate(log).ok()
}

/// The EVM→Core HYPE credit a system-contract `Received` log represents:
/// the sender and the Core amount (18 EVM decimals floored to the token's
/// Core wei — the docs' burn caveat applies to HYPE too).
pub(crate) fn hype_received_of(log: &LogData, token: &TokenInfo) -> Option<(Address, Decimal)> {
    let event = Received::decode_log_data_validate(log).ok()?;
    let credited = floor_to_core_wei(
        evm_to_human(event.amount, HYPE_EVM_DECIMALS)?,
        token.wei_decimals,
    )?;
    Some((event.user, credited))
}

/// The EVM→Core credit an observed transfer represents: the sender, the
/// token, and the human amount. A transfer to a system address of an
/// unlinked token (or a Core→EVM completion from a system address) is not a
/// credit — and neither is a `Transfer` log emitted by any contract other
/// than the token's *linked* EVM contract (a malicious or unrelated emitter
/// could mint Core balances with a look-alike event; the docs' rule is the
/// linked contract's own log).
pub(crate) fn credit_of<'a>(
    transfer: &Transfer,
    tokens: &'a [TokenInfo],
    emitter: Address,
) -> Option<(Address, &'a TokenInfo, Decimal)> {
    if transfer.from.0[0] == 0x20 {
        // The completion path: the system sends out; never re-credit.
        return None;
    }
    let index = token_index_of(transfer.to)?;
    let token = by_index(tokens, u32::try_from(index).ok()?)?;
    let (linked, _extra) = token.evm?;
    if emitter != linked {
        return None;
    }
    let evm_decimals = token.evm_decimals()?;
    let credited =
        floor_to_core_wei(evm_to_human(transfer.value, evm_decimals)?, token.wei_decimals)?;
    Some((transfer.from, token, credited))
}

/// A U256 as a decimal: `None` when the value has more digits than the
/// decimal range carries (a `U256::MAX` credit is unrepresentable, and
/// silently reading it as zero would be a lie about money).
fn decimal_of(value: U256) -> Option<Decimal> {
    value.to_string().parse().ok()
}

/// `10^decimals`, checked: the exponent comes from the venue's public
/// metadata (input-reachable). The rule is rust_decimal's
/// (`from_scientific` caps at `MAX_SCALE`), so an out-of-range exponent is
/// the same `None` the checked loop produced — pinned by the test below.
fn pow10(decimals: i32) -> Option<Decimal> {
    Decimal::from_scientific(&format!("1e{decimals}")).ok()
}

/// The system address's completion send cannot be built: a typed fault the
/// bridge counts rather than a silent drop.
pub(crate) fn no_evm_link(token: &TokenInfo) -> Error {
    Error::bad_request(format!(
            "token `{}` (index {}) has no EVM link; cannot complete Core→EVM",
            token.name, token.index
        ),)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

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
        assert_eq!(evm_to_human(U256::from(1_000_000u64), 6), Some(Decimal::ONE));
        // 1.0 USDC Core wei (1e8) → 1 human.
        assert_eq!(core_to_human(100_000_000, 8), Some(Decimal::ONE));
        // 1 human → 1e6 EVM units.
        assert_eq!(human_to_evm(Decimal::ONE, 6), Some(U256::from(1_000_000u64)));
        assert_eq!(human_to_evm(Decimal::new(15, 1), 6), Some(U256::from(1_500_000u64)));
        // A sub-wei fraction floors at the EVM wei (the real chain burns it).
        assert_eq!(human_to_evm(Decimal::new(1, 7), 6), Some(U256::ZERO));
    }

    #[test]
    fn sub_wei_remainders_are_floored_not_refused() {
        // HYPE: 18 EVM decimals, 8 Core wei — a non-round amount credits the
        // floored value (the rest burns, like the real chain).
        let one_and_dust = Decimal::from_str("1.000000000000000001").unwrap();
        let floored = floor_to_core_wei(one_and_dust, 8);
        assert_eq!(floored, Some(Decimal::from_str("1.00000000").unwrap()));
        // Core→EVM floors at the EVM wei instead of refusing.
        let value = human_to_evm(one_and_dust, 18).unwrap();
        assert_eq!(value, U256::from(1_000_000_000_000_000_001u128));
        // A sub-EVM-wei amount floors to zero (all burned).
        let sub_wei = Decimal::from_str("0.0000000000000000001").unwrap(); // 1e-19
        assert_eq!(human_to_evm(sub_wei, 18).unwrap(), U256::ZERO);
        assert!(human_to_evm(Decimal::from_str("-1").unwrap(), 6).is_none());
    }

    /// One `Transfer` log as the chain emits it: topic0 + two indexed
    /// addresses + one data word.
    fn transfer_log(from: Address, to: Address, value: U256) -> LogData {
        LogData::new_unchecked(
            vec![Transfer::SIGNATURE_HASH, from.into_word(), to.into_word()],
            value.to_be_bytes::<32>().to_vec().into(),
        )
    }

    /// One `Received` log as the system contract emits it: topic0 + the
    /// indexed user + one data word.
    fn received_log(user: Address, amount: U256) -> LogData {
        LogData::new_unchecked(
            vec![Received::SIGNATURE_HASH, user.into_word()],
            amount.to_be_bytes::<32>().to_vec().into(),
        )
    }

    /// The docs' HYPE path: a tx value to `0x2222…2` emits `Received(user,
    /// amount)`; the credit floors 18 EVM decimals to the Core's 8.
    #[test]
    fn a_received_event_credits_hype() {
        let hype = TokenInfo {
            index: 150,
            name: "HYPE".into(),
            wei_decimals: 8,
            evm: None,
        };
        let user = Address::repeat_byte(0xaa);
        let log = received_log(user, U256::from(1_500_000_000_000_000_001u128));
        let (credited_user, amount) = hype_received_of(&log, &hype).expect("decodes");
        assert_eq!(credited_user, user);
        assert_eq!(amount, Decimal::from_str("1.50000000").unwrap(), "18 decimals floored to Core wei");
        // A malformed look-alike is not a credit.
        let malformed = LogData::new_unchecked(vec![Received::SIGNATURE_HASH], Bytes::new());
        assert!(hype_received_of(&malformed, &hype).is_none());
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
        let linked = Address::repeat_byte(0x88);
        let t = transfer_of(&transfer_log(from, system_address(0), U256::from(5_000_000u64)))
            .unwrap();
        let (sender, token, amount) = credit_of(&t, &tokens, linked).unwrap();
        assert_eq!(sender, from);
        assert_eq!(token.name, "USDC");
        assert_eq!(amount, Decimal::from(5));
        // The completion direction (from the system) is never a credit.
        let t = transfer_of(&transfer_log(system_address(0), from, U256::from(5_000_000u64)))
            .unwrap();
        assert!(credit_of(&t, &tokens, linked).is_none());
        // A Transfer log from any other emitter is not a credit (a
        // look-alike event must not mint Core balances).
        let impostor = Address::repeat_byte(0x77);
        assert!(credit_of(&t, &tokens, impostor).is_none());
        // An unlinked token's transfer is not ours to credit.
        let t = transfer_of(&transfer_log(from, system_address(5), U256::from(5_000_000u64)))
            .unwrap();
        assert!(credit_of(&t, &tokens, linked).is_none());
    }

    #[test]
    fn look_alike_and_malformed_events_never_decode() {
        let from = Address::repeat_byte(0xdd);
        // Wrong topic0 (some other event's hash): not a Transfer.
        let look_alike = LogData::new_unchecked(
            vec![
                alloy_primitives::B256::repeat_byte(0x11),
                from.into_word(),
                system_address(0).into_word(),
            ],
            U256::from(1u64).to_be_bytes::<32>().to_vec().into(),
        );
        assert!(transfer_of(&look_alike).is_none());
        // Wrong data length: a truncated word is refused, not truncated.
        let malformed = LogData::new_unchecked(
            vec![
                Transfer::SIGNATURE_HASH,
                from.into_word(),
                system_address(0).into_word(),
            ],
            vec![0u8; 31].into(),
        );
        assert!(transfer_of(&malformed).is_none());
        // alloy's encoder is the one the completion path sends: selector
        // `0xa9059cbb`, 68 bytes total.
        let calldata = transfer_calldata(from, U256::from(5u64));
        assert_eq!(&calldata[..4], &transferCall::SELECTOR);
        assert_eq!(calldata.len(), 68);
    }

    /// The exponent contract: in range answers the power, past the scale cap
    /// answers `None` — the same `Option` the checked loop produced, now
    /// delegated to rust_decimal.
    #[test]
    fn pow10_caps_at_the_scale() {
        assert_eq!(pow10(6), Some(Decimal::from(1_000_000)));
        assert_eq!(pow10(-8), Some(Decimal::new(1, 8)));
        assert_eq!(pow10(29), None);
    }
}
