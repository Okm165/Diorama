//! The account plane's model: accounts, positions, and the pure math that
//! turns them into the venue's served shapes.
//!
//! The internal truth is the *inputs* — the perp wallet's raw USDC balance,
//! positions with entry prices, spot balances — and every served figure is
//! computed from them against the live marks, so the identity daloc gates on
//! (`accountValue == totalRawUsd + Σ signed(positionValue)`) holds by
//! construction:
//!
//! ```text
//! totalRawUsd   = balance − Σ szi·entryPx
//! accountValue  = balance + Σ szi·(mark − entryPx) = totalRawUsd + Σ szi·mark
//! ```
//!
//! (verified exact on the live sample, `wire::tests::live_sample_identity_holds`).
use crate::error::{Error, Result};
use crate::feed::MarketFeed;
use crate::wire::{
    AssetCtx, AssetPosition, ClearinghouseState, CumFunding, Leverage, LeverageType, MarginSummary,
    Position as WirePosition, PositionKind, SpotBalance, UserAbstraction,
};
use alloy_primitives::Address;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One recorded API wallet (the agent identity daloc's cosigner registers).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct AgentWallet {
    pub wallet: Address,
    pub name: String,
}

/// One simulated account (persisted). The fields are the *inputs*; the wire
/// shape is derived per read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Account {
    /// The perp wallet's raw USDC balance (before cost basis).
    #[serde(default)]
    pub perp_balance: Decimal,
    #[serde(default)]
    pub positions: BTreeMap<String, Position>,
    /// Spot balances by token index (USDC = 0).
    #[serde(default)]
    pub spot: BTreeMap<u32, SpotEntry>,
    #[serde(default = "default_mode")]
    pub mode: UserAbstraction,
    /// The sim clock (ms) funding was last accrued to.
    #[serde(default)]
    pub funded_at_ms: u64,
    /// API wallets the CoreWriter registered for this account.
    #[serde(default)]
    pub agents: Vec<AgentWallet>,
    /// The per-coin leverage setting (`updateLeverage`): applied to a
    /// position when it opens, and to an open position immediately — the
    /// venue's own behavior (the setting outlives the position).
    #[serde(default)]
    pub leverage_settings: BTreeMap<String, LeverageSetting>,
    /// The control fixture: the merged `/control/state` patch, when the
    /// harness has set one. The control surface serves it (what the scenario
    /// set is what it reads back — daloc's `resolved_equity` assertions);
    /// `/info` never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<AccountSeed>,
}

fn default_mode() -> UserAbstraction {
    UserAbstraction::Default
}

/// One `updateLeverage` setting (the venue stores cross/isolated + value; it
/// outlives the position, exactly as the venue's does).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LeverageSetting {
    pub is_cross: bool,
    pub leverage: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Position {
    /// Signed size (negative = short).
    pub szi: Decimal,
    /// Size-weighted entry price.
    pub entry_px: Decimal,
    pub leverage: LeverageType,
    pub leverage_value: u32,
    /// The initial margin held (seeded; the matching lane maintains it).
    pub margin_used: Decimal,
    /// Cumulative funding, sign = balance delta (negative = paid).
    #[serde(default)]
    pub funding_all_time: Decimal,
    #[serde(default)]
    pub funding_since_open: Decimal,
    #[serde(default)]
    pub funding_since_change: Decimal,
}

impl Position {
    /// The wire `positionValue`: unsigned, |szi|·mark.
    pub fn position_value(&self, mark: Decimal) -> Result<Decimal> {
        mul(self.szi.abs(), mark, "position value")
    }

    /// Unrealized PnL at the mark: szi·(mark − entry).
    pub fn unrealized_pnl(&self, mark: Decimal) -> Result<Decimal> {
        mul(self.szi, sub(mark, self.entry_px, "mark − entry")?, "unrealized PnL")
    }

    /// The cost basis: szi·entry.
    pub fn cost_basis(&self) -> Result<Decimal> {
        mul(self.szi, self.entry_px, "cost basis")
    }

    pub fn is_long(&self) -> bool {
        !self.szi.is_sign_negative()
    }
}

/// Checked money arithmetic: rust_decimal panics on overflow, and a panic
/// under the state lock poisons the account plane for the process's life.
/// Every step an input can reach goes through these.
pub fn add(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal> {
    a.checked_add(b).ok_or(crate::error::Error::Overflow { what })
}

pub fn sub(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal> {
    a.checked_sub(b).ok_or(crate::error::Error::Overflow { what })
}

pub(crate) fn mul(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal> {
    a.checked_mul(b).ok_or(crate::error::Error::Overflow { what })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SpotEntry {
    pub coin: String,
    pub total: Decimal,
    #[serde(default)]
    pub hold: Decimal,
    #[serde(default)]
    pub entry_ntl: Decimal,
}

/// The account's spot entry for `token`, created zeroed on first touch — the
/// one place the entry's shape is built (the deal, the bridge's credit and
/// the USD class transfer's two directions all go through it).
pub(crate) fn spot_entry<'a>(account: &'a mut Account, token: crate::sim::TokenRef<'_>) -> &'a mut SpotEntry {
    account.spot.entry(token.index).or_insert_with(|| SpotEntry {
        coin: token.coin.to_string(),
        total: Decimal::ZERO,
        hold: Decimal::ZERO,
        entry_ntl: Decimal::ZERO,
    })
}

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
            let existing = account
                .positions
                .values()
                .try_fold(Decimal::ZERO, |acc, p| add(acc, p.cost_basis()?, "cost basis"))?;
            sub(account.perp_balance, existing, "raw usdc")?
        };
        if let Some(positions) = &self.asset_positions {
            apply_positions(account, positions)?;
        }
        if perp_changed {
            let cost_basis = account
                .positions
                .values()
                .try_fold(Decimal::ZERO, |acc, p| add(acc, p.cost_basis()?, "seed cost basis"))?;
            account.perp_balance = add(raw, cost_basis, "seed balance")?;
            account.funded_at_ms = clock;
        }
        if let Some(balances) = &self.spot_balances {
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
        if let Some(mode) = self.user_abstraction {
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
                funding_all_time: p.cum_funding.all_time,
                funding_since_open: p.cum_funding.since_open,
                funding_since_change: p.cum_funding.since_change,
            },
        );
    }
    Ok(())
}

/// The computed account figures, one pass over the positions.
pub(crate) struct AccountView {
    pub margin_summary: MarginSummary,
    pub cross_margin_summary: MarginSummary,
    pub cross_maintenance_margin_used: Decimal,
    pub withdrawable: Decimal,
    pub asset_positions: Vec<AssetPosition>,
    /// Per-position facts for the liquidation check (not served): the same
    /// pass that builds the served rows, so the check reuses their marks and
    /// maintenance instead of recomputing them. A missing entry means exactly
    /// "flat row" — the view skips flat rows, and a missing *price* fails the
    /// view outright ([`PositionFacts::of`]).
    facts: BTreeMap<String, PositionFacts>,
}

/// One position's derived facts: the feed's mark, the unsigned notional
/// (`position_value`) and the maintenance requirement at that mark. Computed
/// once, consumed by both the totals and the served row.
struct PositionFacts {
    mark: Decimal,
    value: Decimal,
    maintenance: Decimal,
}

impl PositionFacts {
    fn of(coin: &str, p: &Position, feed: &MarketFeed, meta: Option<&crate::wire::Meta>) -> Result<Self> {
        let (mark, _oracle) = feed
            .required_prices(coin)
            .ok_or_else(|| crate::error::Error::NoMarket { coin: coin.to_string() })?;
        let value = p.position_value(mark)?;
        let maintenance = maintenance_margin(coin, value, meta)?;
        Ok(Self {
            mark,
            value,
            maintenance,
        })
    }
}

/// The running totals over the non-flat positions — the account summary's
/// inputs, one accumulator instead of eight locals.
#[derive(Default)]
struct PositionTotals {
    notional: Decimal,
    margin: Decimal,
    signed_value: Decimal,
    cost_basis: Decimal,
    cross_notional: Decimal,
    cross_margin: Decimal,
    cross_maintenance: Decimal,
    isolated_margin: Decimal,
}

impl PositionTotals {
    fn add(&mut self, p: &Position, facts: &PositionFacts) -> Result<()> {
        let signed = if p.is_long() { facts.value } else { -facts.value };
        self.notional = add(self.notional, facts.value, "total notional")?;
        self.margin = add(self.margin, p.margin_used, "total margin")?;
        self.signed_value = add(self.signed_value, signed, "signed value")?;
        self.cost_basis = add(self.cost_basis, p.cost_basis()?, "cost basis")?;
        if p.leverage == LeverageType::Cross {
            self.cross_notional = add(self.cross_notional, facts.value, "cross notional")?;
            self.cross_margin = add(self.cross_margin, p.margin_used, "cross margin")?;
            self.cross_maintenance =
                add(self.cross_maintenance, facts.maintenance, "cross maintenance")?;
        } else {
            self.isolated_margin = add(self.isolated_margin, p.margin_used, "isolated margin")?;
        }
        Ok(())
    }
}

/// One served row for a non-flat position.
fn position_row(
    coin: &str,
    p: &Position,
    facts: &PositionFacts,
    meta: Option<&crate::wire::Meta>,
) -> Result<AssetPosition> {
    let u_pnl = p.unrealized_pnl(facts.mark)?;
    let roe = if p.margin_used.is_zero() {
        Decimal::ZERO
    } else {
        u_pnl / p.margin_used
    };
    let liquidation_px = match p.leverage {
        LeverageType::Isolated => Some(liquidation_px(
            p,
            facts.mark,
            maintenance_rate(coin, facts.value, meta),
        )?),
        LeverageType::Cross => None,
    };
    Ok(AssetPosition {
        kind: PositionKind::OneWay,
        position: WirePosition {
            coin: coin.to_string(),
            entry_px: Some(p.entry_px),
            leverage: Leverage {
                kind: p.leverage,
                value: p.leverage_value,
                raw_usd: Some(p.margin_used - facts.value),
            },
            liquidation_px,
            margin_used: p.margin_used,
            // The live bound for a tracked coin; for a coin the meta does not
            // carry (a seeded position outside the markets) the position's own
            // leverage — a value we know, never a fabricated bound.
            max_leverage: max_leverage(coin, meta).unwrap_or(p.leverage_value),
            position_value: facts.value,
            return_on_equity: roe,
            szi: p.szi,
            unrealized_pnl: u_pnl,
            cum_funding: CumFunding {
                all_time: p.funding_all_time,
                since_open: p.funding_since_open,
                since_change: p.funding_since_change,
            },
        },
    })
}

/// Assemble the account view from the walked totals: the identity holds by
/// construction (`accountValue == totalRawUsd + Σ signed(positionValue)`), and
/// the cross view carves isolated margin out (measured exact on the live
/// sample); with no isolated positions it equals the margin summary.
fn assemble(
    account: &Account,
    totals: &PositionTotals,
    rows: Vec<AssetPosition>,
    facts: BTreeMap<String, PositionFacts>,
) -> AccountView {
    // The unified account's single balance per asset (docs: "USDC unifies
    // perps + spot"): the collateral token's spot side is part of the raw
    // balance, so the identity holds with it included — `totalRawUsd` is the
    // same one USDC figure the spot state serves. A `usdClassTransfer` moves
    // between the two sides (never both), so nothing double-counts.
    let collateral = match account.mode {
        UserAbstraction::UnifiedAccount => account
            .spot
            .get(&COLLATERAL_TOKEN)
            .map(|e| e.total)
            .unwrap_or(Decimal::ZERO),
        _ => Decimal::ZERO,
    };
    let total_raw = account.perp_balance + collateral - totals.cost_basis;
    let account_value = total_raw + totals.signed_value;
    let margin_summary = MarginSummary {
        account_value,
        total_ntl_pos: totals.notional,
        total_raw_usd: total_raw,
        total_margin_used: totals.margin,
    };
    let cross_account_value = account_value - totals.isolated_margin;
    let cross_margin_summary = MarginSummary {
        account_value: cross_account_value,
        total_ntl_pos: totals.cross_notional,
        total_raw_usd: total_raw,
        total_margin_used: totals.cross_margin,
    };
    AccountView {
        margin_summary,
        cross_margin_summary,
        cross_maintenance_margin_used: totals.cross_maintenance,
        withdrawable: cross_account_value,
        asset_positions: rows,
        facts,
    }
}

/// Compute the served figures at the feed's current marks.
pub(crate) fn view(account: &Account, feed: &MarketFeed, meta: Option<&crate::wire::Meta>) -> Result<AccountView> {
    let mut totals = PositionTotals::default();
    let mut rows = Vec::with_capacity(account.positions.len());
    let mut by_coin = BTreeMap::new();
    for (coin, p) in &account.positions {
        // Flat rows are dropped from the served state (daloc's gate): a
        // zero-size position is a closed one, not a row.
        if p.szi.is_zero() {
            continue;
        }
        let facts = PositionFacts::of(coin, p, feed, meta)?;
        totals.add(p, &facts)?;
        rows.push(position_row(coin, p, &facts, meta)?);
        by_coin.insert(coin.clone(), facts);
    }
    Ok(assemble(account, &totals, rows, by_coin))
}

/// The served `clearinghouseState` (fresh `time` from the caller's wall clock).
pub(crate) fn clearinghouse(account: &Account, feed: &MarketFeed, meta: Option<&crate::wire::Meta>, time_ms: u64) -> Result<ClearinghouseState> {
    let v = view(account, feed, meta)?;
    Ok(ClearinghouseState {
        margin_summary: v.margin_summary,
        cross_margin_summary: v.cross_margin_summary,
        cross_maintenance_margin_used: v.cross_maintenance_margin_used,
        withdrawable: v.withdrawable,
        asset_positions: v.asset_positions,
        time: time_ms,
    })
}

/// The client's own gate, re-checked at the boundary: the served identity
/// `|accountValue − (totalRawUsd + Σ sign(szi)·positionValue)| ≤ 1e-6`. The
/// construction makes it hold by definition — the counter exists so a
/// regression in the construction is *visible* instead of silently failing
/// daloc's client gate (plan §7.8's invariant counter).
pub(crate) fn identity_holds(state: &ClearinghouseState) -> bool {
    let mut signed = Decimal::ZERO;
    for asset in &state.asset_positions {
        let value = if asset.position.szi.is_sign_negative() {
            -asset.position.position_value
        } else {
            asset.position.position_value
        };
        match signed.checked_add(value) {
            Some(sum) => signed = sum,
            None => return false,
        }
    }
    let Some(expected) = state.margin_summary.total_raw_usd.checked_add(signed) else {
        return false;
    };
    state
        .margin_summary
        .account_value
        .checked_sub(expected)
        .map(|diff| diff.abs() <= Decimal::new(1, 6))
        .unwrap_or(false)
}

/// The served spot balances (USDC token 0 sorts first, like the venue's).
pub(crate) fn spot_balances(account: &Account) -> Vec<SpotBalance> {
    let mut rows: Vec<SpotBalance> = account
        .spot
        .iter()
        .map(|(token, e)| SpotBalance {
            coin: e.coin.clone(),
            token: *token,
            hold: e.hold,
            total: e.total,
            entry_ntl: e.entry_ntl,
        })
        .collect();
    rows.sort_by_key(|b| b.token);
    rows
}

/// USDC: the venue's collateral token for the default dex (daloc's client
/// default, `collateral_token = 0`).
pub(crate) const COLLATERAL_TOKEN: u32 = 0;

/// The unified account's `tokenToAvailableAfterMaintenance`: the collateral
/// token's balance minus the perp maintenance — the identity daloc's client
/// reconciles (`available + maintenance == the token's balance`, its hard
/// gate, measured live against the venue). Standard accounts serve none: their
/// spot side is not read for equity.
pub(crate) fn available_after_maintenance(
    account: &Account,
    feed: &MarketFeed,
    meta: Option<&crate::wire::Meta>,
) -> Result<Vec<(u32, Decimal)>> {
    if account.mode != UserAbstraction::UnifiedAccount {
        return Ok(Vec::new());
    }
    let Some(entry) = account.spot.get(&COLLATERAL_TOKEN) else {
        return Ok(Vec::new());
    };
    let v = view(account, feed, meta)?;
    let isolated = v
        .asset_positions
        .iter()
        .filter(|row| row.position.leverage.kind == LeverageType::Isolated)
        .try_fold(Decimal::ZERO, |acc, row| {
            add(acc, row.position.margin_used, "isolated margin")
        })?;
    let maintenance = add(v.cross_maintenance_margin_used, isolated, "maintenance")?;
    Ok(vec![(
        COLLATERAL_TOKEN,
        sub(entry.total, maintenance, "available after maintenance")?,
    )])
}

/// The maintenance-margin rate for a notional: the margin tier's
/// `1/(2·maxLeverage)` (maintenance = ½ initial, plan §16.3).
pub(crate) fn maintenance_rate(coin: &str, notional: Decimal, meta: Option<&crate::wire::Meta>) -> Decimal {
    let Some(meta) = meta else {
        return Decimal::ZERO;
    };
    let Some(asset) = meta.universe.iter().find(|a| a.name == coin) else {
        return Decimal::ZERO;
    };
    let table = asset
        .margin_table_id
        .and_then(|id| meta.margin_tables.iter().find(|(tid, _)| *tid == id))
        .map(|(_, t)| t);
    let max_leverage = table
        .and_then(|t| {
            t.margin_tiers
                .iter()
                .filter(|tier| tier.lower_bound <= notional)
                .max_by_key(|tier| tier.lower_bound)
                .map(|tier| tier.max_leverage)
        })
        .unwrap_or(asset.max_leverage);
    if max_leverage == 0 {
        return Decimal::ZERO;
    }
    Decimal::ONE / (Decimal::from(2 * max_leverage))
}

pub(crate) fn maintenance_margin(coin: &str, notional: Decimal, meta: Option<&crate::wire::Meta>) -> Result<Decimal> {
    mul(notional, maintenance_rate(coin, notional, meta), "maintenance margin")
}

/// The coin's live max leverage — `None` when the live meta does not carry
/// the coin (a seeded position outside the tracked markets: the feed is
/// warming). Callers pick their own bounded fallback; a fabricated default
/// here would be a silent lie about a margin bound.
fn max_leverage(coin: &str, meta: Option<&crate::wire::Meta>) -> Option<u32> {
    meta.and_then(|m| m.universe.iter().find(|a| a.name == coin))
        .map(|a| a.max_leverage)
}

/// The isolated liquidation price (exact formula, plan §16.3):
/// `price − side·margin_available/position_size/(1 − l·side)`,
/// with the isolated position's own margin as `margin_available`.
pub(crate) fn liquidation_px(p: &Position, mark: Decimal, maintenance_rate: Decimal) -> Result<Decimal> {
    if p.szi.is_zero() {
        return Ok(mark);
    }
    let side = if p.is_long() { Decimal::ONE } else { -Decimal::ONE };
    let position_size = p.szi.abs();
    let denominator = sub(Decimal::ONE, mul(maintenance_rate, side, "liquidation denominator")?, "liquidation denominator")?;
    if denominator.is_zero() {
        return Ok(mark);
    }
    let distance = mul(side, p.margin_used, "liquidation margin")? / position_size / denominator;
    sub(mark, distance, "liquidation price")
}

/// Apply one fill to the account (the matching lane's single mutation): the
/// position's size and size-weighted entry update; a reducing fill realizes
/// its closed part's PnL into the perp balance, so the measured identity
/// (`accountValue == totalRawUsd + Σ signed positionValue`) stays exact; the
/// initial margin follows `|szi|·entry/leverage`. A flip (through zero) opens
/// the remainder at the fill price.
/// The sign of a decimal (−1/0/1); `rust_decimal` has no `signum`.
pub(crate) fn sign(d: Decimal) -> i8 {
    if d > Decimal::ZERO {
        1
    } else if d < Decimal::ZERO {
        -1
    } else {
        0
    }
}

/// One fill's facts (what the matching lane hands the account math): the
/// market, the direction, the filled size, the size-weighted price, the
/// leverage a new position takes, and the reduce-only flag.
#[derive(Debug, Clone)]
pub(crate) struct FillFacts {
    pub coin: String,
    pub is_buy: bool,
    pub size: Decimal,
    pub px: Decimal,
    pub leverage: u32,
    pub reduce_only: bool,
    /// The trading fee (venue base tier; charged on top of the fill — the
    /// plan's `closedPnl = fee + side·(mark−entry)·size`).
    pub fee: Decimal,
}

pub(crate) fn apply_fill(account: &mut Account, fill: &FillFacts) -> Result<()> {
    let signed = if fill.is_buy { fill.size } else { -fill.size };
    let old = account
        .positions
        .get(&fill.coin)
        .map(|p| p.szi)
        .unwrap_or(Decimal::ZERO);
    if fill.reduce_only && (old.is_zero() || sign(old) == sign(signed) || fill.size > old.abs()) {
        // The venue's own string (plan §16.3).
        return Err(Error::bad_request(
            "Reduce only order would increase position.",
        ));
    }
    let position = account
        .positions
        .entry(fill.coin.clone())
        .or_insert_with(|| Position {
            szi: Decimal::ZERO,
            entry_px: fill.px,
            leverage: LeverageType::Cross,
            leverage_value: fill.leverage.max(1),
            margin_used: Decimal::ZERO,
            funding_all_time: Decimal::ZERO,
            funding_since_open: Decimal::ZERO,
            funding_since_change: Decimal::ZERO,
        });
    if old.is_zero() || sign(old) == sign(signed) {
        apply_increase(position, old, fill)?;
    } else {
        let realized = apply_reduction(position, old, fill)?;
        account.perp_balance = add(account.perp_balance, realized, "realized PnL")?;
    }
    // The trading fee (the venue's base tier): charged on the fill's notional,
    // on both sides of a trade, in USDC — so a closing fill pays it too.
    if !fill.fee.is_zero() {
        account.perp_balance = sub(account.perp_balance, fill.fee, "trading fee")?;
    }
    if position.szi.is_zero() {
        account.positions.remove(&fill.coin);
        return Ok(());
    }
    if let Some(position) = account.positions.get_mut(&fill.coin) {
        // The venue's `cumFunding.sinceChange` restarts at every size change
        // (the fill *is* the change; the funding up to it is settled first —
        // the lane accrues before applying, see `land_fill`).
        position.funding_since_change = Decimal::ZERO;
        // `.max(1)` guards the decimal division (rust_decimal panics on a
        // zero divisor; a leverage of 0 is not a state this plane creates).
        position.margin_used = mul(position.szi.abs(), position.entry_px, "margin notional")?
            / Decimal::from(position.leverage_value.max(1));
    }
    Ok(())
}

/// An increase (or a fresh position): the entry becomes the size-weighted
/// average of the old notional and the fill.
fn apply_increase(position: &mut Position, old: Decimal, fill: &FillFacts) -> Result<()> {
    let signed = if fill.is_buy { fill.size } else { -fill.size };
    let old_notional = mul(old.abs(), position.entry_px, "entry notional")?;
    let fill_notional = mul(fill.size, fill.px, "fill notional")?;
    let new_szi = add(old, signed, "position size")?;
    position.entry_px = add(old_notional, fill_notional, "entry notional total")? / new_szi.abs();
    position.szi = new_szi;
    Ok(())
}

/// A reduction (or a flip): the closed part's realized PnL (returned for the
/// caller to credit — the measured identity stays exact); a flip opens the
/// remainder at the fill price.
fn apply_reduction(position: &mut Position, old: Decimal, fill: &FillFacts) -> Result<Decimal> {
    let signed = if fill.is_buy { fill.size } else { -fill.size };
    let closed = old.abs().min(fill.size);
    let realized = mul(
        mul(closed, sub(fill.px, position.entry_px, "px − entry")?, "closed PnL")?,
        Decimal::from(sign(old)),
        "signed PnL",
    )?;
    let new_szi = add(old, signed, "position size")?;
    if sign(new_szi) != sign(old) && !new_szi.is_zero() {
        position.entry_px = fill.px;
    }
    position.szi = new_szi;
    Ok(realized)
}

/// Accrue funding from the account's stamp to `to_ms` at the live rates
/// (hourly, oracle-priced; plan §16.3: `payment = −szi·oracle·rate·Δt/1h`).
///
/// A stamp of 0 means "never accrued": the first call establishes the
/// baseline (an account must not inherit funding from the epoch's beginning).
/// All-or-nothing per account: if any position's coin has no live context,
/// the stamp stays where it is — a partially accrued account would silently
/// lose the skipped positions' funding forever.
pub(crate) fn accrue_funding(account: &mut Account, feed: &MarketFeed, to_ms: u64) -> Result<usize> {
    if account.funded_at_ms == 0 {
        account.funded_at_ms = to_ms;
        return Ok(0);
    }
    let from = account.funded_at_ms;
    if to_ms <= from {
        return Ok(0);
    }
    let priced = account
        .positions
        .keys()
        .all(|coin| feed.required_prices(coin).is_some() && feed.funding(coin).is_some());
    if !priced {
        return Ok(0);
    }
    let hours = Decimal::from(to_ms - from) / Decimal::from(3_600_000u64);
    let mut moved = 0;
    for (coin, p) in account.positions.iter_mut() {
        let Some((_mark, oracle)) = feed.required_prices(coin) else {
            continue;
        };
        let Some(rate) = feed.funding(coin) else {
            continue;
        };
        let payment = mul(mul(mul(-p.szi, oracle, "funding notional")?, rate, "funding payment")?, hours, "funding payment")?;
        if payment.is_zero() {
            continue;
        }
        account.perp_balance = add(account.perp_balance, payment, "funding balance")?;
        p.funding_all_time = add(p.funding_all_time, payment, "cum funding")?;
        p.funding_since_open = add(p.funding_since_open, payment, "cum funding")?;
        p.funding_since_change = add(p.funding_since_change, payment, "cum funding")?;
        moved += 1;
    }
    account.funded_at_ms = to_ms;
    Ok(moved)
}

/// The maintenance check + the v1 close: when the account value falls below
/// its maintenance requirement, every position closes at the mark (the
/// waterfall — partial closes, the backstop, ADL — is Phase 2). An account
/// with no positions is never "below maintenance": negative equity alone is a
/// state the venue reports, not a liquidation to re-run every tick.
/// The v1 full-close (kept for the account-plane tests): the sim's tick runs
/// the venue's waterfall instead (`Sim::check_all`).
#[cfg(test)]
pub(crate) fn liquidate_if_needed(account: &mut Account, feed: &MarketFeed, meta: Option<&crate::wire::Meta>) -> Result<Option<Liquidation>> {
    if account.positions.is_empty() {
        return Ok(None);
    }
    let v = view(account, feed, meta)?;
    let maintenance = add(
        v.cross_maintenance_margin_used,
        isolated_maintenance(account, &v)?,
        "maintenance total",
    )?;
    if v.margin_summary.account_value >= maintenance {
        return Ok(None);
    }
    let closed: Vec<String> = account.positions.keys().cloned().collect();
    let mut realized = Decimal::ZERO;
    for (coin, p) in &account.positions {
        // The facts cover every non-flat position; a flat row contributes no
        // PnL (and has no facts — the view skips flat rows).
        if let Some(facts) = v.facts.get(coin) {
            realized = add(realized, p.unrealized_pnl(facts.mark)?, "realized PnL")?;
        }
    }
    account.positions.clear();
    account.perp_balance = add(account.perp_balance, realized, "liquidated balance")?;
    Ok(Some(Liquidation {
        positions_closed: closed,
        account_value: v.margin_summary.account_value,
        maintenance,
    }))
}

/// The isolated positions' maintenance total, from the view's own facts (the
/// cross side is already in `cross_maintenance_margin_used`). A flat isolated
/// row has no facts and contributes zero.
pub(crate) fn isolated_maintenance(account: &Account, v: &AccountView) -> Result<Decimal> {
    let mut total = Decimal::ZERO;
    for (coin, p) in &account.positions {
        if p.leverage != LeverageType::Isolated {
            continue;
        }
        if let Some(facts) = v.facts.get(coin) {
            total = add(total, facts.maintenance, "isolated maintenance")?;
        }
    }
    Ok(total)
}

/// What a liquidation moved (for the control response and the metrics).
#[derive(Debug, Clone, Serialize)]
pub struct Liquidation {
    pub positions_closed: Vec<String>,
    pub account_value: Decimal,
    pub maintenance: Decimal,
}

/// The account's mode for `userAbstraction` (unknown users are the caller's
/// business — the sim only answers for accounts it owns).
pub(crate) fn mode_of(account: &Account) -> UserAbstraction {
    account.mode
}

/// The coin's live context for `activeAssetData`'s mark (the full live ctx
/// when the feed has one, else a minimal shape from the composed prices).
pub(crate) fn ctx_of(feed: &MarketFeed, coin: &str) -> Option<AssetCtx> {
    let (mark, oracle) = feed.required_prices(coin)?;
    if let Some((_, ctxs)) = feed.meta_and_ctxs() {
        if let Some(found) = ctxs.iter().find(|c| c.mark_px == mark && c.oracle_px == oracle) {
            return Some(found.clone());
        }
    }
    Some(AssetCtx {
        // No funding source (neither an override nor the ctx cache) means this
        // minimal ctx is not servable: a zero would be a silent lie (the
        // caller — the WS lane — skips the push and retries on the next tick).
        funding: feed.funding(coin)?,
        open_interest: Decimal::ZERO,
        prev_day_px: mark,
        day_ntl_vlm: Decimal::ZERO,
        premium: None,
        oracle_px: oracle,
        mark_px: mark,
        mid_px: None,
        impact_pxs: None,
        day_base_vlm: Decimal::ZERO,
    })
}

/// The address-keyed registry type (shared by the sim state and the API).
pub(crate) type Accounts = BTreeMap<Address, Account>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::feed_with;
    use crate::wire::{AssetInfo, MarginTable, MarginTier, Meta};

    fn meta_for(coin: &str, max_leverage: u32) -> Meta {
        Meta {
            universe: vec![AssetInfo {
                name: coin.to_string(),
                sz_decimals: 5,
                max_leverage,
                margin_table_id: Some(50),
                only_isolated: None,
                is_delisted: None,
            }],
            margin_tables: vec![(
                50,
                MarginTable {
                    description: String::new(),
                    margin_tiers: vec![MarginTier {
                        lower_bound: Decimal::ZERO,
                        max_leverage,
                    }],
                },
            )],
            collateral_token: 0,
        }
    }

    /// The served identity holds exactly for a seeded position.
    #[test]
    fn identity_holds_for_seeded_positions() {
        let feed = feed_with(&[("ETH", "2985.9", "2986.0")]);
        let meta = meta_for("ETH", 50);
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
        assert_eq!(s.withdrawable, s.cross_margin_summary.account_value);
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

    /// The unified account's `tokenToAvailableAfterMaintenance` closes daloc's
    /// reconciliation identity: `available + maintenance == the collateral
    /// balance` (their hard gate). A standard account serves none.
    /// The unified account's spot collateral is part of the raw balance (the
    /// single USDC figure the docs describe), so it backs perp margin and the
    /// identity stays exact.
    #[test]
    fn unified_spot_collateral_backs_the_perp_side() {
        let feed = feed_with(&[("BTC", "100.0", "100.0")]);
        let meta = meta_for("BTC", 10);
        let mut account = Account {
            perp_balance: Decimal::ZERO,
            mode: UserAbstraction::UnifiedAccount,
            ..Account::default()
        };
        spot_entry(&mut account, crate::sim::TokenRef { index: COLLATERAL_TOKEN, coin: "USDC" }).total =
            "250".parse().unwrap();
        let v = view(&account, &feed, Some(&meta)).unwrap();
        assert_eq!(
            v.margin_summary.total_raw_usd,
            "250".parse().unwrap(),
            "the spot collateral is the raw balance"
        );
        assert_eq!(
            v.margin_summary.account_value,
            "250".parse().unwrap(),
            "and the account value"
        );
        // The same account in standard mode: the spot side is not read.
        account.mode = UserAbstraction::Default;
        let v = view(&account, &feed, Some(&meta)).unwrap();
        assert_eq!(v.margin_summary.account_value, Decimal::ZERO);
    }

    #[test]
    fn unified_available_closes_the_reconciliation() {
        let feed = feed_with(&[("BTC", "100.0", "100.0")]);
        let meta = meta_for("BTC", 10);
        let mut account = Account {
            perp_balance: Decimal::ZERO,
            mode: UserAbstraction::UnifiedAccount,
            ..Account::default()
        };
        account.positions.insert(
            "BTC".into(),
            Position {
                szi: Decimal::ONE,
                entry_px: Decimal::from(100),
                leverage: LeverageType::Cross,
                leverage_value: 10,
                margin_used: Decimal::ZERO,
                funding_all_time: Decimal::ZERO,
                funding_since_open: Decimal::ZERO,
                funding_since_change: Decimal::ZERO,
            },
        );
        account.spot.insert(
            0,
            SpotEntry {
                coin: "USDC".into(),
                total: Decimal::from(50),
                hold: Decimal::ZERO,
                entry_ntl: Decimal::ZERO,
            },
        );
        let rows = available_after_maintenance(&account, &feed, Some(&meta)).unwrap();
        let available = rows.iter().find(|(t, _)| *t == 0).unwrap().1;
        let v = view(&account, &feed, Some(&meta)).unwrap();
        assert!(v.cross_maintenance_margin_used > Decimal::ZERO);
        assert_eq!(available + v.cross_maintenance_margin_used, Decimal::from(50));

        account.mode = UserAbstraction::Default;
        assert!(available_after_maintenance(&account, &feed, Some(&meta)).unwrap().is_empty());
    }

    /// Funding accrues hourly at the oracle price, and moves the balance.
    #[test]
    fn funding_accrues_hourly_on_oracle() {
        let feed = feed_with(&[("BTC", "100", "101")]);
        let mut account = Account {
            perp_balance: Decimal::ZERO,
            funded_at_ms: 3_600_000,
            ..Account::default()
        };
        account.positions.insert(
            "BTC".into(),
            Position {
                szi: Decimal::ONE,
                entry_px: Decimal::from(100),
                leverage: LeverageType::Cross,
                leverage_value: 10,
                margin_used: Decimal::from(10),
                funding_all_time: Decimal::ZERO,
                funding_since_open: Decimal::ZERO,
                funding_since_change: Decimal::ZERO,
            },
        );
        // One hour at rate 0.0000125 (the published hourly rate) on oracle 101.
        let moved = accrue_funding(&mut account, &feed, 7_200_000).unwrap();
        assert_eq!(moved, 1);
        let expected = -Decimal::ONE * Decimal::from(101) * Decimal::new(125, 7);
        assert_eq!(account.perp_balance, expected);
        assert_eq!(account.positions["BTC"].funding_since_open, expected);
    }

    /// A missing coin's context defers the whole account's accrual: no silent
    /// funding loss for the priced positions while another waits.
    #[test]
    fn funding_waits_for_every_coin_to_be_priced() {
        let feed = feed_with(&[("BTC", "100", "100")]);
        let position = Position {
            szi: Decimal::ONE,
            entry_px: Decimal::from(100),
            leverage: LeverageType::Cross,
            leverage_value: 10,
            margin_used: Decimal::from(10),
            funding_all_time: Decimal::ZERO,
            funding_since_open: Decimal::ZERO,
            funding_since_change: Decimal::ZERO,
        };
        let mut account = Account {
            funded_at_ms: 3_600_000,
            ..Account::default()
        };
        account.positions.insert("BTC".into(), position.clone());
        account.positions.insert("ETH".into(), position);
        assert_eq!(accrue_funding(&mut account, &feed, 7_200_000).unwrap(), 0);
        assert_eq!(account.funded_at_ms, 3_600_000, "the stamp stays put");
        feed.put_ctx(
            "ETH".into(),
            AssetCtx {
                funding: Decimal::new(125, 7),
                open_interest: Decimal::ZERO,
                prev_day_px: Decimal::ZERO,
                day_ntl_vlm: Decimal::ZERO,
                premium: None,
                oracle_px: Decimal::from(100),
                mark_px: Decimal::from(100),
                mid_px: None,
                impact_pxs: None,
                day_base_vlm: Decimal::ZERO,
            },
            1,
        );
        assert_eq!(accrue_funding(&mut account, &feed, 7_200_000).unwrap(), 2);
        assert_eq!(account.funded_at_ms, 7_200_000);
    }

    /// A zero-size position is a closed one: the served state drops the row
    /// (daloc's flat-rows-dropped gate), while the internal map may keep it
    /// until a mutation clears it.
    #[test]
    fn flat_rows_are_dropped_from_the_served_state() {
        let feed = feed_with(&[("BTC", "100", "100")]);
        let mut account = Account {
            perp_balance: Decimal::from(10),
            ..Account::default()
        };
        account.positions.insert(
            "BTC".into(),
            Position {
                szi: Decimal::ZERO,
                entry_px: Decimal::from(100),
                leverage: LeverageType::Cross,
                leverage_value: 10,
                margin_used: Decimal::ZERO,
                funding_all_time: Decimal::ZERO,
                funding_since_open: Decimal::ZERO,
                funding_since_change: Decimal::ZERO,
            },
        );
        let s = clearinghouse(&account, &feed, None, 1).unwrap();
        assert!(s.asset_positions.is_empty(), "flat row served");
        assert_eq!(s.margin_summary.account_value, Decimal::from(10));
    }

    /// An empty account is never "below maintenance" (negative equity is a
    /// state the venue reports, not a repeated liquidation).
    #[test]
    fn empty_account_never_liquidates() {
        let feed = feed_with(&[("BTC", "50", "50")]);
        let mut account = Account {
            perp_balance: Decimal::from(-100),
            ..Account::default()
        };
        assert!(liquidate_if_needed(&mut account, &feed, None).unwrap().is_none());
    }

    /// Below maintenance, the position closes at the mark and the balance
    /// becomes the account value.
    #[test]
    fn liquidation_closes_at_the_mark() {
        let feed = feed_with(&[("BTC", "50", "50")]);
        let meta = meta_for("BTC", 10);
        let mut account = Account {
            perp_balance: Decimal::from(1), // tiny balance
            ..Account::default()
        };
        account.positions.insert(
            "BTC".into(),
            Position {
                szi: Decimal::ONE,
                entry_px: Decimal::from(100), // deep loss: uPnL = −50
                leverage: LeverageType::Cross,
                leverage_value: 10,
                margin_used: Decimal::from(10),
                funding_all_time: Decimal::ZERO,
                funding_since_open: Decimal::ZERO,
                funding_since_change: Decimal::ZERO,
            },
        );
        // accountValue = 1 − 50 = −49; maintenance = 50·1/(2·10) = 2.5 → liquidate.
        let liq = liquidate_if_needed(&mut account, &feed, Some(&meta)).unwrap().expect("liquidated");
        assert_eq!(liq.positions_closed, vec!["BTC"]);
        assert!(account.positions.is_empty());
        assert_eq!(account.perp_balance, Decimal::from(1) - Decimal::from(50));
    }

    /// The isolated liquidation price follows the exact formula.
    #[test]
    fn isolated_liquidation_price_formula() {
        let p = Position {
            szi: Decimal::new(335, 4), // 0.0335
            entry_px: Decimal::new(29863, 1),
            leverage: LeverageType::Isolated,
            leverage_value: 20,
            margin_used: Decimal::new(4_967_826, 6),
            funding_all_time: Decimal::ZERO,
            funding_since_open: Decimal::ZERO,
            funding_since_change: Decimal::ZERO,
        };
        let mark = Decimal::new(29859, 1);
        let rate = Decimal::ONE / Decimal::from(2 * 50); // tier: 50x → l = 1/100
        let liq = liquidation_px(&p, mark, rate).unwrap();
        // Long: mark − margin/size/(1 − l).
        let expected = mark - p.margin_used / p.szi / (Decimal::ONE - rate);
        assert_eq!(liq, expected);
    }

    #[test]
    fn ctx_of_needs_a_funding_source() {
        // A price override owns mark+oracle over an empty cache: the prices
        // compose, but no funding exists anywhere — the minimal ctx is not
        // servable (a zero would be a silent lie), so the WS lane skips the
        // push and retries on the next tick.
        use crate::feed::PriceOverride;
        let feed = MarketFeed::memory(vec!["BTC".into()]);
        feed.own_price(
            "BTC",
            PriceOverride {
                mark: Decimal::from(100),
                oracle: Decimal::from(101),
            },
        )
        .unwrap();
        assert!(ctx_of(&feed, "BTC").is_none());
        // With a funding source the ctx serves (the crash-lane shape: the
        // override wins over the absent cache).
        feed.own_funding("BTC", Decimal::new(125, 7)).unwrap();
        let ctx = ctx_of(&feed, "BTC").unwrap();
        assert_eq!(ctx.funding, Decimal::new(125, 7));
        assert_eq!(ctx.mark_px, Decimal::from(100));
        assert_eq!(ctx.oracle_px, Decimal::from(101));
    }
}
