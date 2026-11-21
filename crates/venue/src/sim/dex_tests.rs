//! The per-(account, dex) clearinghouse: a builder dex's coins trade against
//! that dex's own ledger, in its own collateral token, and nothing crosses
//! between dexes (the real venue keeps one clearinghouse per user and dex).
use super::account::{DexLedger, Position, SpotEntry};
use super::order::{Order, Tif};
use super::{feed_with_funding, meta_of, meta_with, Deal, DealClass, DexKey, Sim, SimState};
use crate::feed::MarketFeed;
use crate::wire::{L2Book, Level, LeverageType, SpotMeta, SpotToken, UserAbstraction};
use alloy_primitives::Address;
use rust_decimal::Decimal;

fn dec(text: &str) -> Decimal {
    text.parse().unwrap()
}

fn book(coin: &str, bid: (&str, &str), ask: (&str, &str)) -> L2Book {
    let level = |(px, sz): (&str, &str)| Level {
        px: dec(px),
        sz: dec(sz),
        n: 1,
    };
    L2Book {
        coin: coin.into(),
        time: 0,
        levels: [vec![level(bid)], vec![level(ask)]],
        spread: None,
    }
}

fn spot_token(name: &str, index: u32) -> SpotToken {
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

/// A feed with the default dex (`ETH`, 10x) and a builder dex `flx` whose
/// collateral is token 360 (`USDH`) listing `flx:TSLA` (10x). Both coins have
/// a live mark and a book; the spot meta names the two tokens.
fn two_dex_feed() -> MarketFeed {
    let feed = MarketFeed::memory(Vec::new());
    let now = crate::now_ms();
    feed.put_meta(meta_with("ETH", 10), now);
    let mut flx = meta_of(&[("flx:TSLA", 10)]);
    flx.collateral_token = 360;
    feed.put_dex_meta(("flx").to_string(), flx, now);
    for (coin, px) in [("ETH", "100"), ("flx:TSLA", "200")] {
        feed.put_ctx(coin.into(), crate::wire::test_ctx(px, px), now);
    }
    feed.put_book(book("ETH", ("99", "50"), ("100", "50")), now);
    feed.put_book(book("flx:TSLA", ("199", "50"), ("200", "50")), now);
    feed.put_oi_caps(Vec::new());
    feed.put_oi_caps_of("flx", Vec::new());
    feed.put_spot_meta(SpotMeta {
        tokens: vec![spot_token("USDC", 0), spot_token("USDH", 360)],
        universe: vec![],
    });
    feed
}

fn deal(sim: &Sim, feed: &MarketFeed, who: Address, dex: Option<&str>, amount: &str) {
    sim.deal(
        feed,
        &Deal {
            address: who,
            class: DealClass::Perp,
            coin: None,
            dex: dex.map(str::to_string),
            amount: dec(amount),
        },
    )
    .unwrap();
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

fn long(entry: &str, margin: &str, leverage: u32) -> Position {
    Position {
        szi: Decimal::ONE,
        entry_px: dec(entry),
        leverage: LeverageType::Cross,
        leverage_value: leverage,
        margin_used: dec(margin),
        isolated_margin: Decimal::ZERO,
        funding_all_time: Decimal::ZERO,
        funding_since_open: Decimal::ZERO,
        funding_since_change: Decimal::ZERO,
    }
}

/// **A builder dex's account has its own ledger, and an order opens a
/// position only against that dex's balance.** The default dex holds nothing,
/// so an `ETH` order is refused while the `flx:TSLA` one fills against the
/// `flx` balance; the fill is paid in the dex's collateral token.
#[test]
fn a_builder_dex_order_trades_against_that_dexs_balance_only() {
    let feed = two_dex_feed();
    let sim = Sim::memory();
    let a = Address::repeat_byte(0xa1);
    deal(&sim, &feed, a, Some("flx"), "100");

    let account = sim.account(a).unwrap().unwrap();
    assert_eq!(account.perp_balance("flx"), dec("100"));
    assert_eq!(account.perp_balance(""), Decimal::ZERO, "no default ledger");

    // The default dex has no margin: the `flx` balance does not back it.
    assert!(
        sim.order(&feed, a, &buy("ETH", "0.5")).is_err(),
        "a builder balance must not margin a default-dex order"
    );

    // $100 notional at 10x needs $10 of the `flx` balance.
    let fill = sim.order(&feed, a, &buy("flx:TSLA", "0.5")).unwrap();
    assert_eq!(fill.total_sz, dec("0.5"));
    let account = sim.account(a).unwrap().unwrap();
    assert!(account.positions.contains_key("flx:TSLA"));
    assert_eq!(account.perp_balance(""), Decimal::ZERO);
    let paid = account.perp_balance("flx");
    assert!(
        paid < dec("100") && paid > dec("99"),
        "the fee came out of the flx ledger, got {paid}"
    );
    assert_eq!(
        account.fills.back().unwrap().fee_token,
        "USDH",
        "the fee is paid in the dex's collateral token"
    );

    // Each dex serves its own clearinghouse.
    let flx = super::account::clearinghouse(&account, &feed, "flx", 1).unwrap();
    assert_eq!(flx.asset_positions.len(), 1);
    let default = super::account::clearinghouse(&account, &feed, "", 1).unwrap();
    assert!(default.asset_positions.is_empty());
    assert_eq!(default.margin_summary.account_value, Decimal::ZERO);
}

/// The default and builder balances never mix: funding the default dex does
/// not back a `flx` order, and a default-dex fill leaves the `flx` ledger
/// untouched.
#[test]
fn the_default_and_builder_balances_do_not_mix() {
    let feed = two_dex_feed();
    let sim = Sim::memory();
    let a = Address::repeat_byte(0xa2);
    deal(&sim, &feed, a, None, "100");

    assert!(
        sim.order(&feed, a, &buy("flx:TSLA", "0.5")).is_err(),
        "the default balance must not margin a builder-dex order"
    );
    sim.order(&feed, a, &buy("ETH", "0.5")).unwrap();
    let account = sim.account(a).unwrap().unwrap();
    assert!(account.perp_balance("") < dec("100"));
    assert_eq!(account.perp_balance("flx"), Decimal::ZERO);
    assert_eq!(account.fills.back().unwrap().fee_token, "USDC");

    // Funding the builder dex afterwards leaves the default ledger alone.
    let before = account.perp_balance("");
    deal(&sim, &feed, a, Some("flx"), "50");
    sim.order(&feed, a, &buy("flx:TSLA", "0.25")).unwrap();
    let account = sim.account(a).unwrap().unwrap();
    assert_eq!(account.perp_balance(""), before);
    assert!(account.perp_balance("flx") < dec("50"));
}

/// Funding accrues per (account, dex): an unpriced builder coin defers only
/// its own dex, and its payment lands in its own ledger.
#[test]
fn funding_defers_per_dex_and_pays_into_the_dex_ledger() {
    let feed = feed_with_funding(&[("ETH", "100", "100")]);
    let mut account = super::account::Account::default();
    for dex in ["", "flx"] {
        account.perps.insert(
            dex.to_string(),
            DexLedger {
                balance: Decimal::ZERO,
                funded_at_ms: 3_600_000,
            },
        );
    }
    account
        .positions
        .insert("ETH".into(), long("100", "10", 10));
    account
        .positions
        .insert("flx:TSLA".into(), long("200", "20", 10));

    // `flx:TSLA` has no context yet: only the default dex accrues.
    let moved = super::funding::accrue_funding(&mut account, &feed, 7_200_000).unwrap();
    assert_eq!(moved, 1);
    assert_eq!(account.funded_at_ms(""), 7_200_000);
    assert_eq!(
        account.funded_at_ms("flx"),
        3_600_000,
        "the unpriced dex keeps its stamp"
    );
    assert!(account.perp_balance("") < Decimal::ZERO);
    assert_eq!(account.perp_balance("flx"), Decimal::ZERO);

    // Once priced, the builder dex accrues into its own ledger.
    let default_after = account.perp_balance("");
    feed.put_ctx("flx:TSLA".into(), crate::wire::funding_ctx("200", "200"), 1);
    let moved = super::funding::accrue_funding(&mut account, &feed, 7_200_000).unwrap();
    assert_eq!(moved, 1);
    assert_eq!(account.funded_at_ms("flx"), 7_200_000);
    assert_eq!(account.perp_balance(""), default_after, "no double payment");
    assert_eq!(
        account.perp_balance("flx"),
        -Decimal::ONE * dec("200") * Decimal::new(125, 7)
    );
}

/// The waterfall liquidates one (account, dex) clearinghouse at a time: an
/// underwater builder-dex position is taken over by the backstop, the same
/// account's healthy default-dex position and balance are not touched.
#[test]
fn a_builder_dex_liquidation_does_not_touch_the_default_dex() {
    let feed = MarketFeed::memory(Vec::new());
    let now = crate::now_ms();
    feed.put_meta(meta_with("ETH", 10), now);
    let mut flx = meta_of(&[("flx:TSLA", 10)]);
    flx.collateral_token = 360;
    feed.put_dex_meta(("flx").to_string(), flx, now);
    feed.put_ctx("ETH".into(), crate::wire::test_ctx("100", "100"), now);
    feed.put_ctx("flx:TSLA".into(), crate::wire::test_ctx("40", "40"), now);

    let sim = Sim::memory();
    let a = Address::repeat_byte(0xa3);
    sim.with_account(a, |acc| {
        *acc.perp_balance_mut("") = dec("1000");
        acc.positions.insert("ETH".into(), long("100", "0", 10));
        // Equity 0 (10 + (40 - 50)), maintenance 2: below two thirds of it.
        *acc.perp_balance_mut("flx") = dec("10");
        acc.positions.insert("flx:TSLA".into(), long("50", "0", 10));
        Ok(())
    })
    .unwrap();

    let out = sim.check_all(&feed, crate::now_ms()).unwrap();
    assert_eq!(out.len(), 1, "one liquidation record: {out:?}");
    let account = sim.account(a).unwrap().unwrap();
    assert!(
        account
            .positions
            .get("flx:TSLA")
            .is_none_or(|p| p.szi.is_zero()),
        "the builder position is gone"
    );
    assert_eq!(account.perp_balance("flx"), Decimal::ZERO);
    assert_eq!(
        account.positions["ETH"].szi,
        Decimal::ONE,
        "the default position survives"
    );
    assert_eq!(account.perp_balance(""), dec("1000"), "its ledger too");

    let vault = sim
        .account(super::BACKSTOP)
        .unwrap()
        .expect("the vault is touched");
    assert_eq!(vault.positions["flx:TSLA"].szi, Decimal::ONE);
    assert!(
        !vault.positions.contains_key("ETH"),
        "the vault took only the builder dex"
    );
}

/// A unified account margins each dex from its collateral token's spot
/// balance, pooled across the dexes that share the token: another dex's
/// margin on the same token is not available again, a dex on another token
/// is not affected.
#[test]
fn a_unified_account_pools_spot_collateral_across_dexes_sharing_a_token() {
    let feed = MarketFeed::memory(Vec::new());
    let now = crate::now_ms();
    feed.put_meta(meta_with("ETH", 10), now);
    for (dex, coin, token) in [
        ("flx", "flx:TSLA", 360),
        ("xyz", "xyz:NVDA", 360),
        ("abc", "abc:GLD", 235),
    ] {
        let mut meta = meta_of(&[(coin, 10)]);
        meta.collateral_token = token;
        feed.put_dex_meta((dex).to_string(), meta, now);
    }
    for (coin, px) in [("flx:TSLA", "200"), ("xyz:NVDA", "100"), ("abc:GLD", "50")] {
        feed.put_ctx(coin.into(), crate::wire::test_ctx(px, px), now);
    }
    let mut account = super::account::Account {
        mode: UserAbstraction::UnifiedAccount,
        ..Default::default()
    };
    account.spot.insert(
        360,
        SpotEntry {
            coin: "USDH".into(),
            total: dec("100"),
            hold: Decimal::ZERO,
            entry_ntl: Decimal::ZERO,
        },
    );
    account
        .positions
        .insert("flx:TSLA".into(), long("200", "30", 10));

    let available = |dex: &str| {
        super::account::available_margin(
            &account,
            &feed,
            dex,
            &super::account::view(&account, &feed, dex).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(
        available("xyz"),
        dec("70"),
        "the sibling dex's margin is spent from the shared pool"
    );
    assert_eq!(
        available("abc"),
        Decimal::ZERO,
        "a dex on another token has none of the USDH pool"
    );
}

/// A state file written before the per-dex ledgers (a single `perp_balance`
/// and `funded_at_ms` per account, bare-address waterfall keys) loads as the
/// default dex's entries and is written back in the new shape.
#[test]
fn an_old_state_file_loads_into_the_default_dex_ledger() {
    let old = r#"{"epoch": 1, "next_oid": 3, "accounts": {
        "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa":
            {"perp_balance": "12.5", "funded_at_ms": 3600000},
        "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb": {"perp_balance": "-3"}},
        "liq_cooldown_until": {"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": 99},
        "waterfall_retry_after": {"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb": 7}}"#;
    let mut state: SimState = serde_json::from_str(old).unwrap();
    state.migrate();
    let a = Address::repeat_byte(0xaa);
    let b = Address::repeat_byte(0xbb);
    assert_eq!(state.accounts[&a].perp_balance(""), dec("12.5"));
    assert_eq!(state.accounts[&a].funded_at_ms(""), 3_600_000);
    assert_eq!(state.accounts[&b].perp_balance(""), dec("-3"));
    assert_eq!(state.liq_cooldown_until[&DexKey::new(a, "")], 99);
    assert_eq!(state.waterfall_retry_after[&DexKey::new(b, "")], 7);

    // Migrating again is a no-op, and the legacy keys are not written back.
    let mut again = state.clone();
    again.migrate();
    assert_eq!(again, state);
    let json = serde_json::to_value(&state).unwrap();
    let account = &json["accounts"]["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"];
    assert!(account.get("perp_balance").is_none());
    assert!(account.get("funded_at_ms").is_none());
    assert_eq!(account["perps"][""]["balance"], "12.5");

    // A builder dex's key round-trips as `address|dex`.
    let mut with_dex = state.clone();
    with_dex.liq_cooldown_until.insert(DexKey::new(a, "flx"), 5);
    let text = serde_json::to_string(&with_dex).unwrap();
    let mut back: SimState = serde_json::from_str(&text).unwrap();
    back.migrate();
    assert_eq!(back, with_dex);
}

/// A seed names the dex it is the state of: its ledger and its positions
/// replace that dex's only, the default dex's stay.
#[test]
fn a_seed_with_a_dex_sets_that_dexs_clearinghouse() {
    let feed = two_dex_feed();
    let sim = Sim::memory();
    let a = Address::repeat_byte(0xa4);
    deal(&sim, &feed, a, None, "100");
    let seed: super::seed::AccountSeed = serde_json::from_str(
        r#"{"dex": "flx",
            "marginSummary": {"accountValue": "50", "totalNtlPos": "0", "totalRawUsd": "50", "totalMarginUsed": "0"}}"#,
    )
    .unwrap();
    sim.set_account(a, seed).unwrap();
    let account = sim.account(a).unwrap().unwrap();
    assert_eq!(account.perp_balance("flx"), dec("50"));
    assert_eq!(
        account.perp_balance(""),
        dec("100"),
        "the default dex stays"
    );

    // The read-back is that dex's state.
    let fixture = sim.control_fixture(a).unwrap();
    let served = super::seed::control_view(fixture.as_ref(), &account, &feed).unwrap();
    assert_eq!(served.dex.as_deref(), Some("flx"));
    assert_eq!(
        served.margin_summary.unwrap().total_raw_usd,
        dec("50"),
        "the flx raw, not the default dex's"
    );
}

/// `/control/deal` takes the dex; the strict body still refuses unknown keys.
#[test]
fn the_deal_request_carries_a_dex_and_stays_strict() {
    let body = r#"{"address": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                   "class": "perp", "dex": "flx", "amount": "5"}"#;
    let req: crate::api::control::DealRequest = serde_json::from_str(body).unwrap();
    assert_eq!(Deal::from(req).dex.as_deref(), Some("flx"));
    let plain = r#"{"address": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "class": "perp", "amount": "5"}"#;
    let req: crate::api::control::DealRequest = serde_json::from_str(plain).unwrap();
    assert_eq!(Deal::from(req).dex, None);
    let unknown = r#"{"address": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                      "class": "perp", "amount": "5", "dx": "flx"}"#;
    assert!(serde_json::from_str::<crate::api::control::DealRequest>(unknown).is_err());
}
