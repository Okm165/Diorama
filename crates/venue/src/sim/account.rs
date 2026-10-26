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
use super::seed::AccountSeed;
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

/// The margin mode's wire string (the venue's own vocabulary): one spelling.
pub(crate) const ISOLATED_MARGIN: &str = "isolated margin";

/// The checked-money contexts, one spelling each (they name the field in every
/// overflow/zero-division fault).
pub(crate) const COST_BASIS: &str = "cost basis";
pub(crate) const FILL_NOTIONAL: &str = "fill notional";

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
    /// The UTC day (epoch days) of the last scheduled-cancel fire, and the
    /// day's count — the venue caps triggers at 10 per day, reset at 00:00
    /// UTC (docs).
    #[serde(default)]
    pub schedule_fire_day: u64,
    #[serde(default)]
    pub schedule_fires: u32,
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
    /// The margin mode (the wire's `isCross`, named).
    pub mode: crate::wire::LeverageType,
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
    /// The isolated margin the account moved onto this position
    /// (`updateIsolatedMargin`; the docs' `leverage.rawUsd`). When set, it is
    /// the position's margin — the derived cross figure does not apply.
    #[serde(default)]
    pub isolated_margin: Decimal,
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
        mul(self.szi, self.entry_px, COST_BASIS)
    }

    pub fn is_long(&self) -> bool {
        !self.szi.is_sign_negative()
    }
}

/// Checked money arithmetic: rust_decimal panics on overflow (and on
/// division by zero), and a panic under the state lock poisons the account
/// plane for the process's life. Every step an input can reach goes through
/// these.
pub fn add(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal> {
    a.checked_add(b).ok_or(crate::error::Error::Overflow { what })
}

pub fn sub(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal> {
    a.checked_sub(b).ok_or(crate::error::Error::Overflow { what })
}

pub(crate) fn mul(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal> {
    a.checked_mul(b).ok_or(crate::error::Error::Overflow { what })
}

/// Checked division: `Decimal`'s `/` panics on a zero divisor, so the
/// divisor is named when it refuses.
pub(crate) fn div(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal> {
    a.checked_div(b)
        .ok_or(crate::error::Error::DivideByZero { what })
}

/// `a / b`, or `fallback` when the divisor is zero: the sim's shape for a
/// ratio whose denominator may legitimately be absent (a position with no
/// margin, a notional with no fill, a mark not yet polled). The fallback is
/// the domain's reading of "absent" — the one place a raw `/` is allowed,
/// because the guard is right here.
pub(crate) fn ratio_or(a: Decimal, b: Decimal, fallback: Decimal) -> Decimal {
    if b.is_zero() {
        fallback
    } else {
        a / b
    }
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

/// Credit a spot entry by `amount`, naming the context in the error; the new
/// total is returned (the deal serves it). The spot lane's one credit.
pub(crate) fn credit_entry(
    account: &mut Account,
    token: crate::sim::TokenRef<'_>,
    amount: Decimal,
    what: &'static str,
) -> Result<Decimal> {
    let entry = spot_entry(account, token);
    entry.total = add(entry.total, amount, what)?;
    Ok(entry.total)
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
    /// view outright ([`PositionFacts::of`]). The waterfall reads one entry
    /// per isolated leg (`Sim::check_account`).
    pub(crate) facts: BTreeMap<String, PositionFacts>,
}

/// One position's derived facts: the feed's mark, the unsigned notional
/// (`position_value`) and the maintenance requirement at that mark. Computed
/// once, consumed by both the totals and the served row.
pub(crate) struct PositionFacts {
    pub(crate) mark: Decimal,
    pub(crate) value: Decimal,
    pub(crate) maintenance: Decimal,
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
    /// The isolated positions' signed value (`Σ sign(szi)·positionValue`):
    /// the cross summary's raw balance subtracts the margins and adds this
    /// back (measured live).
    isolated_signed: Decimal,
}

impl PositionTotals {
    fn add(&mut self, p: &Position, facts: &PositionFacts) -> Result<()> {
        let signed = if p.is_long() { facts.value } else { -facts.value };
        self.notional = add(self.notional, facts.value, "total notional")?;
        self.margin = add(self.margin, p.margin_used, "total margin")?;
        self.signed_value = add(self.signed_value, signed, "signed value")?;
        self.cost_basis = add(self.cost_basis, p.cost_basis()?, COST_BASIS)?;
        if p.leverage == LeverageType::Cross {
            self.cross_notional = add(self.cross_notional, facts.value, "cross notional")?;
            self.cross_margin = add(self.cross_margin, p.margin_used, "cross margin")?;
            self.cross_maintenance =
                add(self.cross_maintenance, facts.maintenance, "cross maintenance")?;
        } else {
            self.isolated_margin = add(self.isolated_margin, p.margin_used, ISOLATED_MARGIN)?;
            self.isolated_signed = add(self.isolated_signed, signed, "isolated signed value")?;
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
    // A position with no margin reads ROE 0 (the ratio's absent-denominator
    // fallback; `ratio_or` is where a raw `/` is allowed).
    let roe = ratio_or(u_pnl, p.margin_used, Decimal::ZERO);
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
) -> Result<AccountView> {
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
    // The venue's totals, measured live (probe, two isolated accounts): the
    // *total* carries the isolated margins as cash — they are the trader's
    // equity, parked on their positions — so
    // `accountValue = totalRawUsd + Σ sign·positionValue` holds with them
    // included; and the cross summary is the total minus the isolated legs'
    // contribution: value minus their margins, raw minus (margin − signed
    // value).
    let cash = account.perp_balance + collateral + totals.isolated_margin;
    let total_raw = cash - totals.cost_basis;
    let account_value = total_raw + totals.signed_value;
    let margin_summary = MarginSummary {
        account_value,
        total_ntl_pos: totals.notional,
        total_raw_usd: total_raw,
        total_margin_used: totals.margin,
    };
    let cross_account_value = account_value - totals.isolated_margin;
    let cross_raw = total_raw - totals.isolated_margin + totals.isolated_signed;
    let cross_margin_summary = MarginSummary {
        account_value: cross_account_value,
        total_ntl_pos: totals.cross_notional,
        total_raw_usd: cross_raw,
        total_margin_used: totals.cross_margin,
    };
    Ok(AccountView {
        margin_summary,
        cross_margin_summary,
        cross_maintenance_margin_used: totals.cross_maintenance,
        withdrawable: withdrawable_of(cross_account_value, totals)?,
        asset_positions: rows,
        facts,
    })
}

/// The docs' transfer rule (margining): "Unrealized pnl can be withdrawn
/// from isolated positions or cross account, but only if the remaining margin
/// is at least 10% of the total notional position value of all open
/// positions. The margin remaining must also meet the initial margin
/// requirement, i.e. `transfer_margin_required = max(initial_margin_required,
/// 0.1 * total_position_value)`" — and "transferring" includes isolated
/// margin transfers. One home for the rule: the served `withdrawable` (the
/// cross summary's scope — isolated margins are the positions' own; the info
/// endpoint's own example pins the flat case) and the isolated removal's
/// floor (`transfer_floor`).
pub(crate) fn transfer_floor_of(notional: Decimal, initial: Decimal) -> Result<Decimal> {
    let ten = mul(notional, Decimal::new(1, 1), "transfer floor")?;
    Ok(initial.max(ten))
}

/// One position's transfer floor at the served mark: the same rule, at the
/// position's granularity (the isolated removal's check).
pub(crate) fn transfer_floor(p: &Position, mark: Decimal) -> Result<Decimal> {
    let notional = mul(p.szi.abs(), mark, "position notional")?;
    let initial = div(notional, Decimal::from(p.leverage_value.max(1)), "initial margin")?;
    transfer_floor_of(notional, initial)
}

fn withdrawable_of(cross_account_value: Decimal, totals: &PositionTotals) -> Result<Decimal> {
    let floor = transfer_floor_of(totals.cross_notional, totals.cross_margin)?;
    Ok(sub(cross_account_value, floor, "withdrawable")?.max(Decimal::ZERO))
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
    assemble(account, &totals, rows, by_coin)
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

/// Refuse the abstraction modes the sim cannot serve. Portfolio margin's
/// cross-asset portfolio (docs: "single portfolio unifying all eligible
/// assets, which are currently HYPE, BTC, USDC, USDT"; the portfolio-wide
/// available figure, measured live on two accounts) is the venue's own
/// accounting — the single-collateral model cannot reproduce it, and a mode
/// whose correctness would depend on the account's future spot holdings must
/// not be accepted silently. Everything else is served: `unifiedAccount` is
/// the modeled single balance (the live identity `available + maintenance ==
/// the collateral token's balance` holds to the digit), `default` is the
/// standard split, and the discontinued `dexAbstraction` is the standard
/// fold by its own definition ("USDC balances default to perps balance, all
/// other collateral defaults to spot balance").
pub(crate) fn refuse_unmodeled_mode(mode: UserAbstraction) -> Result<()> {
    if mode == UserAbstraction::PortfolioMargin {
        return Err(Error::bad_request(
            "portfolio margin is not modeled (its cross-asset portfolio is the venue's own accounting); use unifiedAccount",
        ));
    }
    Ok(())
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
            add(acc, row.position.margin_used, ISOLATED_MARGIN)
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
    let Some(asset) = meta.asset(coin) else {
        return Decimal::ZERO;
    };
    let max_leverage = meta
        .margin_table(coin)
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

/// Whether the asset is *strict isolated* (docs): its margin cannot be
/// removed by hand, and it is released proportionally as the position is
/// closed. The flag is the live meta's (`onlyIsolated`); a warming meta or an
/// unknown coin reads false.
pub(crate) fn strict_isolated(coin: &str, meta: Option<&crate::wire::Meta>) -> bool {
    let Some(a) = meta.and_then(|m| m.asset(coin)) else {
        return false;
    };
    match a.margin_mode {
        Some(crate::wire::MarginMode::StrictIsolated) => true,
        // The deprecated alias, only when the live field is absent (docs:
        // `onlyIsolated` "means either strictIsolated or noCross").
        None => a.only_isolated.unwrap_or(false),
        Some(_) => false,
    }
}

/// Only isolated margin is allowed for the asset: the deprecated
/// `onlyIsolated` flag or a live `strictIsolated`/`noCross` mode (docs:
/// "noCross" "does not allow cross margin").
pub(crate) fn isolated_only(coin: &str, meta: Option<&crate::wire::Meta>) -> bool {
    let Some(a) = meta.and_then(|m| m.asset(coin)) else {
        return false;
    };
    a.only_isolated.unwrap_or(false)
        || matches!(
            a.margin_mode,
            Some(crate::wire::MarginMode::StrictIsolated | crate::wire::MarginMode::NoCross)
        )
}

/// The coin's live max leverage — `None` when the live meta does not carry
/// the coin (a seeded position outside the tracked markets: the feed is
/// warming). Callers pick their own bounded fallback; a fabricated default
/// here would be a silent lie about a margin bound.
pub(crate) fn max_leverage(coin: &str, meta: Option<&crate::wire::Meta>) -> Option<u32> {
    meta.and_then(|m| m.asset(coin))
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
/// Whether the new signed size keeps the position's direction (a flat
/// position has none): the reduce-only, fill and cap checks' shared
/// predicate.
pub(crate) fn same_direction(old: Decimal, new: Decimal) -> bool {
    old.is_zero() || sign(old) == sign(new)
}

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
    pub side: crate::sim::order::Side,
    pub size: Decimal,
    pub px: Decimal,
    pub leverage: u32,
    pub reduce_only: bool,
    /// The trading fee (venue base tier; charged on top of the fill — the
    /// plan's `closedPnl = fee + side·(mark−entry)·size`).
    pub fee: Decimal,
}

impl FillFacts {
    /// A reduce-only close at `px` with no fee: the waterfall's shape (the ADL
    /// pairs and the book's reduce closes).
    pub(crate) fn close(
        coin: &str,
        side: crate::sim::order::Side,
        size: Decimal,
        px: Decimal,
        leverage: u32,
    ) -> Self {
        Self {
            coin: coin.to_string(),
            side,
            size,
            px,
            leverage,
            reduce_only: true,
            fee: Decimal::ZERO,
        }
    }
}

pub(crate) fn apply_fill(
    account: &mut Account,
    fill: &FillFacts,
    meta: Option<&crate::wire::Meta>,
) -> Result<()> {
    let signed = fill.size * fill.side.sign();
    let old = account
        .positions
        .get(&fill.coin)
        .map(|p| p.szi)
        .unwrap_or(Decimal::ZERO);
    refuse_non_reducing(old, signed, fill)?;
    let position = account
        .positions
        .entry(fill.coin.clone())
        .or_insert_with(|| fresh_position(fill));
    let closed = if same_direction(old, signed) {
        apply_increase(position, old, fill)?;
        Decimal::ZERO
    } else {
        let closed = old.abs().min(fill.size);
        let realized = apply_reduction(position, old, fill)?;
        account.perp_balance = add(account.perp_balance, realized, "realized PnL")?;
        closed
    };
    // The trading fee (the venue's base tier): charged on the fill's notional,
    // on both sides of a trade, in USDC — so a closing fill pays it too.
    if !fill.fee.is_zero() {
        account.perp_balance = sub(account.perp_balance, fill.fee, "trading fee")?;
    }
    // The position's last use: the borrow ends here, so the release (which
    // re-looks the position up) can borrow the account whole.
    let flat = position.szi.is_zero();
    release_isolated_margin(account, &fill.coin, old, closed, meta)?;
    if flat {
        account.positions.remove(&fill.coin);
        return Ok(());
    }
    if let Some(position) = account.positions.get_mut(&fill.coin) {
        settle_margin(position)?;
    }
    Ok(())
}

/// The margin a close releases (docs): a fully closed isolated position
/// returns its whole isolated margin to the cross balance — the margin is the
/// trader's collateral, only the position is gone. A *strict isolated* asset
/// releases it proportionally as the position closes (there the margin cannot
/// be moved out by hand); a normal isolated position's margin is the trader's
/// to move (`updateIsolatedMargin`), so a partial close keeps it. A fill that
/// closes nothing (an increase) releases nothing.
fn release_isolated_margin(
    account: &mut Account,
    coin: &str,
    old: Decimal,
    closed: Decimal,
    meta: Option<&crate::wire::Meta>,
) -> Result<()> {
    let Some(position) = account.positions.get_mut(coin) else {
        return Ok(());
    };
    if position.leverage != LeverageType::Isolated || position.isolated_margin.is_zero() {
        return Ok(());
    }
    let released = if position.szi.is_zero() {
        position.isolated_margin
    } else if !closed.is_zero() && strict_isolated(coin, meta) {
        mul(
            position.isolated_margin,
            div(closed, old.abs(), "isolated release fraction")?,
            "isolated margin release",
        )?
    } else {
        Decimal::ZERO
    };
    if released.is_zero() {
        return Ok(());
    }
    position.isolated_margin = sub(position.isolated_margin, released, "isolated margin release")?;
    account.perp_balance = add(account.perp_balance, released, "isolated margin release")?;
    Ok(())
}

/// The reduce-only guard: the venue's own string (plan §16.3) when the fill
/// would not reduce the position it names.
fn refuse_non_reducing(old: Decimal, signed: Decimal, fill: &FillFacts) -> Result<()> {
    if fill.reduce_only && (same_direction(old, signed) || fill.size > old.abs()) {
        return Err(Error::bad_request(
            "Reduce only order would increase position.",
        ));
    }
    Ok(())
}

/// A position's first appearance: cross, at the fill's price and the fill's
/// leverage (the account's setting decides later fills' leverage, not this).
fn fresh_position(fill: &FillFacts) -> Position {
    Position {
        szi: Decimal::ZERO,
        entry_px: fill.px,
        leverage: LeverageType::Cross,
        leverage_value: fill.leverage.max(1),
        margin_used: Decimal::ZERO,
        isolated_margin: Decimal::ZERO,
        funding_all_time: Decimal::ZERO,
        funding_since_open: Decimal::ZERO,
        funding_since_change: Decimal::ZERO,
    }
}

/// The post-fill margin settlement: `cumFunding.sinceChange` restarts at every
/// size change (the fill *is* the change; the funding up to it is settled
/// first — the lane accrues before applying, see `land_fill`), and the margin
/// follows — the isolated margin when the account moved one on
/// (`updateIsolatedMargin`, the docs' `leverage.rawUsd`), else notional over
/// leverage (checked: a zero leverage is a named fault, never a panic).
fn settle_margin(position: &mut Position) -> Result<()> {
    position.funding_since_change = Decimal::ZERO;
    position.margin_used = if position.leverage == LeverageType::Isolated
        && !position.isolated_margin.is_zero()
    {
        position.isolated_margin
    } else {
        div(
            mul(position.szi.abs(), position.entry_px, "margin notional")?,
            Decimal::from(position.leverage_value),
            "margin notional",
        )?
    };
    Ok(())
}

/// An increase (or a fresh position): the entry becomes the size-weighted
/// average of the old notional and the fill.
fn apply_increase(position: &mut Position, old: Decimal, fill: &FillFacts) -> Result<()> {
    let signed = fill.size * fill.side.sign();
    let old_notional = mul(old.abs(), position.entry_px, "entry notional")?;
    let fill_notional = mul(fill.size, fill.px, FILL_NOTIONAL)?;
    let new_szi = add(old, signed, "position size")?;
    position.entry_px = div(
        add(old_notional, fill_notional, "entry notional total")?,
        new_szi.abs(),
        "entry price",
    )?;
    position.szi = new_szi;
    Ok(())
}

/// A reduction (or a flip): the closed part's realized PnL (returned for the
/// caller to credit — the measured identity stays exact); a flip opens the
/// remainder at the fill price.
fn apply_reduction(position: &mut Position, old: Decimal, fill: &FillFacts) -> Result<Decimal> {
    let signed = fill.size * fill.side.sign();
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

/// The maintenance check + the test-only full-close: when the account value
/// falls below its maintenance requirement, every position closes at the
/// mark. An account with no positions is never "below maintenance": negative
/// equity alone is a state the venue reports, not a liquidation to re-run
/// every tick. The full-close is the account-plane tests' path — the sim's
/// tick runs the venue's waterfall instead (`Sim::check_all`: partial closes,
/// the backstop, ADL).
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
/// row has no facts and contributes zero. Production's waterfall reads the
/// per-leg `facts.maintenance` directly; the account-plane test helper
/// (`liquidate_if_needed`) sums here.
#[cfg(test)]
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
    use crate::sim::{feed_with, meta_with_table};

    /// The unified account's `tokenToAvailableAfterMaintenance` closes daloc's
    /// reconciliation identity: `available + maintenance == the collateral
    /// balance` (their hard gate). A standard account serves none.
    /// The unified account's spot collateral is part of the raw balance (the
    /// single USDC figure the docs describe), so it backs perp margin and the
    /// identity stays exact.
    /// The direction predicate: flat is no direction, signs decide (the
    /// reduce-only, fill and cap checks' shared rule).
    #[test]
    fn same_direction_treats_flat_as_no_direction() {
        assert!(same_direction(Decimal::ZERO, Decimal::from(3)));
        assert!(same_direction(Decimal::from(2), Decimal::from(3)));
        assert!(same_direction(Decimal::from(-2), Decimal::from(-3)));
        assert!(!same_direction(Decimal::from(2), Decimal::from(-3)));
    }

    #[test]
    fn unified_spot_collateral_backs_the_perp_side() {
        let feed = feed_with(&[("BTC", "100.0", "100.0")]);
        let meta = meta_with_table("BTC", 10);
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

    /// Portfolio margin is refused by name at the seed patch too: its
    /// cross-asset portfolio (the eligible set, the portfolio-wide available —
    /// measured live on two accounts) is the venue's own accounting, and a
    /// mode whose correctness would depend on the account's future spot
    /// holdings must not enter the state silently. The same patch with
    /// `unifiedAccount` applies.
    #[test]
    fn unified_available_closes_the_reconciliation() {
        let feed = feed_with(&[("BTC", "100.0", "100.0")]);
        let meta = meta_with_table("BTC", 10);
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
                isolated_margin: Decimal::ZERO,
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
                isolated_margin: Decimal::ZERO,
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
        let meta = meta_with_table("BTC", 10);
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
                isolated_margin: Decimal::ZERO,
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
            isolated_margin: Decimal::ZERO,
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
