//! Parity of the liquidation figures with the real venue: accounts captured
//! live (`fixtures/real_liquidation.json`, read-only `clearinghouseState` calls,
//! every address a placeholder) are replayed through the sim's own maintenance
//! and liquidation-price functions, and the recomputed `crossMaintenanceMarginUsed`
//! and per-position `liquidationPx` must be the venue's.
//!
//! Measured on 8,900 live positions of 627 accounts (non-unified accounts, the
//! ones whose equity is the clearinghouse's own): the cross price agrees to a
//! median relative error of 6e-13 (largest 4e-9, the rounding of the captured
//! strings) on 2,133 positions, and the venue's `null` (a long its equity covers
//! to zero) is reproduced on all 1,644 of the others; isolated prices agree to
//! 2e-13 median (largest 2e-8) on 1,704. Unified accounts margin against the
//! venue's own `tokenToAvailableAfterMaintenance + maintenance`, which
//! the unified cases below feed as the equity.
use crate::sim::account::{
    maintenance_margin, maintenance_tiers, solve_liquidation_px, Direction, LiquidationLeg,
};
use crate::wire::{AssetInfo, MarginTable, MarginTier, Meta};
use rust_decimal::Decimal;
use serde_json::Value;

const REAL: &str = include_str!("fixtures/real_liquidation.json");

fn dec(v: &Value) -> Decimal {
    v.as_str().expect("a decimal string").parse().unwrap()
}

/// The captured rows of the coins held, in the sim's meta type.
fn meta_of(case: &Value) -> Meta {
    let universe = case["meta"]["universe"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| AssetInfo {
            name: a["name"].as_str().unwrap().into(),
            max_leverage: u32::try_from(a["maxLeverage"].as_u64().unwrap()).unwrap(),
            margin_table_id: a["marginTableId"]
                .as_u64()
                .map(|id| u32::try_from(id).unwrap()),
            ..Default::default()
        })
        .collect();
    let margin_tables = case["meta"]["marginTables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            let id = u32::try_from(entry[0].as_u64().unwrap()).unwrap();
            let tiers = entry[1]["marginTiers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| MarginTier {
                    lower_bound: dec(&t["lowerBound"]),
                    max_leverage: u32::try_from(t["maxLeverage"].as_u64().unwrap()).unwrap(),
                })
                .collect();
            (
                id,
                MarginTable {
                    description: String::new(),
                    margin_tiers: tiers,
                },
            )
        })
        .collect();
    Meta {
        universe,
        margin_tables,
        collateral_token: 0,
    }
}

fn close(sim: Decimal, real: Decimal, tolerance: &str) -> bool {
    let tolerance: Decimal = tolerance.parse().unwrap();
    (sim - real).abs() <= real.abs() * tolerance
}

/// One captured case: the maintenance and every position's price. Returns the
/// count of priced positions and of `null` ones.
#[allow(clippy::too_many_lines)] // one captured case: maintenance, then every position
fn replay(case: &Value, equity: Decimal, tolerance: &str) -> (usize, usize) {
    let meta = meta_of(case);
    let positions: Vec<&Value> = case["state"]["assetPositions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["position"])
        .collect();
    let maintenance_of = |p: &Value| {
        maintenance_margin(
            p["coin"].as_str().unwrap(),
            dec(&p["positionValue"]),
            Some(&meta),
        )
        .unwrap()
    };
    let served_maintenance = dec(&case["state"]["crossMaintenanceMarginUsed"]);
    let maintenance: Decimal = positions
        .iter()
        .filter(|p| p["leverage"]["type"] == "cross")
        .map(|p| maintenance_of(p))
        .sum();
    assert!(
        close(maintenance, served_maintenance, "0.000001")
            || (maintenance - served_maintenance).abs() < Decimal::new(1, 5),
        "{}: maintenance {maintenance} vs the venue's {served_maintenance}",
        case["user"]
    );

    let (mut priced, mut nulls) = (0, 0);
    for p in &positions {
        let coin = p["coin"].as_str().unwrap();
        let szi = dec(&p["szi"]);
        let size = szi.abs();
        let value = dec(&p["positionValue"]);
        let tiers = maintenance_tiers(coin, Some(&meta)).unwrap();
        let (equity, other_maintenance) = if p["leverage"]["type"] == "isolated" {
            // The venue's isolated `marginUsed` is the leg's equity: the
            // parked margin plus the unrealized pnl.
            (dec(&p["marginUsed"]), Decimal::ZERO)
        } else {
            (equity, served_maintenance - maintenance_of(p))
        };
        let sim = solve_liquidation_px(&LiquidationLeg {
            tiers: &tiers,
            equity,
            other_maintenance,
            direction: if szi.is_sign_negative() {
                Direction::Short
            } else {
                Direction::Long
            },
            size,
            mark: value / size,
        })
        .unwrap();
        match (sim, p["liquidationPx"].as_str()) {
            (None, None) => nulls += 1,
            (Some(sim), Some(real)) => {
                let real: Decimal = real.parse().unwrap();
                assert!(
                    close(sim, real, tolerance),
                    "{} {coin}: sim {sim} vs the venue's {real}",
                    case["user"]
                );
                priced += 1;
            }
            (sim, real) => panic!(
                "{} {coin}: sim {sim:?} vs the venue's {real:?}",
                case["user"]
            ),
        }
    }
    (priced, nulls)
}

/// Standard accounts (`disabled`/`default`): the clearinghouse's own cross
/// account value is the equity.
#[test]
fn the_cross_and_isolated_prices_match_captured_real_accounts() {
    let real: Value = serde_json::from_str(REAL).unwrap();
    let (mut priced, mut nulls) = (0, 0);
    let mut tags = std::collections::BTreeSet::new();
    for case in real["cases"].as_array().unwrap() {
        let equity = dec(&case["state"]["crossMarginSummary"]["accountValue"]);
        let (p, n) = replay(case, equity, "0.0000001");
        priced += p;
        nulls += n;
        tags.extend(
            case["tags"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap().to_string()),
        );
    }
    assert!(priced >= 25 && nulls >= 5, "priced {priced}, null {nulls}");
    // The capture covers every shape the formula has a branch for.
    for tag in [
        "cross",
        "isolated",
        "long",
        "short",
        "null",
        "builder",
        "default",
        "multi",
        "whaleTier",
        "rootInHigherTier",
    ] {
        assert!(tags.contains(tag), "the capture lost its `{tag}` case");
    }
}

/// Unified accounts: the venue's equity is `tokenToAvailableAfterMaintenance`
/// plus the cross maintenance (its own accounting of the spot collateral); the
/// same functions reproduce its prices from that figure.
#[test]
fn the_unified_prices_match_with_the_venues_collateral_figure() {
    let real: Value = serde_json::from_str(REAL).unwrap();
    let cases = real["unified"].as_array().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let available = dec(&case["availableAfterMaintenance"]);
        let served = dec(&case["state"]["crossMaintenanceMarginUsed"]);
        // The spot figure is read a moment after the state (two calls): the
        // collateral drifts by funding and fees in between.
        replay(case, available + served, "0.001");
    }
}

/// The maintenance deduction (docs, margin tiers): a notional above the first
/// tier's bound is charged `notional·rate − deduction`, which keeps the
/// requirement continuous. The captured whale (22 positions, one above 20M of
/// notional) is the live proof: the flat 1/(2·5x) rate would overstate its
/// requirement by exactly the deduction.
#[test]
fn the_maintenance_deduction_keeps_the_requirement_continuous() {
    let mut meta = crate::sim::meta_of(&[("X", 10)]);
    meta.universe[0].margin_table_id = Some(1);
    meta.margin_tables = vec![(
        1,
        MarginTable {
            description: String::new(),
            margin_tiers: vec![
                MarginTier {
                    lower_bound: Decimal::ZERO,
                    max_leverage: 10,
                },
                MarginTier {
                    lower_bound: Decimal::from(20_000_000),
                    max_leverage: 5,
                },
            ],
        },
    )];
    let at = |n: i64| maintenance_margin("X", Decimal::from(n), Some(&meta)).unwrap();
    // 1/20 below the bound, 1/10 above it, minus 20M * (1/10 - 1/20) = 1M.
    assert_eq!(at(20_000_000), Decimal::from(1_000_000));
    assert_eq!(at(24_000_000), Decimal::from(1_400_000));
    assert_eq!(
        at(20_000_001) - at(20_000_000),
        Decimal::new(1, 1),
        "the marginal rate above the bound"
    );
    assert_eq!(
        at(19_999_999) - at(19_999_998),
        Decimal::new(5, 2),
        "and below it"
    );
}

/// Every surface that serves an account's clearinghouse carries the same
/// `liquidationPx` for the same position: the REST `clearinghouseState`, the
/// `clearinghouseState` channel and the `allDexsClearinghouseState` channel
/// (`webData3` carries no per-dex clearinghouse: the venue's public socket
/// sends only the identity facts there, and the sim mirrors that).
#[test]
#[allow(clippy::too_many_lines)] // one world and the three surfaces read from it
fn every_surface_serves_the_same_liquidation_px() {
    use crate::api::ws::{pushes, request_reply, Live};
    use crate::api::{info, ApiState};
    use crate::sim::order::{Order, Tif};
    use crate::sim::{Deal, DealClass};
    use crate::wire::{L2Book, Level};
    use alloy_primitives::Address;
    use axum::body::Bytes;
    use axum::extract::State;
    use std::sync::Arc;
    use std::time::Duration;

    let feed = Arc::new(crate::feed::MarketFeed::memory(Vec::new()));
    let now = crate::now_ms();
    feed.put_meta(crate::sim::meta_with("ETH", 10), now);
    feed.put_dex_meta(("xyz").to_string(), crate::sim::meta_of(&[("xyz:TSLA", 20)]), now);
    feed.put_oi_caps(Vec::new());
    feed.put_oi_caps_of("xyz", Vec::new());
    for coin in ["ETH", "xyz:TSLA"] {
        feed.put_ctx(coin.into(), crate::wire::test_ctx("100", "100"), now);
        let level = |px: &str| Level {
            px: px.parse().unwrap(),
            sz: "1000".parse().unwrap(),
            n: 1,
        };
        feed.put_book(
            L2Book {
                coin: coin.into(),
                time: now,
                levels: [vec![level("99.9")], vec![level("100.1")]],
                spread: None,
            },
            now,
        );
    }
    let api = crate::feed::rest::RestClient::new(&["http://127.0.0.1:1".to_string()]).unwrap();
    let state = Arc::new(
        ApiState::new(
            feed.clone(),
            Arc::new(crate::sim::Sim::memory()),
            api,
            None,
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap(),
    );
    let user = Address::repeat_byte(0xaa);
    for dex in [None, Some("xyz".to_string())] {
        state
            .sim
            .deal(
                &feed,
                &Deal {
                    address: user,
                    class: DealClass::Perp,
                    coin: None,
                    dex,
                    amount: Decimal::from(150),
                },
            )
            .unwrap();
    }
    for (coin, is_buy) in [("ETH", true), ("xyz:TSLA", false)] {
        let order = Order {
            coin: coin.into(),
            is_buy,
            size: "10".parse().unwrap(),
            limit_px: None,
            tif: Tif::Ioc,
            reduce_only: false,
            cloid: None,
            trigger: None,
        };
        state.sim.order(&feed, user, &order).unwrap();
    }

    let rest = |dex: &'static str| {
        let state = state.clone();
        async move {
            let body = serde_json::json!({"type": "clearinghouseState", "user": user, "dex": dex});
            let response = info::info(State(state), Bytes::from(body.to_string()))
                .await
                .unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        }
    };
    let prices = |clearinghouse: &Value| -> Vec<(String, Value)> {
        clearinghouse["assetPositions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["position"]["coin"].as_str().unwrap().to_string(),
                    row["position"]["liquidationPx"].clone(),
                )
            })
            .collect()
    };
    let subscribe = |subscription: Value| -> Value {
        let mut subs: Vec<Live> = Vec::new();
        let request = serde_json::json!({"method": "subscribe", "subscription": subscription});
        request_reply(&request.to_string(), &mut subs, &state);
        let mut live = subs.pop().expect("subscribed");
        let frame = pushes(&mut live, &state, crate::now_ms())
            .into_iter()
            .next()
            .expect("a first frame");
        serde_json::from_str(&frame).unwrap()
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (dex, coin) in [("", "ETH"), ("xyz", "xyz:TSLA")] {
        let from_rest = prices(&runtime.block_on(rest(dex)));
        assert_eq!(from_rest.len(), 1);
        assert_eq!(from_rest[0].0, coin);
        let px = from_rest[0]
            .1
            .as_str()
            .expect("a cross position serves a price");
        let long = coin == "ETH";
        let px: Decimal = px.parse().unwrap();
        assert_eq!(px > Decimal::from(100), !long, "{coin}: {px}");

        let channel =
            subscribe(serde_json::json!({"type": "clearinghouseState", "user": user, "dex": dex}));
        assert_eq!(channel["channel"], "clearinghouseState");
        let from_channel = prices(&channel["data"]["clearinghouseState"]);
        assert_eq!(from_channel, from_rest, "the channel for `{dex}`");
    }
    let all = subscribe(serde_json::json!({"type": "allDexsClearinghouseState", "user": user}));
    let states = all["data"]["clearinghouseStates"].as_array().unwrap();
    assert_eq!(states.len(), 2);
    for entry in states {
        let dex: &'static str = if entry[0] == "" { "" } else { "xyz" };
        let from_rest = prices(&runtime.block_on(rest(dex)));
        assert_eq!(
            prices(&entry[1]),
            from_rest,
            "allDexsClearinghouseState `{dex}`"
        );
    }
}
