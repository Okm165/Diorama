//! The transfer matrix: every direction a balance can move (spot, the perp
//! clearinghouse under each dex name, another account, the EVM side), with
//! the balances, holds, fees and refusals each one carries. The fee rules are
//! researched in [`super::fees`]: an account first funded by a deposit or an
//! inbound transfer pays 1 USDC on top of its first outbound transfer (Circle),
//! a Core to EVM transfer costs HyperEVM gas.
//!
//! The sim's account plane keeps one USDC perp clearinghouse (the per-dex
//! ledger is the next account-plane step): a transfer names its dex (the
//! venue's `sourceDex`/`destinationDex` strings), every USDC dex moves the
//! same cash, and a USDH-collateral dex is refused by name. The tests that
//! need independent per-dex balances, the unified-ledger merge and the Core
//! precompiles are the next step's; they are not expressed here rather than
//! asserted against a state the sim does not build.
use super::account::Account;
use super::fees::{RowKind, ACTIVATION_FEE};
use super::order::{Order, Tif};
use super::transfer::{AssetSend, CoreToEvm, Wallet};
use super::{meta_of, meta_with, Applied, Deal, DealClass, Sim, SimState, TokenRef};
use crate::error::Error;
use crate::feed::MarketFeed;
use crate::wire::{L2Book, Level, SpotMeta, SpotPair, SpotToken, UserAbstraction};
use alloy_primitives::Address;
use rust_decimal::Decimal;
use std::collections::BTreeMap;

const USDC: TokenRef<'static> = TokenRef {
    index: 0,
    coin: "USDC",
};
const USDH: TokenRef<'static> = TokenRef {
    index: 360,
    coin: "USDH",
};
const HYPE: TokenRef<'static> = TokenRef {
    index: 150,
    coin: "HYPE",
};
const PURR: TokenRef<'static> = TokenRef {
    index: 1,
    coin: "PURR",
};

fn dec(text: &str) -> Decimal {
    text.parse().unwrap()
}

fn token(name: &str, index: u32) -> SpotToken {
    SpotToken {
        name: name.into(),
        sz_decimals: 8,
        wei_decimals: 8,
        index,
        is_canonical: true,
        evm_contract: None,
        token_id: None,
        full_name: None,
        deployer_trading_fee_share: Decimal::ZERO,
    }
}

fn book(coin: &str, px: &str) -> L2Book {
    let level = |p: &str| Level {
        px: dec(p),
        sz: dec("50"),
        n: 1,
    };
    L2Book {
        coin: coin.into(),
        time: 0,
        levels: [vec![level(px)], vec![level(px)]],
        spread: None,
    }
}

/// The default dex (`ETH`), two USDC builder dexes (`xyz`, `abc`) and a USDH
/// builder dex (`flx`); the spot registry lists USDC, PURR, HYPE and USDH, and
/// HYPE trades at 5 USDC (pair `@107`).
fn feed() -> MarketFeed {
    let feed = MarketFeed::memory(vec![
        "ETH".into(),
        "xyz:TSLA".into(),
        "abc:GOLD".into(),
        "flx:TSLA".into(),
    ]);
    let now = crate::now_ms();
    feed.put_meta(meta_with("ETH", 10), now);
    feed.put_dex_meta("xyz".into(), meta_of(&[("xyz:TSLA", 10)]), now);
    feed.put_dex_meta("abc".into(), meta_of(&[("abc:GOLD", 10)]), now);
    let mut flx = meta_of(&[("flx:TSLA", 10)]);
    flx.collateral_token = 360;
    feed.put_dex_meta("flx".into(), flx, now);
    for coin in ["ETH", "xyz:TSLA", "abc:GOLD", "flx:TSLA"] {
        feed.put_ctx(coin.into(), crate::wire::test_ctx("100", "100"), now);
        feed.put_book(book(coin, "100"), now);
    }
    feed.put_oi_caps(Vec::new());
    feed.put_spot_meta(SpotMeta {
        tokens: vec![
            token("USDC", 0),
            token("PURR", 1),
            token("HYPE", 150),
            token("USDH", 360),
        ],
        universe: vec![SpotPair {
            tokens: [150, 0],
            name: "@107".into(),
            index: 107,
            is_canonical: true,
        }],
    });
    feed.put_mids(BTreeMap::from([("@107".to_string(), dec("5"))]), now);
    feed
}

const ME: Address = Address::repeat_byte(0xa1);
const YOU: Address = Address::repeat_byte(0xb2);
const NEWCOMER: Address = Address::repeat_byte(0xc3);

fn spot(sim: &Sim, who: Address, token: TokenRef<'_>, amount: &str) {
    sim.credit_spot(who, token, dec(amount)).unwrap();
}

fn deal_perp(sim: &Sim, feed: &MarketFeed, who: Address, dex: Option<&str>, amount: &str) {
    // The sim's perp wallet is the USDC clearinghouse: the dex name is the
    // action's, not a separate balance yet.
    let _ = dex;
    sim.deal(
        feed,
        &Deal {
            address: who,
            class: DealClass::Perp,
            coin: None,
            amount: dec(amount),
        },
    )
    .unwrap();
}

fn perp(dex: &str) -> Wallet {
    Wallet::Perp(dex.to_string())
}

fn send(
    sim: &Sim,
    feed: &MarketFeed,
    (from, to): (Address, Address),
    (token, amount): (TokenRef<'_>, &str),
    (source, destination): (Wallet, Wallet),
) -> Result<(), Error> {
    sim.send_asset(
        &AssetSend {
            from,
            to,
            token,
            amount: dec(amount),
            source,
            destination,
            kind: RowKind::Send,
        },
        feed,
    )
}

fn refusal(result: Result<(), Error>) -> String {
    match result {
        Err(Error::BadRequest { reason }) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn account(sim: &Sim, who: Address) -> Account {
    sim.account(who).unwrap().unwrap()
}

fn spot_of(sim: &Sim, who: Address, token: TokenRef<'_>) -> Decimal {
    sim.account(who)
        .unwrap()
        .and_then(|a| a.spot.get(&token.index).map(|e| e.total))
        .unwrap_or_default()
}

fn perp_of(sim: &Sim, who: Address, dex: &str) -> Decimal {
    let _ = dex;
    sim.account(who)
        .unwrap()
        .map_or(Decimal::ZERO, |a| a.perp_balance)
}

fn owes(sim: &Sim, who: Address) -> bool {
    sim.with_state(|s| Ok(s.activation_owed.contains(&who)))
        .unwrap()
}

fn exists(sim: &Sim, who: Address) -> bool {
    sim.with_state(|s| Ok(s.exists(who))).unwrap()
}

/// The `delta` objects of an account's transfer ledger, oldest first.
fn deltas(sim: &Sim, who: Address) -> Vec<serde_json::Value> {
    sim.ledger_updates(who, (0, None))
        .unwrap_or_default()
        .into_iter()
        .map(|row| row.delta)
        .collect()
}

fn buy(coin: &str, size: &str) -> Order {
    Order {
        coin: coin.into(),
        is_buy: true,
        size: dec(size),
        limit_px: None,
        tif: Tif::Ioc,
        reduce_only: false,
        cloid: None,
        trigger: None,
    }
}

// ---- spot and the perp clearinghouse ----------------------------------------

/// An own-wallet move costs nothing (it is the account's own money): spot to a
/// USDC builder dex and back conserves the total, and each row carries the
/// venue's `sourceDex` / `destinationDex` names.
#[test]
fn own_wallet_moves_carry_the_dex_names_and_cost_nothing() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "100");

    send(&sim, &feed, (ME, ME), (USDC, "40"), (Wallet::Spot, perp("xyz"))).unwrap();
    assert_eq!(
        (spot_of(&sim, ME, USDC), perp_of(&sim, ME, "")),
        (dec("60"), dec("40"))
    );
    send(&sim, &feed, (ME, ME), (USDC, "15"), (perp("xyz"), Wallet::Spot)).unwrap();
    let a = account(&sim, ME);
    assert_eq!(a.spot[&0].total + a.perp_balance, dec("100"));
    assert_eq!(perp_of(&sim, ME, ""), dec("25"));

    let rows = deltas(&sim, ME);
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|r| r["fee"] == "0.0" && r["feeToken"] == ""));
    assert_eq!(rows[0]["sourceDex"], "spot");
    assert_eq!(rows[0]["destinationDex"], "xyz");
    assert_eq!(rows[1]["sourceDex"], "xyz");
    assert_eq!(rows[1]["destinationDex"], "spot");
}

/// The token must be the collateral of every perp endpoint: USDH does not go
/// to a USDC dex (or the default dex), and the USDH-collateral dex is outside
/// the single USDC clearinghouse.
#[test]
fn a_token_that_is_not_the_dexs_collateral_is_refused() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "100");
    spot(&sim, ME, USDH, "100");

    let reason = refusal(send(
        &sim,
        &feed,
        (ME, ME),
        (USDH, "10"),
        (Wallet::Spot, perp("xyz")),
    ));
    assert!(reason.contains("(0)"), "{reason}");
    refusal(send(
        &sim,
        &feed,
        (ME, ME),
        (USDH, "10"),
        (Wallet::Spot, perp("")),
    ));
    // The matching token reaches the USDH dex's endpoint, which this account
    // plane cannot express: the name says why, and nothing moves.
    let reason = refusal(send(
        &sim,
        &feed,
        (ME, ME),
        (USDH, "10"),
        (Wallet::Spot, perp("flx")),
    ));
    assert!(reason.contains("USDC clearinghouse only"), "{reason}");
    assert_eq!(
        (spot_of(&sim, ME, USDH), perp_of(&sim, ME, "")),
        (dec("100"), Decimal::ZERO)
    );
}

/// A zero, a negative and an unknown sender are refused and move nothing;
/// an unknown dex is a named refusal.
#[test]
fn degenerate_sends_are_refused() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "10");
    for amount in ["0", "-1"] {
        refusal(send(
            &sim,
            &feed,
            (ME, YOU),
            (USDC, amount),
            (Wallet::Spot, Wallet::Spot),
        ));
    }
    let reason = refusal(send(
        &sim,
        &feed,
        (ME, ME),
        (USDC, "1"),
        (Wallet::Spot, perp("nope")),
    ));
    assert!(
        reason.contains("not in the venue's dex registry"),
        "{reason}"
    );
    let reason = refusal(send(
        &sim,
        &feed,
        (NEWCOMER, ME),
        (USDC, "1"),
        (Wallet::Spot, Wallet::Spot),
    ));
    assert!(reason.contains("not a venue-sim account"), "{reason}");
    assert_eq!(spot_of(&sim, ME, USDC), dec("10"));
    assert!(sim.account(YOU).unwrap().is_none());
}

/// A resting order's hold stays in the spot wallet: only what is free moves.
#[test]
fn what_a_spot_hold_covers_cannot_be_sent() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "100");
    sim.with_account(ME, |a| {
        a.spot.get_mut(&0).unwrap().hold = dec("30");
        Ok(())
    })
    .unwrap();
    let reason = refusal(send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "80"),
        (Wallet::Spot, Wallet::Spot),
    ));
    assert!(reason.contains("may send now (70)"), "{reason}");
    // The free 70 goes whole: ME owes no activation.
    send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "70"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    assert_eq!(spot_of(&sim, ME, USDC), dec("30"), "the hold is untouched");
}

/// A unified account's spot USDC is one pool behind the positions of every dex
/// that margins against it: a position on `xyz` and one on the default dex both
/// lower what spot can send. A standard account's spot is free of its perp
/// margin.
#[test]
fn a_unified_spot_pool_keeps_the_maintenance_of_every_dex_sharing_the_token() {
    let feed = feed();
    let unified = Sim::memory();
    unified
        .set_mode(ME, UserAbstraction::UnifiedAccount)
        .unwrap();
    spot(&unified, ME, USDC, "100");
    unified.order(&feed, ME, &buy("ETH", "1")).unwrap();
    let one_dex = super::transfer::spot_free(&account(&unified, ME), &feed, 0).unwrap();
    unified.order(&feed, ME, &buy("xyz:TSLA", "1")).unwrap();
    let two_dexes = super::transfer::spot_free(&account(&unified, ME), &feed, 0).unwrap();
    assert!(
        one_dex < dec("100") && two_dexes < one_dex,
        "{one_dex} {two_dexes}"
    );
    let reason = refusal(send(
        &unified,
        &feed,
        (ME, ME),
        (USDC, "100"),
        (Wallet::Spot, perp("abc")),
    ));
    assert!(reason.contains("the spot wallet may send"), "{reason}");

    // Standard: the same 100 in spot is not margin for the perp clearinghouse.
    let standard = Sim::memory();
    spot(&standard, ME, USDC, "100");
    deal_perp(&standard, &feed, ME, None, "50");
    standard.order(&feed, ME, &buy("ETH", "1")).unwrap();
    assert_eq!(
        super::transfer::spot_free(&account(&standard, ME), &feed, 0).unwrap(),
        dec("100")
    );
}

/// The perp wallet keeps the margin its positions hold: with a position open
/// only the clearinghouse's withdrawable can leave, never the whole cash.
#[test]
fn the_margin_in_use_cannot_be_transferred_out() {
    let feed = feed();
    let sim = Sim::memory();
    deal_perp(&sim, &feed, ME, None, "100");
    // 1 ETH at 100, 10x: initial margin 10, notional 100.
    sim.order(&feed, ME, &buy("ETH", "1")).unwrap();

    // The transfer floor is max(initial 10, 10% of 100 = 10): about 90 may
    // move, never the whole balance.
    let reason = refusal(send(
        &sim,
        &feed,
        (ME, ME),
        (USDC, "95"),
        (perp(""), Wallet::Spot),
    ));
    assert!(
        reason.contains("exceeds what the default perp dex may send"),
        "{reason}"
    );
    send(
        &sim,
        &feed,
        (ME, ME),
        (USDC, "85"),
        (perp(""), Wallet::Spot),
    )
    .unwrap();
    assert_eq!(spot_of(&sim, ME, USDC), dec("85"));
}

// ---- the activation fee --------------------------------------------------------

/// Circle's rule: an account created by an inbound transfer is credited in
/// full and pays the 1 USDC activation on its first outbound transfer, on top
/// of the amount; the second is free. The destination's own newness never
/// matters (the funded sender pays nothing to send to a brand new address).
#[test]
#[allow(clippy::too_many_lines)] // one script: the sends in order, then the rows they wrote
fn an_account_pays_its_activation_on_its_first_outbound_transfer() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "100");
    assert!(!exists(&sim, YOU));
    // ME was funded by a helper: it owes nothing, and YOU being new costs it nothing.
    send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "10"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    assert_eq!(
        (spot_of(&sim, ME, USDC), spot_of(&sim, YOU, USDC)),
        (dec("90"), dec("10")),
        "the inbound amount is credited in full"
    );
    assert!(exists(&sim, YOU));
    assert!(owes(&sim, YOU) && !owes(&sim, ME));
    // YOU's first outbound transfer (to an existing account) pays 1 on top.
    send(
        &sim,
        &feed,
        (YOU, ME),
        (USDC, "4"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    assert_eq!(
        (spot_of(&sim, YOU, USDC), spot_of(&sim, ME, USDC)),
        (dec("5"), dec("94"))
    );
    assert!(!owes(&sim, YOU), "the debt is paid once");
    send(
        &sim,
        &feed,
        (YOU, ME),
        (USDC, "1"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    assert_eq!(spot_of(&sim, YOU, USDC), dec("4"), "the second one is free");
    let rows = deltas(&sim, YOU);
    assert_eq!(rows.len(), 3);
    let paid: Vec<_> = rows
        .iter()
        .map(|r| (r["fee"].clone(), r["feeToken"].clone()))
        .collect();
    assert_eq!(
        paid,
        [
            ("0.0".into(), "".into()),
            ("1.0".into(), "USDC".into()),
            ("0.0".into(), "".into())
        ]
    );
    assert_eq!(rows[1]["nativeTokenFee"], "0.0");
    assert_eq!(rows[1]["amount"], "4.0");
    assert_eq!(
        deltas(&sim, ME)[1],
        rows[1],
        "both parties see the same row"
    );
}

/// The first outbound transfer needs the amount plus the fee: 10 from an
/// account that holds exactly 10 is refused (nothing moves, the debt stays),
/// 9 goes through (Circle: "the first outbound action requires a balance of at
/// least 1 USDC").
#[test]
fn the_first_outbound_transfer_needs_the_amount_plus_the_fee() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "10");
    send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "10"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    let reason = refusal(send(
        &sim,
        &feed,
        (YOU, ME),
        (USDC, "10"),
        (Wallet::Spot, Wallet::Spot),
    ));
    assert!(
        reason.contains("plus the 1 USDC activation fee"),
        "{reason}"
    );
    assert_eq!(spot_of(&sim, YOU, USDC), dec("10"));
    assert!(owes(&sim, YOU));
    send(
        &sim,
        &feed,
        (YOU, ME),
        (USDC, "9"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    assert_eq!(spot_of(&sim, YOU, USDC), Decimal::ZERO);
    assert!(!owes(&sim, YOU));
}

/// A deposit under 1 USDC is credited (no minimum) and the first outbound
/// transfer then fails until the balance covers amount plus fee.
#[test]
fn a_dust_deposit_succeeds_and_its_first_outbound_transfer_fails() {
    let feed = feed();
    let sim = Sim::memory();
    sim.credit_evm_deposit(ME, USDC, dec("0.5"), &feed).unwrap();
    assert_eq!(spot_of(&sim, ME, USDC), dec("0.5"));
    refusal(send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "0.1"),
        (Wallet::Spot, Wallet::Spot),
    ));
    assert_eq!(core_to_evm(&sim, &feed, USDC, "0.1"), Applied::Refused);
}

/// `spotSend` writes a `spotTransfer` row and `usdSend` (a perp wallet to a
/// perp wallet) a `send` row with its dexes; both are outbound transfers that
/// pay the owed activation, from the sending wallet.
#[test]
#[allow(clippy::too_many_lines)] // one script: the sends in order, then the rows they wrote
fn spot_send_and_usd_send_pay_the_owed_activation() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "100");
    // YOU is created by a spot receipt, NEWCOMER by a perp-wallet one.
    sim.spot_send((ME, YOU), USDC, dec("10"), &feed)
        .unwrap();
    send(
        &sim,
        &feed,
        (ME, NEWCOMER),
        (USDC, "10"),
        (Wallet::Spot, perp("")),
    )
    .unwrap();
    assert!(owes(&sim, YOU) && owes(&sim, NEWCOMER));
    assert_eq!(
        sim.spot_send((YOU, ME), USDC, dec("3"), &feed).unwrap(),
        Applied::Applied
    );
    assert_eq!(spot_of(&sim, YOU, USDC), dec("6"));
    let row = deltas(&sim, YOU).pop().unwrap();
    assert_eq!(
        (
            row["type"].clone(),
            row["fee"].clone(),
            row["feeToken"].clone()
        ),
        ("spotTransfer".into(), "1.0".into(), "USDC".into())
    );
    // usdSend: NEWCOMER's perp wallet to ME's.
    send(
        &sim,
        &feed,
        (NEWCOMER, ME),
        (USDC, "4"),
        (perp(""), perp("")),
    )
    .unwrap();
    assert_eq!(
        perp_of(&sim, NEWCOMER, ""),
        dec("5"),
        "the fee came out of the sending wallet"
    );
    let row = deltas(&sim, NEWCOMER).pop().unwrap();
    assert_eq!(
        (
            row["type"].clone(),
            row["sourceDex"].clone(),
            row["fee"].clone()
        ),
        ("send".into(), "".into(), "1.0".into())
    );
}

/// The fee is 1 USDC whatever is sent: an account created by a PURR receipt
/// takes the fee from its spot USDC on its first outbound PURR send, and is
/// refused (nothing moves, the debt stays) while it has none.
#[test]
#[allow(clippy::too_many_lines)] // one script: the sends in order, then the rows they wrote
fn a_non_usdc_first_send_pays_the_fee_from_spot_usdc() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, PURR, "50");
    spot(&sim, ME, USDC, "10");
    send(
        &sim,
        &feed,
        (ME, YOU),
        (PURR, "20"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    assert_eq!(
        spot_of(&sim, ME, USDC),
        dec("10"),
        "the sender paid nothing"
    );
    let reason = refusal(send(
        &sim,
        &feed,
        (YOU, ME),
        (PURR, "5"),
        (Wallet::Spot, Wallet::Spot),
    ));
    assert!(reason.contains("activation fee"), "{reason}");
    assert_eq!(spot_of(&sim, YOU, PURR), dec("20"));
    assert!(owes(&sim, YOU));
    send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "3"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    send(
        &sim,
        &feed,
        (YOU, ME),
        (PURR, "5"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    assert_eq!(
        (spot_of(&sim, YOU, PURR), spot_of(&sim, YOU, USDC)),
        (dec("15"), dec("2"))
    );
    let row = deltas(&sim, YOU).pop().unwrap();
    assert_eq!(
        (
            row["token"].clone(),
            row["fee"].clone(),
            row["feeToken"].clone()
        ),
        ("PURR".into(), "1.0".into(), "USDC".into())
    );
}

/// An account's own wallet moves are not outbound transfers: they pay nothing
/// and keep the debt for the first real send.
#[test]
fn an_own_transfer_neither_pays_nor_clears_the_activation() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "20");
    send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "10"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    send(
        &sim,
        &feed,
        (YOU, YOU),
        (USDC, "10"),
        (Wallet::Spot, perp("")),
    )
    .unwrap();
    assert_eq!(perp_of(&sim, YOU, ""), dec("10"));
    assert!(owes(&sim, YOU));
    // The fee is then paid from the wallet the transfer leaves.
    send(
        &sim,
        &feed,
        (YOU, ME),
        (USDC, "5"),
        (perp(""), Wallet::Spot),
    )
    .unwrap();
    assert_eq!(perp_of(&sim, YOU, ""), dec("4"));
    assert!(!owes(&sim, YOU));
}

/// An inbound transfer into a builder dex's wallet creates the account too,
/// and the row names the destination dex.
#[test]
fn an_inbound_send_into_a_builder_dex_creates_the_account_and_its_debt() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "20");
    send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "10"),
        (Wallet::Spot, perp("xyz")),
    )
    .unwrap();
    assert_eq!(
        (spot_of(&sim, ME, USDC), perp_of(&sim, YOU, "")),
        (dec("10"), dec("10"))
    );
    assert_eq!(deltas(&sim, YOU)[0]["destinationDex"], "xyz");
    assert!(exists(&sim, YOU) && owes(&sim, YOU));
}

/// Accounts that were funded by a helper (`deal`, a seed) or by a send while
/// they already existed owe nothing: only the first creation by inbound funds
/// sets the debt.
#[test]
fn helpers_and_later_receipts_never_create_the_debt() {
    let feed = feed();
    let sim = Sim::memory();
    deal_perp(&sim, &feed, ME, None, "50");
    spot(&sim, YOU, USDC, "5");
    assert!(!owes(&sim, ME) && !owes(&sim, YOU));
    send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "10"),
        (perp(""), Wallet::Spot),
    )
    .unwrap();
    assert!(!owes(&sim, YOU), "YOU existed before the receipt");
    send(
        &sim,
        &feed,
        (YOU, ME),
        (USDC, "15"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    assert_eq!(spot_of(&sim, YOU, USDC), Decimal::ZERO, "no fee was due");
}

/// A state saved before the fee existed loads with every account existing and
/// nothing owed; the new fields round-trip through the file.
#[test]
fn a_saved_state_owes_nothing() {
    let sim = Sim::memory();
    spot(&sim, YOU, USDC, "1");
    let state = sim.with_state(|s| Ok(s.clone())).unwrap();
    let mut json = serde_json::to_value(&state).unwrap();
    for key in [
        "activated",
        "activation_owed",
        "transfer_ledger",
        "next_ledger_id",
    ] {
        json.as_object_mut().unwrap().remove(key);
    }
    let mut old: SimState = serde_json::from_value(json).unwrap();
    assert!(old.activated.is_empty(), "the field defaults empty");
    old.migrate();
    assert!(old.activated.contains(&YOU));
    assert!(old.exists(YOU) && !old.exists(NEWCOMER));
    assert_eq!(old.activation_due(YOU, ME), Decimal::ZERO);
    assert!(old.activation_owed.is_empty());
    old.activation_owed.insert(NEWCOMER);
    assert_eq!(old.activation_due(NEWCOMER, ME), ACTIVATION_FEE);
    assert_eq!(old.activation_due(NEWCOMER, NEWCOMER), Decimal::ZERO);
    let again: SimState = serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
    assert_eq!(again, old);
}

// ---- Core to EVM and EVM to Core ------------------------------------------------

fn core_to_evm(sim: &Sim, feed: &MarketFeed, token: TokenRef<'_>, amount: &str) -> Applied {
    let destination = if token.coin == "HYPE" {
        crate::bridge::system::HYPE_SYSTEM
    } else {
        crate::bridge::system::system_address(u64::from(token.index))
    };
    sim.debit_core_to_evm(
        &CoreToEvm {
            from: ME,
            destination,
            token,
            amount: dec(amount),
            kind: RowKind::Send,
        },
        feed,
    )
    .unwrap()
}

/// Core to EVM costs 200k gas at 0.1 gwei = 0.00002 HYPE. A sender with HYPE
/// pays it in HYPE (`nativeTokenFee`), the token arrives whole.
#[test]
fn core_to_evm_gas_is_paid_in_hype_when_the_sender_holds_it() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "100");
    spot(&sim, ME, HYPE, "1");
    assert_eq!(core_to_evm(&sim, &feed, USDC, "100"), Applied::Applied);
    assert_eq!(
        spot_of(&sim, ME, USDC),
        Decimal::ZERO,
        "the whole amount left"
    );
    assert_eq!(spot_of(&sim, ME, HYPE), dec("0.99998"));
    let row = &deltas(&sim, ME)[0];
    assert_eq!(
        row["destination"],
        "0x2000000000000000000000000000000000000000"
    );
    assert_eq!(row["nativeTokenFee"], "0.00002");
    assert_eq!(row["fee"], "0.0");
    assert_eq!(row["feeToken"], "");
}

/// Without HYPE the gas is taken in the token sent, valued at the HYPE price
/// (0.00002 HYPE x 5 = 0.0001 USDC), so the amount plus the fee must fit.
#[test]
fn core_to_evm_gas_without_hype_is_taken_in_the_token() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "100");
    // The whole balance does not cover the amount and the gas.
    assert_eq!(core_to_evm(&sim, &feed, USDC, "100"), Applied::Refused);
    assert_eq!(
        spot_of(&sim, ME, USDC),
        dec("100"),
        "a refusal moves nothing"
    );
    assert!(deltas(&sim, ME).is_empty());
    assert_eq!(core_to_evm(&sim, &feed, USDC, "99"), Applied::Applied);
    assert_eq!(spot_of(&sim, ME, USDC), dec("0.9999"));
    let row = &deltas(&sim, ME)[0];
    assert_eq!(
        (
            row["fee"].clone(),
            row["feeToken"].clone(),
            row["nativeTokenFee"].clone()
        ),
        ("0.0001".into(), "USDC".into(), "0.0".into())
    );
}

/// HYPE itself leaves with no fee (every real HYPE row carries none), to its
/// own system address `0x2222...`.
#[test]
fn hype_leaves_core_with_no_gas_fee() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, HYPE, "2");
    assert_eq!(core_to_evm(&sim, &feed, HYPE, "2"), Applied::Applied);
    assert_eq!(spot_of(&sim, ME, HYPE), Decimal::ZERO);
    let row = &deltas(&sim, ME)[0];
    assert_eq!(
        row["destination"],
        "0x2222222222222222222222222222222222222222"
    );
    assert_eq!(row["fee"], "0.0");
    assert_eq!(row["nativeTokenFee"], "0.0");
}

/// An account a deposit created owes its activation: its first Core to EVM
/// transfer pays 1 USDC more, the second does not, and an account that
/// existed already never owes it.
#[test]
fn a_deposited_account_pays_its_activation_on_its_first_outbound_transfer() {
    let feed = feed();
    let sim = Sim::memory();
    sim.credit_evm_deposit(ME, USDC, dec("10"), &feed).unwrap();
    // The deposit is credited whole, and the system address's row is its record.
    assert_eq!(spot_of(&sim, ME, USDC), dec("10"));
    let row = &deltas(&sim, ME)[0];
    assert_eq!(row["type"], "spotTransfer");
    assert_eq!(row["user"], "0x2000000000000000000000000000000000000000");
    assert_eq!(row["destination"], format!("{ME:#x}"));
    assert_eq!(row["fee"], "0.0");
    // 10 is not enough for 9.5 + 1 + gas, and it moves nothing.
    assert_eq!(core_to_evm(&sim, &feed, USDC, "9.5"), Applied::Refused);
    assert_eq!(spot_of(&sim, ME, USDC), dec("10"));
    assert_eq!(core_to_evm(&sim, &feed, USDC, "5"), Applied::Applied);
    assert_eq!(
        spot_of(&sim, ME, USDC),
        dec("3.9999"),
        "5 + 1 + 0.0001 left"
    );
    let row = &deltas(&sim, ME)[1];
    assert_eq!(row["fee"], "1.0001");
    // The second one owes only gas.
    assert_eq!(core_to_evm(&sim, &feed, USDC, "1"), Applied::Applied);
    assert_eq!(deltas(&sim, ME)[2]["fee"], "0.0001");

    // A deposit into an account that already existed owes nothing.
    let existing = Sim::memory();
    spot(&existing, ME, USDC, "5");
    existing
        .credit_evm_deposit(ME, USDC, dec("5"), &feed)
        .unwrap();
    assert!(existing
        .with_state(|s| Ok(s.activation_owed.is_empty()))
        .unwrap());
}

/// A CoreDepositWallet deposit names the wallet it lands in: the perp wallet
/// credits the perp balance and no spot, the row records the send from the
/// system address; the spot wallet stays the plain deposit's default.
#[test]
fn a_deposit_can_land_in_the_perp_wallet_instead_of_spot() {
    let feed = feed();
    let sim = Sim::memory();
    sim.credit_evm_deposit_to((ME, &Wallet::Perp(String::new())), USDC, dec("10"), &feed)
        .unwrap();
    assert_eq!(perp_of(&sim, ME, ""), dec("10"));
    assert_eq!(spot_of(&sim, ME, USDC), Decimal::ZERO);
    let row = &deltas(&sim, ME)[0];
    assert_eq!(row["type"], "send");
    assert_eq!(row["user"], "0x2000000000000000000000000000000000000000");
    assert_eq!(row["destination"], format!("{ME:#x}"));
    assert_eq!(row["sourceDex"], "spot");
    assert_eq!(row["destinationDex"], "");
    // The account exists from now on and owes its activation, as for a spot deposit.
    assert!(owes(&sim, ME));

    // The spot wallet stays the default of the plain deposit.
    sim.credit_evm_deposit_to((ME, &Wallet::Spot), USDC, dec("1"), &feed)
        .unwrap();
    assert_eq!(spot_of(&sim, ME, USDC), dec("1"));
    assert_eq!(deltas(&sim, ME)[1]["type"], "spotTransfer");
}

/// The debt follows the account, not the destination: a deposited account's
/// first send to a brand new address pays the 1 USDC once, the new address
/// owes its own, and the second send from the first account is free.
#[test]
fn the_debt_moves_with_the_account_not_the_destination() {
    let feed = feed();
    let sim = Sim::memory();
    sim.credit_evm_deposit(ME, USDC, dec("10"), &feed).unwrap();
    send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "5"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    assert_eq!(spot_of(&sim, ME, USDC), dec("4"), "5 and the 1 USDC fee");
    assert_eq!(deltas(&sim, ME)[1]["fee"], "1.0");
    assert!(!owes(&sim, ME) && owes(&sim, YOU));
    send(
        &sim,
        &feed,
        (ME, YOU),
        (USDC, "1"),
        (Wallet::Spot, Wallet::Spot),
    )
    .unwrap();
    assert_eq!(
        spot_of(&sim, ME, USDC),
        dec("3"),
        "nothing is owed any more"
    );
    assert_eq!(spot_of(&sim, YOU, USDC), dec("6"));
}

/// A HYPE deposit is recorded from `0x2222...`; a unified account's Core to EVM
/// transfer is bounded by what its positions leave free.
#[test]
fn hype_deposits_come_from_the_hype_system_address_and_unified_bounds_apply() {
    let feed = feed();
    let sim = Sim::memory();
    sim.credit_evm_deposit(YOU, HYPE, dec("3"), &feed).unwrap();
    assert_eq!(
        deltas(&sim, YOU)[0]["user"],
        "0x2222222222222222222222222222222222222222"
    );
    sim.set_mode(ME, UserAbstraction::UnifiedAccount).unwrap();
    spot(&sim, ME, USDC, "100");
    spot(&sim, ME, HYPE, "1");
    sim.order(&feed, ME, &buy("ETH", "1")).unwrap();
    let free = super::transfer::spot_free(&account(&sim, ME), &feed, 0).unwrap();
    assert!(free < dec("100"), "{free}");
    assert_eq!(core_to_evm(&sim, &feed, USDC, "100"), Applied::Refused);
    assert_eq!(
        core_to_evm(&sim, &feed, USDC, &free.floor().to_string()),
        Applied::Applied
    );
}

// ---- the ledger ----------------------------------------------------------------

/// A usdClassTransfer writes an `accountClassTransfer` row, and the window and
/// end bound of `userNonFundingLedgerUpdates` apply; an address the sim never
/// held is `None` (the API passes the read through).
#[test]
fn the_ledger_serves_class_transfers_in_the_requested_window() {
    let feed = feed();
    let sim = Sim::memory();
    spot(&sim, ME, USDC, "10");
    sim.usd_class_transfer(
        ME,
        &super::ClassTransfer {
            token: USDC,
            amount: dec("4"),
            direction: super::Direction::ToPerp,
        },
        &feed,
    )
    .unwrap();
    let rows = sim.ledger_updates(ME, (0, None)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].delta["type"], "accountClassTransfer");
    assert_eq!(rows[0].delta["usdc"], "4.0");
    assert_eq!(rows[0].delta["toPerp"], true);
    let time = rows[0].time;
    assert_eq!(sim.ledger_updates(ME, (time + 1, None)).unwrap().len(), 0);
    assert_eq!(
        sim.ledger_updates(ME, (0, Some(time - 1))).unwrap().len(),
        0
    );
    assert!(sim.ledger_updates(NEWCOMER, (0, None)).is_none());
}
