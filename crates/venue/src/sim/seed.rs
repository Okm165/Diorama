//! The seed patch (`set_account`/`/control/state`): the venue's served
//! clearinghouse shape applied as a *patch* — a named field replaces, an
//! unnamed one keeps (hl_mock's rule; a perp-only seed must not wipe spot
//! balances), and the patch is remembered as the control fixture so
//! `/control/state` reads back what was set. The measured divergences fixed
//! 2026-10-23 live here.
use super::account::{
    add, available_after_maintenance, refuse_unmodeled_mode, spot_balances, sub, view, Account,
    Position, SpotEntry, COLLATERAL_TOKEN, COST_BASIS,
};
use crate::error::Result;
use crate::feed::MarketFeed;
use crate::wire::{AssetPosition, LeverageType, MarginSummary, SpotBalance, UserAbstraction};
use serde::{Deserialize, Serialize};
use rust_decimal::Decimal;

/// The seeding shape — and the **control fixture**: hl_mock serves the venue's
/// own state, so a seed is a (possibly partial) `clearinghouseState` in the
/// control patch shape. The sim reads the mechanical fields (`totalRawUsd`,
/// the positions, the spot side, the mode) and derives the rest at serve time;
/// the fixture-only fields (`crossMarginSummary`, `crossMaintenanceMarginUsed`,
/// `withdrawable`, `tokenToAvailableAfterMaintenance`, `spotToken`, `dex`) are
/// kept verbatim and served back by `/control/state` — the control surface is
/// "what the scenario set" (daloc's `resolved_equity` assertions read it; live
/// marks must not move a fixture read), while `/info` is the simulator's live
/// view.
///
/// **Patch semantics** (their `venue_mock::set_venue_account` contract, and
/// their mock's own `AccountState::apply`): a field the caller does not name
/// keeps its previous value — `None` = not named, `Some([])` = named-and-
/// empty. The field names are the **patch shape's** (`userAbstraction`,
/// `spotBalances` — not the info endpoints' `userAbstraction`/`balances`).
/// Measured divergences fixed 2026-10-23: a perp-only seed used to *replace*
/// the account and silently wipe its spot balances (the
/// deal-spot-then-seed sequence lost the deal), and the mode/spot fields
/// carried the served names, so a `userAbstraction`/`spotBalances` patch was
/// silently ignored.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountSeed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_summary: Option<MarginSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_positions: Option<Vec<AssetPosition>>,
    /// The spot side, in the patch shape (`spotBalances`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot_balances: Option<Vec<SpotBalance>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_abstraction: Option<UserAbstraction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_margin_summary: Option<MarginSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_maintenance_margin_used: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawable: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_to_available_after_maintenance: Option<Vec<(u32, Decimal)>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot_token: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dex: Option<String>,
}

impl AccountSeed {
    /// Nothing named at all: an all-empty patch would silently register a zero
    /// account (a named-but-empty field, `Some([])`, is a legitimate clear).
    pub fn is_empty(&self) -> bool {
        self.margin_summary.is_none()
            && self.asset_positions.is_none()
            && self.spot_balances.is_none()
            && self.user_abstraction.is_none()
            && self.cross_margin_summary.is_none()
            && self.cross_maintenance_margin_used.is_none()
            && self.withdrawable.is_none()
            && self.token_to_available_after_maintenance.is_none()
            && self.spot_token.is_none()
            && self.dex.is_none()
    }

    /// hl_mock's patch merge (their `AccountState::apply`): a named field
    /// replaces, an unnamed one keeps its previous value — the control
    /// fixture's rule, so a mode-only patch does not erase the book.
    pub fn merged_with(self, previous: Option<Self>) -> Self {
        let Some(p) = previous else {
            return self;
        };
        Self {
            margin_summary: self.margin_summary.or(p.margin_summary),
            asset_positions: self.asset_positions.or(p.asset_positions),
            spot_balances: self.spot_balances.or(p.spot_balances),
            user_abstraction: self.user_abstraction.or(p.user_abstraction),
            cross_margin_summary: self.cross_margin_summary.or(p.cross_margin_summary),
            cross_maintenance_margin_used: self
                .cross_maintenance_margin_used
                .or(p.cross_maintenance_margin_used),
            withdrawable: self.withdrawable.or(p.withdrawable),
            token_to_available_after_maintenance: self
                .token_to_available_after_maintenance
                .or(p.token_to_available_after_maintenance),
            spot_token: self.spot_token.or(p.spot_token),
            dex: self.dex.or(p.dex),
        }
    }

    /// Fold the seed into the account as a **patch**: named fields replace,
    /// unnamed fields keep their previous value. `totalRawUsd` is preserved
    /// exactly: `perp_balance = totalRawUsd + Σ szi·entryPx`, so the served
    /// totalRawUsd round-trips byte-for-byte (the operator's recall amount).
    /// Fallible: absurd seeds must refuse, not panic (the overflow-poison
    /// lesson).
    pub fn apply_to(&self, account: &mut Account, clock: u64) -> Result<()> {
        let perp_changed = self.margin_summary.is_some() || self.asset_positions.is_some();
        // The raw USDC base: the named summary's totalRawUsd, else the
        // existing one (reverse the current cost-basis fold).
        let raw = if let Some(summary) = &self.margin_summary {
            summary.total_raw_usd
        } else {
            sub(account.perp_balance, cost_basis_total(account)?, "raw usdc")?
        };
        if let Some(positions) = &self.asset_positions {
            apply_positions(account, positions)?;
        }
        if perp_changed {
            account.perp_balance = add(raw, cost_basis_total(account)?, "seed balance")?;
            account.funded_at_ms = clock;
        }
        if let Some(balances) = &self.spot_balances {
            apply_spot_balances(account, balances);
        }
        if let Some(mode) = self.user_abstraction {
            refuse_unmodeled_mode(mode)?;
            account.mode = mode;
        }
        Ok(())
    }
}

/// The `/control/state` view: the fixture's fields where the scenario owns
/// them, the sim's derivation where it does not — so a patched account reads
/// back exactly what the harness set (daloc's `resolved_equity` assertions
/// read this; live marks must not move a fixture read), while a never-patched
/// account still reads sensibly. `/info` is unaffected: it always serves the
/// live view.
pub(crate) fn control_view(
    fixture: Option<&AccountSeed>,
    account: &Account,
    feed: &MarketFeed,
    meta: Option<&crate::wire::Meta>,
) -> Result<AccountSeed> {
    let f = fixture.cloned().unwrap_or_default();
    let v = view(account, feed, meta)?;
    // hl_mock's `effective`: an unnamed cross view mirrors the margin summary.
    let mirror = f.margin_summary.clone();
    Ok(AccountSeed {
        user_abstraction: f.user_abstraction.or(Some(account.mode)),
        margin_summary: f.margin_summary.or(Some(v.margin_summary)),
        cross_margin_summary: f.cross_margin_summary.or(mirror).or(Some(v.cross_margin_summary)),
        cross_maintenance_margin_used: f
            .cross_maintenance_margin_used
            .or(Some(v.cross_maintenance_margin_used)),
        withdrawable: f.withdrawable.or(Some(v.withdrawable)),
        asset_positions: f.asset_positions.or(Some(v.asset_positions)),
        spot_balances: f.spot_balances.or_else(|| Some(spot_balances(account))),
        token_to_available_after_maintenance: f
            .token_to_available_after_maintenance
            .or(Some(available_after_maintenance(account, feed, meta)?)),
        spot_token: f.spot_token.or(Some(COLLATERAL_TOKEN)),
        dex: f.dex,
    })
}

/// Replace the account's positions from the seed's rows. A non-flat row
/// without an entry price cannot reconstruct the raw base
/// (`perp_balance = totalRawUsd + Σ szi·entryPx`): refused loudly, never a
/// zero-cost basis; a flat row's entry price is irrelevant.
/// The account's positions' cost basis, summed — the seed's fold (the raw
/// USDC is the balance minus it; the patch's balance is the raw plus it).
fn cost_basis_total(account: &Account) -> Result<Decimal> {
    account
        .positions
        .values()
        .try_fold(Decimal::ZERO, |acc, p| add(acc, p.cost_basis()?, COST_BASIS))
}

/// Replace the spot balances (the patch's `spotBalances`: a replace, never a
/// merge — the measured hl_mock semantics), the spot twin of
/// [`apply_positions`].
fn apply_spot_balances(account: &mut Account, balances: &[SpotBalance]) {
    account.spot.clear();
    for b in balances {
        account.spot.insert(
            b.token,
            SpotEntry {
                coin: b.coin.clone(),
                total: b.total,
                hold: b.hold,
                entry_ntl: b.entry_ntl,
            },
        );
    }
}

fn apply_positions(account: &mut Account, positions: &[AssetPosition]) -> Result<()> {
    account.positions.clear();
    for asset in positions {
        let p = &asset.position;
        let entry = match p.entry_px {
            Some(px) => px,
            None if p.szi.is_zero() => Decimal::ZERO,
            None => {
                return Err(crate::error::Error::bad_request(format!(
                        "position {} has size {} but no entryPx (the raw base needs it)",
                        p.coin, p.szi
                    ),))
            }
        };
        account.positions.insert(
            p.coin.clone(),
            Position {
                szi: p.szi,
                entry_px: entry,
                leverage: p.leverage.kind,
                leverage_value: p.leverage.value,
                margin_used: p.margin_used,
                isolated_margin: if p.leverage.kind == LeverageType::Isolated {
                    p.margin_used
                } else {
                    Decimal::ZERO
                },
                funding_all_time: p.cum_funding.all_time,
                funding_since_open: p.cum_funding.since_open,
                funding_since_change: p.cum_funding.since_change,
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::sim::account::clearinghouse;
    use crate::sim::meta_with_table;
    use super::*;
    use crate::sim::feed_with;

    /// The served identity holds exactly for a seeded position.
    #[test]
    fn identity_holds_for_seeded_positions() {
        let feed = feed_with(&[("ETH", "2985.9", "2986.0")]);
        let meta = meta_with_table("ETH", 50);
        let mut account = Account {
            perp_balance: Decimal::from(1000),
            ..Account::default()
        };
        account.positions.insert(
            "ETH".into(),
            Position {
                szi: "0.0335".parse().unwrap(),
                entry_px: "2986.3".parse().unwrap(),
                leverage: LeverageType::Cross,
                leverage_value: 20,
                margin_used: "4.967826".parse().unwrap(),
                isolated_margin: Decimal::ZERO,
                funding_all_time: Decimal::ZERO,
                funding_since_open: Decimal::ZERO,
                funding_since_change: Decimal::ZERO,
            },
        );
        let s = clearinghouse(&account, &feed, Some(&meta), 1_000).unwrap();
        let p = &s.asset_positions[0].position;
        let signed = if p.szi.is_sign_negative() { -p.position_value } else { p.position_value };
        assert_eq!(s.margin_summary.account_value, s.margin_summary.total_raw_usd + signed);
        assert_eq!(p.position_value, Decimal::new(335, 4) * Decimal::new(29859, 1));
        // totalRawUsd = balance − szi·entry (exactly).
        assert_eq!(
            s.margin_summary.total_raw_usd,
            account.perp_balance - p.szi * p.entry_px.unwrap()
        );
        // Cross view with no isolated positions equals the summary.
        assert_eq!(s.cross_margin_summary.account_value, s.margin_summary.account_value);
        // The docs' transfer rule bounds `withdrawable` by
        // max(initial_margin_required, 0.1 * total_position_value). The seed
        // here is a *cross* position (the endpoint's own example is an
        // isolated one, whose cross side is flat and whose withdrawable is
        // therefore the full cross value), so the 10 % floor binds.
        let floor = s
            .cross_margin_summary
            .total_margin_used
            .max(p.position_value * Decimal::new(1, 1));
        assert_eq!(
            s.withdrawable,
            s.cross_margin_summary.account_value - floor,
            "the transfer floor stays behind"
        );
    }

    /// The seed round-trips totalRawUsd exactly (the operator's recall).
    #[test]
    fn seed_preserves_total_raw_usd() {
        let seed: AccountSeed = serde_json::from_str(
            r#"{
                "marginSummary": {"accountValue": "100.5", "totalNtlPos": "10.0", "totalRawUsd": "100.0", "totalMarginUsed": "5.0"},
                "assetPositions": [{
                    "type": "oneWay",
                    "position": {
                        "coin": "BTC", "entryPx": "10.0", "leverage": {"type": "cross", "value": 10},
                        "marginUsed": "5.0", "maxLeverage": 20, "positionValue": "10.5",
                        "returnOnEquity": "0.1", "szi": "1.05", "unrealizedPnl": "0.5",
                        "cumFunding": {"allTime": "0.0", "sinceOpen": "0.0", "sinceChange": "0.0"}
                    }
                }]
            }"#,
        )
        .unwrap();
        let mut account = Account::default();
        seed.apply_to(&mut account, 0).unwrap();
        assert_eq!(account.perp_balance, Decimal::from(100) + Decimal::new(105, 2) * Decimal::from(10));
        let feed = feed_with(&[("BTC", "10.0", "10.0")]);
        let s = clearinghouse(&account, &feed, None, 1).unwrap();
        assert_eq!(s.margin_summary.total_raw_usd, Decimal::from(100));
    }

    /// The seed is a patch: an unnamed field keeps its previous value — the
    /// deal-spot-then-seed sequence must not lose the spot balance. A named
    /// but empty field clears.
    #[test]
    fn seed_patches_named_fields_only() {
        let mut account = Account {
            perp_balance: Decimal::from(500),
            ..Account::default()
        };
        account.spot.insert(
            0,
            SpotEntry {
                coin: "USDC".into(),
                total: Decimal::from(250),
                hold: Decimal::ZERO,
                entry_ntl: Decimal::ZERO,
            },
        );
        let seed: AccountSeed =
            serde_json::from_str(r#"{"marginSummary": {"accountValue": "100", "totalNtlPos": "0", "totalRawUsd": "100", "totalMarginUsed": "0"}}"#)
                .unwrap();
        seed.apply_to(&mut account, 7).unwrap();
        assert_eq!(account.perp_balance, Decimal::from(100), "the named summary replaces the raw base");
        assert_eq!(account.spot[&0].total, Decimal::from(250), "unnamed balances keep their value");
        assert_eq!(account.funded_at_ms, 7);

        let clear: AccountSeed = serde_json::from_str(r#"{"spotBalances": []}"#).unwrap();
        clear.apply_to(&mut account, 8).unwrap();
        assert!(account.spot.is_empty(), "a named empty spotBalances list clears");
        assert_eq!(account.perp_balance, Decimal::from(100), "a spotBalances-only patch keeps the perp side");
        assert_eq!(account.funded_at_ms, 7, "a spotBalances-only patch does not re-stamp the funding baseline");

        // The patch shape's own field names land (measured: the served names
        // were silently ignored).
        let mode: AccountSeed = serde_json::from_str(r#"{"userAbstraction": "unifiedAccount"}"#).unwrap();
        mode.apply_to(&mut account, 9).unwrap();
        assert_eq!(account.mode, UserAbstraction::UnifiedAccount);
    }

    /// The hl_mock control patch shape — daloc's `standard_cross` output: no
    /// `maxLeverage`, no `returnOnEquity`, `entryPx: "0.0"`, a cross short, and
    /// the fixture-only fields the info surface never reads. It must parse (the
    /// parser is a superset), apply (the raw base round-trips), and read back
    /// through `/control/state` *exactly as set* while the live view follows the
    /// mark — the two surfaces are deliberately different.
    #[test]
    fn hl_mock_patch_parses_and_reads_back_as_the_fixture() {
        let seed: AccountSeed = serde_json::from_str(
            r#"{
                "userAbstraction": "disabled",
                "marginSummary": {"accountValue": "50000.0", "totalNtlPos": "30000.0", "totalRawUsd": "80000.0", "totalMarginUsed": "0.0"},
                "crossMaintenanceMarginUsed": "0.0",
                "withdrawable": "50000.0",
                "assetPositions": [{
                    "type": "oneWay",
                    "position": {
                        "coin": "HYPE", "szi": "-100.00", "leverage": {"type": "cross", "value": 0},
                        "entryPx": "0.0", "positionValue": "30000.0", "unrealizedPnl": "0.0",
                        "marginUsed": "0.0",
                        "cumFunding": {"allTime": "0.0", "sinceOpen": "0.0", "sinceChange": "0.0"}
                    }
                }]
            }"#,
        )
        .unwrap();
        let mut account = Account::default();
        seed.apply_to(&mut account, 0).unwrap();
        assert_eq!(account.perp_balance, Decimal::from(80_000), "entryPx 0.0: raw == perp balance");

        let feed = feed_with(&[("HYPE", "300", "300")]);
        let before = control_view(Some(&seed), &account, &feed, None).unwrap();
        assert_eq!(before.margin_summary.unwrap().account_value, Decimal::from(50_000));
        let live = view(&account, &feed, None).unwrap();
        assert_eq!(live.margin_summary.account_value, Decimal::from(50_000), "the live view agrees at the setup mark");

        feed.put_ctx("HYPE".into(), crate::wire::test_ctx("310", "310"), 2);
        let after = control_view(Some(&seed), &account, &feed, None).unwrap();
        assert_eq!(
            after.margin_summary.unwrap().account_value,
            Decimal::from(50_000),
            "the fixture read must not move with the mark"
        );
        let live = view(&account, &feed, None).unwrap();
        assert_eq!(live.margin_summary.account_value, Decimal::from(49_000), "the live view follows the mark");
    }

    /// hl_mock's patch merge: a mode-only patch must not erase the book (the
    /// control fixture keeps the unnamed fields); a named-but-empty field is a
    /// clear, not a keep.
    #[test]
    fn seed_merge_keeps_unnamed_fields() {
        let first: AccountSeed = serde_json::from_str(
            r#"{"marginSummary": {"accountValue": "10", "totalNtlPos": "0", "totalRawUsd": "10", "totalMarginUsed": "0"}}"#,
        )
        .unwrap();
        let second: AccountSeed =
            serde_json::from_str(r#"{"userAbstraction": "unifiedAccount"}"#).unwrap();
        let merged = second.merged_with(Some(first));
        assert!(merged.margin_summary.is_some(), "the unnamed summary keeps its value");
        assert_eq!(merged.user_abstraction, Some(UserAbstraction::UnifiedAccount));

        let clear: AccountSeed = serde_json::from_str(r#"{"spotBalances": []}"#).unwrap();
        let merged = clear.merged_with(Some(merged));
        assert_eq!(merged.spot_balances, Some(Vec::new()), "a named empty field clears in the fixture too");
        assert!(merged.margin_summary.is_some(), "and the other fields keep");
    }

    /// Portfolio margin is refused by name at the seed patch too: its
    /// cross-asset portfolio (the eligible set, the portfolio-wide available —
    /// measured live on two accounts) is the venue's own accounting, and a
    /// mode whose correctness would depend on the account's future spot
    /// holdings must not enter the state silently. The same patch with
    /// `unifiedAccount` applies.
    #[test]
    fn the_seed_patch_refuses_portfolio_margin() {
        let seed: AccountSeed =
            serde_json::from_str(r#"{"userAbstraction": "portfolioMargin"}"#).unwrap();
        let mut account = Account::default();
        let refused = seed.apply_to(&mut account, 0);
        assert!(refused.is_err(), "portfolio margin is not modeled");
        assert_eq!(account.mode, UserAbstraction::Default, "the mode is untouched");
        let seed: AccountSeed =
            serde_json::from_str(r#"{"userAbstraction": "unifiedAccount"}"#).unwrap();
        seed.apply_to(&mut account, 0).unwrap();
        assert_eq!(account.mode, UserAbstraction::UnifiedAccount);
    }
}
