//! Liquidation scenarios: a position's served `liquidationPx` and the tick's
//! liquidation are the same figure. Each scenario opens a position through the
//! order lane, reads `liquidationPx` from the served `clearinghouseState`, moves
//! the mark to just before it (nothing happens) and then just past it (the
//! waterfall closes the position through the book), and checks the fills, their
//! `liquidation` object and the balances.
use super::account::{self, LeverageSetting, SpotEntry};
use super::ledger::{LiquidationMethod, UserFill};
use super::order::{Order, Tif};
use super::{meta_of, Sim};
use crate::feed::MarketFeed;
use crate::wire::{ClearinghouseState, L2Book, Level, LeverageType, Meta, UserAbstraction};
use alloy_primitives::Address;
use rust_decimal::Decimal;

fn dec(text: &str) -> Decimal {
    text.parse().unwrap()
}

/// A margin table on `coin`: `(lowerBound, maxLeverage)` tiers.
fn with_table(meta: &mut Meta, coin: &str, id: u32, tiers: &[(&str, u32)]) {
    meta.universe
        .iter_mut()
        .find(|a| a.name == coin)
        .expect("the coin is in the meta")
        .margin_table_id = Some(id);
    meta.margin_tables.push((
        id,
        crate::wire::MarginTable {
            description: String::new(),
            margin_tiers: tiers
                .iter()
                .map(|(lower_bound, max_leverage)| crate::wire::MarginTier {
                    lower_bound: dec(lower_bound),
                    max_leverage: *max_leverage,
                })
                .collect(),
        },
    ));
}

/// The scenario's venue: the default dex lists `ETH` (10x) and `BTC` (20x,
/// tiered: 10x above 10000 of notional); the builder dex `xyz` lists
/// `xyz:TSLA` (20x, tiered: 10x above 1000 and 5x above 3000, a non-trivial
/// table). Every coin marks at 100.
struct Rig {
    sim: Sim,
    feed: MarketFeed,
}

impl Rig {
    fn new() -> Self {
        let feed = MarketFeed::memory(Vec::new());
        let now = crate::now_ms();
        let mut default = meta_of(&[("ETH", 10), ("BTC", 20)]);
        with_table(&mut default, "BTC", 40, &[("0", 20), ("10000", 10)]);
        feed.put_meta(default, now);
        let mut xyz = meta_of(&[("xyz:TSLA", 20)]);
        with_table(
            &mut xyz,
            "xyz:TSLA",
            41,
            &[("0", 20), ("1000", 10), ("3000", 5)],
        );
        feed.put_dex_meta(("xyz").to_string(), xyz, now);
        feed.put_oi_caps(Vec::new());
        feed.put_oi_caps_of("xyz", Vec::new());
        let rig = Self {
            sim: Sim::memory(),
            feed,
        };
        for coin in ["ETH", "BTC", "xyz:TSLA"] {
            rig.mark(coin, dec("100"));
        }
        rig
    }

    /// Move a coin's mark (and re-quote its book 0.1 either side: the close
    /// walks the book at the new price).
    fn mark(&self, coin: &str, px: Decimal) {
        let now = crate::now_ms();
        let text = px.to_string();
        self.feed
            .put_ctx(coin.into(), crate::wire::test_ctx(&text, &text), now);
        let level = |px: Decimal| Level {
            px,
            sz: dec("1000000"),
            n: 1,
        };
        self.feed.put_book(
            L2Book {
                coin: coin.into(),
                time: now,
                levels: [vec![level(px - dec("0.1"))], vec![level(px + dec("0.1"))]],
                spread: None,
            },
            now,
        );
    }

    fn fund(&self, who: Address, dex: &str, amount: &str) {
        self.sim
            .with_account(who, |a| {
                a.credit_perp(dex, dec(amount), "test deal")?;
                Ok(())
            })
            .unwrap();
    }

    /// A unified account holding `amount` of USDC in spot and nothing on the
    /// perp side (the collateral the dexes margin against).
    fn fund_unified(&self, who: Address, amount: &str) {
        self.sim
            .with_account(who, |a| {
                a.mode = UserAbstraction::UnifiedAccount;
                a.spot.insert(
                    0,
                    SpotEntry {
                        coin: "USDC".into(),
                        total: dec(amount),
                        hold: Decimal::ZERO,
                        entry_ntl: Decimal::ZERO,
                    },
                );
                Ok(())
            })
            .unwrap();
    }

    /// Open a cross position at `leverage` through the order lane.
    fn open(&self, who: Address, coin: &str, order: (bool, &str, u32)) {
        let (is_buy, size, leverage) = order;
        let meta = self.feed.meta_for_lookup(coin);
        self.sim
            .set_leverage(
                who,
                coin,
                LeverageSetting {
                    mode: LeverageType::Cross,
                    leverage,
                },
                meta.as_ref(),
            )
            .unwrap();
        let order = Order {
            coin: coin.into(),
            is_buy,
            size: dec(size),
            limit_px: None,
            tif: Tif::Ioc,
            reduce_only: false,
            cloid: None,
            trigger: None,
        };
        let fill = self.sim.order(&self.feed, who, &order).unwrap();
        assert_eq!(fill.total_sz, dec(size), "the opening order fills in full");
    }

    /// Switch an open position to isolated margin (the position parks its
    /// initial margin).
    fn isolate(&self, who: Address, coin: &str, leverage: u32) {
        let meta = self.feed.meta_for_lookup(coin);
        self.sim
            .set_leverage(
                who,
                coin,
                LeverageSetting {
                    mode: LeverageType::Isolated,
                    leverage,
                },
                meta.as_ref(),
            )
            .unwrap();
    }

    fn served(&self, who: Address, dex: &str) -> ClearinghouseState {
        let account = self.sim.account(who).unwrap().unwrap();
        account::clearinghouse(&account, &self.feed, dex, 1).unwrap()
    }

    /// The served `liquidationPx` of a coin's position.
    fn liq(&self, who: Address, coin: &str) -> Option<Decimal> {
        let dex = crate::feed::dex::dex_of_coin(coin);
        self.served(who, dex)
            .asset_positions
            .iter()
            .find(|row| row.position.coin == coin)
            .unwrap_or_else(|| panic!("no served position for {coin}"))
            .position
            .liquidation_px
    }

    /// One waterfall pass at the wall clock.
    fn tick(&self) -> Vec<account::Liquidation> {
        self.sim.check_all(&self.feed, crate::now_ms()).unwrap()
    }

    fn fills(&self, who: Address) -> Vec<UserFill> {
        self.sim.user_fills(who).unwrap()
    }

    fn szi(&self, who: Address, coin: &str) -> Decimal {
        self.sim
            .account(who)
            .unwrap()
            .unwrap()
            .positions
            .get(coin)
            .map_or(Decimal::ZERO, |p| p.szi)
    }
}

/// Which side of the served price the mark steps to.
fn around(liq: Decimal, long: bool) -> (Decimal, Decimal) {
    let step = liq / Decimal::from(10_000);
    if long {
        (liq + step, liq - step)
    } else {
        (liq - step, liq + step)
    }
}

/// The single-position scenario: the mark one step short of the served price
/// liquidates nothing, one step past it closes the position through the book.
/// Returns the closing fill and the account before the liquidation.
#[allow(clippy::too_many_lines)] // one walk: before, past, the fill, the balances
fn liquidates_at_the_served_price(
    rig: &Rig,
    who: Address,
    coin: &str,
    long: bool,
) -> (UserFill, account::Account) {
    let liq = rig
        .liq(who, coin)
        .expect("the position has a liquidation price");
    let (before, past) = around(liq, long);

    rig.mark(coin, before);
    assert!(
        rig.tick().is_empty(),
        "no liquidation at {before} (liq {liq})"
    );
    assert_ne!(rig.szi(who, coin), Decimal::ZERO, "the position survives");
    assert_eq!(
        rig.liq(who, coin),
        Some(liq),
        "a lone position's liquidation price does not move with the mark"
    );

    rig.mark(coin, past);
    let snapshot = rig.sim.account(who).unwrap().unwrap();
    let out = rig.tick();
    assert_eq!(out.len(), 1, "liquidated at {past} (liq {liq})");
    assert_eq!(out[0].positions_closed, vec![coin.to_string()]);
    assert_eq!(rig.szi(who, coin), Decimal::ZERO, "the position is closed");

    let fill = rig.fills(who).pop().expect("the liquidation's fill");
    assert_eq!(fill.coin, coin);
    assert_eq!(fill.dir, if long { "Close Long" } else { "Close Short" });
    assert_eq!(fill.side, if long { "A" } else { "B" });
    assert!(fill.crossed, "a liquidation takes the book");
    assert_eq!(fill.fee, Decimal::ZERO, "no fee on a liquidation");
    assert_eq!(fill.start_position, snapshot.positions[coin].szi);
    assert_eq!(
        fill.sz,
        snapshot.positions[coin].szi.abs(),
        "closed in full"
    );
    let tag = fill.liquidation.as_ref().expect("the liquidation object");
    assert_eq!(tag.liquidated_user, format!("{who:#x}"));
    assert_eq!(tag.mark_px, past, "the mark the liquidation was decided at");
    assert_eq!(tag.method, LiquidationMethod::Market);
    let json = serde_json::to_value(&fill).unwrap();
    assert_eq!(json["liquidation"]["method"], "market");
    assert_eq!(json["liquidation"]["liquidatedUser"], format!("{who:#x}"));
    // The close walked the book at the new mark: a long sells into the bid,
    // a short buys the ask.
    assert_eq!(
        fill.px,
        if long {
            past - dec("0.1")
        } else {
            past + dec("0.1")
        }
    );
    let expected_pnl = snapshot.positions[coin].szi * (fill.px - snapshot.positions[coin].entry_px);
    assert_eq!(fill.closed_pnl, expected_pnl);
    (fill, snapshot)
}

/// Long and short, cross, on the default dex: the served price is the
/// liquidation trigger, and the balance is the account value at the close.
#[test]
fn a_cross_long_liquidates_at_its_served_price() {
    let rig = Rig::new();
    let a = Address::repeat_byte(0xa1);
    rig.fund(a, "", "150");
    rig.open(a, "ETH", (true, "10", 10));
    let liq = rig.liq(a, "ETH").unwrap();
    // Hand check against the docs' closed form, `l = 1/20` (10x maintenance
    // is half the 10x initial margin): equity E, maintenance 0.05 * 1000.
    let equity = rig.served(a, "").cross_margin_summary.account_value;
    let expected = dec("100") - (equity - dec("50")) / dec("10") / (Decimal::ONE - dec("0.05"));
    assert!(
        (liq - expected).abs() < dec("0.000000001"),
        "{liq} vs {expected}"
    );
    assert!(liq < dec("100"), "a long liquidates below the mark");

    let (fill, before) = liquidates_at_the_served_price(&rig, a, "ETH", true);
    let after = rig.sim.account(a).unwrap().unwrap();
    assert_eq!(
        after.perp_balance(""),
        before.perp_balance("") + fill.closed_pnl,
        "the balance is the cash plus the realized loss"
    );
    assert!(after.positions.is_empty());
}

#[test]
fn a_cross_short_liquidates_at_its_served_price() {
    let rig = Rig::new();
    let a = Address::repeat_byte(0xa2);
    rig.fund(a, "", "150");
    rig.open(a, "ETH", (false, "10", 10));
    let liq = rig.liq(a, "ETH").unwrap();
    assert!(liq > dec("100"), "a short liquidates above the mark");
    let equity = rig.served(a, "").cross_margin_summary.account_value;
    // Short: p = m + (E - M)/q/(1 + l).
    let expected = dec("100") + (equity - dec("50")) / dec("10") / (Decimal::ONE + dec("0.05"));
    assert!(
        (liq - expected).abs() < dec("0.000000001"),
        "{liq} vs {expected}"
    );

    let (fill, before) = liquidates_at_the_served_price(&rig, a, "ETH", false);
    let after = rig.sim.account(a).unwrap().unwrap();
    assert_eq!(
        after.perp_balance(""),
        before.perp_balance("") + fill.closed_pnl
    );
}

/// A long the equity covers all the way to zero has no liquidation price (the
/// venue serves `null`); a short always has one.
#[test]
fn a_fully_collateralized_long_has_no_liquidation_price() {
    let rig = Rig::new();
    let a = Address::repeat_byte(0xa3);
    rig.fund(a, "", "5000");
    rig.open(a, "ETH", (true, "10", 10));
    assert_eq!(
        rig.liq(a, "ETH"),
        None,
        "5000 covers a 1000 long at any price"
    );
    let b = Address::repeat_byte(0xa4);
    rig.fund(b, "", "5000");
    rig.open(b, "ETH", (false, "10", 10));
    let liq = rig.liq(b, "ETH").expect("a short always has one");
    assert!(liq > dec("500"), "{liq}");
    let served = serde_json::to_value(rig.served(a, "")).unwrap();
    assert!(served["assetPositions"][0]["position"]["liquidationPx"].is_null());
}

/// Isolated margin, both sides: the position's own margin and pnl against its
/// own maintenance; the cross balance is untouched until the close returns the
/// margin.
#[test]
fn isolated_positions_liquidate_at_their_served_prices() {
    for long in [true, false] {
        let rig = Rig::new();
        let a = Address::repeat_byte(0xa5);
        rig.fund(a, "", "500");
        rig.open(a, "ETH", (long, "10", 10));
        rig.isolate(a, "ETH", 10);
        let position = rig.sim.account(a).unwrap().unwrap().positions["ETH"].clone();
        let margin = position.isolated_margin;
        assert!(margin > Decimal::ZERO, "the initial margin is parked");
        // The leg's equity: the margin plus the position's pnl at the mark.
        let equity = margin + position.unrealized_pnl(dec("100")).unwrap();
        let liq = rig.liq(a, "ETH").unwrap();
        // The docs' closed form with the position's own margin.
        let side = if long { Decimal::ONE } else { -Decimal::ONE };
        let rate = dec("0.05");
        let expected = dec("100")
            - side * (equity - dec("1000") * rate) / dec("10") / (Decimal::ONE - rate * side);
        assert!(
            (liq - expected).abs() < dec("0.000000001"),
            "{liq} vs {expected}"
        );

        let (fill, before) = liquidates_at_the_served_price(&rig, a, "ETH", long);
        let after = rig.sim.account(a).unwrap().unwrap();
        assert_eq!(
            after.perp_balance(""),
            before.perp_balance("") + margin + fill.closed_pnl,
            "the margin comes back with the realized pnl"
        );
    }
}

/// An isolated position's loss is its own: with almost no free cash beside it,
/// the cross figure stays the cash (not cash minus the isolated loss), the
/// waterfall leaves the solvent isolated leg alone, and the position row
/// serves the venue's figures (`marginUsed` = margin + pnl, `rawUsd` =
/// `marginUsed` less the signed value, positive for a short).
#[test]
fn an_isolated_loss_beyond_the_free_cash_is_not_a_cross_liquidation() {
    for long in [true, false] {
        let rig = Rig::new();
        let a = Address::repeat_byte(0xa6);
        rig.fund(a, "", "110");
        rig.open(a, "ETH", (long, "10", 10));
        rig.isolate(a, "ETH", 10);
        let cash = rig.sim.account(a).unwrap().unwrap().perp_balance("");
        // 4 % against the position: a loss of 40 on a margin of 100, far more
        // than the free cash, still above the leg's 5 % maintenance (48).
        let mark = if long { dec("96") } else { dec("104") };
        rig.mark("ETH", mark);
        let held = rig.sim.account(a).unwrap().unwrap().positions["ETH"].clone();
        let equity = held.isolated_margin + held.unrealized_pnl(mark).unwrap();
        assert!(
            rig.tick().is_empty(),
            "a solvent isolated leg is not liquidated"
        );
        assert_ne!(rig.szi(a, "ETH"), Decimal::ZERO);
        let served = rig.served(a, "");
        assert_eq!(
            served.cross_margin_summary.account_value, cash,
            "the cross figure is the free cash alone"
        );
        let position = &served.asset_positions[0].position;
        let value = position.position_value;
        assert_eq!(position.margin_used, equity, "the margin plus the pnl");
        assert_eq!(
            position.leverage.raw_usd,
            Some(if long { equity - value } else { equity + value }),
        );
        assert_eq!(served.margin_summary.total_margin_used, equity);
    }
}

/// A builder dex's margin table is applied at the liquidation price: the short
/// below sits in the 20x tier at the mark and in the 10x tier at its
/// liquidation price (notional above 1000), so the flat 20x rate would put the
/// price a little further away.
#[test]
fn a_builder_dex_short_liquidates_in_its_tier_at_the_liquidation_price() {
    let rig = Rig::new();
    let a = Address::repeat_byte(0xb1);
    rig.fund(a, "xyz", "300");
    rig.open(a, "xyz:TSLA", (false, "8", 20));
    let state = rig.served(a, "xyz");
    let equity = state.cross_margin_summary.account_value;
    let liq = rig.liq(a, "xyz:TSLA").unwrap();
    // Tier 1 (10x, rate 1/20, deduction 1000 * (1/20 - 1/40) = 25):
    // E - 8 (p - 100) = 8 p / 20 - 25.
    let tier1 = (equity + dec("800") + dec("25")) / dec("8.4");
    assert!((liq - tier1).abs() < dec("0.000000001"), "{liq} vs {tier1}");
    assert!(liq * dec("8") >= dec("1000"), "the root is in the 10x tier");
    // The flat 20x rate (the notional at the mark) would give a different price.
    let flat = dec("100") + (equity - dec("20")) / dec("8") / (Decimal::ONE + dec("0.025"));
    assert!(flat > liq, "{flat} vs {liq}");

    // The mark between the two prices already liquidates: the tier at the
    // liquidation price is what the waterfall uses too.
    let between = (liq + flat) / dec("2");
    rig.mark("xyz:TSLA", between);
    assert_eq!(rig.tick().len(), 1, "liquidated at {between}, past {liq}");
    assert_eq!(rig.szi(a, "xyz:TSLA"), Decimal::ZERO);
}

/// The same builder dex, long, through the full before/after walk; and the
/// maintenance the served state reports is the deducted figure.
#[test]
fn a_builder_dex_long_liquidates_at_its_served_price() {
    let rig = Rig::new();
    let a = Address::repeat_byte(0xb2);
    rig.fund(a, "xyz", "60");
    rig.open(a, "xyz:TSLA", (true, "8", 20));
    let state = rig.served(a, "xyz");
    assert_eq!(
        state.cross_maintenance_margin_used,
        dec("800") * dec("0.025"),
        "20x tier: 800 * 1/40"
    );
    liquidates_at_the_served_price(&rig, a, "xyz:TSLA", true);
    // The liquidation settles on the dex's own ledger and in its own books.
    let account = rig.sim.account(a).unwrap().unwrap();
    assert!(account.perp_balance("xyz") < dec("60"));
    assert_eq!(account.perp_balance(""), Decimal::ZERO);
}

/// Two cross positions on one dex share one equity: each one's price assumes
/// the other at its current mark, and the cross leg liquidates as a whole.
#[test]
fn two_cross_positions_on_one_dex_liquidate_together() {
    let rig = Rig::new();
    let a = Address::repeat_byte(0xc1);
    rig.fund(a, "", "200");
    rig.open(a, "ETH", (true, "10", 10));
    rig.open(a, "BTC", (false, "5", 20));
    let state = rig.served(a, "");
    let equity = state.cross_margin_summary.account_value;
    let maintenance = state.cross_maintenance_margin_used;
    // ETH 1000 * 1/20, BTC 500 * 1/40.
    assert_eq!(maintenance, dec("50") + dec("12.5"));
    let eth = rig.liq(a, "ETH").unwrap();
    let btc = rig.liq(a, "BTC").unwrap();
    assert!(eth < dec("100") && btc > dec("100"));
    // The ETH price solves `equity(p) == maintenance(p)` with BTC held at its
    // mark: E + 10 (p - 100) = 0.05 * 10 p + 12.5.
    let solved = (dec("12.5") + dec("1000") - equity) / dec("9.5");
    assert!(
        (eth - solved).abs() < dec("0.000000001"),
        "{eth} vs {solved}"
    );
    // And the BTC one with ETH held: E - 5 (p - 100) = 0.025 * 5 p + 50.
    let solved = (equity + dec("500") - dec("50")) / dec("5.125");
    assert!(
        (btc - solved).abs() < dec("0.000000001"),
        "{btc} vs {solved}"
    );

    // Moving the other coin moves this one's price: BTC up costs equity.
    rig.mark("BTC", dec("102"));
    assert!(
        rig.liq(a, "ETH").unwrap() > eth,
        "BTC losing raises ETH's price"
    );
    rig.mark("BTC", dec("100"));

    // The ETH mark just before: nothing; just past: both positions go.
    let (before, past) = around(eth, true);
    rig.mark("ETH", before);
    assert!(rig.tick().is_empty());
    rig.mark("ETH", past);
    let out = rig.tick();
    assert_eq!(out.len(), 1);
    let mut closed = out[0].positions_closed.clone();
    closed.sort();
    assert_eq!(closed, vec!["BTC".to_string(), "ETH".to_string()]);
    assert_eq!(rig.szi(a, "ETH"), Decimal::ZERO);
    assert_eq!(rig.szi(a, "BTC"), Decimal::ZERO);
    let tagged: Vec<String> = rig
        .fills(a)
        .into_iter()
        .filter(|f| f.liquidation.is_some())
        .map(|f| f.coin)
        .collect();
    assert_eq!(
        tagged.len(),
        2,
        "both closes are liquidation fills: {tagged:?}"
    );
}

/// The other leg of the pair: the short crossing its own price liquidates the
/// whole cross leg just as well.
#[test]
fn the_second_cross_position_triggers_the_same_liquidation() {
    let rig = Rig::new();
    let a = Address::repeat_byte(0xc2);
    rig.fund(a, "", "200");
    rig.open(a, "ETH", (true, "10", 10));
    rig.open(a, "BTC", (false, "5", 20));
    let btc = rig.liq(a, "BTC").unwrap();
    let (before, past) = around(btc, false);
    rig.mark("BTC", before);
    assert!(rig.tick().is_empty());
    rig.mark("BTC", past);
    let out = rig.tick();
    assert_eq!(out.len(), 1);
    assert_eq!(rig.szi(a, "ETH"), Decimal::ZERO);
    assert_eq!(rig.szi(a, "BTC"), Decimal::ZERO);
}

/// Each dex is its own clearinghouse: a position on another dex neither
/// moves this dex's price nor liquidates with it.
#[test]
fn a_position_on_another_dex_does_not_affect_this_dex() {
    let rig = Rig::new();
    // The same ETH position alone, in its own venue.
    let lone = Rig::new();
    let twin = Address::repeat_byte(0xd0);
    lone.fund(twin, "", "150");
    lone.open(twin, "ETH", (true, "10", 10));
    let alone = lone.liq(twin, "ETH").unwrap();

    let a = Address::repeat_byte(0xd1);
    rig.fund(a, "", "150");
    rig.fund(a, "xyz", "100");
    rig.open(a, "ETH", (true, "10", 10));
    rig.open(a, "xyz:TSLA", (true, "8", 20));
    assert_eq!(
        rig.liq(a, "ETH"),
        Some(alone),
        "the xyz book is not in ETH's equity"
    );
    let xyz = rig.liq(a, "xyz:TSLA").unwrap();

    // The xyz position crossing its price liquidates xyz only.
    let (before, past) = around(xyz, true);
    rig.mark("xyz:TSLA", before);
    assert!(rig.tick().is_empty());
    rig.mark("xyz:TSLA", past);
    let out = rig.tick();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].positions_closed, vec!["xyz:TSLA".to_string()]);
    assert_ne!(
        rig.szi(a, "ETH"),
        Decimal::ZERO,
        "the default dex is untouched"
    );
    assert_eq!(rig.liq(a, "ETH"), Some(alone));

    // And the default dex liquidating leaves a surviving xyz position alone:
    // a fresh xyz position and the ETH one crossing.
    rig.mark("xyz:TSLA", dec("100"));
    rig.fund(a, "xyz", "100");
    rig.open(a, "xyz:TSLA", (true, "8", 20));
    let (_, past) = around(alone, true);
    rig.mark("ETH", past);
    let out = rig.tick();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].positions_closed, vec!["ETH".to_string()]);
    assert_ne!(
        rig.szi(a, "xyz:TSLA"),
        Decimal::ZERO,
        "the xyz position survives"
    );
}

/// A unified account margins against its spot collateral: the served price
/// is the one an equally funded standard account has, and the tick
/// liquidates at it (the waterfall's equity is the same figure).
#[test]
fn a_unified_account_liquidates_against_its_spot_collateral() {
    let rig = Rig::new();
    // The equally funded standard account, in its own venue so that its
    // position does not trade with the unified one's.
    let twin = Rig::new();
    let standard = Address::repeat_byte(0xe0);
    twin.fund(standard, "", "150");
    twin.open(standard, "ETH", (true, "10", 10));
    let standard_liq = twin.liq(standard, "ETH").unwrap();

    let a = Address::repeat_byte(0xe1);
    rig.fund_unified(a, "150");
    rig.open(a, "ETH", (true, "10", 10));
    let state = rig.served(a, "");
    // The served perp summary carries the spot-funded margin as cash (the real
    // venue's `hold + unrealized`); the spot collateral is what the order lane
    // and the waterfall both margin against.
    assert!(
        state.margin_summary.account_value > Decimal::ONE,
        "the summary carries the spot-funded margin: {}",
        state.margin_summary.account_value
    );
    let liq = rig
        .liq(a, "ETH")
        .expect("the spot collateral backs the long");
    assert!(
        (liq - standard_liq).abs() < dec("0.000001"),
        "unified {liq} vs standard {standard_liq}"
    );

    let before_spot = rig.sim.account(a).unwrap().unwrap().spot[&0].total;
    let (fill, _) = liquidates_at_the_served_price(&rig, a, "ETH", true);
    assert!(fill.liquidation.is_some());
    // The loss settles into the perp ledger; the spot collateral is intact.
    let after = rig.sim.account(a).unwrap().unwrap();
    assert_eq!(after.spot[&0].total, before_spot);
    assert!(after.perp_balance("") < Decimal::ZERO);
}

/// The partial-liquidation rule is unchanged by the new trigger: above the
/// $100k notional threshold the first attempt takes 20 % of the position and
/// starts the cooldown.
#[test]
fn a_large_position_is_liquidated_in_part_first() {
    let rig = Rig::new();
    let a = Address::repeat_byte(0xf1);
    // 2000 ETH = 200k notional at 10x: margin 20k.
    rig.fund(a, "", "21000");
    rig.open(a, "ETH", (true, "2000", 10));
    let liq = rig.liq(a, "ETH").unwrap();
    let (before, past) = around(liq, true);
    rig.mark("ETH", before);
    assert!(rig.tick().is_empty());
    rig.mark("ETH", past);
    let out = rig.tick();
    assert_eq!(out.len(), 1);
    let fill = rig.fills(a).pop().unwrap();
    assert_eq!(fill.sz, dec("400"), "20% of 2000");
    assert_eq!(
        fill.liquidation.as_ref().unwrap().method,
        LiquidationMethod::Market
    );
    assert_eq!(rig.szi(a, "ETH"), dec("1600"));
    assert_eq!(
        rig.sim
            .metrics
            .partial_liquidations_total
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

/// Far past the price (below two thirds of the maintenance requirement) with
/// no book to take the close, the backstop takes the position over: the
/// vault's fill is the liquidated position's own label and both sides carry
/// the `backstop` liquidation object.
#[test]
fn a_gap_through_the_backstop_threshold_goes_to_the_vault() {
    let rig = Rig::new();
    let a = Address::repeat_byte(0xf2);
    rig.fund(a, "", "150");
    rig.open(a, "ETH", (true, "10", 10));
    let liq = rig.liq(a, "ETH").unwrap();
    // No book at the new mark: the book cannot act. 3.5 below the price the
    // equity is about a fifth of the maintenance requirement: positive (not
    // ADL) and under two thirds of it (the backstop).
    let gap = liq - dec("3.5");
    let text = gap.to_string();
    rig.feed.put_ctx(
        "ETH".into(),
        crate::wire::test_ctx(&text, &text),
        crate::now_ms(),
    );
    rig.feed.put_book(
        L2Book {
            coin: "ETH".into(),
            time: crate::now_ms(),
            levels: [vec![], vec![]],
            spread: None,
        },
        crate::now_ms(),
    );
    let out = rig.tick();
    assert_eq!(out.len(), 1, "the backstop acts at {gap}");
    assert_eq!(rig.szi(a, "ETH"), Decimal::ZERO);
    let mine = rig.fills(a).pop().unwrap();
    let tag = mine.liquidation.expect("the trader's close is tagged");
    assert_eq!(tag.method, LiquidationMethod::Backstop);
    assert_eq!(tag.mark_px, gap);
    let vault = rig.sim.user_fills(super::BACKSTOP).unwrap().pop().unwrap();
    assert_eq!(vault.dir, "Liquidated Cross Long");
    assert_eq!(vault.side, "B");
    assert!(vault.crossed);
    assert_eq!(
        vault.liquidation.unwrap().liquidated_user,
        format!("{a:#x}")
    );
}

/// Every surface that serves a clearinghouse reads the one view: the REST
/// state of a dex, the per-dex `webData3` and `allDexsClearinghouseState`
/// states all carry the same `liquidationPx` for cross and isolated rows.
#[test]
fn every_clearinghouse_surface_carries_the_same_liquidation_px() {
    let rig = Rig::new();
    let a = Address::repeat_byte(0xa9);
    rig.fund(a, "", "400");
    rig.fund(a, "xyz", "100");
    rig.open(a, "ETH", (true, "10", 10));
    rig.open(a, "BTC", (false, "5", 20));
    rig.isolate(a, "BTC", 20);
    rig.open(a, "xyz:TSLA", (false, "8", 20));
    let account = rig.sim.account(a).unwrap().unwrap();
    for dex in ["", "xyz"] {
        let direct = account::clearinghouse(&account, &rig.feed, dex, 1).unwrap();
        let wire = serde_json::to_value(&direct).unwrap();
        for row in wire["assetPositions"].as_array().unwrap() {
            let position = &row["position"];
            let coin = position["coin"].as_str().unwrap();
            let served = rig.liq(a, coin).expect("every row here has a price");
            let text = position["liquidationPx"]
                .as_str()
                .unwrap_or_else(|| panic!("{coin} serves a null liquidationPx"));
            let parsed: Decimal = text.parse().unwrap();
            assert_eq!(parsed, served.round_dp(10).normalize(), "{coin}");
            assert!(
                text.split('.').nth(1).is_none_or(|f| f.len() <= 10),
                "ten fractional digits at most: {text}"
            );
        }
    }
}
