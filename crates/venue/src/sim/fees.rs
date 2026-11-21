//! What a transfer costs on the venue, and the ledger rows that record it.
//!
//! Researched rules (the docs, Circle's CCTP guide, and the real
//! `userNonFundingLedgerUpdates` rows read on 2026-11-17):
//!
//! * **Activation fee** (Circle, <https://developers.circle.com/cctp/cctp-on-hypercore>):
//!   "New HyperCore accounts are subject to a one-time 1 USDC activation fee,
//!   managed entirely by the Hyperliquid protocol." When an account is first
//!   funded "the full deposit amount is credited to their tradable balance.
//!   The 1 USDC activation fee is earmarked at the account level and charged
//!   on the user's first outbound action", such as a withdrawal or a
//!   `SendAsset` transfer; "the first outbound action requires a balance of at
//!   least 1 USDC" or it fails, and later actions "are not subject to it". The
//!   Hyperliquid page ("Activation gas fee") words it as a fee on "the first
//!   transaction which has the new account as destination address"; the owner
//!   tested the real venue and follows Circle, so the sim does: the debt is
//!   set on the account when it first receives funds (a deposit or any
//!   inbound transfer; [`SimState::activation_owed`]), the destination's own
//!   newness never matters, and the first outbound transfer pays 1 USDC on top
//!   of the amount (row: `fee: "1.0"`, `feeToken: "USDC"`).
//! * **Core to EVM gas.** "A transfer from HyperCore to HyperEVM costs 200k
//!   gas at the base gas price of the next HyperEVM block." The real rows show
//!   it as `nativeTokenFee` (HYPE, `0.00002` = 200k gas at 0.1 gwei) when the
//!   sender holds HYPE, otherwise as `fee` in the token sent, valued at the
//!   HYPE price. HYPE itself is sent with no fee in every row read. The
//!   EVM to Core direction costs EVM gas only (paid on the EVM side).
//! * `withdraw3` costs 1 USDC (docs, "$1 fee"; real `withdraw` rows carry
//!   `fee: "1.0"`); the devnet does not run that bridge, so it stays refused.
//!
//! Choices where the sources are silent: a move between one's own wallets is
//! not an outbound transfer (it pays nothing and keeps the debt), and the
//! funding helpers (`deal`, seeds, `/control`) do not create the debt.
use super::account::Account;
use super::{SimState, TokenRef};
use crate::feed::MarketFeed;
use alloy_primitives::Address;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// The activation fee: one unit of the quote token.
pub(crate) const ACTIVATION_FEE: Decimal = Decimal::ONE;

/// The quote token the activation fee is charged in (spot token 0, USDC).
pub(crate) const QUOTE_TOKEN: TokenRef<'static> = TokenRef {
    index: 0,
    coin: "USDC",
};

/// HyperEVM gas a Core to EVM transfer costs (docs).
const CORE_TO_EVM_GAS: i128 = 200_000;

/// HyperEVM's base gas price in wei: the 0.1 gwei floor every real
/// `nativeTokenFee: "0.00002"` row implies (200k gas x 0.1 gwei = 0.00002 HYPE).
const BASE_GAS_PRICE_WEI: i128 = 100_000_000;

/// The most ledger rows kept per account (the oldest drop first).
const LEDGER_BOUND: usize = 10_000;

/// The native gas token's name on Core.
pub(crate) const GAS_TOKEN: &str = "HYPE";

/// The HYPE a Core to EVM transfer costs at the base gas price.
pub(crate) fn core_to_evm_gas_hype() -> Decimal {
    Decimal::from_i128_with_scale(CORE_TO_EVM_GAS * BASE_GAS_PRICE_WEI, 18)
}

/// The USD value of one `token`: USDC is 1, a token with a USDC spot pair its
/// mid, any other perp collateral token (a stable) 1; `None` when unpriced.
pub(crate) fn token_usd(feed: &MarketFeed, token: u32) -> Option<Decimal> {
    if token == QUOTE_TOKEN.index {
        return Some(Decimal::ONE);
    }
    let pair = feed
        .spot_meta()?
        .universe
        .into_iter()
        .find(|p| p.tokens == [token, QUOTE_TOKEN.index]);
    if let Some(mid) = pair.and_then(|p| feed.all_mids().0.get(&p.name).copied()) {
        return Some(mid);
    }
    feed.is_collateral_token(token).then_some(Decimal::ONE)
}

/// The venue's decimal text on ledger rows: trimmed, with a `.0` on a whole
/// number (`"1.0"`, `"13.76"`).
pub(crate) fn real_text(value: Decimal) -> String {
    let text = value.round_dp(9).normalize().to_string();
    if text.contains('.') {
        text
    } else {
        format!("{text}.0")
    }
}

/// Whether the account holds or ever held anything: a balance row, a
/// clearinghouse, a position or a fill.
fn holds_anything(account: &Account) -> bool {
    !account.spot.is_empty()
        || !account.perps.is_empty()
        || !account.positions.is_empty()
        || !account.fills.is_empty()
}

impl SimState {
    /// Whether the address exists on the venue: it was activated (received or
    /// held something) or the sim holds a funded account for it.
    pub(crate) fn exists(&self, who: Address) -> bool {
        self.activated.contains(&who) || self.accounts.get(&who).is_some_and(holds_anything)
    }

    /// The activation fee a send from `from` to `to` pays now: 1 USDC when
    /// `from` still owes its account's activation. Where the funds go does not
    /// matter; a move to oneself is not an outbound transfer.
    pub(crate) fn activation_due(&self, from: Address, to: Address) -> Decimal {
        if from != to && self.activation_owed.contains(&from) {
            ACTIVATION_FEE
        } else {
            Decimal::ZERO
        }
    }

    /// Book a send's two sides: the sender's debt, when it was charged, is
    /// paid; a recipient that did not exist before (`new_recipient`) now owes
    /// its own; both addresses exist from now on.
    pub(crate) fn settle_activation(
        &mut self,
        (from, to): (Address, Address),
        new_recipient: bool,
    ) {
        if from != to {
            self.activation_owed.remove(&from);
            if new_recipient {
                self.activation_owed.insert(to);
            }
        }
        self.activated.insert(to);
        self.activated.insert(from);
    }

    /// Append `delta` to the ledger of every party (once each), stamped `time`.
    pub(crate) fn record_ledger(
        &mut self,
        parties: &[Address],
        time: u64,
        delta: &serde_json::Value,
    ) {
        self.next_ledger_id = self.next_ledger_id.saturating_add(1);
        let row = LedgerUpdate {
            time,
            hash: format!("0x{:064x}", self.next_ledger_id),
            delta: delta.clone(),
        };
        let mut seen: Vec<Address> = Vec::new();
        for party in parties {
            if seen.contains(party) {
                continue;
            }
            seen.push(*party);
            let list = self.transfer_ledger.entry(*party).or_default();
            list.push_back(row.clone());
            while list.len() > LEDGER_BOUND {
                list.pop_front();
            }
        }
    }
}

/// One `userNonFundingLedgerUpdates` row: `{time, hash, delta}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct LedgerUpdate {
    pub time: u64,
    pub hash: String,
    pub delta: serde_json::Value,
}

/// Which action's row a transfer writes: `spotSend` is a `spotTransfer`,
/// `sendAsset` and `usdSend` a `send`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    SpotTransfer,
    Send,
}

/// What a transfer paid on top of its amount: the activation or gas in the
/// token's own terms (`fee`, in `fee_token`) and the HYPE gas (`native`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Paid {
    pub fee: Decimal,
    pub fee_token: String,
    pub native: Decimal,
}

/// One transfer's facts for its ledger row.
pub(crate) struct TransferRow<'a> {
    pub kind: RowKind,
    pub user: Address,
    pub destination: Address,
    /// `sourceDex` / `destinationDex` (`""` default dex, `"spot"`).
    pub dexes: (&'a str, &'a str),
    pub token: &'a str,
    pub amount: Decimal,
    pub usdc_value: Decimal,
    pub paid: &'a Paid,
    pub nonce: u64,
}

impl TransferRow<'_> {
    /// The row's `delta`, with the real venue's field names.
    pub(crate) fn delta(&self) -> serde_json::Value {
        let mut delta = serde_json::json!({
            "type": match self.kind { RowKind::SpotTransfer => "spotTransfer", RowKind::Send => "send" },
            "token": self.token,
            "amount": real_text(self.amount),
            "usdcValue": real_text(self.usdc_value),
            "user": format!("{:#x}", self.user),
            "destination": format!("{:#x}", self.destination),
            "fee": real_text(self.paid.fee),
            "nativeTokenFee": real_text(self.paid.native),
            "nonce": self.nonce,
            "feeToken": self.paid.fee_token,
        });
        if self.kind == RowKind::Send {
            let (source, destination) = self.dexes;
            delta["sourceDex"] = source.into();
            delta["destinationDex"] = destination.into();
        }
        delta
    }
}

/// The ledger of `user`: rows from `start_ms` (and up to `end_ms`), oldest
/// first. `None` when the sim never held an account for the address.
pub(crate) fn rows_of(
    state: &SimState,
    user: Address,
    (start_ms, end_ms): (u64, Option<u64>),
) -> Option<Vec<LedgerUpdate>> {
    if !state.accounts.contains_key(&user) && !state.transfer_ledger.contains_key(&user) {
        return None;
    }
    let empty = VecDeque::new();
    let list = state.transfer_ledger.get(&user).unwrap_or(&empty);
    Some(
        list.iter()
            .filter(|r| r.time >= start_ms && end_ms.is_none_or(|end| r.time <= end))
            .cloned()
            .collect(),
    )
}

/// What `preTransferCheck` reads: the destination's standing and whether the
/// source still owes its activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreTransfer {
    pub destination_exists: bool,
    pub source_owes: bool,
    pub destination_has_sent: bool,
}

impl super::Sim {
    /// `preTransferCheck`: what a transfer from `source` to `destination`
    /// would meet now. The `/exchange` action adopts it with step 5.
    #[allow(dead_code)]
    pub(crate) fn pre_transfer(
        &self,
        destination: Address,
        source: Address,
    ) -> crate::error::Result<PreTransfer> {
        self.with_state(|state| {
            let exists = state.exists(destination);
            Ok(PreTransfer {
                destination_exists: exists,
                source_owes: state.activation_owed.contains(&source),
                destination_has_sent: exists && !state.activation_owed.contains(&destination),
            })
        })
    }

    /// The account's transfer rows for `userNonFundingLedgerUpdates`, oldest
    /// first; `None` when the sim never held the address (the caller passes
    /// the read through to the real venue). A poisoned lock also answers
    /// `None`. The `/info` read adopts it with step 6.
    #[allow(dead_code)]
    pub(crate) fn ledger_updates(
        &self,
        user: Address,
        window: (u64, Option<u64>),
    ) -> Option<Vec<LedgerUpdate>> {
        let state = self.state.lock().ok()?;
        rows_of(&state, user, window)
    }
}
