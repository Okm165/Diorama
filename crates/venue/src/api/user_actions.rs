//! The `/exchange` transfer and account-setup actions: the user-signed ones
//! (`usdClassTransfer`, `sendAsset`, `spotSend`, `usdSend`, `approveAgent`,
//! `approveBuilderFee`, `userSetAbstraction`, the refused `withdraw3`) and the
//! agent-signed `agentSetAbstraction`. Each acts for the account the signer
//! resolved to (`exchange.rs`) and answers the venue's no-data success, or its
//! `err` envelope. The effects are the sim's own (`sim::transfer`, the
//! CoreWriter bridge's setters), so an HTTP transfer and its CoreWriter twin
//! move the same ledgers by the same rules.
use super::exchange::{default_ok, err_envelope};
use super::ApiState;
use crate::sim::fees::RowKind;
use crate::sim::transfer::{AssetSend, Wallet};
use crate::sim::{Applied, ClassTransfer, Direction, TokenRef};
use crate::wire::UserAbstraction;
use alloy_primitives::Address;
use axum::response::Response;
use rust_decimal::Decimal;

/// The venue's no-data success, or its `err` envelope carrying the reason.
fn answer(result: Result<(), String>) -> Response {
    match result {
        Ok(()) => default_ok(),
        Err(reason) => err_envelope(reason),
    }
}

/// A wire amount: a decimal string (the venue's own form).
fn amount_of(text: &str) -> Result<Decimal, String> {
    text.parse::<Decimal>()
        .map_err(|_| format!("venue-sim: amount `{text}` is not a decimal"))
}

/// USDC: spot token 0, the default dex's collateral.
const USDC: TokenRef<'static> = TokenRef {
    index: 0,
    coin: "USDC",
};

/// `usdClassTransfer`: USDC between the account's spot wallet and its
/// default perp dex.
// step-9: the venue disables this action on a unified account (one USDC
// pool) with `UNIFIED_ACTION_REFUSAL`; the unified switch itself is deferred,
// so the guard lands with it.
pub(super) fn usd_class_transfer(
    state: &ApiState,
    account: Address,
    amount: &str,
    to_perp: bool,
) -> Response {
    answer((|| {
        let amount = amount_of(amount)?;
        if amount <= Decimal::ZERO {
            return Err("venue-sim: the transfer amount must be positive".to_string());
        }
        let direction = if to_perp {
            Direction::ToPerp
        } else {
            Direction::ToSpot
        };
        let applied = state
            .sim
            .usd_class_transfer(
                account,
                &ClassTransfer {
                    token: USDC,
                    amount,
                    direction,
                },
                &state.feed,
            )
            .map_err(|e| e.to_string())?;
        match applied {
            Applied::Applied => Ok(()),
            Applied::Refused => Err(format!(
                "venue-sim: usdClassTransfer of {amount} USDC refused: the source wallet cannot \
                 send it now (its balance, less the margin its positions hold)"
            )),
        }
    })())
}

/// One transfer between two wallets (the token named the way the actions
/// name it, `NAME` or `NAME:tokenId`).
fn transfer(
    state: &ApiState,
    (from, to): (Address, Address),
    (token, amount): (&str, &str),
    (source, destination): (Wallet, Wallet),
    kind: RowKind,
) -> Result<(), String> {
    let (index, name) = state
        .feed
        .resolve_token(token)
        .ok_or_else(|| format!("venue-sim: unknown token `{token}`"))?;
    let amount = amount_of(amount)?;
    if crate::bridge::system::token_index_of(to).is_some()
        || to == crate::bridge::system::HYPE_SYSTEM
    {
        return Err(
            "venue-sim: a token's system address is the Core to EVM path, not an \
                    /exchange destination; send it from the spot wallet with CoreWriter's \
                    sendAsset"
                .to_string(),
        );
    }
    state
        .sim
        .send_asset(
            &AssetSend {
                from,
                to,
                token: TokenRef { index, coin: &name },
                amount,
                source,
                destination,
                kind,
            },
            &state.feed,
        )
        .map_err(|e| e.to_string())
}

/// The wire fields of a `sendAsset`.
pub(super) struct SendAsset<'a> {
    pub destination: Address,
    pub source_dex: &'a str,
    pub destination_dex: &'a str,
    pub token: &'a str,
    pub amount: &'a str,
    pub from_sub_account: &'a str,
}

/// `sendAsset`: a token between two wallets (a dex's clearinghouse or the
/// spot wallet), to the account itself or to another.
pub(super) fn send_asset(state: &ApiState, account: Address, send: &SendAsset<'_>) -> Response {
    answer((|| {
        if !send.from_sub_account.is_empty() {
            return Err(
                "venue-sim: `fromSubAccount` is not modeled (the sim does not track \
                        subaccount ownership); sign with the account the funds are in"
                    .to_string(),
            );
        }
        transfer(
            state,
            (account, send.destination),
            (send.token, send.amount),
            (
                Wallet::of_dex_name(send.source_dex),
                Wallet::of_dex_name(send.destination_dex),
            ),
            RowKind::Send,
        )
    })())
}

/// `spotSend`: a spot token to another account's spot wallet.
// step-9: the venue disables the API-signed `spotSend` on a unified account
// (any token, any destination) with `UNIFIED_ACTION_REFUSAL`; the unified
// switch itself is deferred, so the guard lands with it.
pub(super) fn spot_send(
    state: &ApiState,
    account: Address,
    destination: Address,
    (token, amount): (&str, &str),
) -> Response {
    answer(transfer(
        state,
        (account, destination),
        (token, amount),
        (Wallet::Spot, Wallet::Spot),
        RowKind::SpotTransfer,
    ))
}

/// `usdSend`: USDC from the account's default perp dex to another account's.
pub(super) fn usd_send(
    state: &ApiState,
    account: Address,
    destination: Address,
    amount: &str,
) -> Response {
    answer(transfer(
        state,
        (account, destination),
        (USDC.coin, amount),
        (Wallet::of_dex_name(""), Wallet::of_dex_name("")),
        RowKind::Send,
    ))
}

/// `withdraw3` bridges USDC out through Arbitrum, which the devnet does not
/// run: refused by name, with the path that does exist.
pub(super) fn withdraw3() -> Response {
    err_envelope(
        "venue-sim: withdraw3 bridges USDC out through Arbitrum, which the devnet does not \
         run; to move funds to the EVM side, send the token from the spot wallet to its \
         system address (CoreWriter sendAsset). See docs/INTEGRATION.md."
            .to_string(),
    )
}

/// `approveAgent`: register the API wallet on the account (CoreWriter action
/// 9's effect).
pub(super) fn approve_agent(
    state: &ApiState,
    account: Address,
    agent: Address,
    name: Option<&str>,
) -> Response {
    answer(
        state
            .sim
            .note_agent(account, agent, name.unwrap_or_default())
            .map_err(|e| e.to_string()),
    )
}

/// The most a builder fee may be approved up to, in tenths of a basis point
/// (1%, the venue's spot cap; the perp cap, 0.1%, gates the orders).
const MAX_APPROVED_FEE: u64 = 1_000;

/// `maxFeeRate` (`"0.01%"`) in tenths of a basis point (`10`): 0.001% is one.
fn fee_tenths(text: &str) -> Result<u64, String> {
    let percent = text
        .strip_suffix('%')
        .and_then(|p| p.parse::<Decimal>().ok())
        .ok_or_else(|| format!("venue-sim: maxFeeRate `{text}` is not a percent like \"0.01%\""))?;
    let tenths = percent * Decimal::from(1_000);
    if tenths.fract() != Decimal::ZERO || tenths.is_sign_negative() {
        return Err(format!(
            "venue-sim: maxFeeRate `{text}` is not a whole multiple of 0.001%"
        ));
    }
    u64::try_from(tenths)
        .ok()
        .filter(|t| *t <= MAX_APPROVED_FEE)
        .ok_or_else(|| format!("venue-sim: maxFeeRate `{text}` is above the 1% cap"))
}

/// `approveBuilderFee`: the account's maximum fee for `builder` (CoreWriter
/// action 12's effect: the maximum is *set*, and 0% revokes it).
pub(super) fn approve_builder_fee(
    state: &ApiState,
    account: Address,
    builder: Address,
    max_fee_rate: &str,
) -> Response {
    answer(fee_tenths(max_fee_rate).and_then(|tenths| {
        state
            .sim
            .approve_builder_fee(account, builder, tenths)
            .map_err(|e| e.to_string())
    }))
}

/// Set the account's mode; portfolio margin is refused by the sim.
fn set_mode(state: &ApiState, account: Address, mode: UserAbstraction) -> Response {
    answer(state.sim.set_mode(account, mode).map_err(|e| e.to_string()))
}

/// `userSetAbstraction`: the signer sets its own account's mode.
pub(super) fn user_set_abstraction(
    state: &ApiState,
    account: Address,
    user: Address,
    abstraction: &str,
) -> Response {
    if user != account {
        return err_envelope(format!(
            "venue-sim: userSetAbstraction names {user}, but the account that signed it is \
             {account}"
        ));
    }
    match serde_json::from_value::<UserAbstraction>(abstraction.into()) {
        Ok(mode) => set_mode(state, account, mode),
        Err(_) => err_envelope(format!(
            "venue-sim: unknown abstraction `{abstraction}` (default, disabled, unifiedAccount, \
             portfolioMargin, dexAbstraction)"
        )),
    }
}

/// `agentSetAbstraction`: the account's agent sets its mode (`i` disabled,
/// `u` unified, `p` portfolio margin: refused, as on every other path).
pub(super) fn agent_set_abstraction(
    state: &ApiState,
    account: Address,
    abstraction: &str,
) -> Response {
    let mode = match abstraction {
        "i" => UserAbstraction::Disabled,
        "u" => UserAbstraction::UnifiedAccount,
        "p" => UserAbstraction::PortfolioMargin,
        other => {
            return err_envelope(format!(
                "venue-sim: unknown abstraction `{other}` (\"i\" disabled, \"u\" unified, \
                 \"p\" portfolio margin)"
            ));
        }
    };
    set_mode(state, account, mode)
}
