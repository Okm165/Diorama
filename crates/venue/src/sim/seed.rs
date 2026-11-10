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
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

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
    /// Whether this patch changes the account's **balances or positions** (as
    /// opposed to a mode-only setup). A value-changing patch is a scenario
    /// input: the venue waterfall must not liquidate it and the served account
    /// must carry `controlOwned` until the fixture is cleared. A mode-only
    /// patch is ordinary account setup and keeps market provenance.
    pub fn changes_account_state(&self) -> bool {
        self.margin_summary.is_some()
            || self.asset_positions.is_some()
            || self.spot_balances.is_some()
    }

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
    /// unnamed fields keep their previous value.
    ///
    /// # `totalRawUsd` is the whole-account raw, not the perp leg
    ///
    /// The venue's `marginSummary.totalRawUsd` is the **perp sub-ledger's**
    /// raw cash. The unified collateral token's spot side is a separate ledger
    /// (the spot-only fixture serves `accountValue = 0` beside 2995.67 spot),
    /// so a patch names both without one being derived from the other. The old
    /// fold added spot to the perp raw at serve time and then subtracted it
    /// here, which kept the served sum equal to the patch but made the perp
    /// wallet carry a phantom negative balance; bridging the spot then drove
    /// the served perp equity negative (the live `cf16` 240.510247 recovery).
    /// The perp leg is now exactly the named raw:
    ///
    /// `perp_balance = totalRawUsd − isolatedMargin + Σ szi·entryPx`
    ///
    /// A patch that names a position set must also carry an `accountValue`
    /// consistent with its own rows (`accountValue == totalRawUsd + Σ sign(szi)·positionValue`);
    /// an inconsistent fixture is refused rather than silently resolved in
    /// either direction. The measured 4a patch (`accountValue=1.25`,
    /// `totalRawUsd=1.25`, short worth 6.99632) is exactly that inconsistency.
    ///
    /// A **summary-only** patch is checked before it touches the account: on an
    /// account with open positions it cannot be verified against the book
    /// (the account's positions were last marked at a different instant), so
    /// it must name `assetPositions` too. On a flat book its `accountValue`
    /// must equal its `totalRawUsd`. This is the guard against the measured
    /// recovery patch `totalRawUsd=0, accountValue=0.08` on a book holding a
    /// short and spot cash: the old fold accepted it and drove the account to
    /// `-14.894333`, stranding the residual and blocking the recall forever.
    ///
    /// Fallible: absurd or internally inconsistent seeds must refuse, not
    /// panic (the overflow-poison lesson).
    #[allow(clippy::too_many_lines)]
    pub fn apply_to(&self, account: &mut Account, clock: u64) -> Result<()> {
        self.validate(account)?;

        if let Some(mode) = self.user_abstraction {
            refuse_unmodeled_mode(mode)?;
            account.mode = mode;
        }
        if let Some(positions) = &self.asset_positions {
            apply_positions(account, positions)?;
        }
        if let Some(balances) = &self.spot_balances {
            apply_spot_balances(account, balances);
        }

        // A named summary sets the whole-account raw. A positions-only or
        // spot-only patch leaves the perp wallet's cash untouched: replacing
        // positions changes the cost basis (and therefore `totalRawUsd`), but
        // it must never move cash. The old fold preserved `totalRawUsd`
        // across a positions-only patch, which silently debited the new cost
        // basis from the perp wallet; on a standard→unified switch it also
        // subtracted the newly-visible spot collateral. The measured 22f
        // patch moved `7.82` to `-7.074333` and was liquidated on the
        // resulting fake deficit.
        if let Some(summary) = &self.margin_summary {
            let isolated = isolated_margin_total(account);
            let cost = cost_basis_total(account)?;
            let perp = add(
                sub(summary.total_raw_usd, isolated, "seed perp raw")?,
                cost,
                "seed balance",
            )?;
            account.perp_balance = perp;
            account.funded_at_ms = clock;
        }
        Ok(())
    }
}

impl AccountSeed {
    /// The fixture's own identity checks: a summary-only patch on an open
    /// book, or a patch whose `accountValue` disagrees with its positions, is
    /// refused before it touches the account.
    fn validate(&self, account: &Account) -> Result<()> {
        // A summary-only patch cannot be checked against an open book: the
        // summary's `accountValue` is the whole-book value at the caller's
        // mark, while the account's existing positions are at a different
        // mark. Refuse it rather than silently rewriting cash from an
        // unverified value. A flat book has no such ambiguity.
        if let Some(summary) = &self.margin_summary {
            let has_open_positions = account
                .positions
                .values()
                .any(|position| !position.szi.is_zero());
            if self.asset_positions.is_none() && has_open_positions {
                return Err(crate::error::Error::bad_request(
                    "a marginSummary patch on an account with open positions must also name assetPositions: the summary's accountValue cannot be checked against an unnamed book",
                ));
            }
            if self.asset_positions.is_none() && summary.account_value != summary.total_raw_usd {
                return Err(crate::error::Error::bad_request(format!(
                    "inconsistent flat-book account patch: accountValue {} != totalRawUsd {} with no positions named",
                    summary.account_value, summary.total_raw_usd
                )));
            }
        }

        // A fixture that names its positions is checked against its own
        // identity before it touches the account. The venue's wire contract is
        // `accountValue = totalRawUsd + Σ sign(szi)·positionValue`; a patch
        // that violates it has no single correct reading, so it is refused.
        if let (Some(summary), Some(positions)) = (&self.margin_summary, &self.asset_positions) {
            let signed = signed_seed_value(positions)?;
            let expected = add(summary.total_raw_usd, signed, "seed account value")?;
            if summary.account_value != expected {
                return Err(crate::error::Error::bad_request(format!(
                    "inconsistent account patch: accountValue {} != totalRawUsd {} + signed positionValue {} = {}",
                    summary.account_value, summary.total_raw_usd, signed, expected
                )));
            }
        }
        Ok(())
    }
}

/// The isolated legs' margin, which the venue's whole-account raw carries as
/// cash parked on the positions.
fn isolated_margin_total(account: &Account) -> Decimal {
    account
        .positions
        .values()
        .map(|position| position.isolated_margin)
        .fold(Decimal::ZERO, |sum, margin| sum + margin)
}

/// The signed notional of a seed's position rows: `Σ sign(szi)·positionValue`.
fn signed_seed_value(positions: &[AssetPosition]) -> Result<Decimal> {
    let mut signed = Decimal::ZERO;
    for asset in positions {
        let position = &asset.position;
        let value = if position.szi.is_sign_negative() {
            -position.position_value
        } else {
            position.position_value
        };
        signed = add(signed, value, "seed signed position value")?;
    }
    Ok(signed)
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
        cross_margin_summary: f
            .cross_margin_summary
            .or(mirror)
            .or(Some(v.cross_margin_summary)),
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
        .try_fold(Decimal::ZERO, |acc, p| {
            add(acc, p.cost_basis()?, COST_BASIS)
        })
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
                )));
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
                // The wire's `cumFunding` is a cost accumulator; the internal
                // figure is the balance delta. A seed is a wire snapshot, so
                // the sign flips back on the way in.
                funding_all_time: -p.cum_funding.all_time,
                funding_since_open: -p.cum_funding.since_open,
                funding_since_change: -p.cum_funding.since_change,
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::account::clearinghouse;
    use crate::sim::feed_with;
    use crate::sim::meta_with;
    use crate::sim::meta_with_table;

    /// The served identity holds exactly for a seeded position.
    #[test]
    #[allow(clippy::too_many_lines)] // one transition table: splitting it would hide the rule
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
        let signed = if p.szi.is_sign_negative() {
            -p.position_value
        } else {
            p.position_value
        };
        assert_eq!(
            s.margin_summary.account_value,
            s.margin_summary.total_raw_usd + signed
        );
        assert_eq!(
            p.position_value,
            Decimal::new(335, 4) * Decimal::new(29859, 1)
        );
        // totalRawUsd = balance − szi·entry (exactly).
        assert_eq!(
            s.margin_summary.total_raw_usd,
            account.perp_balance - p.szi * p.entry_px.unwrap()
        );
        // Cross view with no isolated positions equals the summary.
        assert_eq!(
            s.cross_margin_summary.account_value,
            s.margin_summary.account_value
        );
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
                "marginSummary": {"accountValue": "110.5", "totalNtlPos": "10.0", "totalRawUsd": "100.0", "totalMarginUsed": "5.0"},
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
        assert_eq!(
            account.perp_balance,
            Decimal::from(100) + Decimal::new(105, 2) * Decimal::from(10)
        );
        let feed = feed_with(&[("BTC", "10.0", "10.0")]);
        let meta = meta_with("BTC", 20);
        let s = clearinghouse(&account, &feed, Some(&meta), 1).unwrap();
        assert_eq!(s.margin_summary.total_raw_usd, Decimal::from(100));
    }

    /// **A unified seed names two ledgers, not one.** `totalRawUsd` is the
    /// perp sub-ledger's raw and `spotBalances` the separate spot ledger (the
    /// spot-only fixture serves `accountValue = 0` beside 2995.67 spot). A
    /// patch naming 0.5 in each serves 0.5 in the perp summary and 0.5 in
    /// spot; the old fold folded them into one 0.5 figure and then left the
    /// perp wallet at zero, so a later spot bridge drove the served perp
    /// equity negative.
    #[test]
    fn a_unified_seed_keeps_the_perp_and_spot_ledgers_separate() {
        let seed: AccountSeed = serde_json::from_str(
            r#"{
                "marginSummary": {"accountValue": "0.5", "totalNtlPos": "0", "totalRawUsd": "0.5", "totalMarginUsed": "0"},
                "spotBalances": [{"coin": "USDC", "token": 0, "hold": "0", "total": "0.5", "entryNtl": "0"}],
                "userAbstraction": "unifiedAccount"
            }"#,
        )
        .unwrap();
        let mut account = Account::default();
        seed.apply_to(&mut account, 0).unwrap();
        assert_eq!(
            account.perp_balance,
            "0.5".parse().unwrap(),
            "the perp raw is the named figure, not the spot side"
        );
        assert_eq!(
            account.spot.get(&COLLATERAL_TOKEN).map(|e| e.total),
            Some("0.5".parse().unwrap()),
            "the spot ledger is separate and preserved"
        );
        let feed = feed_with(&[("HYPE", "87.0", "87.0")]);
        let live = view(&account, &feed, None).unwrap();
        assert_eq!(live.margin_summary.total_raw_usd, "0.5".parse().unwrap());
        assert_eq!(live.margin_summary.account_value, "0.5".parse().unwrap());
    }

    /// **The measured 4a fixture is internally inconsistent and is refused.**
    /// It claims `accountValue=1.25` while naming a short whose `positionValue`
    /// is 6.99632 and `totalRawUsd=1.25`: the identity requires -5.74632. The
    /// old fold silently picked one reading and served 2.5; the patch now has
    /// no correct reading and is rejected before it touches the account.
    #[test]
    fn an_inconsistent_account_patch_is_refused() {
        let seed: AccountSeed = serde_json::from_str(
            r#"{
                "marginSummary": {"accountValue": "1.25", "totalNtlPos": "6.99632", "totalRawUsd": "1.25", "totalMarginUsed": "0.699632"},
                "assetPositions": [{
                    "type": "oneWay",
                    "position": {
                        "coin": "HYPE", "entryPx": "87.454", "leverage": {"type": "cross", "value": 10},
                        "marginUsed": "0.699632", "maxLeverage": 10, "positionValue": "6.99632",
                        "returnOnEquity": "0", "szi": "-0.08", "unrealizedPnl": "0",
                        "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
                    }
                }],
                "spotBalances": [{"coin": "USDC", "token": 0, "hold": "0", "total": "1.25", "entryNtl": "0"}],
                "userAbstraction": "unifiedAccount"
            }"#,
        )
        .unwrap();
        let mut account = Account::default();
        let error = seed.apply_to(&mut account, 0).unwrap_err();
        assert!(
            error.to_string().contains("inconsistent account patch"),
            "the refusal names the inconsistency: {error}"
        );
    }

    /// The same 4a book with a coherent `accountValue` applies as one account:
    /// 1.25 total raw, 1.25 spot, and the short's 6.99632 liability, so the
    /// served account value is -5.74632.
    #[test]
    fn a_coherent_non_flat_unified_seed_keeps_the_identity() {
        let seed: AccountSeed = serde_json::from_str(
            r#"{
                "marginSummary": {"accountValue": "-5.74632", "totalNtlPos": "6.99632", "totalRawUsd": "1.25", "totalMarginUsed": "0.699632"},
                "assetPositions": [{
                    "type": "oneWay",
                    "position": {
                        "coin": "HYPE", "entryPx": "87.454", "leverage": {"type": "cross", "value": 10},
                        "marginUsed": "0.699632", "maxLeverage": 10, "positionValue": "6.99632",
                        "returnOnEquity": "0", "szi": "-0.08", "unrealizedPnl": "0",
                        "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
                    }
                }],
                "spotBalances": [{"coin": "USDC", "token": 0, "hold": "0", "total": "1.25", "entryNtl": "0"}],
                "userAbstraction": "unifiedAccount"
            }"#,
        )
        .unwrap();
        let mut account = Account::default();
        seed.apply_to(&mut account, 0).unwrap();
        let feed = feed_with(&[("HYPE", "87.454", "87.454")]);
        let meta = meta_with("HYPE", 10);
        let live = view(&account, &feed, Some(&meta)).unwrap();
        assert_eq!(live.margin_summary.total_raw_usd, "1.25".parse().unwrap());
        assert_eq!(
            live.margin_summary.account_value,
            "-5.74632".parse().unwrap()
        );
    }

    /// **The measured 22f regression.** A positions-only patch that switches a
    /// standard account to unified must preserve the perp wallet's cash. The
    /// old fold preserved `totalRawUsd`, then subtracted the newly-visible spot
    /// collateral and the short's cost basis from the perp leg: `7.82` became
    /// `-7.074333`, the served account value fell from `15.677973` to `0.78`,
    /// and the position was liquidated on a fake deficit two minutes after
    /// creation.
    #[test]
    fn a_positions_only_patch_preserves_cash_across_a_mode_switch() {
        let mut account = Account {
            perp_balance: "7.82".parse().unwrap(),
            ..Account::default()
        };
        account.spot.insert(
            0,
            SpotEntry {
                coin: "USDC".into(),
                total: "7.857973".parse().unwrap(),
                hold: Decimal::ZERO,
                entry_ntl: Decimal::ZERO,
            },
        );
        let seed: AccountSeed = serde_json::from_str(
            r#"{
                "userAbstraction": "unifiedAccount",
                "assetPositions": [{
                    "type": "oneWay",
                    "position": {
                        "coin": "HYPE", "entryPx": "87.9545", "leverage": {"type": "cross", "value": 10},
                        "marginUsed": "0.703636", "maxLeverage": 10, "positionValue": "7.03636",
                        "returnOnEquity": "0", "szi": "-0.08", "unrealizedPnl": "0",
                        "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
                    }
                }]
            }"#,
        )
        .unwrap();
        seed.apply_to(&mut account, 0).unwrap();
        assert_eq!(
            account.perp_balance,
            "7.82".parse().unwrap(),
            "positions do not move cash"
        );
        let feed = feed_with(&[("HYPE", "87.9545", "87.9545")]);
        let meta = meta_with("HYPE", 10);
        let live = view(&account, &feed, Some(&meta)).unwrap();
        assert_eq!(
            live.margin_summary.total_raw_usd,
            "14.856360".parse().unwrap(),
            "the perp summary carries the perp wallet and the cost basis only"
        );
        assert_eq!(
            live.margin_summary.account_value,
            "7.82".parse().unwrap(),
            "the 7.857973 spot balance is a separate ledger"
        );
    }

    /// **The measured recovery zeroing.** A summary-only patch cannot rewrite
    /// a book whose positions it does not name. The old fold accepted
    /// `totalRawUsd=0, accountValue=0.08` on a book holding a short and spot
    /// cash, drove the account to `-14.894333`, and left the recall with
    /// `withdrawable=0` forever.
    #[test]
    fn a_summary_only_patch_cannot_rewrite_an_open_book() {
        let mut account = Account {
            perp_balance: "7.82".parse().unwrap(),
            ..Account::default()
        };
        account.spot.insert(
            0,
            SpotEntry {
                coin: "USDC".into(),
                total: "7.857973".parse().unwrap(),
                hold: Decimal::ZERO,
                entry_ntl: Decimal::ZERO,
            },
        );
        account.positions.insert(
            "HYPE".into(),
            Position {
                szi: "-0.08".parse().unwrap(),
                entry_px: "87.9545".parse().unwrap(),
                leverage: LeverageType::Cross,
                leverage_value: 10,
                margin_used: "0.703636".parse().unwrap(),
                isolated_margin: Decimal::ZERO,
                funding_all_time: Decimal::ZERO,
                funding_since_open: Decimal::ZERO,
                funding_since_change: Decimal::ZERO,
            },
        );
        let seed: AccountSeed = serde_json::from_str(
            r#"{
                "marginSummary": {"accountValue": "0.08", "totalNtlPos": "7.03636", "totalRawUsd": "0", "totalMarginUsed": "0.07"}
            }"#,
        )
        .unwrap();
        let error = seed.apply_to(&mut account, 0).unwrap_err();
        assert!(
            error.to_string().contains("must also name assetPositions"),
            "the refusal names the missing book: {error}"
        );
        assert_eq!(
            account.perp_balance,
            "7.82".parse().unwrap(),
            "the refused patch did not move cash"
        );
        assert!(
            account.positions.contains_key("HYPE"),
            "the refused patch did not clear positions"
        );
        assert_eq!(account.spot[&0].total, "7.857973".parse().unwrap());
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
        assert_eq!(
            account.perp_balance,
            Decimal::from(100),
            "the named summary replaces the raw base"
        );
        assert_eq!(
            account.spot[&0].total,
            Decimal::from(250),
            "unnamed balances keep their value"
        );
        assert_eq!(account.funded_at_ms, 7);

        let clear: AccountSeed = serde_json::from_str(r#"{"spotBalances": []}"#).unwrap();
        clear.apply_to(&mut account, 8).unwrap();
        assert!(
            account.spot.is_empty(),
            "a named empty spotBalances list clears"
        );
        assert_eq!(
            account.perp_balance,
            Decimal::from(100),
            "a spotBalances-only patch keeps the perp side"
        );
        assert_eq!(
            account.funded_at_ms, 7,
            "a spotBalances-only patch does not re-stamp the funding baseline"
        );

        // The patch shape's own field names land (measured: the served names
        // were silently ignored).
        let mode: AccountSeed =
            serde_json::from_str(r#"{"userAbstraction": "unifiedAccount"}"#).unwrap();
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
    #[allow(clippy::too_many_lines)] // one fixture: splitting it would hide the round-trip
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
        assert_eq!(
            account.perp_balance,
            Decimal::from(80_000),
            "entryPx 0.0: raw == perp balance"
        );

        let feed = feed_with(&[("HYPE", "300", "300")]);
        let meta = meta_with("HYPE", 10);
        let before = control_view(Some(&seed), &account, &feed, Some(&meta)).unwrap();
        assert_eq!(
            before.margin_summary.unwrap().account_value,
            Decimal::from(50_000)
        );
        let live = view(&account, &feed, Some(&meta)).unwrap();
        assert_eq!(
            live.margin_summary.account_value,
            Decimal::from(50_000),
            "the live view agrees at the setup mark"
        );

        feed.put_ctx("HYPE".into(), crate::wire::test_ctx("310", "310"), 2);
        let after = control_view(Some(&seed), &account, &feed, Some(&meta)).unwrap();
        assert_eq!(
            after.margin_summary.unwrap().account_value,
            Decimal::from(50_000),
            "the fixture read must not move with the mark"
        );
        let live = view(&account, &feed, Some(&meta)).unwrap();
        assert_eq!(
            live.margin_summary.account_value,
            Decimal::from(49_000),
            "the live view follows the mark"
        );
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
        assert!(
            merged.margin_summary.is_some(),
            "the unnamed summary keeps its value"
        );
        assert_eq!(
            merged.user_abstraction,
            Some(UserAbstraction::UnifiedAccount)
        );

        let clear: AccountSeed = serde_json::from_str(r#"{"spotBalances": []}"#).unwrap();
        let merged = clear.merged_with(Some(merged));
        assert_eq!(
            merged.spot_balances,
            Some(Vec::new()),
            "a named empty field clears in the fixture too"
        );
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
        assert_eq!(
            account.mode,
            UserAbstraction::Default,
            "the mode is untouched"
        );
        let seed: AccountSeed =
            serde_json::from_str(r#"{"userAbstraction": "unifiedAccount"}"#).unwrap();
        seed.apply_to(&mut account, 0).unwrap();
        assert_eq!(account.mode, UserAbstraction::UnifiedAccount);
    }
}
