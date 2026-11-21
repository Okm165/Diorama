//! The HyperEVM L1 read precompiles, mirrored from the sim.
//!
//! A forked HyperEVM has no L1 read precompiles (`0x0800`..`0x0813`): the node
//! answers them from HyperCore, and the fork's EVM has none. The margin agents
//! read their own Core state through them (`withdrawable`, `spotBalance`,
//! `accountMarginSummary`, `coreUserExists`), so without them a recall reads
//! zero. Here the sim *is* HyperCore, so the bridge installs one tiny contract
//! (`assets/PrecompileMock.sol`) at each address and writes the sim's own
//! answers into its storage: the answer to a call lives under the keccak of the
//! call's calldata, so this module owns every ABI and the contract none.
//!
//! The reads served (units are the precompiles' own):
//! - `0x0801` `spotBalance(user, token)`: `(total, hold, entryNtl)` in the
//!   token's Core wei; `entryNtl` is not modeled (0);
//! - `0x0803` `withdrawable(user)`: the default perp dex's served `withdrawable`,
//!   perp-class USD (1e6);
//! - `0x080f` `accountMarginSummary(dex, user)`: `(accountValue, marginUsed,
//!   ntlPos, rawUsd)` of that dex's clearinghouse (1e6);
//! - `0x0810` `coreUserExists(user)`.
//!
//! The market-data and position precompiles are not mirrored (nothing the
//! deployment reads); `0x080c` (token info) keeps the deployment's own mock.
//! An account or query the sim never held answers zeros, the same as a real
//! account with nothing.
use super::system::{self, TokenInfo};
use crate::feed::MarketFeed;
use crate::sim::account::{self, Account};
use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy_sol_types::SolValue;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

/// The mock's runtime code (compiled from `assets/PrecompileMock.sol`).
const MOCK_CODE: &str = include_str!("../../assets/precompile_mock.hex");

pub(crate) const SPOT_BALANCE: Address = address!("0000000000000000000000000000000000000801");
pub(crate) const WITHDRAWABLE: Address = address!("0000000000000000000000000000000000000803");
pub(crate) const MARGIN_SUMMARY: Address = address!("000000000000000000000000000000000000080f");
pub(crate) const USER_EXISTS: Address = address!("0000000000000000000000000000000000000810");

/// Every address the mirror owns (the mock's code goes to each).
pub(crate) const MIRRORED: [Address; 4] = [SPOT_BALANCE, WITHDRAWABLE, MARGIN_SUMMARY, USER_EXISTS];

/// The perp class's USD scale (the `withdrawable` / margin summary unit).
const PERP_USD_DECIMALS: u32 = 6;

/// The mock's runtime code as bytes.
pub(crate) fn mock_code() -> Bytes {
    let hex = MOCK_CODE.trim().trim_start_matches("0x");
    Bytes::from(alloy_primitives::hex::decode(hex).expect("the embedded mock code is hex"))
}

/// One precompile answer: the call (`target` + calldata) and its return data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub target: Address,
    pub calldata: Vec<u8>,
    pub returned: Vec<u8>,
}

impl Answer {
    fn new(target: Address, calldata: Vec<u8>, returned: Vec<u8>) -> Self {
        Self {
            target,
            calldata,
            returned,
        }
    }

    /// The mock's storage slot of the answer (the keccak of the calldata).
    pub(crate) fn slot(&self) -> U256 {
        U256::from_be_bytes(keccak256(&self.calldata).0)
    }

    /// The mock's storage for this answer: the byte length under the keccak of
    /// the calldata, then the 32-byte words after it.
    pub(crate) fn storage(&self) -> Vec<(U256, B256)> {
        let key = self.slot();
        let mut out = vec![(
            key,
            B256::from(U256::from(self.returned.len()).to_be_bytes::<32>()),
        )];
        for (i, chunk) in self.returned.chunks(32).enumerate() {
            let mut word = [0u8; 32];
            word[..chunk.len()].copy_from_slice(chunk);
            out.push((key + U256::from(1 + i), B256::from(word)));
        }
        out
    }

    /// The write that clears the answer (a zero length reads as "nothing").
    pub(crate) fn cleared(&self) -> (U256, B256) {
        (self.slot(), B256::ZERO)
    }
}

/// `amount` at `decimals`, floored; `None` when it does not fit a `u64`
/// (negative included).
fn scaled_u64(amount: Decimal, decimals: u32) -> Option<u64> {
    (amount.checked_mul(Decimal::from(10u64.checked_pow(decimals)?))?)
        .trunc()
        .to_u64()
}

/// The signed form, for the account value and raw USD.
fn scaled_i64(amount: Decimal, decimals: u32) -> Option<i64> {
    (amount.checked_mul(Decimal::from(10u64.checked_pow(decimals)?))?)
        .trunc()
        .to_i64()
}

/// The spot-balance answers: each token's `total` and `hold` in core wei. The
/// `hold` is what the venue serves: resting orders plus a unified account's
/// position margin ([`account::served_hold`]). A figure that does not compute
/// or overflows is left out.
fn spot_balance_answers(
    user: Address,
    account: &Account,
    feed: &MarketFeed,
    tokens: &[TokenInfo],
) -> Vec<Answer> {
    let mut out = Vec::new();
    for (index, entry) in &account.spot {
        let Some(token) = system::by_index(tokens, *index) else {
            continue;
        };
        let Ok(served) = account::served_hold(account, feed, entry, *index) else {
            continue;
        };
        let (Some(total), Some(hold)) = (
            scaled_u64(entry.total, token.wei_decimals),
            scaled_u64(served, token.wei_decimals),
        ) else {
            continue;
        };
        out.push(Answer::new(
            SPOT_BALANCE,
            (user, u64::from(*index)).abi_encode_params(),
            (total, hold, 0u64).abi_encode_params(),
        ));
    }
    out
}

/// Every answer the sim's account `user` has: its spot balances, the default
/// dex's `withdrawable`, each dex's margin summary and its existence. A figure
/// the sim cannot compute (a missing mark) or that overflows is left out, so
/// the mock answers zeros for it rather than a wrong number.
pub(crate) fn answers_of(
    user: Address,
    account: &Account,
    feed: &MarketFeed,
    tokens: &[TokenInfo],
) -> Vec<Answer> {
    let mut out = vec![Answer::new(
        USER_EXISTS,
        (user,).abi_encode_params(),
        true.abi_encode(),
    )];
    out.extend(spot_balance_answers(user, account, feed, tokens));
    let mut dex = 0u32;
    while let Some(name) = feed.dex_name_at(dex) {
        if let Ok(view) = account::view(account, feed, &name) {
            let summary = &view.margin_summary;
            if let (Some(value), Some(used), Some(ntl), Some(raw)) = (
                scaled_i64(summary.account_value, PERP_USD_DECIMALS),
                scaled_u64(summary.total_margin_used, PERP_USD_DECIMALS),
                scaled_u64(summary.total_ntl_pos, PERP_USD_DECIMALS),
                scaled_i64(summary.total_raw_usd, PERP_USD_DECIMALS),
            ) {
                out.push(Answer::new(
                    MARGIN_SUMMARY,
                    (dex, user).abi_encode_params(),
                    (value, used, ntl, raw).abi_encode_params(),
                ));
            }
            if name.is_empty() {
                if let Some(withdrawable) = scaled_u64(view.withdrawable, PERP_USD_DECIMALS) {
                    out.push(Answer::new(
                        WITHDRAWABLE,
                        (user,).abi_encode_params(),
                        withdrawable.abi_encode(),
                    ));
                }
            }
        }
        dex += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mock_code_is_embedded_hex() {
        let code = mock_code();
        assert!(code.len() > 40, "{} bytes", code.len());
        // The runtime starts with the free-memory-pointer prelude.
        assert_eq!(&code[..4], &[0x60, 0x80, 0x60, 0x40]);
    }

    #[test]
    fn an_answer_is_stored_under_the_calldata_hash_with_its_length() {
        let user = Address::repeat_byte(0xaa);
        let calldata = (user, 0u64).abi_encode_params();
        let returned = (5u64, 2u64, 0u64).abi_encode_params();
        let answer = Answer::new(SPOT_BALANCE, calldata.clone(), returned);
        let storage = answer.storage();
        let key = U256::from_be_bytes(keccak256(&calldata).0);
        // The length word, then one slot per 32-byte word of the answer.
        assert_eq!(storage.len(), 1 + 3);
        assert_eq!(
            storage[0],
            (key, B256::from(U256::from(96u64).to_be_bytes::<32>()))
        );
        assert_eq!(storage[1].0, key + U256::from(1));
        assert_eq!(
            storage[1].1,
            B256::from(U256::from(5u64).to_be_bytes::<32>())
        );
        assert_eq!(
            storage[2].1,
            B256::from(U256::from(2u64).to_be_bytes::<32>())
        );
        assert_eq!(answer.cleared(), (key, B256::ZERO));
    }

    #[test]
    fn amounts_are_floored_to_the_precompiles_units() {
        assert_eq!(scaled_u64("1.2345679".parse().unwrap(), 6), Some(1_234_567));
        assert_eq!(
            scaled_u64("2049.839536".parse().unwrap(), 8),
            Some(204_983_953_600)
        );
        // A negative balance cannot be a u64; the signed reads keep the sign.
        assert_eq!(scaled_u64("-1".parse().unwrap(), 6), None);
        assert_eq!(scaled_i64("-7.0296".parse().unwrap(), 6), Some(-7_029_600));
    }

    #[test]
    fn the_calldata_matches_what_the_agents_send() {
        // PrecompileLib: `abi.encode(user, token)` / `abi.encode(user)` /
        // `abi.encode(perpDexIndex, user)`: static words, no selector.
        let user = Address::repeat_byte(0xbb);
        assert_eq!((user, 0u64).abi_encode_params().len(), 64);
        assert_eq!((user,).abi_encode_params().len(), 32);
        assert_eq!((3u32, user).abi_encode_params().len(), 64);
        assert_eq!(true.abi_encode().len(), 32);
    }
}
