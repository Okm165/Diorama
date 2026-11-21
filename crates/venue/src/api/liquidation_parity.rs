//! Parity of the liquidation figures with the real venue: accounts captured
//! live (`fixtures/real_liquidation.json`, read-only `clearinghouseState` calls,
//! every address a placeholder) are replayed through the sim's own maintenance
//! and liquidation-price functions.
//!
//! This step's liquidation machinery is the older one in `waterfall.rs`: a
//! flat maintenance rate per margin tier, and an isolated liquidation price.
//! So the harness verifies what that machinery computes — the venue's
//! `crossMaintenanceMarginUsed` and every isolated position's `liquidationPx` —
//! and marks the fork's newer branches deferred with a pointer instead of
//! recomputing them here:
//!
//! - the margin-tier **deduction** (`notional·rate − deduction`, the
//!   continuous requirement above a tier's bound) is not in `waterfall.rs`;
//!   the one captured case that exercises it (`whaleTier`/`rootInHigherTier`,
//!   1,000,000 of deduction on a 24M HYPE position) is skipped;
//! - the cross-position liquidation price solver is not in `waterfall.rs`
//!   (a cross row serves no `liquidationPx` today), so the capture's cross
//!   prices are not recomputed;
//! - unified accounts are the deferred single-ledger switch: their captured
//!   collateral figure (`availableAfterMaintenance`) and cross prices are not
//!   replayed. The isolated rows are still exercised through the same
//!   formula, but the unified case as a whole is left to step 9 rather than
//!   split into a fake.
//!
//! Measured on the cases that do run: maintenance agrees to the venue's
//! printed precision (relative 1e-6), isolated prices to 1e-9 on every one of
//! the 8 captured isolated positions.
use crate::sim::account::{liquidation_px, maintenance_margin, maintenance_rate, Position};
use crate::wire::{AssetInfo, LeverageType, MarginTable, MarginTier, Meta};
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
            sz_decimals: 5,
            max_leverage: u32::try_from(a["maxLeverage"].as_u64().unwrap()).unwrap(),
            margin_table_id: a["marginTableId"]
                .as_u64()
                .map(|id| u32::try_from(id).unwrap()),
            margin_mode: None,
            only_isolated: None,
            is_delisted: None,
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

/// Whether any held position sits in a margin tier above the table's first:
/// the branch the fork's deduction models and this step's flat rate does not.
/// (Its second-tier rate is charged without the `notional·Δrate` deduction,
/// which overstates the requirement by exactly that amount.)
fn exercises_tier_deduction(case: &Value) -> bool {
    let meta = meta_of(case);
    case["state"]["assetPositions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| {
            let p = &row["position"];
            let coin = p["coin"].as_str().unwrap();
            let notional = dec(&p["positionValue"]);
            let Some(table) = meta.margin_table(coin) else {
                return false;
            };
            let Some(first) = table.margin_tiers.first() else {
                return false;
            };
            table
                .margin_tiers
                .iter()
                .filter(|tier| tier.lower_bound <= notional)
                .max_by_key(|tier| tier.lower_bound)
                .is_some_and(|tier| tier.max_leverage < first.max_leverage)
        })
}

/// The sim's position from a captured row; `marginUsed` is the venue's own
/// equity figure for an isolated leg (the parked margin plus its pnl).
fn position_of(p: &Value, leverage: LeverageType) -> Position {
    Position {
        szi: dec(&p["szi"]),
        entry_px: dec(&p["entryPx"]),
        leverage,
        leverage_value: u32::try_from(p["leverage"]["value"].as_u64().unwrap()).unwrap(),
        margin_used: dec(&p["marginUsed"]),
        isolated_margin: if leverage == LeverageType::Isolated {
            dec(&p["marginUsed"])
        } else {
            Decimal::ZERO
        },
        funding_all_time: Decimal::ZERO,
        funding_since_open: Decimal::ZERO,
        funding_since_change: Decimal::ZERO,
    }
}

/// Standard accounts (`disabled`/`default`): the clearinghouse's own cross
/// account value is the equity. Returns the number of isolated prices checked.
#[allow(clippy::too_many_lines)] // one captured case: maintenance, then every isolated position
fn replay(case: &Value) -> usize {
    let meta = meta_of(case);
    let positions: Vec<&Value> = case["state"]["assetPositions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["position"])
        .collect();

    // The cross maintenance is the sum of the positions' own requirements.
    let maintenance: Decimal = positions
        .iter()
        .filter(|p| p["leverage"]["type"] == "cross")
        .map(|p| {
            maintenance_margin(
                p["coin"].as_str().unwrap(),
                dec(&p["positionValue"]),
                Some(&meta),
            )
            .unwrap()
        })
        .sum();
    let served_maintenance = dec(&case["state"]["crossMaintenanceMarginUsed"]);
    assert!(
        close(maintenance, served_maintenance, "0.000001")
            || (maintenance - served_maintenance).abs() < Decimal::new(1, 5),
        "{}: maintenance {maintenance} vs the venue's {served_maintenance}",
        case["user"]
    );

    // Every isolated leg's price is its own margin against its own
    // maintenance (the formula `liquidation_px` implements).
    let mut priced = 0;
    for p in &positions {
        if p["leverage"]["type"] != "isolated" {
            continue;
        }
        let coin = p["coin"].as_str().unwrap();
        let value = dec(&p["positionValue"]);
        let sim = liquidation_px(
            &position_of(p, LeverageType::Isolated),
            value / dec(&p["szi"]).abs(),
            maintenance_rate(coin, value, Some(&meta)).unwrap(),
        )
        .unwrap();
        let real: Decimal = p["liquidationPx"]
            .as_str()
            .expect("an isolated row serves a price")
            .parse()
            .unwrap();
        assert!(
            close(sim, real, "0.000000001"),
            "{} {coin}: sim {sim} vs the venue's {real}",
            case["user"]
        );
        priced += 1;
    }
    priced
}

/// Standard accounts: the captured maintenance and isolated prices, minus the
/// one case whose requirement depends on the deferred tier deduction.
#[test]
#[allow(clippy::too_many_lines)] // one capture: every standard account, then the branch coverage
fn the_maintenance_and_isolated_prices_match_captured_real_accounts() {
    let real: Value = serde_json::from_str(REAL).unwrap();
    let cases = real["cases"].as_array().unwrap();
    let mut replayed = 0;
    let mut priced = 0;
    let mut deferred = 0;
    let mut tags = std::collections::BTreeSet::new();
    for case in cases {
        if exercises_tier_deduction(case) {
            deferred += 1;
            continue;
        }
        priced += replay(case);
        replayed += 1;
        tags.extend(
            case["tags"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap().to_string()),
        );
    }
    assert!(deferred >= 1, "the capture lost its deduction case");
    assert!(
        replayed >= 10,
        "replayed {replayed} of {} cases",
        cases.len()
    );
    assert!(priced >= 8, "checked {priced} isolated prices");
    // The capture covers every branch this machinery has: cross and isolated
    // positions, both directions, the builder and default ledgers, and more
    // than one position on a clearinghouse. `whaleTier` is the deferred one.
    for tag in [
        "cross",
        "isolated",
        "long",
        "short",
        "null",
        "builder",
        "default",
        "multi",
        "rootInHigherTier",
    ] {
        assert!(tags.contains(tag), "the capture lost its `{tag}` case");
    }
    assert!(
        !tags.contains("whaleTier"),
        "whaleTier is the deferred case"
    );
}

// step-9: the fork's remaining liquidation parity tests land with the newer
// liquidation machinery, not as stand-ins here:
//
// - `the_maintenance_deduction_keeps_the_requirement_continuous` pins
//   `notional·rate − deduction` across a tier boundary. `waterfall.rs`'s
//   `maintenance_rate` has no deduction term yet (case 0 above is the live
//   evidence: the flat rate overstates the venue by exactly 1,000,000).
// - `the_unified_prices_match_with_the_venues_collateral_figure` feeds the
//   venue's `tokenToAvailableAfterMaintenance + maintenance` as the equity;
//   that is the deferred unified-account switch and a cross-price solver.
// - `every_surface_serves_the_same_liquidation_px` needs the ws channels
//   (step 8) and the per-dex clearinghouse ledger (step 9), and compares cross
//   prices this step's `clearinghouse` does not compute.
