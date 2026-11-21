//! The account plane's model: accounts, positions, and the pure math that
//! turns them into the venue's served shapes.
//!
//! The internal truth is the *inputs* — the perp wallet's raw USDC balance,
//! positions with entry prices, spot balances — and every served figure is
//! computed from them against the live marks, so the identity the consumer gates on
//! (`accountValue == totalRawUsd + Σ signed(positionValue)`) holds by
//! construction:
//!
//! ```text
//! totalRawUsd   = balance − Σ szi·entryPx
//! accountValue  = balance + Σ szi·(mark − entryPx) = totalRawUsd + Σ szi·mark
//! ```
//!
//! (verified exact on the live sample, `wire::tests::live_sample_identity_holds`).
use super::ledger::{shared, FillEffect, UserFill, UserFunding};
use super::seed::AccountSeed;
use crate::error::{Error, Result};
use crate::feed::dex::dex_of_coin;
use crate::feed::{MarketFeed, Prices};
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

/// One recorded API wallet (the agent identity the consumer's cosigner registers).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct AgentWallet {
    pub wallet: Address,
    pub name: String,
}

/// One dex's clearinghouse ledger of an account: the raw collateral balance
/// (before cost basis; USDC on the default dex, the dex's own collateral
/// token on a builder dex) and the sim clock (ms) its funding was last
/// accrued to. The real venue keeps one such clearinghouse per (user, dex).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct DexLedger {
    #[serde(default)]
    pub balance: Decimal,
    /// The sim clock (ms) funding was last accrued to (0 = never).
    #[serde(default)]
    pub funded_at_ms: u64,
}

/// One simulated account (persisted). The fields are the *inputs*; the wire
/// shape is derived per read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Account {
    /// The perp clearinghouses, one per dex (`""` = the default dex). Cross
    /// margin of different dexes never shares: each ledger funds only its own
    /// dex's positions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub perps: BTreeMap<String, DexLedger>,
    /// The single USDC balance a state file written before the per-dex
    /// ledgers carries. Read on load and folded into `perps[""]` by
    /// [`Account::migrate_legacy`]; never written back.
    #[serde(default, rename = "perp_balance", skip_serializing)]
    pub(crate) legacy_perp_balance: Option<Decimal>,
    /// The single funding stamp of the same older files (see above).
    #[serde(default, rename = "funded_at_ms", skip_serializing)]
    pub(crate) legacy_funded_at_ms: Option<u64>,
    #[serde(default)]
    pub positions: BTreeMap<String, Position>,
    /// Spot balances by token index (USDC = 0).
    #[serde(default)]
    pub spot: BTreeMap<u32, SpotEntry>,
    #[serde(default = "default_mode")]
    pub mode: UserAbstraction,
    /// The fills this account made, oldest first, bounded (`userFills`).
    /// History only: no account figure reads it.
    #[serde(
        default,
        skip_serializing_if = "shared::is_empty",
        serialize_with = "shared::serialize",
        deserialize_with = "shared::deserialize"
    )]
    pub fills: std::sync::Arc<std::collections::VecDeque<UserFill>>,
    /// The funding payments this account made, oldest first, bounded
    /// (`userFunding`). History only.
    #[serde(
        default,
        skip_serializing_if = "shared::is_empty",
        serialize_with = "shared::serialize",
        deserialize_with = "shared::deserialize"
    )]
    pub fundings: std::sync::Arc<std::collections::VecDeque<UserFunding>>,
    /// API wallets the CoreWriter registered for this account.
    #[serde(default)]
    pub agents: Vec<AgentWallet>,
    /// The builder fees this account approved (CoreWriter action 12 /
    /// `approveBuilderFee`): builder address to the approved maximum fee rate,
    /// in tenths of a basis point. A later approval for the same builder
    /// overwrites; an approval of 0 revokes (the entry is removed). A plain
    /// map, not an `Arc`: an account approves a handful of builders, so the
    /// clone every read takes is a few entries (unlike the ledgers below).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub builder_approvals: BTreeMap<Address, u64>,
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
    /// set is what it reads back — the consumer's `resolved_equity` assertions);
    /// `/info` never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<AccountSeed>,
}

fn default_mode() -> UserAbstraction {
    UserAbstraction::Default
}

/// Credit (negative: debit) one dex's ledger in the ledger map itself, so a
/// caller holding a borrow of the account's positions can still settle cash
/// (a disjoint-field borrow, which a method on the whole account is not).
pub(crate) fn credit_ledger(
    perps: &mut BTreeMap<String, DexLedger>,
    dex: &str,
    amount: Decimal,
    what: &'static str,
) -> Result<Decimal> {
    let ledger = perps.entry(dex.to_string()).or_default();
    ledger.balance = add(ledger.balance, amount, what)?;
    Ok(ledger.balance)
}

/// The ledger map of a default-dex-only account (the tests' shorthand).
#[cfg(test)]
pub(crate) fn default_perps(balance: Decimal, funded_at_ms: u64) -> BTreeMap<String, DexLedger> {
    BTreeMap::from([(
        String::new(),
        DexLedger {
            balance,
            funded_at_ms,
        },
    )])
}

impl Account {
    /// Fold the legacy single balance and funding stamp (a state file written
    /// before the per-dex ledgers) into the default dex's ledger. Idempotent:
    /// the legacy fields are consumed, and a ledger the file already carries
    /// wins over them.
    pub(crate) fn migrate_legacy(&mut self) {
        let balance = self.legacy_perp_balance.take();
        let stamp = self.legacy_funded_at_ms.take();
        if balance.is_none() && stamp.is_none() {
            return;
        }
        if self.perps.contains_key("") {
            return;
        }
        self.perps.insert(
            String::new(),
            DexLedger {
                balance: balance.unwrap_or_default(),
                funded_at_ms: stamp.unwrap_or_default(),
            },
        );
    }

    /// The dex's raw collateral balance (zero for a dex the account never
    /// funded).
    pub(crate) fn perp_balance(&self, dex: &str) -> Decimal {
        self.perps.get(dex).map_or(Decimal::ZERO, |l| l.balance)
    }

    /// The dex's ledger, created empty on first touch.
    pub(crate) fn ledger_mut(&mut self, dex: &str) -> &mut DexLedger {
        self.perps.entry(dex.to_string()).or_default()
    }

    /// The dex's balance for mutation (the ledger is created on first touch).
    pub(crate) fn perp_balance_mut(&mut self, dex: &str) -> &mut Decimal {
        &mut self.ledger_mut(dex).balance
    }

    /// The sim clock (ms) the dex's funding was last accrued to (0 = never).
    pub(crate) fn funded_at_ms(&self, dex: &str) -> u64 {
        self.perps.get(dex).map_or(0, |l| l.funded_at_ms)
    }

    /// Credit (or, negative, debit) the dex's balance, naming the context in
    /// an overflow; the new balance is returned.
    pub(crate) fn credit_perp(
        &mut self,
        dex: &str,
        amount: Decimal,
        what: &'static str,
    ) -> Result<Decimal> {
        credit_ledger(&mut self.perps, dex, amount, what)
    }

    /// The dexes the account holds a non-flat position in, sorted (`""`
    /// first). The waterfall walks these: a balance alone carries no risk.
    pub(crate) fn position_dexs(&self) -> Vec<String> {
        let dexs: std::collections::BTreeSet<&str> = self
            .positions
            .iter()
            .filter(|(_, p)| !p.szi.is_zero())
            .map(|(coin, _)| dex_of_coin(coin))
            .collect();
        dexs.into_iter().map(str::to_string).collect()
    }

    /// The dexes the account has a clearinghouse in: one with a ledger (it was
    /// funded or traded) or a position. Sorted, `""` first.
    pub(crate) fn dexs_with_activity(&self) -> Vec<String> {
        let mut dexs: std::collections::BTreeSet<String> = self.perps.keys().cloned().collect();
        dexs.extend(
            self.positions
                .keys()
                .map(|coin| dex_of_coin(coin).to_string()),
        );
        dexs.into_iter().collect()
    }
}

/// One `updateLeverage` setting (the venue stores cross/isolated + value; it
/// outlives the position, exactly as the venue's does).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LeverageSetting {
    /// The margin mode (the wire's `isCross`, named; the aliases and the
    /// tolerant `LeverageType` deserializer migrate pre-enum state files).
    #[serde(alias = "isCross", alias = "is_cross")]
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
        mul(
            self.szi,
            sub(mark, self.entry_px, "mark − entry")?,
            "unrealized PnL",
        )
    }

    /// The cost basis: szi·entry.
    pub fn cost_basis(&self) -> Result<Decimal> {
        mul(self.szi, self.entry_px, COST_BASIS)
    }

    /// The position's direction (a flat size reads as long — the callers
    /// guard the flat case).
    pub(crate) fn direction(&self) -> Direction {
        if self.szi.is_sign_negative() {
            Direction::Short
        } else {
            Direction::Long
        }
    }
}

/// Which way a position is on (the ADL ranking's opposite-side test, the
/// vault's same-side take).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Long,
    Short,
}

impl Direction {
    /// The side the position was entered on (a long bought; the closes flip).
    pub(crate) fn entry_side(self) -> crate::sim::order::Side {
        match self {
            Direction::Long => crate::sim::order::Side::Buy,
            Direction::Short => crate::sim::order::Side::Sell,
        }
    }
}

/// Checked money arithmetic: rust_decimal panics on overflow (and on
/// division by zero), and a panic under the state lock poisons the account
/// plane for the process's life. Every step an input can reach goes through
/// these.
pub fn add(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal> {
    a.checked_add(b)
        .ok_or(crate::error::Error::Overflow { what })
}

pub fn sub(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal> {
    a.checked_sub(b)
        .ok_or(crate::error::Error::Overflow { what })
}

pub(crate) fn mul(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal> {
    a.checked_mul(b)
        .ok_or(crate::error::Error::Overflow { what })
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
pub(crate) fn spot_entry<'a>(
    account: &'a mut Account,
    token: crate::sim::TokenRef<'_>,
) -> &'a mut SpotEntry {
    account
        .spot
        .entry(token.index)
        .or_insert_with(|| SpotEntry {
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
    fn of(
        coin: &str,
        p: &Position,
        feed: &MarketFeed,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<Self> {
        let Prices { mark, .. } =
            feed.required_prices(coin)
                .ok_or_else(|| crate::error::Error::NoMarket {
                    coin: coin.to_string(),
                })?;
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
    /// The isolated positions' own equity, `margin + unrealized pnl`: what the
    /// venue serves as an isolated position's `marginUsed` and carves out of
    /// the cross summary (the cross figure is the free cash alone: an isolated
    /// position's pnl is its own).
    isolated_equity: Decimal,
    /// The isolated positions' signed value (`Σ sign(szi)·positionValue`):
    /// the cross summary's raw balance subtracts the margins and adds this
    /// back (measured live).
    isolated_signed: Decimal,
}

impl PositionTotals {
    fn add(&mut self, p: &Position, facts: &PositionFacts) -> Result<()> {
        let signed = match p.direction() {
            Direction::Long => facts.value,
            Direction::Short => -facts.value,
        };
        self.notional = add(self.notional, facts.value, "total notional")?;
        // An isolated position's `marginUsed` is its equity (margin plus
        // unrealized pnl), measured live: `rawUsd + positionValue` of the row.
        let served_margin = match p.leverage {
            LeverageType::Cross => p.margin_used,
            LeverageType::Isolated => add(
                p.margin_used,
                p.unrealized_pnl(facts.mark)?,
                "isolated equity",
            )?,
        };
        self.margin = add(self.margin, served_margin, "total margin")?;
        self.signed_value = add(self.signed_value, signed, "signed value")?;
        self.cost_basis = add(self.cost_basis, p.cost_basis()?, COST_BASIS)?;
        if p.leverage == LeverageType::Cross {
            self.cross_notional = add(self.cross_notional, facts.value, "cross notional")?;
            self.cross_margin = add(self.cross_margin, p.margin_used, "cross margin")?;
            self.cross_maintenance = add(
                self.cross_maintenance,
                facts.maintenance,
                "cross maintenance",
            )?;
        } else {
            self.isolated_margin = add(self.isolated_margin, p.margin_used, ISOLATED_MARGIN)?;
            self.isolated_equity = add(self.isolated_equity, served_margin, ISOLATED_MARGIN)?;
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
            maintenance_rate(coin, facts.value, meta)?,
        )?),
        LeverageType::Cross => None,
    };
    // The served `marginUsed` of an isolated row is its equity (measured
    // live); a cross row keeps its margin.
    let served_margin = match p.leverage {
        LeverageType::Cross => p.margin_used,
        LeverageType::Isolated => p.margin_used + u_pnl,
    };
    Ok(AssetPosition {
        kind: PositionKind::OneWay,
        position: WirePosition {
            coin: coin.to_string(),
            entry_px: Some(p.entry_px),
            leverage: Leverage {
                kind: p.leverage,
                value: p.leverage_value,
                // The isolated carve-out only: the venue omits it on a cross
                // position. `marginUsed` less the position's signed value: a
                // short's raw balance is positive (it holds the proceeds),
                // measured live.
                raw_usd: (p.leverage == LeverageType::Isolated).then(|| {
                    let signed = match p.direction() {
                        Direction::Long => facts.value,
                        Direction::Short => -facts.value,
                    };
                    served_margin - signed
                }),
            },
            liquidation_px,
            margin_used: served_margin,
            // The live bound for a tracked coin; for a coin the meta does not
            // carry (a seeded position outside the markets) the position's own
            // leverage — a value we know, never a fabricated bound.
            max_leverage: max_leverage(coin, meta).unwrap_or(p.leverage_value),
            position_value: facts.value,
            return_on_equity: roe,
            szi: p.szi,
            unrealized_pnl: u_pnl,
            // The venue's wire field is a **cost** accumulator (positive =
            // paid); the sim's internal figure is the balance delta (positive
            // = received). The two must not be conflated: the equity is
            // correct either way, but a consumer that reads `cumFunding` as a
            // cost would invert every funding attribution.
            cum_funding: CumFunding {
                all_time: -p.funding_all_time,
                since_open: -p.funding_since_open,
                since_change: -p.funding_since_change,
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
    dex: &str,
    totals: &PositionTotals,
    rows: Vec<AssetPosition>,
    facts: BTreeMap<String, PositionFacts>,
) -> Result<AccountView> {
    // The venue's `marginSummary` is the **perp sub-ledger's** summary. A
    // unified account's spot collateral is a separate ledger: the committed
    // `clearinghouseState_unified_spot_only.json` carries `accountValue = 0`
    // beside a 2995.67224054 USDC spot balance, and the unified reconciliation
    // is `availableAfterMaintenance + maintenance == the spot balance`, not a
    // term of the perp identity. Adding spot here double-counted it (and left
    // a negative perp balance when the spot was later bridged: the live `cf16`
    // recovery moved 240.510247 spot and the served perp equity went to
    // −240.510247). The perp summary is the perp wallet alone.
    //
    // The venue's totals, measured live (probe, two isolated accounts): the
    // *total* carries the isolated margins as cash — they are the trader's
    // equity, parked on their positions — so
    // `accountValue = totalRawUsd + Σ sign·positionValue` holds with them
    // included; and the cross summary is the total minus the isolated legs'
    // contribution: value minus their equity (`margin + pnl`), raw minus
    // (equity − signed value).
    let cash = account.perp_balance(dex) + totals.isolated_margin;
    let total_raw = cash - totals.cost_basis;
    let account_value = total_raw + totals.signed_value;
    let margin_summary = MarginSummary {
        account_value,
        total_ntl_pos: totals.notional,
        total_raw_usd: total_raw,
        total_margin_used: totals.margin,
    };
    let cross_account_value = account_value - totals.isolated_equity;
    let cross_raw = total_raw - totals.isolated_equity + totals.isolated_signed;
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
    let initial = div(
        notional,
        Decimal::from(p.leverage_value.max(1)),
        "initial margin",
    )?;
    transfer_floor_of(notional, initial)
}

fn withdrawable_of(cross_account_value: Decimal, totals: &PositionTotals) -> Result<Decimal> {
    let floor = transfer_floor_of(totals.cross_notional, totals.cross_margin)?;
    Ok(sub(cross_account_value, floor, "withdrawable")?.max(Decimal::ZERO))
}

/// Compute one dex's served figures at the feed's current marks: the dex's
/// own ledger, its positions only, margined against the dex's own meta. The
/// venue keeps one clearinghouse per (user, dex) and cross margin never
/// crosses dexes.
pub(crate) fn view(account: &Account, feed: &MarketFeed, dex: &str) -> Result<AccountView> {
    let meta = feed.meta_of(dex);
    let meta = meta.as_ref();
    let mut totals = PositionTotals::default();
    let mut rows = Vec::with_capacity(account.positions.len());
    let mut by_coin = BTreeMap::new();
    for (coin, p) in &account.positions {
        // Flat rows are dropped from the served state (the consumer's gate): a
        // zero-size position is a closed one, not a row.
        if p.szi.is_zero() || dex_of_coin(coin) != dex {
            continue;
        }
        let facts = PositionFacts::of(coin, p, feed, meta)?;
        totals.add(p, &facts)?;
        rows.push(position_row(coin, p, &facts, meta)?);
        by_coin.insert(coin.clone(), facts);
    }
    assemble(account, dex, &totals, rows, by_coin)
}

/// The served `clearinghouseState` of one dex (fresh `time` from the caller's
/// wall clock).
pub(crate) fn clearinghouse(
    account: &Account,
    feed: &MarketFeed,
    dex: &str,
    time_ms: u64,
) -> Result<ClearinghouseState> {
    let v = view(account, feed, dex)?;
    // The account's figures are a simulation when either a mark behind them is
    // scenario-owned **or** the account's balances were written by the control
    // plane. The old derivation read only the marks, so a balance patch on a
    // flat account (5195) and a cash debit on a live one (4a) were served as
    // ordinary market observations. A mode-only setup patch is not a balance
    // write and keeps market provenance.
    let control_owned = account
        .control
        .as_ref()
        .is_some_and(super::seed::AccountSeed::changes_account_state)
        || account
            .positions
            .keys()
            .any(|coin| dex_of_coin(coin) == dex && feed.market_is_owned(coin));
    Ok(ClearinghouseState {
        margin_summary: v.margin_summary,
        cross_margin_summary: v.cross_margin_summary,
        cross_maintenance_margin_used: v.cross_maintenance_margin_used,
        withdrawable: v.withdrawable,
        asset_positions: v.asset_positions,
        time: time_ms,
        control_owned,
    })
}

/// The client's own gate, re-checked at the boundary: the served identity
/// `|accountValue − (totalRawUsd + Σ sign(szi)·positionValue)| ≤ 1e-6`. The
/// construction makes it hold by definition — the counter exists so a
/// regression in the construction is *visible* instead of silently failing
/// The identity gate (the invariant counter).
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

/// The spot balances as `/info` and the websocket serve them. The control
/// plane and the seed read [`spot_balances`], the stored state.
#[allow(clippy::unnecessary_wraps)] // the served hold computes per token in the unified switch
pub(crate) fn served_spot_balances(
    account: &Account,
    _feed: &MarketFeed,
) -> Result<Vec<SpotBalance>> {
    Ok(spot_balances(account))
}

/// The stored spot balances (USDC token 0 sorts first, like the venue's).
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

/// USDC: the venue's collateral token for the default dex (the consumer
/// default, `collateral_token = 0`).
pub(crate) const COLLATERAL_TOKEN: u32 = 0;

/// The unified account's `tokenToAvailableAfterMaintenance`: each collateral
/// token's balance minus the perp maintenance of every dex margining against
/// it — the identity the consumer reconciles (`available + maintenance == the
/// token's balance`, its hard gate, measured live against the venue). Standard
/// accounts serve none: their spot side is not read for equity.
///
/// Every collateral token the account holds in spot is reported (the default
/// dex's USDC and any builder dex's own token), net of the maintenance of
/// every dex that margins against that token.
pub(crate) fn available_after_maintenance(
    account: &Account,
    feed: &MarketFeed,
) -> Result<Vec<(u32, Decimal)>> {
    if account.mode != UserAbstraction::UnifiedAccount {
        return Ok(Vec::new());
    }
    let mut maintenance: BTreeMap<u32, Decimal> = BTreeMap::new();
    for dex in account.position_dexs() {
        let v = view(account, feed, &dex)?;
        let isolated = v
            .asset_positions
            .iter()
            .filter(|row| row.position.leverage.kind == LeverageType::Isolated)
            .try_fold(Decimal::ZERO, |acc, row| {
                add(acc, row.position.margin_used, ISOLATED_MARGIN)
            })?;
        let of_dex = add(v.cross_maintenance_margin_used, isolated, "maintenance")?;
        let token = feed.collateral_token_of(&dex);
        let total = maintenance.entry(token).or_insert(Decimal::ZERO);
        *total = add(*total, of_dex, "maintenance")?;
    }
    let mut collateral: Vec<u32> = feed
        .dex_collateral_tokens()
        .into_iter()
        .map(|(_, token)| token)
        .collect();
    collateral.push(COLLATERAL_TOKEN);
    let mut out = Vec::new();
    for (token, entry) in &account.spot {
        if !collateral.contains(token) {
            continue;
        }
        let held = maintenance.get(token).copied().unwrap_or(Decimal::ZERO);
        out.push((
            *token,
            sub(entry.total, held, "available after maintenance")?,
        ));
    }
    Ok(out)
}

/// The spot balance of `token` free for margin: total minus hold (zero when
/// the account holds none).
fn spot_collateral(account: &Account, token: u32) -> Result<Decimal> {
    match account.spot.get(&token) {
        Some(entry) => sub(entry.total, entry.hold, "spot hold"),
        None => Ok(Decimal::ZERO),
    }
}

/// The initial margin the account's positions hold on every dex that margins
/// against `token`, except `except` (the dex under check).
fn pooled_margin(
    account: &Account,
    feed: &MarketFeed,
    token: u32,
    except: Option<&str>,
) -> Result<Decimal> {
    let mut tokens: BTreeMap<String, u32> = BTreeMap::new();
    let mut total = Decimal::ZERO;
    for (coin, p) in &account.positions {
        let dex = dex_of_coin(coin);
        if Some(dex) == except || p.szi.is_zero() {
            continue;
        }
        let of_dex = *tokens
            .entry(dex.to_string())
            .or_insert_with(|| feed.collateral_token_of(dex));
        if of_dex == token {
            total = add(total, p.margin_used, "pooled margin")?;
        }
    }
    Ok(total)
}

/// The spot collateral a unified account's dex can margin against. THE ONE
/// PLACE that encodes the non-USDC collateral rule, which is modeled, not
/// measured (no sampled account held a position on a USDH dex): a dex
/// margins against the spot balance of its own `collateralToken`, and the
/// balance is pooled across every dex sharing that token, so the margin the
/// account's other dexes hold on the same token is not available to this one.
fn unified_spot_available(account: &Account, feed: &MarketFeed, dex: &str) -> Result<Decimal> {
    let token = feed.collateral_token_of(dex);
    sub(
        spot_collateral(account, token)?,
        pooled_margin(account, feed, token, Some(dex))?,
        "pooled spot collateral",
    )
}

/// The account's free margin for an order: the perp sub-ledger's value, plus a
/// **unified** account's spot collateral (the venue's single balance), less
/// the positions' initial margin. A standard account's spot holdings are not
/// margin and contribute nothing.
///
/// This is deliberately separate from [`view`]'s `marginSummary`: the venue
/// serves the perp sub-ledger's summary alone (a unified account with only
/// spot collateral serves `accountValue = 0` beside its spot balance), but the
/// same account can still trade perps against that spot collateral. The order
/// lane's placement and post-fill checks read this figure; the served summary
/// does not change.
///
/// Per dex: `dex`'s own ledger, plus (unified) the spot collateral of the
/// dex's token net of the margin the account's other dexes hold on that token.
pub(crate) fn available_margin(
    account: &Account,
    feed: &MarketFeed,
    dex: &str,
    view: &AccountView,
) -> Result<Decimal> {
    let mut value = view.margin_summary.account_value;
    if account.mode == UserAbstraction::UnifiedAccount {
        value = add(
            value,
            unified_spot_available(account, feed, dex)?,
            "unified spot collateral",
        )?;
    }
    sub(
        value,
        view.margin_summary.total_margin_used,
        "available margin",
    )
}

/// The account's cross equity for the liquidation waterfall: the perp
/// sub-ledger's cross value plus a **unified** account's spot collateral. The
/// waterfall must see the same unified balance the order lane trades against;
/// a spot-only unified account with a small perp position is not insolvent.
pub(crate) fn unified_equity(
    account: &Account,
    feed: &MarketFeed,
    dex: &str,
    view: &AccountView,
) -> Result<Decimal> {
    let mut equity = view.cross_margin_summary.account_value;
    if account.mode == UserAbstraction::UnifiedAccount {
        equity = add(
            equity,
            unified_spot_available(account, feed, dex)?,
            "unified spot collateral",
        )?;
    }
    Ok(equity)
}

/// The Core→EVM spot bridge bound: the collateral token's
/// `tokenToAvailableAfterMaintenance` for a unified account — the venue's own
/// figure for what spot may leave after the perp book's maintenance is kept.
/// `None` for a standard account: its spot holdings are not margin, so the
/// balance alone bounds the move.
///
/// **Not `withdrawable`.** The served `withdrawable` is the *perp class's*
/// transferable figure; a unified account can hold free spot while its perp
/// wallet is empty (the live `cf16` recovery shape: 240.510247 spot against
/// 0.000000455 perp). Gating a spot→EVM bridge on the perp figure refused the
/// whole spot balance, which is exactly the recovery leg `recallAllFromCore`
/// exists to move.
pub(crate) fn spot_bridge_bound(
    account: &Account,
    feed: &MarketFeed,
    token: u32,
) -> Result<Option<Decimal>> {
    if account.mode != UserAbstraction::UnifiedAccount {
        return Ok(None);
    }
    Ok(available_after_maintenance(account, feed)?
        .into_iter()
        .find(|(t, _)| *t == token)
        .map(|(_, available)| available))
}

/// What one dex's perp wallet may send out now: the docs' transfer rule at the
/// live marks, the served `withdrawable` (the margin floor; "transferring
/// includes ... transfer to spot wallet"), so the margin in use never leaves.
/// A unified account's perp view is one slice of a pool and may go negative,
/// so the floor alone bounds it; a standard account's wallet also never sends
/// more than its cash. `None` when a mark behind the dex's positions is not
/// served: the caller refuses rather than guesses. The sibling unified figure
/// is [`available_after_maintenance`].
pub(crate) fn perp_transfer_bound(
    account: &Account,
    feed: &MarketFeed,
    dex: &str,
) -> Option<Decimal> {
    let withdrawable = view(account, feed, dex).ok()?.withdrawable;
    Some(if account.mode == UserAbstraction::UnifiedAccount {
        withdrawable
    } else {
        withdrawable.min(account.perp_balance(dex).max(Decimal::ZERO))
    })
}

/// The maintenance-margin rate for a notional: the margin tier's
/// `1/(2·maxLeverage)` (maintenance = ½ initial). There is no honest rate
/// without the live meta: a missing meta is warming, a missing asset (or a
/// zero max leverage) is a named refusal. Returning zero would understate
/// maintenance and let the liquidation check skip a real shortfall.
pub(crate) fn maintenance_rate(
    coin: &str,
    notional: Decimal,
    meta: Option<&crate::wire::Meta>,
) -> Result<Decimal> {
    let meta = meta.ok_or(crate::error::Error::Warming)?;
    let asset = meta
        .asset(coin)
        .ok_or_else(|| crate::error::Error::NotFound {
            what: format!("asset `{coin}` in the live meta (maintenance rate)"),
        })?;
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
        return Err(crate::error::Error::NotFound {
            what: format!("a positive max leverage for `{coin}` in the live meta"),
        });
    }
    Ok(Decimal::ONE / (Decimal::from(2 * max_leverage)))
}

pub(crate) fn maintenance_margin(
    coin: &str,
    notional: Decimal,
    meta: Option<&crate::wire::Meta>,
) -> Result<Decimal> {
    mul(
        notional,
        maintenance_rate(coin, notional, meta)?,
        "maintenance margin",
    )
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
    meta.and_then(|m| m.asset(coin)).map(|a| a.max_leverage)
}

/// The isolated liquidation price (exact formula):
/// `price − side·margin_available/position_size/(1 − l·side)`,
/// with `margin_available = margin − |size|·mark·maintenance_rate` — the
/// maintenance margin is already owed, so it cannot also be available to
/// absorb a loss. The docs' live isolated example (`2866.26936529`) is the
/// regression pin: omitting the subtraction gives `2836.1`, a liquidation
/// price that is too far away.
pub(crate) fn liquidation_px(
    p: &Position,
    mark: Decimal,
    maintenance_rate: Decimal,
) -> Result<Decimal> {
    if p.szi.is_zero() {
        return Ok(mark);
    }
    let side = match p.direction() {
        Direction::Long => Decimal::ONE,
        Direction::Short => -Decimal::ONE,
    };
    let position_size = p.szi.abs();
    let denominator = sub(
        Decimal::ONE,
        mul(maintenance_rate, side, "liquidation denominator")?,
        "liquidation denominator",
    )?;
    if denominator.is_zero() {
        return Err(Error::DivideByZero {
            what: "liquidation denominator",
        });
    }
    let maintenance = mul(
        mul(position_size, mark, "liquidation maintenance notional")?,
        maintenance_rate,
        "liquidation maintenance",
    )?;
    let margin_available = sub(p.margin_used, maintenance, "liquidation margin available")?;
    let distance = mul(side, margin_available, "liquidation margin")? / position_size / denominator;
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
    /// The margin mode a new position opens in (the account's setting for the
    /// coin, else the market's default); an existing position keeps its own.
    pub mode: LeverageType,
    pub reduce: crate::sim::order::Reduce,
    /// The trading fee (venue base tier; charged on top of the fill — the
    /// plan's `closedPnl = fee + side·(mark−entry)·size`).
    pub fee: Decimal,
    /// The builder fee the fill's order carries, computed (`None` for an
    /// order without a builder, and for every liquidation, ADL and
    /// settlement fill). Debited with the trading fee, on top of it.
    pub builder: Option<crate::sim::order::BuilderCharge>,
}

impl FillFacts {
    /// A reduce-only close at `px` with no fee: the waterfall's shape (the ADL
    /// pairs and the book's reduce closes).
    pub(crate) fn close(
        coin: &str,
        side: crate::sim::order::Side,
        sz: Decimal,
        px: Decimal,
        leverage: u32,
    ) -> Self {
        Self {
            coin: coin.to_string(),
            side,
            size: sz,
            px,
            leverage,
            mode: LeverageType::Cross,
            reduce: crate::sim::order::Reduce::Only,
            fee: Decimal::ZERO,
            builder: None,
        }
    }
}

/// Apply one fill to the account; the effect (the size before, the realized
/// PnL of the closed part, the direction) is what the ledger records.
#[allow(clippy::too_many_lines)] // one ledger update: position, margin and realized pnl
pub(crate) fn apply_fill(
    account: &mut Account,
    fill: &FillFacts,
    meta: Option<&crate::wire::Meta>,
) -> Result<FillEffect> {
    let signed = fill.size * fill.side.sign();
    let old = account
        .positions
        .get(&fill.coin)
        .map(|p| p.szi)
        .unwrap_or(Decimal::ZERO);
    refuse_non_reducing(old, signed, fill)?;
    // The fill settles in its own dex's clearinghouse.
    let dex = dex_of_coin(&fill.coin).to_string();
    let position = account
        .positions
        .entry(fill.coin.clone())
        .or_insert_with(|| fresh_position(fill));
    let mut allocated = Decimal::ZERO;
    let (closed, realized) = if same_direction(old, signed) {
        allocated = apply_increase(position, old, fill)?;
        position.isolated_margin = add(
            position.isolated_margin,
            allocated,
            "isolated margin allocation",
        )?;
        (Decimal::ZERO, Decimal::ZERO)
    } else {
        let closed = old.abs().min(fill.size);
        let realized = apply_reduction(position, old, fill)?;
        credit_ledger(&mut account.perps, &dex, realized, "realized PnL")?;
        (closed, realized)
    };
    // An isolated position funds its own margin: the increase's initial margin
    // leaves the cross balance for the position (the venue's isolated open).
    if !allocated.is_zero() {
        credit_ledger(
            &mut account.perps,
            &dex,
            -allocated,
            "isolated margin allocation",
        )?;
    }
    // The trading fee (the venue's base tier): charged on the fill's notional,
    // on both sides of a trade, in the dex's collateral token — so a closing
    // fill pays it too.
    if !fill.fee.is_zero() {
        credit_ledger(&mut account.perps, &dex, -fill.fee, "trading fee")?;
    }
    // The builder fee rides on top of the trading fee (the order's `builder`),
    // debited from the same balance; the builder's credit is the caller's.
    if let Some(charge) = &fill.builder {
        credit_ledger(&mut account.perps, &dex, -charge.amount, "builder fee")?;
    }
    // The position's last use: the borrow ends here, so the release (which
    // re-looks the position up) can borrow the account whole.
    let flat = position.szi.is_zero();
    release_isolated_margin(account, &fill.coin, old, closed, meta)?;
    let effect = FillEffect::of(old, signed, realized);
    if flat {
        account.positions.remove(&fill.coin);
        return Ok(effect);
    }
    if let Some(position) = account.positions.get_mut(&fill.coin) {
        settle_margin(position)?;
    }
    Ok(effect)
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
    position.isolated_margin = sub(
        position.isolated_margin,
        released,
        "isolated margin release",
    )?;
    account.credit_perp(dex_of_coin(coin), released, "isolated margin release")?;
    Ok(())
}

/// The reduce-only guard: the venue's own string when the fill
/// would not reduce the position it names.
fn refuse_non_reducing(old: Decimal, signed: Decimal, fill: &FillFacts) -> Result<()> {
    if fill.reduce == crate::sim::order::Reduce::Only
        && (same_direction(old, signed) || fill.size > old.abs())
    {
        return Err(Error::bad_request(
            "Reduce only order would increase position.",
        ));
    }
    Ok(())
}

/// A position's first appearance: the fill's margin mode, at the fill's price
/// and the fill's leverage (the account's setting decides later fills'
/// leverage, not this).
fn fresh_position(fill: &FillFacts) -> Position {
    Position {
        szi: Decimal::ZERO,
        entry_px: fill.px,
        leverage: fill.mode,
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
    position.margin_used =
        if position.leverage == LeverageType::Isolated && !position.isolated_margin.is_zero() {
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
/// average of the old notional and the fill; an isolated position returns the
/// fill's own initial margin (moved out of the cross balance by the caller).
fn apply_increase(position: &mut Position, old: Decimal, fill: &FillFacts) -> Result<Decimal> {
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
    // An isolated position funds its own margin: the increase's initial margin
    // leaves the cross balance for the position (the venue's isolated open).
    if position.leverage == LeverageType::Isolated {
        return div(
            fill_notional,
            Decimal::from(position.leverage_value),
            "isolated margin allocation",
        );
    }
    Ok(Decimal::ZERO)
}

/// A reduction (or a flip): the closed part's realized PnL (returned for the
/// caller to credit — the measured identity stays exact); a flip opens the
/// remainder at the fill price.
fn apply_reduction(position: &mut Position, old: Decimal, fill: &FillFacts) -> Result<Decimal> {
    let signed = fill.size * fill.side.sign();
    let closed = old.abs().min(fill.size);
    let realized = mul(
        mul(
            closed,
            sub(fill.px, position.entry_px, "px − entry")?,
            "closed PnL",
        )?,
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
pub(crate) fn liquidate_if_needed(
    account: &mut Account,
    feed: &MarketFeed,
    dex: &str,
) -> Result<Option<Liquidation>> {
    if account.positions.is_empty() {
        return Ok(None);
    }
    let v = view(account, feed, dex)?;
    let maintenance = add(
        v.cross_maintenance_margin_used,
        isolated_maintenance(account, &v)?,
        "maintenance total",
    )?;
    if v.margin_summary.account_value >= maintenance {
        return Ok(None);
    }
    let closed: Vec<String> = account
        .positions
        .keys()
        .filter(|coin| dex_of_coin(coin) == dex)
        .cloned()
        .collect();
    let mut realized = Decimal::ZERO;
    for (coin, p) in &account.positions {
        // The facts cover every non-flat position; a flat row contributes no
        // PnL (and has no facts — the view skips flat rows).
        if let Some(facts) = v.facts.get(coin) {
            realized = add(realized, p.unrealized_pnl(facts.mark)?, "realized PnL")?;
        }
    }
    account.positions.retain(|coin, _| dex_of_coin(coin) != dex);
    account.credit_perp(dex, realized, "liquidated balance")?;
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
    // The feed keeps its contexts by coin name (builder coins included): a
    // direct lookup, never a match on the mark and the oracle (two coins can
    // share both and still carry their own open interest).
    if let Some(ctx) = feed.ctx(coin) {
        return Some(ctx);
    }
    let Prices { mark, oracle } = feed.required_prices(coin)?;
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
        control_owned: feed.market_is_owned(coin),
        funding_owned: feed.funding_is_owned(coin),
    })
}

/// The address-keyed registry type (shared by the sim state and the API).
pub(crate) type Accounts = BTreeMap<Address, Account>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{feed_with, feed_with_meta, meta_with, meta_with_table};

    /// The unified account's `tokenToAvailableAfterMaintenance` closes the consumer's
    /// reconciliation identity: `available + maintenance == the collateral
    /// balance` (their hard gate). A standard account serves none.
    /// The unified account's spot collateral is part of the raw balance (the
    /// single USDC figure the docs describe), so it backs perp margin and the
    /// identity stays exact.
    /// The direction predicate: flat is no direction, signs decide (the
    /// reduce-only, fill and cap checks' shared rule).
    /// **The spot bridge's bound is the spot available, not the perp
    /// `withdrawable`.** The live `cf16` recovery shape: 240.510247 free spot
    /// with a 0.000000455 perp wallet. The perp figure bounds a USD-class
    /// transfer, not a spot→EVM bridge; using it refused the whole recovery.
    #[test]
    fn the_spot_bridge_bound_is_the_spot_available_not_the_perp_withdrawable() {
        let feed = feed_with_meta(&[], meta_with_table("BTC", 10));
        let mut account = Account {
            // The `cf16` seed's shape: the perp wallet carries the 0.000000455
            // residue, the spot class the 240.510247 principal.
            perps: crate::sim::account::default_perps(
                "0.000000455".parse::<Decimal>().unwrap()
                    - "240.510247".parse::<Decimal>().unwrap(),
                0,
            ),
            mode: UserAbstraction::UnifiedAccount,
            ..Account::default()
        };
        spot_entry(
            &mut account,
            crate::sim::TokenRef {
                index: COLLATERAL_TOKEN,
                coin: "USDC",
            },
        )
        .total = "240.510247".parse().unwrap();
        let bound = spot_bridge_bound(&account, &feed, COLLATERAL_TOKEN)
            .expect("the bound resolves")
            .expect("a unified account carries the spot bound");
        assert_eq!(bound, "240.510247".parse::<Decimal>().unwrap());
        assert!(
            bound > view(&account, &feed, "").unwrap().withdrawable,
            "the perp withdrawable must not bound the spot bridge"
        );
        // A standard account has no unified spot bound: the balance alone applies.
        account.mode = UserAbstraction::Default;
        assert_eq!(
            spot_bridge_bound(&account, &feed, COLLATERAL_TOKEN).unwrap(),
            None
        );
    }

    #[test]
    fn same_direction_treats_flat_as_no_direction() {
        assert!(same_direction(Decimal::ZERO, Decimal::from(3)));
        assert!(same_direction(Decimal::from(2), Decimal::from(3)));
        assert!(same_direction(Decimal::from(-2), Decimal::from(-3)));
        assert!(!same_direction(Decimal::from(2), Decimal::from(-3)));
    }

    /// **The unified spot collateral is a separate ledger from the perp
    /// summary.** The committed `clearinghouseState_unified_spot_only.json`
    /// serves `accountValue = 0` beside 2995.67224054 USDC in spot; the
    /// reconciliation is `availableAfterMaintenance + maintenance == the spot
    /// balance`, not a term of the perp identity. Adding spot to the perp
    /// summary double-counted it and made a later spot bridge drive the served
    /// perp equity negative (the live `cf16` recovery).
    #[test]
    fn the_unified_spot_is_a_separate_ledger_from_the_perp_summary() {
        let feed = feed_with_meta(&[("BTC", "100.0", "100.0")], meta_with_table("BTC", 10));
        let mut account = Account {
            perps: crate::sim::account::default_perps(Decimal::ZERO, 0),
            mode: UserAbstraction::UnifiedAccount,
            ..Account::default()
        };
        spot_entry(
            &mut account,
            crate::sim::TokenRef {
                index: COLLATERAL_TOKEN,
                coin: "USDC",
            },
        )
        .total = "250".parse().unwrap();
        let v = view(&account, &feed, "").unwrap();
        assert_eq!(
            v.margin_summary.total_raw_usd,
            Decimal::ZERO,
            "the perp summary carries only the perp wallet"
        );
        assert_eq!(v.margin_summary.account_value, Decimal::ZERO);
        assert_eq!(
            available_after_maintenance(&account, &feed).unwrap(),
            vec![(COLLATERAL_TOKEN, "250".parse().unwrap())],
            "the spot collateral is served as the spot available"
        );
        // The same account in standard mode: the spot side is not read.
        account.mode = UserAbstraction::Default;
        let v = view(&account, &feed, "").unwrap();
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
        let feed = feed_with_meta(&[("BTC", "100.0", "100.0")], meta_with_table("BTC", 10));
        let mut account = Account {
            perps: crate::sim::account::default_perps(Decimal::ZERO, 0),
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
        let rows = available_after_maintenance(&account, &feed).unwrap();
        let available = rows.iter().find(|(t, _)| *t == 0).unwrap().1;
        let v = view(&account, &feed, "").unwrap();
        assert!(v.cross_maintenance_margin_used > Decimal::ZERO);
        assert_eq!(
            available + v.cross_maintenance_margin_used,
            Decimal::from(50)
        );

        account.mode = UserAbstraction::Default;
        assert!(available_after_maintenance(&account, &feed)
            .unwrap()
            .is_empty());
    }

    /// Maintenance is never fabricated: no meta is warming, an unknown asset
    /// or a zero max leverage is a named refusal — not a zero rate that would
    /// let the liquidation check skip a shortfall.
    #[test]
    fn maintenance_requires_the_live_meta() {
        let err = maintenance_rate("BTC", Decimal::from(100), None).unwrap_err();
        assert!(matches!(err, crate::error::Error::Warming), "{err}");
        let meta = meta_with("ETH", 10);
        let err = maintenance_rate("BTC", Decimal::from(100), Some(&meta)).unwrap_err();
        assert!(matches!(err, crate::error::Error::NotFound { .. }), "{err}");
        let zero = meta_with("BTC", 0);
        let err = maintenance_rate("BTC", Decimal::from(100), Some(&zero)).unwrap_err();
        assert!(matches!(err, crate::error::Error::NotFound { .. }), "{err}");
        let meta = meta_with("BTC", 10);
        assert_eq!(
            maintenance_rate("BTC", Decimal::from(100), Some(&meta)).unwrap(),
            Decimal::new(5, 2),
            "1 / (2 * 10x)"
        );
    }

    /// A liquidation denominator of zero has no price: refused by name, never
    /// answered with the mark (which would look like a valid liquidation).
    #[test]
    fn a_zero_liquidation_denominator_is_refused() {
        let p = Position {
            szi: Decimal::ONE,
            entry_px: Decimal::from(100),
            leverage: LeverageType::Isolated,
            leverage_value: 1,
            margin_used: Decimal::ONE,
            isolated_margin: Decimal::ONE,
            funding_all_time: Decimal::ZERO,
            funding_since_open: Decimal::ZERO,
            funding_since_change: Decimal::ZERO,
        };
        let err = liquidation_px(&p, Decimal::from(100), Decimal::ONE).unwrap_err();
        assert!(matches!(err, Error::DivideByZero { .. }), "{err}");
    }

    /// A zero-size position is a closed one: the served state drops the row
    /// (the consumer's flat-rows-dropped gate), while the internal map may keep it
    /// until a mutation clears it.
    #[test]
    fn flat_rows_are_dropped_from_the_served_state() {
        let feed = feed_with(&[("BTC", "100", "100")]);
        let mut account = Account {
            perps: crate::sim::account::default_perps(Decimal::from(10), 0),
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
        let s = clearinghouse(&account, &feed, "", 1).unwrap();
        assert!(s.asset_positions.is_empty(), "flat row served");
        assert_eq!(s.margin_summary.account_value, Decimal::from(10));
    }

    /// An empty account is never "below maintenance" (negative equity is a
    /// state the venue reports, not a repeated liquidation).
    #[test]
    fn empty_account_never_liquidates() {
        let feed = feed_with(&[("BTC", "50", "50")]);
        let mut account = Account {
            perps: crate::sim::account::default_perps(Decimal::from(-100), 0),
            ..Account::default()
        };
        assert!(liquidate_if_needed(&mut account, &feed, "")
            .unwrap()
            .is_none());
    }

    /// Below maintenance, the position closes at the mark and the balance
    /// becomes the account value.
    #[test]
    fn liquidation_closes_at_the_mark() {
        let feed = feed_with_meta(&[("BTC", "50", "50")], meta_with_table("BTC", 10));
        let mut account = Account {
            perps: crate::sim::account::default_perps(Decimal::from(1), 0), // tiny balance
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
        let liq = liquidate_if_needed(&mut account, &feed, "")
            .unwrap()
            .expect("liquidated");
        assert_eq!(liq.positions_closed, vec!["BTC"]);
        assert!(account.positions.is_empty());
        assert_eq!(account.perp_balance(""), Decimal::from(1) - Decimal::from(50));
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
        // Long: mark − (margin − |size|·mark·l)/size/(1 − l). The docs' live
        // example is 2866.26936529; the old formula (no maintenance
        // subtraction) gave 2836.1087743102668475802804161.
        let maintenance = p.szi.abs() * mark * rate;
        let expected = mark - (p.margin_used - maintenance) / p.szi / (Decimal::ONE - rate);
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
        feed.own_funding("BTC", Decimal::new(125, 7), None).unwrap();
        let ctx = ctx_of(&feed, "BTC").unwrap();
        assert_eq!(ctx.funding, Decimal::new(125, 7));
        assert_eq!(ctx.mark_px, Decimal::from(100));
        assert_eq!(ctx.oracle_px, Decimal::from(101));
    }

    /// An isolated position's loss is its own: with almost no free cash beside
    /// it, the cross figure stays the cash (not cash minus the isolated loss),
    /// the waterfall leaves the solvent isolated leg alone, and the position
    /// row serves the venue's figures (`marginUsed` = margin + pnl, `rawUsd` =
    /// `marginUsed` less the signed value, positive for a short).
    #[test]
    #[allow(clippy::too_many_lines)] // one scenario: the served figures and the waterfall's verdict
    fn an_isolated_loss_beyond_the_free_cash_is_not_a_cross_liquidation() {
        for long in [true, false] {
            let feed = feed_with_meta(&[("ETH", "100", "100")], meta_with("ETH", 10));
            feed.put_oi_caps(Vec::new());
            let sim = crate::sim::Sim::memory();
            let a = Address::repeat_byte(0xa6);
            let signed = if long { Decimal::ONE } else { -Decimal::ONE };
            sim.with_account(a, |account| {
                // 110 funded, 10 at 10x: 100 parked on the isolated position.
                *account.perp_balance_mut("") = Decimal::from(10);
                account.positions.insert(
                    "ETH".into(),
                    Position {
                        szi: signed * Decimal::from(10),
                        entry_px: Decimal::from(100),
                        leverage: LeverageType::Isolated,
                        leverage_value: 10,
                        margin_used: Decimal::from(100),
                        isolated_margin: Decimal::from(100),
                        funding_all_time: Decimal::ZERO,
                        funding_since_open: Decimal::ZERO,
                        funding_since_change: Decimal::ZERO,
                    },
                );
                Ok(())
            })
            .unwrap();
            let cash = sim.account(a).unwrap().unwrap().perp_balance("");
            // 4 % against the position: a loss of 40 on a margin of 100, far
            // more than the free cash, still above the leg's 5 % maintenance
            // (48).
            let mark = if long {
                Decimal::from(96)
            } else {
                Decimal::from(104)
            };
            let text = mark.to_string();
            feed.put_ctx(
                "ETH".into(),
                crate::wire::test_ctx(&text, &text),
                crate::now_ms(),
            );
            let held = sim.account(a).unwrap().unwrap().positions["ETH"].clone();
            let equity = held.isolated_margin + held.unrealized_pnl(mark).unwrap();
            assert!(
                sim.check_all(&feed, crate::now_ms()).unwrap().is_empty(),
                "a solvent isolated leg is not liquidated"
            );
            let account = sim.account(a).unwrap().unwrap();
            assert_ne!(account.positions["ETH"].szi, Decimal::ZERO);
            let served = clearinghouse(&account, &feed, "", 0).unwrap();
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
}
