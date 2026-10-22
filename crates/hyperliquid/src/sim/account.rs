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
use crate::error::Result;
use crate::feed::MarketFeed;
use crate::wire::{
    AssetCtx, AssetPosition, ClearinghouseState, CumFunding, Leverage, LeverageType, MarginSummary,
    Position as WirePosition, PositionKind, SpotBalance, UserAbstraction,
};
use alloy_primitives::Address;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One simulated account (persisted). The fields are the *inputs*; the wire
/// shape is derived per read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Account {
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
}

fn default_mode() -> UserAbstraction {
    UserAbstraction::Default
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Position {
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
    pub fn position_value(&self, mark: Decimal) -> Decimal {
        self.szi.abs() * mark
    }

    /// Unrealized PnL at the mark: szi·(mark − entry).
    pub fn unrealized_pnl(&self, mark: Decimal) -> Decimal {
        self.szi * (mark - self.entry_px)
    }

    /// The cost basis: szi·entry.
    pub fn cost_basis(&self) -> Decimal {
        self.szi * self.entry_px
    }

    pub fn is_long(&self) -> bool {
        !self.szi.is_sign_negative()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpotEntry {
    pub coin: String,
    pub total: Decimal,
    #[serde(default)]
    pub hold: Decimal,
    #[serde(default)]
    pub entry_ntl: Decimal,
}

/// The seeding shape: hl_mock serves the venue's own state, so the seed is a
/// (possibly partial) `clearinghouseState` — the sim reads `totalRawUsd` and
/// the positions and derives the rest at serve time.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AccountSeed {
    #[serde(default)]
    pub margin_summary: Option<MarginSummary>,
    #[serde(default)]
    pub asset_positions: Vec<AssetPosition>,
    /// The spot side (`spotClearinghouseState`'s `balances`).
    #[serde(default)]
    pub balances: Vec<SpotBalance>,
    #[serde(default)]
    pub abstraction: Option<UserAbstraction>,
}

impl AccountSeed {
    /// Fold the seed into internal inputs. `totalRawUsd` is preserved exactly:
    /// `balance = totalRawUsd + Σ szi·entryPx`, so the served totalRawUsd
    /// round-trips byte-for-byte (the operator's recall amount).
    pub fn into_account(self) -> Account {
        let mut account = Account {
            mode: self.abstraction.unwrap_or_default(),
            ..Account::default()
        };
        let mut cost_basis = Decimal::ZERO;
        for asset in self.asset_positions {
            let p = asset.position;
            let entry = p.entry_px.unwrap_or_default();
            let position = Position {
                szi: p.szi,
                entry_px: entry,
                leverage: p.leverage.kind,
                leverage_value: p.leverage.value,
                margin_used: p.margin_used,
                funding_all_time: p.cum_funding.all_time,
                funding_since_open: p.cum_funding.since_open,
                funding_since_change: p.cum_funding.since_change,
            };
            cost_basis += position.cost_basis();
            account.positions.insert(p.coin, position);
        }
        let raw = self
            .margin_summary
            .map(|m| m.total_raw_usd)
            .unwrap_or_default();
        account.perp_balance = raw + cost_basis;
        for b in self.balances {
            account.spot.insert(
                b.token,
                SpotEntry {
                    coin: b.coin,
                    total: b.total,
                    hold: b.hold,
                    entry_ntl: b.entry_ntl,
                },
            );
        }
        account
    }
}

/// The computed account figures, one pass over the positions.
pub struct AccountView {
    pub margin_summary: MarginSummary,
    pub cross_margin_summary: MarginSummary,
    pub cross_maintenance_margin_used: Decimal,
    pub withdrawable: Decimal,
    pub asset_positions: Vec<AssetPosition>,
    /// Per-position mark, for the liquidation check (not served).
    pub marks: BTreeMap<String, Decimal>,
}

/// Compute the served figures at the feed's current marks.
pub fn view(account: &Account, feed: &MarketFeed, meta: Option<&crate::wire::Meta>) -> Result<AccountView> {
    let mut total_ntl = Decimal::ZERO;
    let mut total_margin = Decimal::ZERO;
    let mut total_signed_value = Decimal::ZERO;
    let mut cost_basis = Decimal::ZERO;
    let mut cross_ntl = Decimal::ZERO;
    let mut cross_margin = Decimal::ZERO;
    let mut cross_maintenance = Decimal::ZERO;
    let mut isolated_margin = Decimal::ZERO;
    let mut rows = Vec::with_capacity(account.positions.len());
    let mut marks = BTreeMap::new();

    for (coin, p) in &account.positions {
        let (mark, _oracle) = feed
            .required_prices(coin)
            .ok_or_else(|| crate::error::Error::NoMarket { coin: coin.clone() })?;
        marks.insert(coin.clone(), mark);
        let value = p.position_value(mark);
        let u_pnl = p.unrealized_pnl(mark);
        let signed_value = if p.is_long() { value } else { -value };
        let maintenance = maintenance_margin(coin, value, meta);

        total_ntl += value;
        total_margin += p.margin_used;
        total_signed_value += signed_value;
        cost_basis += p.cost_basis();
        if p.leverage == LeverageType::Cross {
            cross_ntl += value;
            cross_margin += p.margin_used;
            cross_maintenance += maintenance;
        } else {
            isolated_margin += p.margin_used;
        }

        let roe = if p.margin_used.is_zero() {
            Decimal::ZERO
        } else {
            u_pnl / p.margin_used
        };
        let liquidation_px = (p.leverage == LeverageType::Isolated)
            .then(|| liquidation_px(p, mark, maintenance_rate(coin, value, meta)));
        rows.push(AssetPosition {
            kind: PositionKind::OneWay,
            position: WirePosition {
                coin: coin.clone(),
                entry_px: Some(p.entry_px),
                leverage: Leverage {
                    kind: p.leverage,
                    value: p.leverage_value,
                    raw_usd: Some(p.margin_used - value),
                },
                liquidation_px,
                margin_used: p.margin_used,
                max_leverage: max_leverage(coin, meta),
                position_value: value,
                return_on_equity: roe,
                szi: p.szi,
                unrealized_pnl: u_pnl,
                cum_funding: CumFunding {
                    all_time: p.funding_all_time,
                    since_open: p.funding_since_open,
                    since_change: p.funding_since_change,
                },
            },
        });
    }

    // The identity, by construction: accountValue = totalRawUsd + Σ signed.
    let total_raw = account.perp_balance - cost_basis;
    let account_value = total_raw + total_signed_value;
    let margin_summary = MarginSummary {
        account_value,
        total_ntl_pos: total_ntl,
        total_raw_usd: total_raw,
        total_margin_used: total_margin,
    };
    // The cross view carves isolated margin out (measured exact on the live
    // sample); with no isolated positions it equals the margin summary.
    let cross_account_value = account_value - isolated_margin;
    let cross_margin_summary = MarginSummary {
        account_value: cross_account_value,
        total_ntl_pos: cross_ntl,
        total_raw_usd: total_raw,
        total_margin_used: cross_margin,
    };
    Ok(AccountView {
        margin_summary,
        cross_margin_summary,
        cross_maintenance_margin_used: cross_maintenance,
        withdrawable: cross_account_value,
        asset_positions: rows,
        marks,
    })
}

/// The served `clearinghouseState` (fresh `time` from the caller's wall clock).
pub fn clearinghouse(account: &Account, feed: &MarketFeed, meta: Option<&crate::wire::Meta>, time_ms: u64) -> Result<ClearinghouseState> {
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

/// The served spot balances (USDC token 0 sorts first, like the venue's).
pub fn spot_balances(account: &Account) -> Vec<SpotBalance> {
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

/// The maintenance-margin rate for a notional: the margin tier's
/// `1/(2·maxLeverage)` (maintenance = ½ initial, plan §16.3).
pub fn maintenance_rate(coin: &str, notional: Decimal, meta: Option<&crate::wire::Meta>) -> Decimal {
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

pub fn maintenance_margin(coin: &str, notional: Decimal, meta: Option<&crate::wire::Meta>) -> Decimal {
    notional * maintenance_rate(coin, notional, meta)
}

fn max_leverage(coin: &str, meta: Option<&crate::wire::Meta>) -> u32 {
    meta.and_then(|m| m.universe.iter().find(|a| a.name == coin))
        .map(|a| a.max_leverage)
        .unwrap_or(20)
}

/// The isolated liquidation price (exact formula, plan §16.3):
/// `price − side·margin_available/position_size/(1 − l·side)`,
/// with the isolated position's own margin as `margin_available`.
pub fn liquidation_px(p: &Position, mark: Decimal, maintenance_rate: Decimal) -> Decimal {
    if p.szi.is_zero() {
        return mark;
    }
    let side = if p.is_long() { Decimal::ONE } else { -Decimal::ONE };
    let position_size = p.szi.abs();
    let denominator = Decimal::ONE - maintenance_rate * side;
    if denominator.is_zero() {
        return mark;
    }
    mark - side * p.margin_used / position_size / denominator
}

/// Accrue funding from the account's stamp to `to_ms` at the live rates
/// (hourly, oracle-priced; plan §16.3: `payment = −szi·oracle·rate·Δt/1h`).
///
/// A stamp of 0 means "never accrued": the first call establishes the
/// baseline (an account must not inherit funding from the epoch's beginning).
/// All-or-nothing per account: if any position's coin has no live context,
/// the stamp stays where it is — a partially accrued account would silently
/// lose the skipped positions' funding forever.
pub fn accrue_funding(account: &mut Account, feed: &MarketFeed, to_ms: u64) -> usize {
    if account.funded_at_ms == 0 {
        account.funded_at_ms = to_ms;
        return 0;
    }
    let from = account.funded_at_ms;
    if to_ms <= from {
        return 0;
    }
    let priced = account
        .positions
        .keys()
        .all(|coin| feed.required_prices(coin).is_some() && feed.funding(coin).is_some());
    if !priced {
        return 0;
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
        let payment = -p.szi * oracle * rate * hours;
        if payment.is_zero() {
            continue;
        }
        account.perp_balance += payment;
        p.funding_all_time += payment;
        p.funding_since_open += payment;
        p.funding_since_change += payment;
        moved += 1;
    }
    account.funded_at_ms = to_ms;
    moved
}

/// The maintenance check + the v1 close: when the account value falls below
/// its maintenance requirement, every position closes at the mark (the
/// waterfall — partial closes, the backstop, ADL — is Phase 2). An account
/// with no positions is never "below maintenance": negative equity alone is a
/// state the venue reports, not a liquidation to re-run every tick.
pub fn liquidate_if_needed(account: &mut Account, feed: &MarketFeed, meta: Option<&crate::wire::Meta>) -> Result<Option<Liquidation>> {
    if account.positions.is_empty() {
        return Ok(None);
    }
    let v = view(account, feed, meta)?;
    let maintenance = v.cross_maintenance_margin_used + isolated_maintenance(account, &v, meta);
    if v.margin_summary.account_value >= maintenance {
        return Ok(None);
    }
    let closed: Vec<String> = account.positions.keys().cloned().collect();
    let mut realized = Decimal::ZERO;
    for coin in &closed {
        if let (Some(p), Some(mark)) = (account.positions.get(coin), v.marks.get(coin)) {
            realized += p.unrealized_pnl(*mark);
        }
    }
    account.positions.clear();
    account.perp_balance += realized;
    Ok(Some(Liquidation {
        positions_closed: closed,
        account_value: v.margin_summary.account_value,
        maintenance,
    }))
}

fn isolated_maintenance(account: &Account, v: &AccountView, meta: Option<&crate::wire::Meta>) -> Decimal {
    account
        .positions
        .iter()
        .filter(|(_, p)| p.leverage == LeverageType::Isolated)
        .map(|(coin, p)| {
            let mark = v.marks.get(coin).copied().unwrap_or_default();
            maintenance_margin(coin, p.position_value(mark), meta)
        })
        .sum()
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
pub fn mode_of(account: &Account) -> UserAbstraction {
    account.mode
}

/// The coin's live context for `activeAssetData`'s mark (the full live ctx
/// when the feed has one, else a minimal shape from the composed prices).
pub fn ctx_of(feed: &MarketFeed, coin: &str) -> Option<AssetCtx> {
    let (mark, oracle) = feed.required_prices(coin)?;
    if let Some((_, ctxs)) = feed.meta_and_ctxs() {
        if let Some(found) = ctxs.iter().find(|c| c.mark_px == mark && c.oracle_px == oracle) {
            return Some(found.clone());
        }
    }
    Some(AssetCtx {
        funding: feed.funding(coin).unwrap_or_default(),
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
pub type Accounts = BTreeMap<Address, Account>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{AssetInfo, MarginTable, MarginTier, Meta};

    fn feed_with(marks: &[(&str, &str, &str)]) -> MarketFeed {
        let feed = MarketFeed::memory(marks.iter().map(|(c, _, _)| c.to_string()).collect());
        for (coin, mark, oracle) in marks {
            feed.put_ctx(
                coin.to_string(),
                AssetCtx {
                    funding: Decimal::new(125, 7),
                    open_interest: Decimal::ZERO,
                    prev_day_px: Decimal::ZERO,
                    day_ntl_vlm: Decimal::ZERO,
                    premium: None,
                    oracle_px: oracle.parse().unwrap(),
                    mark_px: mark.parse().unwrap(),
                    mid_px: None,
                    impact_pxs: None,
                    day_base_vlm: Decimal::ZERO,
                },
                1,
            );
        }
        feed
    }

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
        let account = seed.into_account();
        assert_eq!(account.perp_balance, Decimal::from(100) + Decimal::new(105, 2) * Decimal::from(10));
        let feed = feed_with(&[("BTC", "10.0", "10.0")]);
        let s = clearinghouse(&account, &feed, None, 1).unwrap();
        assert_eq!(s.margin_summary.total_raw_usd, Decimal::from(100));
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
        let moved = accrue_funding(&mut account, &feed, 7_200_000);
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
        assert_eq!(accrue_funding(&mut account, &feed, 7_200_000), 0);
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
        assert_eq!(accrue_funding(&mut account, &feed, 7_200_000), 2);
        assert_eq!(account.funded_at_ms, 7_200_000);
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
        let liq = liquidation_px(&p, mark, rate);
        // Long: mark − margin/size/(1 − l).
        let expected = mark - p.margin_used / p.szi / (Decimal::ONE - rate);
        assert_eq!(liq, expected);
    }
}
