//! The matching lane: a devnet order fills against the **real book** — the
//! live depth the feed caches — with per-level consumption (the COW
//! analogue), or rests, or arms as a trigger; cancels, modifies, schedule
//! cancels and the isolated-margin moves live here too. The plan's identity
//! (I3): an order moves the *book*, never the *mark*; the pricing oracle
//! moves only by an explicit crash. The trader's own state moves the way the
//! venue's would: size and size-weighted entry update, a reducing fill
//! realizes its PnL into the perp balance (the measured identity
//! `accountValue == totalRawUsd + Σ signed positionValue` stays exact), and
//! the initial margin follows `|szi|·mark/leverage`.
//!
//! Scope, named rather than implied: the signature is accepted but never
//! verified (the devnet has no keys — the anvil-impersonation analogue), and
//! the depth is the served book's (the live book minus what the devnet
//! already consumed, so a second order sees the first one's impact).
use super::Sim;
use super::validate::{validate_order, validate_trigger_side};
use crate::error::{Error, Result};
use crate::feed::{Consumption, MarketFeed};
use crate::wire::L2Book;
use alloy_primitives::Address;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// The docs' default open-order limit (the rate-limits page); the volume
/// bonus (one per 5M USDC traded, capped at 5000) is not modeled.
const OPEN_ORDER_LIMIT: usize = 1000;

/// One order: the venue's own fields, decoded from either surface (the
/// control route's body or the `/exchange` action).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub(crate) struct Order {
    pub coin: String,
    pub is_buy: bool,
    pub size: Decimal,
    /// The limit price (absent = market: fill whatever the book gives).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_px: Option<Decimal>,
    #[serde(default)]
    pub tif: Tif,
    #[serde(default)]
    pub reduce_only: bool,
    /// The client order id (echoed; it also addresses cancels/modifies and
    /// resting orders, as the venue's does).
    #[serde(default)]
    pub cloid: Option<alloy_primitives::B128>,
    /// A trigger order (`t.trigger`): armed instead of walked, fired by the
    /// tick when the mark crosses.
    #[serde(default)]
    pub trigger: Option<Trigger>,
}

impl Order {
    /// The side, converted from the wire's `is_buy` once.
    pub(crate) fn side(&self) -> Side {
        Side::of(self.is_buy)
    }
}

/// Time in force (docs: `"Alo" | "Ioc" | "Gtc"`). The two wire encodings live
/// here, one home each (the venue's `tif` strings and the CoreWriter byte).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Tif {
    #[default]
    Ioc,
    /// Rests on the book; the crossing part still fills immediately (docs:
    /// "GTC orders have no special behavior").
    Gtc,
    /// Post-only: a crossing order is canceled instead of matching (docs);
    /// the refusal carries the bbo in the venue's own string.
    Alo,
}

impl Tif {
    /// The venue's wire spelling (docs: `"Alo" | "Ioc" | "Gtc"`; an unknown
    /// one is `None`, refused by name at the caller).
    pub(crate) fn from_wire(tif: &str) -> Option<Tif> {
        match tif {
            "Ioc" => Some(Tif::Ioc),
            "Gtc" => Some(Tif::Gtc),
            "Alo" => Some(Tif::Alo),
            _ => None,
        }
    }

    /// The CoreWriter byte (docs, action 1: `1` Alo, `2` Gtc, `3` Ioc).
    pub(crate) fn from_corewriter(tif: u8) -> Option<Tif> {
        match tif {
            1 => Some(Tif::Alo),
            2 => Some(Tif::Gtc),
            3 => Some(Tif::Ioc),
            _ => None,
        }
    }
}

/// A trigger order's parameters (docs: `t.trigger`).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Trigger {
    pub is_market: bool,
    pub trigger_px: Decimal,
    pub tpsl: Tpsl,
}

/// Take-profit / stop-loss (docs).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Tpsl {
    Tp,
    Sl,
}

/// How a cancel is requested: the terminal status to record, and whether the
/// wire's `fast` flag rode it — the venue rejects a fast cancel that refers
/// to a trigger order (docs), checked where the order is known.
#[derive(Clone, Copy)]
struct CancelMode {
    status: TerminalStatus,
    speed: CancelSpeed,
}

/// The wire's `fast` flag, named (docs: a fast cancel must not refer to a
/// trigger order).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelSpeed {
    Normal,
    Fast,
}

impl CancelSpeed {
    /// The wire's `fast` flag (the venue's own shape), converted once here.
    pub(crate) fn of(fast: bool) -> Self {
        if fast {
            CancelSpeed::Fast
        } else {
            CancelSpeed::Normal
        }
    }
}

/// Whether an order may only reduce the position (the wire's `reduce_only`,
/// named): a reduce-only order frees margin instead of using it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reduce {
    Only,
    Any,
}

/// Which way an order goes: the sim's own vocabulary. The wire keeps the
/// venue's `is_buy` boolean (what daloc sends); [`Side::of`] is the one
/// conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Side {
    Buy,
    Sell,
}

impl<'de> Deserialize<'de> for Side {
    /// Reads both spellings: the wire's `is_buy` flag — the compact payloads
    /// and the pre-enum state files — and the enum's own snake_case name.
    /// A persisted state outlives a code change; the deserializer migrates it.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Flag(bool),
            Named(String),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Flag(true) => Ok(Side::Buy),
            Wire::Flag(false) => Ok(Side::Sell),
            Wire::Named(name) => match name.as_str() {
                "buy" => Ok(Side::Buy),
                "sell" => Ok(Side::Sell),
                other => Err(serde::de::Error::custom(format!("unknown side {other:?}"))),
            },
        }
    }
}

impl Side {
    /// The wire's `is_buy` flag, converted once at the boundary.
    pub(crate) fn of(is_buy: bool) -> Self {
        if is_buy {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    /// The wire's `is_buy` flag (the venue's own shape).
    pub(crate) fn is_buy(self) -> bool {
        matches!(self, Side::Buy)
    }

    /// The opposite side (a close takes the side the position does not hold).
    pub(crate) fn flip(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    /// The sign a fill of this side carries on the position (+1 buy, -1 sell).
    pub(crate) fn sign(self) -> Decimal {
        match self {
            Side::Buy => Decimal::ONE,
            Side::Sell => -Decimal::ONE,
        }
    }

    /// The book side index the served depth uses: bids 0, asks 1 (the venue's
    /// own l2Book order) — a buy consumes the asks, a sell the bids.
    pub(crate) fn book_index(self) -> usize {
        match self {
            Side::Buy => 1,
            Side::Sell => 0,
        }
    }
}

/// Move a finished order into the bounded terminal history (`orderStatus`
/// answers for filled and canceled orders, as the venue's does).
fn terminalize(
    terminal: &mut std::collections::BTreeMap<u64, TerminalOrder>,
    order: RestingOrder,
    status: TerminalStatus,
    filled_sz: Decimal,
) {
    let avg = super::account::ratio_or(order.filled_ntl, order.filled_sz, Decimal::ZERO);
    let oid = order.oid;
    terminal.insert(
        oid,
        TerminalOrder {
            order,
            status,
            filled_sz,
            avg_px: avg,
            status_ms: crate::now_ms(),
        },
    );
    while terminal.len() > 1024 {
        let Some(first) = terminal.keys().next().copied() else {
            break;
        };
        terminal.remove(&first);
    }
}

/// The docs' leverage constraints: "Leverage can be set by a user to any
/// integer between 1 and the max leverage" (margining); the exchange
/// endpoint's updateLeverage is "subject to leverage constraints on that
/// coin"; a strict-isolated or no-cross asset "does not allow cross margin".
/// One position's leverage-mode switch: the leverage fields and the initial
/// margin, then the margin moving across the boundary — converting to
/// isolated parks it on the position (an isolated position always carries its
/// margin: the waterfall's per-leg check reads it), converting back returns
/// it to the cross balance.
fn switch_position_mode(
    balance: &mut Decimal,
    p: &mut super::account::Position,
    mode: crate::wire::LeverageType,
    leverage: u32,
) -> Result<()> {
    let was_isolated = p.leverage == crate::wire::LeverageType::Isolated;
    let initial = super::account::div(
        super::account::mul(p.szi.abs(), p.entry_px, "margin notional")?,
        Decimal::from(leverage),
        "margin notional",
    )?;
    p.leverage = mode;
    p.leverage_value = leverage;
    p.margin_used = initial;
    if matches!(mode, crate::wire::LeverageType::Cross) {
        *balance = super::account::add(
            *balance,
            std::mem::take(&mut p.isolated_margin),
            "isolated margin return",
        )?;
    } else if !was_isolated {
        *balance = super::account::sub(*balance, initial, "isolated margin allocation")?;
        p.isolated_margin = initial;
    }
    Ok(())
}

fn validate_leverage(
    coin: &str,
    mode: crate::wire::LeverageType,
    leverage: u32,
    meta: Option<&crate::wire::Meta>,
) -> Result<()> {
    if leverage == 0 {
        return Err(Error::bad_request("leverage must be at least 1"));
    }
    if let Some(max) = super::account::max_leverage(coin, meta) {
        if leverage > max {
            return Err(Error::bad_request(format!(
                "leverage {leverage} exceeds {coin}'s max leverage {max}"
            )));
        }
    }
    if matches!(mode, crate::wire::LeverageType::Cross)
        && super::account::isolated_only(coin, meta)
    {
        return Err(Error::bad_request(format!(
            "{coin} does not allow cross margin (docs: margin modes)"
        )));
    }
    Ok(())
}

/// One order's initial margin requirement: notional over the account's
/// leverage, the notional at the *mark* (docs, margining: "The margin
/// required to open a position is `position_size * mark_price / leverage`";
/// robust price indices: mark "is used for margining, liquidations,
/// triggering TP/SL, and computing unrealized pnl"). Reduce-only orders free
/// margin instead of using it.
pub(crate) fn order_requirement(sz: Decimal, px: Decimal, leverage: u32, reduce: Reduce) -> Result<Decimal> {
    if matches!(reduce, Reduce::Only) {
        return Ok(Decimal::ZERO);
    }
    Ok(super::account::mul(sz, px, "order notional")? / Decimal::from(leverage.max(1)))
}

/// The venue's base fee tier (docs, fees page: 4.5 bps taker, 1.5 bps maker).
/// Volume-based tier discounts are not modeled — the sim serves the base tier,
/// as a flat account would see.
pub(crate) fn taker_fee() -> Decimal {
    Decimal::new(45, 5)
}

pub(crate) fn maker_fee() -> Decimal {
    Decimal::new(15, 5)
}

/// A resting order (the devnet's own book): the venue's openOrders row plus
/// the account that placed it. Persisted with the scenario — a restart keeps
/// the book.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RestingOrder {
    pub oid: u64,
    pub account: Address,
    pub coin: String,
    /// The side (`isBuy`/`is_buy` in pre-enum state files; the aliases and
    /// the tolerant `Side` deserializer migrate them).
    #[serde(alias = "isBuy", alias = "is_buy")]
    pub side: Side,
    /// The limit price (a resting order always has one).
    pub px: Decimal,
    /// The remaining size.
    pub sz: Decimal,
    pub orig_sz: Decimal,
    pub reduce_only: bool,
    pub tif: Tif,
    #[serde(default)]
    pub cloid: Option<alloy_primitives::B128>,
    #[serde(default)]
    pub trigger: Option<Trigger>,
    pub placed_ms: u64,
    /// What has filled so far (the venue reports a partial fill then the
    /// remainder's state in `orderStatus`).
    #[serde(default)]
    pub filled_sz: Decimal,
    #[serde(default)]
    pub filled_ntl: Decimal,
}

/// The venue's terminal order states (orderStatus): what happened to an order
/// that no longer rests.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TerminalStatus {
    Filled,
    Canceled,
    /// The venue's `marginCanceled`: the resting order died at match time
    /// because its account could not carry the fill.
    #[serde(rename = "marginCanceled")]
    MarginCanceled,
}

/// One terminal order (a bounded history so `orderStatus` can answer for a
/// filled/canceled order, as the venue does).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalOrder {
    pub order: RestingOrder,
    pub status: TerminalStatus,
    /// What actually filled (a partial then canceled, or the full size).
    pub filled_sz: Decimal,
    pub avg_px: Decimal,
    pub status_ms: u64,
}

/// What a fill took (the response shape both surfaces serve).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Fill {
    pub total_sz: Decimal,
    pub avg_px: Decimal,
    /// The levels consumed, as the feed recorded them (the ownership write).
    pub consumed: Vec<Consumption>,
    pub oid: u64,
}

/// What a placement produced: the venue's `filled` status if anything filled
/// (the remainder of a Gtc/Alo rests and shows in `openOrders`), else the
/// `resting` status.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Placed {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<Fill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resting: Option<u64>,
}

/// The resting-only outcome (a trigger armed, an ALO posted): no fill.
fn resting_only(oid: u64) -> Placed {
    Placed {
        fill: None,
        resting: Some(oid),
    }
}

impl Sim {
    /// `place`: the venue's order entry, end to end — validations, the
    /// immediate take against the served book *and* the devnet's own resting
    /// orders (fees on both sides), the remainder resting (Gtc/Alo), or the
    /// trigger armed (no walk; the tick fires it when the mark crosses).
    pub(crate) fn place(&self, feed: &MarketFeed, address: Address, req: &Order) -> Result<Placed> {
        let meta = feed.meta();
        validate_order(feed, meta.as_ref(), req)?;
        self.pre_place_refusals(feed, address, req, meta.as_ref())?;
        // A trigger arms instead of walking.
        if let Some(trigger) = req.trigger {
            if trigger.trigger_px <= Decimal::ZERO {
                return Err(Error::bad_request("Invalid TP/SL price."));
            }
            validate_trigger_side(feed, req, &trigger)?;
            let px = req.limit_px.unwrap_or(trigger.trigger_px);
            self.require_order_margin(feed, address, req, (px, req.size))?;
            let oid = self.rest_order(address, req, px, req.size)?;
            self.metrics
                .triggers_armed_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(account = %address, coin = %req.coin, oid, "trigger order armed");
            return Ok(resting_only(oid));
        }
        // Post-only: a crossing order is canceled instead of matching (docs).
        if req.tif == Tif::Alo {
            let oid = self.place_alo(feed, address, req)?;
            return Ok(resting_only(oid));
        }
        // The venue's self-trade rule: the taker's own crossing resting orders
        // are expired, not matched.
        self.expire_self_crossing(address, feed, req)?;
        // A Gtc/Alo that does not cross rests without a take (the venue's
        // behavior: nothing to match, so the order simply joins the book).
        let take = req.tif == Tif::Ioc || crosses(feed, req, &self.resting_orders(None)?);
        let fill = if take {
            Some(self.order(feed, address, req)?)
        } else {
            None
        };
        let remainder = req.size - fill.as_ref().map(|f| f.total_sz).unwrap_or(Decimal::ZERO);
        let resting = self.rest_remainder(feed, address, req, remainder)?;
        Ok(Placed { fill, resting })
    }

    /// The pre-place refusals (the open-interest cap and the margin-tier
    /// limit): each gate's reason is the venue's refusal envelope.
    fn pre_place_refusals(
        &self,
        feed: &MarketFeed,
        address: Address,
        req: &Order,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<()> {
        if let Some(reason) = self.open_order_limit_refusal(address)? {
            tracing::warn!(coin = %req.coin, "order refused: open-order limit");
            return Err(Error::bad_request(reason));
        }
        if let Some(reason) = self.oi_cap_refusal(feed, address, req)? {
            tracing::warn!(coin = %req.coin, "order refused: open interest cap");
            return Err(Error::bad_request(reason));
        }
        if let Some(reason) = self.tier_limit_refusal(feed, meta, address, req)? {
            tracing::warn!(coin = %req.coin, "order refused: margin tier limit");
            return Err(Error::bad_request(reason));
        }
        Ok(())
    }

    /// The match-time re-margin (plan §5): the maker's *current* available
    /// margin (value minus the positions' margin minus the account's other
    /// open orders' reservations) must cover the order's own requirement.
    pub(crate) fn remargin_ok(&self, feed: &MarketFeed, r: &RestingOrder) -> Result<bool> {
        let meta = feed.meta();
        let leverage = self.leverage_for(r.account, &r.coin, meta.as_ref())?;
        let requirement = order_requirement(
            r.sz,
            feed.mark_or(&r.coin, r.px),
            leverage,
            if r.reduce_only {
                Reduce::Only
            } else {
                Reduce::Any
            },
        )?;
        let Some(available) = self.available_for(feed, r.account, Some(r.oid), meta.as_ref())?
        else {
            return Ok(true); // no account state: nothing to re-margin against
        };
        Ok(available >= requirement)
    }

    /// A Gtc remainder rests (after the venue's order-margin check); anything
    /// else, or a filled order, rests nothing.
    fn rest_remainder(
        &self,
        feed: &MarketFeed,
        address: Address,
        req: &Order,
        remainder: Decimal,
    ) -> Result<Option<u64>> {
        if req.tif != Tif::Gtc || remainder <= Decimal::ZERO {
            return Ok(None);
        }
        let Some(px) = req.limit_px else {
            return Ok(None); // a market order cannot rest
        };
        self.require_order_margin(feed, address, req, (px, remainder))?;
        Ok(Some(self.rest_order(address, req, px, remainder)?))
    }

    /// The venue's order margin (docs: `availableToTrade` accounts for open
    /// orders): a resting order reserves its own initial margin at its price,
    /// and the placement is refused with the venue's own string when the
    /// account cannot carry it.
    pub(crate) fn require_order_margin(
        &self,
        feed: &MarketFeed,
        address: Address,
        req: &Order,
        resting_at: (Decimal, Decimal),
    ) -> Result<()> {
        if req.reduce_only {
            return Ok(());
        }
        let (px, sz) = resting_at;
        let meta = feed.meta();
        let leverage = self.leverage_for(address, &req.coin, meta.as_ref())?;
        let requirement = order_requirement(
            sz,
            feed.mark_or(&req.coin, px),
            leverage,
            Reduce::Any,
        )?;
        let Some(available) = self.available_for(feed, address, None, meta.as_ref())? else {
            return Ok(());
        };
        if available < requirement {
            return Err(Error::bad_request("Insufficient margin to place order."));
        }
        Ok(())
    }

    /// The account's free margin (the venue's `availableToTrade`): value minus
    /// the positions' margin minus the open orders' reservations at the marks.
    /// One home — the placement check, the match-time re-margin and the served
    /// `activeAssetData` read the same figure. `exclude_oid` skips one resting
    /// order (the re-margin's own); `None` = no account state.
    pub(crate) fn available_for(
        &self,
        feed: &MarketFeed,
        address: Address,
        exclude_oid: Option<u64>,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<Option<Decimal>> {
        let Some(account) = self.account(address)? else {
            return Ok(None);
        };
        let v = super::account::view(&account, feed, meta)?;
        let reserved = self.reserved_margin(address, exclude_oid, feed, meta)?;
        Ok(Some(super::account::sub(
            super::account::sub(
                v.margin_summary.account_value,
                v.margin_summary.total_margin_used,
                "available margin",
            )?,
            reserved,
            "available margin",
        )?))
    }

    /// The account's open orders' reserved margin (their notional over the
    /// account's leverage, at the *mark* — the margining price; a warming mark
    /// falls back to the order's own price; reduce-only orders reserve
    /// nothing). `exclude_oid` skips the order under check.
    pub(crate) fn reserved_margin(
        &self,
        account: Address,
        exclude_oid: Option<u64>,
        feed: &MarketFeed,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<Decimal> {
        let mut total = Decimal::ZERO;
        for r in self.resting_orders(Some(account))? {
            if Some(r.oid) == exclude_oid || r.reduce_only {
                continue;
            }
            let leverage = self.leverage_for(account, &r.coin, meta)?;
            let px = feed.mark_or(&r.coin, r.px);
            total = super::account::add(
                total,
                order_requirement(r.sz, px, leverage, Reduce::Any)?,
                "reserved margin",
            )?;
        }
        Ok(total)
    }

    /// A liquidation's market close: the same walk as an order, but with no
    /// fee (docs: "there is no clearance fee on liquidations") and no opening
    /// probe (the venue closes regardless of the resulting margin).
    pub(crate) fn liquidate_position(
        &self,
        feed: &MarketFeed,
        address: Address,
        coin: &str,
        size: Decimal,
    ) -> Result<Fill> {
        let meta = feed.meta();
        let long = self
            .account(address)?
            .and_then(|a| a.positions.get(coin).map(super::account::Position::is_long))
            .unwrap_or(true);
        let req = Order {
            coin: coin.to_string(),
            is_buy: Side::of(long).flip().is_buy(), // close the position
            size,
            limit_px: None, // market: whatever the book gives (partial fills ok)
            tif: Tif::Ioc,
            reduce_only: true,
            cloid: None,
            trigger: None,
        };
        // The venue's self-trade rule reaches this close too: the account's
        // own crossed resting orders are expired, not matched (the same
        // docs sentence; the close is a market order to the book).
        self.expire_self_crossing(address, feed, &req)?;
        let resting = self.resting_orders(None)?;
        let (legs, filled, avg_px) = walk_served_book(feed, meta.as_ref(), &req, &resting)?;
        let leverage = self.leverage_for(address, coin, meta.as_ref())?;
        let facts = super::account::FillFacts::close(coin, req.side(), filled, avg_px, leverage);
        self.with_account_now(address, feed, |account| {
            super::account::apply_fill(account, &facts, meta.as_ref())
        })?;
        self.settle_legs(feed, &req, &legs, meta.as_ref())?;
        let oid = self.next_oid()?;
        self.mark_dirty()?;
        Ok(Fill {
            total_sz: filled,
            avg_px,
            consumed: Vec::new(),
            oid,
        })
    }

    /// Drop the resting orders whose makers cannot carry their fill right
    /// now, canceling them (the venue's `marginCanceled` status).
    fn remargin_makers(
        &self,
        feed: &MarketFeed,
        req: &Order,
        resting: Vec<RestingOrder>,
    ) -> Result<Vec<RestingOrder>> {
        let mut kept = Vec::with_capacity(resting.len());
        for r in resting {
            let crosses = r.coin == req.coin
                && r.side != req.side()
                && r.trigger.is_none()
                && match req.limit_px {
                    None => true,
                    Some(limit) => match req.side() {
                        Side::Buy => r.px <= limit,
                        Side::Sell => r.px >= limit,
                    },
                };
            if !crosses {
                kept.push(r);
                continue;
            }
            if self.remargin_ok(feed, &r)? {
                kept.push(r);
            } else {
                tracing::warn!(account = %r.account, oid = r.oid, coin = %r.coin, "resting order canceled at match time (marginCanceled)");
                self.margin_cancel(r.account, &r.coin, r.oid)?;
            }
        }
        Ok(kept)
    }

    /// A post-only placement: refuses with the venue's own string (and the
    /// bbo) when it would cross, else rests.
    fn place_alo(&self, feed: &MarketFeed, address: Address, req: &Order) -> Result<u64> {
        let Some(px) = req.limit_px else {
            return Err(Error::bad_request(
                "Post only order needs a limit price (a market order cannot rest).",
            ));
        };
        let resting = self.resting_orders(Some(address))?;
        if crosses(feed, req, &resting) {
            let bbo = bbo(feed, &req.coin, req.side())
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".into());
            tracing::warn!(coin = %req.coin, bbo = %bbo, "post-only refused");
            return Err(Error::bad_request(format!(
                "Post only order would have immediately matched, bbo was {bbo}."
            )));
        }
        self.rest_order(address, req, px, req.size)
    }

    /// `order`: the immediate take — fill `req` for `address` against the
    /// served book plus the devnet's resting orders. The taker pays the taker
    /// fee; each maker's account is settled at their own price with the maker
    /// fee (no opening probe: the venue reserves margin at placement).
    pub(crate) fn order(&self, feed: &MarketFeed, address: Address, req: &Order) -> Result<Fill> {
        let meta = feed.meta();
        let resting = self.resting_orders(None)?;
        // The plan's rule: resting orders are re-margined at match time (the
        // oracle moves after placement). A maker whose account cannot carry
        // the fill is canceled (the venue's `marginCanceled`), never filled.
        let resting = self.remargin_makers(feed, req, resting)?;
        let (legs, filled, avg_px) = walk_served_book(feed, meta.as_ref(), req, &resting)?;
        let leverage = self.leverage_for(address, &req.coin, meta.as_ref())?;
        let notional = super::account::mul(filled, avg_px, super::account::FILL_NOTIONAL)?;
        let facts = super::account::FillFacts {
            coin: req.coin.clone(),
            side: req.side(),
            size: filled,
            px: avg_px,
            leverage,
            reduce_only: req.reduce_only,
            fee: super::account::mul(notional, taker_fee(), "taker fee")?,
        };
        self.land_fill(feed, address, &facts, meta.as_ref())?;
        let consumed = self.settle_legs(feed, req, &legs, meta.as_ref())?;
        let oid = self.next_oid()?;
        self.metrics
            .orders_filled_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.metrics
            .fees_charged_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.mark_dirty()?;
        Ok(Fill {
            total_sz: filled,
            avg_px,
            consumed,
            oid,
        })
    }

    /// Settle the walk's legs: live levels are consumed (the impact lane);
    /// makers are paid their fill and their resting size reduced. Returns the
    /// live consumptions (the response's `consumed`).
    fn settle_legs(
        &self,
        feed: &MarketFeed,
        req: &Order,
        legs: &[Candidate],
        meta: Option<&crate::wire::Meta>,
    ) -> Result<Vec<Consumption>> {
        let mut consumed = Vec::new();
        for leg in legs {
            match leg {
                Candidate::Live(c) => {
                    feed.consume(c.clone())?;
                    consumed.push(c.clone());
                }
                Candidate::Maker { oid, maker, px, sz } => {
                    let leverage = self.leverage_for(*maker, &req.coin, meta)?;
                    let notional = super::account::mul(*sz, *px, "maker notional")?;
                    let facts = super::account::FillFacts {
                        coin: req.coin.clone(),
                        side: req.side().flip(),
                        size: *sz,
                        px: *px,
                        leverage,
                        reduce_only: self.resting_flag(*oid)?,
                        fee: super::account::mul(notional, maker_fee(), "maker fee")?,
                    };
                    self.with_account_now(*maker, feed, |account| {
                        super::account::apply_fill(account, &facts, meta)
                    })?;
                    self.fill_resting(*oid, *sz, *px)?;
                    tracing::info!(maker = %maker, oid, "resting order filled");
                }
            }
        }
        Ok(consumed)
    }

    /// Reduce a resting order by a fill; a fully filled order moves to the
    /// terminal history (`orderStatus` reports it, as the venue does).
    fn fill_resting(&self, oid: u64, sz: Decimal, px: Decimal) -> Result<()> {
        self.with_state(|st| {
            let Some(order) = st.resting.get_mut(&oid) else {
                return Ok(());
            };
            order.sz = order.sz.saturating_sub(sz);
            order.filled_sz = super::account::add(order.filled_sz, sz, "filled size")?;
            order.filled_ntl = super::account::add(
                order.filled_ntl,
                super::account::mul(sz, px, "filled notional")?,
                "filled notional",
            )?;
            if order.sz > Decimal::ZERO {
                return Ok(());
            }
            let Some(done) = st.resting.remove(&oid) else {
                return Ok(());
            };
            terminalize(&mut st.terminal, done, TerminalStatus::Filled, sz);
            Ok(())
        })
    }

    /// The taker's own crossing resting orders are expired (the venue's
    /// self-trade rule: expire-maker, never a self-match).
    fn expire_self_crossing(&self, account: Address, feed: &MarketFeed, req: &Order) -> Result<()> {
        let mine = self.resting_orders(Some(account))?;
        for r in mine {
            if r.coin != req.coin || r.side == req.side() || r.trigger.is_some() {
                continue;
            }
            let crosses = match req.limit_px {
                None => true,
                Some(limit) => match req.side() {
                    Side::Buy => r.px <= limit,
                    Side::Sell => r.px >= limit,
                },
            };
            if crosses {
                let _ = feed; // (the feed is the caller's context; no read needed)
                tracing::info!(account = %account, oid = r.oid, "self-crossing resting order expired (expire-maker)");
                self.cancel(account, &r.coin, r.oid, CancelSpeed::Normal)?;
            }
        }
        Ok(())
    }

    /// The taker's own resting flag (for a maker fill's reduce-only semantics).
    fn resting_flag(&self, oid: u64) -> Result<bool> {
        Ok(self
            .state
            .lock()?
            .resting
            .get(&oid)
            .map(|r| r.reduce_only)
            .unwrap_or(false))
    }

    /// `updateLeverage`: the account's per-coin setting (docs: "subject to
    /// leverage constraints on that coin") — applied to an open position
    /// immediately, and kept for the next one.
    pub(crate) fn set_leverage(
        &self,
        address: Address,
        coin: &str,
        setting: super::account::LeverageSetting,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<()> {
        let super::account::LeverageSetting { mode, leverage } = setting;
        validate_leverage(coin, mode, leverage, meta)?;
        self.with_account(address, |account| {
            account.leverage_settings.insert(
                coin.to_string(),
                super::account::LeverageSetting { mode, leverage },
            );
            if let Some(p) = account.positions.get_mut(coin) {
                switch_position_mode(&mut account.perp_balance, p, mode, leverage)?;
            }
            Ok(())
        })
    }

    /// The venue's margin-tier cap (docs' string): at the position's current
    /// leverage the resulting notional must stay below the next tier down's
    /// lower bound.
    fn tier_limit_refusal(
        &self,
        feed: &MarketFeed,
        meta: Option<&crate::wire::Meta>,
        address: Address,
        req: &Order,
    ) -> Result<Option<&'static str>> {
        let Some(table) = meta.and_then(|m| m.margin_table(&req.coin)) else {
            return Ok(None);
        };
        let leverage = self.leverage_for(address, &req.coin, meta)?;
        let bound = table
            .margin_tiers
            .iter()
            .filter(|t| t.max_leverage < leverage)
            .map(|t| t.lower_bound)
            .min();
        let Some(bound) = bound else {
            return Ok(None);
        };
        let old = self
            .account(address)?
            .and_then(|a| a.positions.get(&req.coin).map(|p| p.szi.abs()))
            .unwrap_or(Decimal::ZERO);
        let Some(mark) = feed.mark(&req.coin) else {
            return Ok(None);
        };
        let resulting = super::account::mul(old.saturating_add(req.size), mark, "tier notional")?;
        Ok((resulting > bound).then_some(
            "Order would cause position to exceed margin tier limit at current leverage",
        ))
    }

    /// The venue's OI-cap refusal: a coin at its cap accepts only orders that
    /// reduce the position (plan §5's own string).
    /// The docs' open-order limit (the rate-limits page: "Each user has a
    /// default open order limit of 1000 plus one additional order for every
    /// 5M USDC of volume, capped at a total of 5000 open orders"): the base
    /// 1000 is enforced; the volume bonus is not modeled (a named
    /// simplification — without it the limit subsumes the page's rule that a
    /// reduce-only or trigger order is rejected once 1000 others are open).
    fn open_order_limit_refusal(&self, address: Address) -> Result<Option<String>> {
        let open = self.resting_orders(Some(address))?.len();
        if open >= OPEN_ORDER_LIMIT {
            return Ok(Some(format!(
                "venue-sim: the open-order limit is {OPEN_ORDER_LIMIT} (docs: the default limit; the volume bonus is not modeled). See docs/INTEGRATION.md."
            )));
        }
        Ok(None)
    }

    fn oi_cap_refusal(
        &self,
        feed: &MarketFeed,
        address: Address,
        req: &Order,
    ) -> Result<Option<&'static str>> {
        if !feed.open_interest_caps().iter().any(|c| c == &req.coin) {
            return Ok(None);
        }
        let old = self
            .account(address)?
            .and_then(|a| a.positions.get(&req.coin).map(|p| p.szi))
            .unwrap_or(Decimal::ZERO);
        let signed = req.size * req.side().sign();
        let increases =
            super::account::same_direction(old, signed);
        if !increases {
            return Ok(None);
        }
        // A price more aggressive than the oracle gets the docs' own string.
        let aggressive = req
            .limit_px
            .zip(feed.oracle(&req.coin))
            .is_some_and(|(px, oracle)| match req.side() {
                Side::Buy => px > oracle,
                Side::Sell => px < oracle,
            });
        Ok(Some(if aggressive {
            "Order rejected due to price more aggressive than oracle while at open interest cap"
        } else {
            "Order would increase open interest while open interest is capped"
        }))
    }

    /// The tick's passive fill (`fill_resting`, one implementation).
    pub(crate) fn fill_resting_pub(&self, oid: u64, sz: Decimal, px: Decimal) -> Result<()> {
        self.fill_resting(oid, sz, px)
    }

    /// Every resting order, or one account's (`openOrders`).
    pub(crate) fn resting_orders(&self, account: Option<Address>) -> Result<Vec<RestingOrder>> {
        self.with_state(|st| {
            Ok(st
                .resting
                .values()
                .filter(|r| account.is_none_or(|a| r.account == a))
                .cloned()
                .collect())
        })
    }

    /// Insert a resting order under a fresh oid.
    fn rest_order(&self, account: Address, req: &Order, px: Decimal, sz: Decimal) -> Result<u64> {
        let oid = self.next_oid()?;
        let order = RestingOrder {
            oid,
            account,
            coin: req.coin.clone(),
            side: req.side(),
            px,
            sz,
            orig_sz: sz,
            reduce_only: req.reduce_only,
            tif: req.tif,
            cloid: req.cloid,
            trigger: req.trigger,
            placed_ms: crate::now_ms(),
            filled_sz: Decimal::ZERO,
            filled_ntl: Decimal::ZERO,
        };
        self.mutate_state(|st| {
            st.resting.insert(oid, order);
            Ok(())
        })?;
        tracing::info!(account = %account, coin = %req.coin, oid, px = %px, sz = %sz, "order resting");
        Ok(oid)
    }

    /// `cancel`: the venue's own refusal when the order is unknown to the
    /// caller (`Order was never placed, already canceled, or filled.`).
    /// A cancel; `fast` is the wire flag — the venue rejects a fast cancel
    /// that refers to a trigger order (docs), so the flag is checked where
    /// the order is known.
    pub(crate) fn cancel(&self, account: Address, coin: &str, oid: u64, speed: CancelSpeed) -> Result<()> {
        self.cancel_with(
            account,
            coin,
            oid,
            CancelMode {
                status: TerminalStatus::Canceled,
                speed,
            },
        )
    }

    /// The match-time margin cancel (the venue's `marginCanceled` status).
    pub(crate) fn margin_cancel(&self, account: Address, coin: &str, oid: u64) -> Result<()> {
        self.cancel_with(
            account,
            coin,
            oid,
            CancelMode {
                status: TerminalStatus::MarginCanceled,
                speed: CancelSpeed::Normal,
            },
        )
    }

    /// The cancel's one home: move the resting order to the terminal history
    /// under the given status.
    fn cancel_with(
        &self,
        account: Address,
        coin: &str,
        oid: u64,
        mode: CancelMode,
    ) -> Result<()> {
        let CancelMode { status, speed } = mode;
        self.with_state(|st| {
            match st.resting.get(&oid) {
                Some(r) if r.account == account && r.coin == coin => {
                    if matches!(speed, CancelSpeed::Fast) && r.trigger.is_some() {
                        return Err(Error::bad_request(
                            "Fast cancel cannot cancel a trigger order.",
                        ));
                    }
                    let Some(done) = st.resting.remove(&oid) else {
                        return Ok(());
                    };
                    st.terminal.insert(
                        oid,
                        TerminalOrder {
                            order: done,
                            status,
                            filled_sz: Decimal::ZERO,
                            avg_px: Decimal::ZERO,
                            status_ms: crate::now_ms(),
                        },
                    );
                    Ok(())
                }
                _ => Err(Error::bad_request(
                    "Order was never placed, already canceled, or filled.",
                )),
            }
        })?;
        self.mark_dirty()?;
        Ok(())
    }

    /// `cancelByCloid`: the same, addressed by the client id.
    pub(crate) fn cancel_by_cloid(
        &self,
        account: Address,
        coin: &str,
        cloid: alloy_primitives::B128,
        speed: CancelSpeed,
    ) -> Result<()> {
        let oid = self.with_state(|st| {
            Ok(st
                .resting
                .values()
                .find(|r| r.account == account && r.coin == coin && r.cloid == Some(cloid))
                .map(|r| r.oid))
        })?;
        match oid {
            Some(oid) => self.cancel(account, coin, oid, speed),
            None => Err(Error::bad_request(
                "Order was never placed, already canceled, or filled.",
            )),
        }
    }

    /// The resting order a status read may find (open), or its terminal
    /// record (filled/canceled).
    pub(crate) fn resting_by_oid(&self, account: Address, oid: u64) -> Result<Option<RestingOrder>> {
        self.with_state(|st| {
            Ok(st
                .resting
                .get(&oid)
                .filter(|r| r.account == account)
                .cloned())
        })
    }

    pub(crate) fn terminal_by_oid(&self, account: Address, oid: u64) -> Result<Option<TerminalOrder>> {
        self.with_state(|st| {
            Ok(st
                .terminal
                .get(&oid)
                .filter(|t| t.order.account == account)
                .cloned())
        })
    }

    pub(crate) fn resting_by_cloid(
        &self,
        account: Address,
        cloid: alloy_primitives::B128,
    ) -> Result<Option<RestingOrder>> {
        self.with_state(|st| {
            Ok(st
                .resting
                .values()
                .find(|r| r.account == account && r.cloid == Some(cloid))
                .cloned())
        })
    }

    pub(crate) fn terminal_by_cloid(
        &self,
        account: Address,
        cloid: alloy_primitives::B128,
    ) -> Result<Option<TerminalOrder>> {
        self.with_state(|st| {
            Ok(st
                .terminal
                .values()
                .find(|t| t.order.account == account && t.order.cloid == Some(cloid))
                .cloned())
        })
    }

    /// `scheduleCancel`: a dead-man's switch — at `time` (ms) every resting
    /// order of the account is canceled (the tick enforces it).
    pub(crate) fn schedule_cancel(&self, account: Address, time_ms: Option<u64>) -> Result<u64> {
        let now = crate::now_ms();
        let t = self.mutate_state(|st| match time_ms {
            None => {
                st.scheduled_cancels.remove(&account);
                Ok(0)
            }
            Some(t) => {
                if t < now + 5_000 {
                    return Err(Error::bad_request(
                        "Scheduled cancel time must be at least 5 seconds in the future.",
                    ));
                }
                // The venue caps scheduled-cancel triggers at 10 per UTC day
                // (docs; the count increments when one fires, reset at 00:00
                // UTC — the day of the *fire* is what counts, so the cap is
                // checked against today's).
                let day = now / 86_400_000;
                let fires = st
                    .accounts
                    .get(&account)
                    .map(|a| if a.schedule_fire_day == day { a.schedule_fires } else { 0 })
                    .unwrap_or(0);
                if fires >= 10 {
                    return Err(Error::bad_request(
                        "The maximum number of scheduled cancel triggers per day is 10.",
                    ));
                }
                st.scheduled_cancels.insert(account, t);
                Ok(t)
            }
        })?;
        Ok(t)
    }

    /// The fill's landing: the opening check runs on a clone, so a refused
    /// order leaves no trace (all-or-nothing).
    fn land_fill(
        &self,
        feed: &MarketFeed,
        address: Address,
        facts: &super::account::FillFacts,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<()> {
        let coin = facts.coin.clone();
        // The venue settles funding at the trade: accrue to now on the
        // *pre-fill* size, then the size change restarts since-change. The
        // all-or-nothing accrual leaves the stamp where it is when a position
        // lacks a live context (the shared rule).
        self.with_account_now(address, feed, |account| {
            let mut probe = account.clone();
            super::account::apply_fill(&mut probe, facts, meta)?;
            let view = super::account::view(&probe, feed, meta)?;
            if view.margin_summary.account_value < view.margin_summary.total_margin_used {
                // The venue's own string; the numbers ride the log (a client
                // matching on the vocabulary sees exactly the venue's).
                tracing::warn!(
                    account = %address,
                    coin,
                    needs = %view.margin_summary.total_margin_used,
                    worth = %view.margin_summary.account_value,
                    "order refused: insufficient margin"
                );
                return Err(Error::bad_request("Insufficient margin to place order."));
            }
            super::account::apply_fill(account, facts, meta)
        })
    }

    /// The leverage a fill uses: the position's own, else the asset's max
    /// leverage (the venue's default for a new position), else 20 — the
    /// measured no-position default the venue itself serves (`activeAssetData`
    /// for a flat account; §16.5's probe).
    pub(crate) fn leverage_for(
        &self,
        address: Address,
        coin: &str,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<u32> {
        if let Some(p) = self
            .state
            .lock()?
            .accounts
            .get(&address)
            .and_then(|a| a.positions.get(coin))
        {
            return Ok(p.leverage_value);
        }
        if let Some(s) = self
            .state
            .lock()?
            .accounts
            .get(&address)
            .and_then(|a| a.leverage_settings.get(coin))
        {
            return Ok(s.leverage.max(1));
        }
        Ok(meta
            .and_then(|m| m.asset(coin))
            .map(|a| a.max_leverage)
            .filter(|l| *l > 0)
            .unwrap_or(20))
    }

    /// The next order id (the venue's own numbering; monotonic across the
    /// scenario, persisted with the state).
    fn next_oid(&self) -> Result<u64> {
        self.with_state(|state| {
            state.next_oid = state.next_oid.saturating_add(1);
            Ok(state.next_oid)
        })
    }
}

/// One candidate the taker's walk can take: a live level (external liquidity,
/// recorded as a consumption) or a devnet resting order (an internal trade
/// that updates its maker's account).
#[derive(Debug, Clone)]
pub(crate) enum Candidate {
    Live(Consumption),
    Maker { oid: u64, maker: Address, px: Decimal, sz: Decimal },
}

/// The best opposite price the served book shows (the `bbo` in the venue's
/// post-only refusal): the cheapest ask for a buy, the richest bid for a sell.
pub(crate) fn bbo(feed: &MarketFeed, coin: &str, side: Side) -> Option<Decimal> {
    let book = feed.book(coin, crate::now_ms())?;
    book.levels[side.book_index()].first().map(|l| l.px)
}

/// Whether `req` would cross anything the served book (live levels *and*
/// devnet resting orders) currently offers.
pub(crate) fn crosses(
    feed: &MarketFeed,
    req: &Order,
    resting: &[RestingOrder],
) -> bool {
    let Some(limit) = req.limit_px else {
        return true; // a market order always crosses
    };
    if let Some(book) = feed.book(&req.coin, crate::now_ms()) {
        let side = req.side().book_index();
        if let Some(level) = book.levels[side].first() {
            let crosses = match req.side() {
                Side::Buy => level.px <= limit,
                Side::Sell => level.px >= limit,
            };
            if crosses {
                return true;
            }
        }
    }
    resting.iter().any(|r| {
        r.coin == req.coin && r.side != req.side() && {
            match req.side() {
                Side::Buy => r.px <= limit,
                Side::Sell => r.px >= limit,
            }
        }
    })
}

/// Walk the served book (the live depth minus what the devnet already
/// consumed) and validate the request: positive size, real depth within the
/// limit (an `Ioc`; a resting tif takes the rest path instead).
fn walk_served_book(
    feed: &MarketFeed,
    meta: Option<&crate::wire::Meta>,
    req: &Order,
    resting: &[RestingOrder],
) -> Result<(Vec<Candidate>, Decimal, Decimal)> {
    validate_order(feed, meta, req)?;
    let now = crate::now_ms();
    let book = feed.book(&req.coin, now).ok_or(Error::Warming)?;
    let (legs, filled, avg_px) = walk(&book, req, resting)?;
    if filled <= Decimal::ZERO {
        // The venue's two depth refusals, by order type: a limit that cannot
        // match, and a market order with no liquidity.
        return Err(Error::bad_request(match req.limit_px {
            Some(_) => "Order could not immediately match against any resting orders.",
            None => "No liquidity available for market order.",
        }));
    }
    Ok((legs, filled, avg_px))
}

/// The merged opposite side, price-ordered: (px, is_maker, oid, maker, sz).
/// At the same price the devnet's own resting orders go first (they are
/// time-stamped; the live level's age is unknowable).
fn merge_candidates(
    book: &L2Book,
    req: &Order,
    resting: &[RestingOrder],
) -> Vec<(Decimal, bool, u64, Address, Decimal)> {
    let side = req.side().book_index();
    let mut candidates: Vec<(Decimal, bool, u64, Address, Decimal)> = book.levels[side]
        .iter()
        .map(|l| (l.px, false, 0u64, Address::ZERO, l.sz))
        .collect();
    for r in resting
        .iter()
        .filter(|r| r.coin == book.coin && r.side != req.side() && r.trigger.is_none())
    {
        candidates.push((r.px, true, r.oid, r.account, r.sz));
    }
    candidates.sort_by(|a, b| {
        let by_px = match req.side() {
            Side::Buy => a.0.cmp(&b.0),
            Side::Sell => b.0.cmp(&a.0),
        };
        by_px.then_with(|| b.1.cmp(&a.1))
    });
    candidates
}

/// Walk a book in price order: asks ascending for a buy, bids descending for
/// a sell; the limit stops the walk. Returns (the consumed candidates, the
/// filled size, the size-weighted average).
fn walk(book: &L2Book, req: &Order, resting: &[RestingOrder]) -> Result<(Vec<Candidate>, Decimal, Decimal)> {
    let candidates = merge_candidates(book, req, resting);
    let mut remaining = req.size;
    let mut notional = Decimal::ZERO;
    let mut legs = Vec::new();
    for (px, is_maker, oid, maker, sz) in candidates {
        if remaining <= Decimal::ZERO {
            break;
        }
        if let Some(limit) = req.limit_px {
            let within = match req.side() {
                Side::Buy => px <= limit,
                Side::Sell => px >= limit,
            };
            if !within {
                break;
            }
        }
        let take = remaining.min(sz);
        if take <= Decimal::ZERO {
            continue;
        }
        remaining -= take;
        notional = super::account::add(
            notional,
            super::account::mul(take, px, super::account::FILL_NOTIONAL)?,
            "fill total",
        )?;
        if is_maker {
            legs.push(Candidate::Maker { oid, maker, px, sz: take });
        } else {
            legs.push(Candidate::Live(Consumption {
                coin: book.coin.clone(),
                side: req.side().book_index() as u8,
                price: px,
                size: take,
            }));
        }
    }
    let filled = req.size - remaining;
    let avg_px = super::account::ratio_or(notional, filled, Decimal::ZERO);
    Ok((legs, filled, avg_px))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{meta_of, meta_with, Deal, DealClass};
    use crate::wire::{Level, Meta};

    /// A book with the given (bids, asks) as (px, sz) pairs.
    fn book(coin: &str, bids: &[(&str, &str)], asks: &[(&str, &str)]) -> L2Book {
        let level = |(px, sz): &(&str, &str)| Level {
            px: px.parse().unwrap(),
            sz: sz.parse().unwrap(),
            n: 1,
        };
        L2Book {
            coin: coin.into(),
            time: 0,
            levels: [
                bids.iter().map(level).collect(),
                asks.iter().map(level).collect(),
            ],
            spread: None,
        }
    }

    /// A feed serving the book with a live mark (the best ask, else 1) — the
    /// margin check values positions against it.
    fn feed_with_book(book: L2Book, meta: Meta) -> MarketFeed {
        let feed = MarketFeed::memory(vec!["ETH".to_string()]);
        // Honest stamps: the lane's freshness gate reads the ctx age, and a
        // fixture stamped at the epoch would read as permanently stale.
        let now = crate::now_ms();
        feed.put_meta(meta, now);
        let mark = book.levels[1]
            .first()
            .map(|l| l.px)
            .unwrap_or(Decimal::ONE);
        feed.put_ctx(
            "ETH".into(),
            crate::wire::test_ctx(&mark.to_string(), &mark.to_string()),
            now,
        );
        feed.put_book(book, now);
        feed
    }

    /// A sim with `amount` USDC dealt to a fresh account.
    fn funded_sim(feed: &MarketFeed, amount: &str) -> (Sim, Address) {
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        sim.deal(
            feed,
            &Deal {
                address: a,
                class: DealClass::Perp,
                coin: None,
                amount: amount.parse().unwrap(),
            },
        )
        .unwrap();
        (sim, a)
    }

    /// An IOC order on ETH.
    fn ioc(side: Side, size: &str) -> Order {
        Order {
            coin: "ETH".into(),
            is_buy: side.is_buy(),
            size: size.parse().unwrap(),
            limit_px: None,
            tif: Tif::Ioc,
            reduce_only: false,
            cloid: None,
            trigger: None,
        }
    }

    /// The same, reduce-only.
    fn reduce(side: Side, size: &str) -> Order {
        Order {
            reduce_only: true,
            ..ioc(side, size)
        }
    }

    /// An unknown coin refuses by name (404), never the warming shape: a
    /// name the meta does not carry cannot warm up (measured: it read as
    /// `503 warming up`, a transient shape that invites infinite retries). A
    /// *configured* market with no served data keeps the warming shape — the
    /// two faults stay distinct (the l2Book read's 200-null principle).
    #[test]
    fn an_unknown_coin_refuses_by_name_a_bare_market_still_warms() {
        let feed = feed_with_book(
            book("ETH", &[("100", "2")], &[("101", "2")]),
            meta_of(&[("ETH", 10), ("SOL", 10)]),
        );
        let (sim, a) = funded_sim(&feed, "1000");

        let err = sim
            .order(&feed, a, &Order { coin: "NOPE".into(), ..ioc(Side::Buy, "1") })
            .unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }), "{err:?}");

        let err = sim
            .order(&feed, a, &Order { coin: "SOL".into(), ..ioc(Side::Buy, "1") })
            .unwrap_err();
        assert!(matches!(err, Error::Warming), "{err:?}");
    }

    /// I3: the fill moves the book and the position, never the mark.
    #[test]
    fn a_fill_moves_the_book_never_the_mark() {
        let feed = feed_with_book(book("ETH", &[("100", "2")], &[("101", "2")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        let mark_before = feed.mark("ETH").unwrap();

        let fill = sim.order(&feed, a, &ioc(Side::Buy, "1.5")).unwrap();
        assert_eq!(fill.total_sz, "1.5".parse().unwrap());
        assert_eq!(fill.avg_px, "101".parse().unwrap());
        // I3: the mark is untouched.
        assert_eq!(feed.mark("ETH").unwrap(), mark_before);
        // The position moved: size and the size-weighted entry.
        let account = sim.account(a).unwrap().unwrap();
        let p = account.positions.get("ETH").unwrap();
        assert_eq!(p.szi, "1.5".parse().unwrap());
        assert_eq!(p.entry_px, "101".parse().unwrap());
        // The identity the client gates on stays exact.
        let served =
            crate::sim::account::clearinghouse(&account, &feed, Some(&meta_with("ETH", 10)), 0)
                .unwrap();
        assert!(
            crate::sim::account::identity_holds(&served),
            "the identity stays exact"
        );
    }

    /// I1: only the consumed levels are owned — a second order sees the first
    /// one's impact, and an untouched level keeps its live size.
    #[test]
    fn consumption_is_level_scoped_and_accumulates() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "1"), ("102", "1")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "10000");
        // Take the whole 101 ask; the 102 ask stays live.
        sim.order(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        let served = feed.book("ETH", 0).unwrap();
        assert_eq!(served.levels[1].len(), 1, "the consumed ask left the book");
        assert_eq!(served.levels[1][0].px, "102".parse().unwrap());
        assert_eq!(served.levels[0][0].sz, "5".parse().unwrap(), "the bid is live");
        assert_eq!(feed.owned_levels(), 1);
        // A second order walks into the next level.
        let fill = sim.order(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        assert_eq!(fill.avg_px, "102".parse().unwrap());
        assert_eq!(feed.owned_levels(), 2);
    }

    /// I2/I6: a live update refreshes the unowned levels, keeps the owned
    /// delta, and prunes the records the market moved past.
    #[test]
    fn live_updates_keep_the_delta_and_prune_stale_records() {
        let feed = feed_with_book(book("ETH", &[], &[("101", "1"), ("102", "1")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        // The live book refreshes 101 with a bigger size: the delta persists
        // (served = live − consumed) and the untouched 102 refreshes verbatim.
        feed.put_book(book("ETH", &[], &[("101", "3"), ("102", "7")]), 10);
        let served = feed.book("ETH", 0).unwrap();
        assert_eq!(served.levels[1][0].sz, "2".parse().unwrap(), "3 live − 1 consumed");
        assert_eq!(served.levels[1][1].sz, "7".parse().unwrap());
        assert_eq!(feed.owned_levels(), 1);
        // The market moves past 101: the record is pruned, never re-owned.
        feed.put_book(book("ETH", &[], &[("102", "7")]), 20);
        assert_eq!(feed.owned_levels(), 0);
        let served = feed.book("ETH", 0).unwrap();
        assert_eq!(served.levels[1][0].px, "102".parse().unwrap());
    }

    /// I4: the book hand-back restores the live book exactly.
    #[test]
    fn hand_back_restores_the_live_book() {
        let feed = feed_with_book(book("ETH", &[], &[("101", "2")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        assert_eq!(feed.book("ETH", 0).unwrap().levels[1][0].sz, "1".parse().unwrap());
        feed.clear_book("ETH").unwrap();
        assert_eq!(feed.book("ETH", 0).unwrap().levels[1][0].sz, "2".parse().unwrap());
        assert_eq!(feed.owned_levels(), 0);
    }

    /// A reducing fill realizes its PnL into the perp balance.
    #[test]
    fn a_reduce_realizes_pnl() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        // The test is the PnL identity, not funding: the fills settle funding
        // (a sub-nano term with the fixture's tiny rate) — pin it to zero so
        // the assertion is exact.
        feed.own_funding("ETH", Decimal::ZERO).unwrap();
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "2")).unwrap();
        let before = sim.account(a).unwrap().unwrap().perp_balance;
        // Sell 1 at 100: closed 1·(100 − 101) = −1.
        sim.order(&feed, a, &reduce(Side::Sell, "1")).unwrap();
        let after = sim.account(a).unwrap().unwrap().perp_balance;
        // Realized −1, minus the reduce's own taker fee (100 notional × 4.5 bps).
        let fee = Decimal::from(100) * taker_fee();
        assert_eq!(after, before - Decimal::ONE - fee, "realized −1 net of the fee");
        // The position is 1 long at 101, still.
        let account = sim.account(a).unwrap().unwrap();
        let p = account.positions.get("ETH").unwrap();
        assert_eq!(p.szi, Decimal::ONE);
        assert_eq!(p.entry_px, "101".parse().unwrap());
    }

    /// The limit stops the walk (both directions), and the depth check is by
    /// name.
    #[test]
    fn the_limit_stops_the_walk() {
        let feed = feed_with_book(book("ETH", &[("100", "1")], &[("101", "1")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        let mut o = ioc(Side::Buy, "1");
        o.limit_px = Some("100.5".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("could not immediately match"), "{err}");
        let mut o = ioc(Side::Sell, "1");
        o.limit_px = Some("100.5".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("could not immediately match"), "{err}");
    }

    /// A stale live book refuses by name; a scripted book for the same coin
    /// is the scenario's present and fills.
    #[test]
    fn a_stale_live_book_refuses_but_a_scripted_one_fills() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        // Age the live ctx past the shared window.
        let old = crate::now_ms() - MarketFeed::MAX_STALE_MS - 1_000;
        feed.put_ctx(
            "ETH".into(),
            crate::wire::test_ctx("100", "100"),
            old,
        );
        let err = sim.order(&feed, a, &ioc(Side::Buy, "0.1")).unwrap_err();
        assert!(format!("{err}").contains("stale"), "{err}");
        // The scenario scripts the coin: its present is never stale.
        feed.own_book(&book("ETH", &[("100", "5")], &[("101", "5")])).unwrap();
        let fill = sim.order(&feed, a, &ioc(Side::Buy, "0.1")).unwrap();
        assert_eq!(fill.total_sz, "0.1".parse().unwrap());
    }

    /// The venue settles funding at the trade: a fill accrues to now on the
    /// *pre-fill* size and restarts `cumFunding.sinceChange`.
    #[test]
    fn a_fill_settles_funding_and_restarts_since_change() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        // The shared fixture is rate-free (see `test_ctx`); this test is
        // about funding, so its ctx carries the published hourly rate.
        feed.put_ctx("ETH".into(), crate::wire::funding_ctx("101", "101"), crate::now_ms());
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "0.1")).unwrap();
        // An hour passes, then a second fill: the pre-fill size pays the
        // hour, and the change restarts since-change.
        sim.advance(&feed, 3600, 1).unwrap();
        sim.order(&feed, a, &ioc(Side::Buy, "0.1")).unwrap();
        let p = sim.account(a).unwrap().unwrap().positions.remove("ETH").unwrap();
        assert_ne!(p.funding_all_time, Decimal::ZERO, "the hour was paid");
        assert_eq!(
            p.funding_since_change,
            Decimal::ZERO,
            "the size change restarts since-change"
        );
        assert_ne!(p.funding_since_open, Decimal::ZERO, "since-open keeps accruing");
    }

    /// The venue's tick/lot rules and the $10 minimum refuse by name; an
    /// integer price of any width is always valid.
    #[test]
    fn tick_lot_and_minimum_refuse_by_name() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 4));
        let (sim, a) = funded_sim(&feed, "1000");
        // 5 decimals > szDecimals 4.
        let err = sim.order(&feed, a, &ioc(Side::Buy, "0.00001")).unwrap_err();
        assert!(format!("{err}").contains("szDecimals"), "{err}");
        // 3 decimals > 6 − 4: the venue's tick-size refusal.
        let mut o = ioc(Side::Buy, "1");
        o.limit_px = Some("100.123".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(
            format!("{err}").contains("divisible by tick size"),
            "{err}"
        );
        // 6 significant figures.
        let mut o = ioc(Side::Buy, "1");
        o.limit_px = Some("12345.6".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("significant"), "{err}");
        // 1 × 5 = 5 < the $10 minimum.
        let mut o = ioc(Side::Buy, "1");
        o.limit_px = Some("5".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("minimum"), "{err}");
        // 0.1 × 101 = 10.1 ≥ the minimum: valid.
        let mut o = ioc(Side::Buy, "0.1");
        o.limit_px = Some("101".parse().unwrap());
        assert!(sim.order(&feed, a, &o).is_ok(), "a valid order fills");
    }

    /// Every refusal is by name: resting orders, reduce-only that would not
    /// reduce, and an opening the account cannot margin.
    #[test]
    fn refusals_are_named() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        // Gtc below the market: nothing crosses, so the order rests (the
        // venue's own behavior; the remainder shows in openOrders).
        let mut gtc = ioc(Side::Buy, "0.2");
        gtc.tif = Tif::Gtc;
        gtc.limit_px = Some("99".parse().unwrap());
        let placed = sim.place(&feed, a, &gtc).unwrap();
        assert!(placed.resting.is_some(), "a Gtc order rests: {placed:?}");
        // Post-only above the market: the venue's own refusal, with the bbo.
        let mut alo = ioc(Side::Buy, "0.2");
        alo.tif = Tif::Alo;
        alo.limit_px = Some("102".parse().unwrap());
        let err = sim.place(&feed, a, &alo).unwrap_err();
        assert!(
            format!("{err}").contains("Post only order would have immediately matched"),
            "{err}"
        );
        // A reduce-only order with nothing to reduce.
        let err = sim.order(&feed, a, &reduce(Side::Sell, "1")).unwrap_err();
        assert!(
            format!("{err}").contains("Reduce only order would increase position"),
            "{err}"
        );
        // An unfunded opening: the numbers ride in the refusal.
        let b = Address::repeat_byte(0xbb);
        let err = sim.order(&feed, b, &ioc(Side::Buy, "5")).unwrap_err();
        assert!(
            format!("{err}").contains("Insufficient margin to place order."),
            "{err}"
        );
    }

    /// The venue's order margin: a resting order reserves its own initial
    /// margin at its price, and a placement the account cannot carry is
    /// refused with the venue's own string.
    #[test]
    fn a_resting_order_reserves_its_margin() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "5");
        // $500 notional at 10× needs $50: the thin account cannot rest it.
        let mut rest = ioc(Side::Buy, "5");
        rest.tif = Tif::Gtc;
        rest.limit_px = Some("100".parse().unwrap());
        let err = sim.place(&feed, a, &rest).unwrap_err();
        assert!(
            format!("{err}").contains("Insufficient margin to place order."),
            "{err}"
        );
        // A reduce-only order reserves nothing.
        let mut close = reduce(Side::Sell, "0.2");
        close.tif = Tif::Gtc;
        close.limit_px = Some("105".parse().unwrap());
        assert!(sim.place(&feed, a, &close).is_ok(), "reduce-only rests free");
    }

    /// The plan's rule: a resting order is re-margined at match time — a maker
    /// that cannot carry its fill is canceled (the venue's `marginCanceled`),
    /// never filled.
    #[test]
    fn a_breaching_maker_is_canceled_at_match_time() {
        // No bids in the served book: the maker's resting bid is the only
        // crossing liquidity, so its cancellation shows up as no fill.
        let feed = feed_with_book(book("ETH", &[], &[("101", "2")]), meta_with("ETH", 10));
        let (sim, maker) = funded_sim(&feed, "1000");
        let taker = Address::repeat_byte(0xbb);
        sim.deal(
            &feed,
            &Deal {
                address: taker,
                class: DealClass::Perp,
                coin: None,
                amount: "1000".parse().unwrap(),
            },
        )
        .unwrap();
        // $500 notional at 10× needs $50: affordable at placement.
        let mut rest = ioc(Side::Buy, "5");
        rest.tif = Tif::Gtc;
        rest.limit_px = Some("100".parse().unwrap());
        let oid = sim
            .place(&feed, maker, &rest)
            .unwrap()
            .resting
            .expect("a Gtc order rests");
        // The account drains (a loss, a withdrawal — the oracle moved): the
        // match-time re-margin must refuse the fill.
        sim.deal(
            &feed,
            &Deal {
                address: maker,
                class: DealClass::Perp,
                coin: None,
                amount: "-990".parse().unwrap(),
            },
        )
        .unwrap();
        // The taker crosses at the maker's price.
        let mut cross = ioc(Side::Sell, "1");
        cross.limit_px = Some("100".parse().unwrap());
        let err = sim.place(&feed, taker, &cross).unwrap_err();
        assert!(format!("{err}").contains("could not immediately match"), "{err}");
        assert!(
            sim.resting_orders(None).unwrap().iter().all(|r| r.oid != oid),
            "the breaching maker is off the book"
        );
        let st = sim.state.lock().unwrap();
        assert_eq!(
            st.terminal.get(&oid).map(|t| t.status),
            Some(TerminalStatus::MarginCanceled)
        );
    }

    /// The docs' self-trade rule: "Trades between the same address cancel
    /// the resting order instead of causing a fill. No fees are deducted,
    /// nor does the the cancel show up in the trade feed." The taker's own
    /// crossed order is expired and the walk continues against the
    /// liquidity behind it.
    #[test]
    fn a_self_crossing_resting_order_expires_and_the_walk_continues() {
        // The user's own bid at 100 sits above the served bid at 98.
        let feed = feed_with_book(
            book("ETH", &[("98", "5")], &[("101", "2")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let mut rest = ioc(Side::Buy, "1");
        rest.tif = Tif::Gtc;
        rest.limit_px = Some("100".parse().unwrap());
        let oid = sim.place(&feed, a, &rest).unwrap().resting.expect("a Gtc rests");
        // The same address sells through its own bid: expired, not filled;
        // the served bid behind it takes the fill.
        let sell = ioc(Side::Sell, "1");
        let fill = sim
            .place(&feed, a, &sell)
            .unwrap()
            .fill
            .expect("the served bid fills");
        assert_eq!(fill.total_sz, "1".parse().unwrap());
        assert_eq!(fill.avg_px, "98".parse().unwrap(), "behind the expired order");
        assert!(
            sim.resting_orders(None).unwrap().iter().all(|r| r.oid != oid),
            "the user's own order is off the book"
        );
        // Expired, not filled: the terminal history says canceled.
        let st = sim.state.lock().unwrap();
        assert_eq!(
            st.terminal.get(&oid).map(|t| t.status),
            Some(TerminalStatus::Canceled)
        );
    }

    /// The same docs sentence reaches the liquidation's close: the account's
    /// own crossed resting orders are expired, not matched.
    #[test]
    fn a_liquidation_expires_the_accounts_own_crossing_order() {
        // The account's own bid at 100 sits above the served bid at 98.
        let feed = feed_with_book(
            book("ETH", &[("98", "5")], &[("101", "2")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        // A long position (the served ask), then a resting bid of its own.
        let mut open = ioc(Side::Buy, "1");
        open.limit_px = Some("101".parse().unwrap());
        sim.place(&feed, a, &open).unwrap();
        let mut rest = ioc(Side::Buy, "1");
        rest.tif = Tif::Gtc;
        rest.limit_px = Some("100".parse().unwrap());
        let oid = sim.place(&feed, a, &rest).unwrap().resting.expect("a Gtc rests");
        // The close: the account's own bid must be expired; the served bid
        // behind it takes the fill.
        let fill = sim
            .liquidate_position(&feed, a, "ETH", "1".parse().unwrap())
            .unwrap();
        assert_eq!(fill.avg_px, "98".parse().unwrap(), "behind the expired order");
        assert!(
            sim.resting_orders(None).unwrap().iter().all(|r| r.oid != oid),
            "the account's own order is off the book"
        );
    }

    /// The docs' margining price: "The margin required to open a position is
    /// `position_size * mark_price / leverage`" — a resting order reserves at
    /// the *mark*, not at its own price (the fixture's mark is the best ask,
    /// 101). A deep bid whose own-price margin would fit is refused.
    #[test]
    fn the_order_margin_reserves_at_the_mark() {
        let feed = feed_with_book(
            book("ETH", &[("50", "50")], &[("101", "50")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "100");
        // 15 × 50 / 10 = 75 at its own price — fits; 15 × 101 / 10 = 151.5 at
        // the mark — refused.
        let mut deep = ioc(Side::Buy, "15");
        deep.tif = Tif::Gtc;
        deep.limit_px = Some("50".parse().unwrap());
        let err = sim.place(&feed, a, &deep).unwrap_err();
        assert!(format!("{err}").contains("Insufficient margin"), "{err}");
        // A size the mark-margin can carry places.
        let mut ok = ioc(Side::Buy, "5");
        ok.tif = Tif::Gtc;
        ok.limit_px = Some("50".parse().unwrap());
        assert!(sim.place(&feed, a, &ok).is_ok(), "5 × 101 / 10 = 50.5 ≤ 100");
    }

    /// The docs' transfer rule reaches the isolated margin transfer:
    /// "transferring ... includes ... isolated margin transfers", and the
    /// margin left must be at least max(initial, 10 % of the notional).
    #[test]
    fn an_isolated_removal_must_leave_the_transfer_margin() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        sim.update_isolated_margin(a, "ETH", 200_000_000, &feed).unwrap();
        // 1 ETH at the mark 101: initial = 10.1, the 10 % floor = 10.1 →
        // leaving 5 must refuse; leaving 11 may.
        let err = sim
            .update_isolated_margin(a, "ETH", -195_000_000, &feed)
            .unwrap_err();
        assert!(format!("{err}").contains("below the required"), "{err}");
        sim.update_isolated_margin(a, "ETH", -189_000_000, &feed)
            .unwrap();
        let margin = sim
            .account(a)
            .unwrap()
            .unwrap()
            .positions
            .get("ETH")
            .unwrap()
            .isolated_margin;
        assert_eq!(margin, Decimal::from(11));
    }

    /// The docs' delisting: "When an asset is delisted, all positions are
    /// settled and open orders are cancelled ... After settlement, no new
    /// orders will be accepted." The settlement closes at the served oracle
    /// (the docs' 1 h TWAP spot oracle, a named simplification); no fee.
    #[test]
    fn a_delisted_asset_settles_and_refuses_new_orders() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "1")).unwrap(); // long 1 at the ask 101
        let mut rest = ioc(Side::Sell, "1");
        rest.tif = Tif::Gtc;
        rest.limit_px = Some("120".parse().unwrap());
        let oid = sim.place(&feed, a, &rest).unwrap().resting.expect("a Gtc rests");
        let before = sim.account(a).unwrap().unwrap().perp_balance;
        // The meta flips to delisted (the operator's act; the feed carries it
        // live) — the next tick settles.
        let mut meta = meta_with("ETH", 10);
        meta.universe[0].is_delisted = Some(true);
        feed.put_meta(meta, crate::now_ms());
        sim.evaluate(&feed).unwrap();
        assert!(
            sim.resting_orders(None).unwrap().iter().all(|r| r.oid != oid),
            "the open order is cancelled"
        );
        let account = sim.account(a).unwrap().unwrap();
        assert!(!account.positions.contains_key("ETH"), "settled flat");
        // Entry 101, the oracle 101 (the fixture's ctx): zero PnL, no fee.
        assert_eq!(account.perp_balance, before);
        let err = sim.place(&feed, a, &ioc(Side::Buy, "1")).unwrap_err();
        assert!(format!("{err}").contains("no new orders"), "{err}");
    }

    /// The docs' open-order limit (the rate-limits page): the 1000th resting
    /// order is the last; the next refuses by name (the volume bonus is not
    /// modeled — the named simplification above the rule).
    #[test]
    fn the_open_order_limit_refuses_by_name() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "100000");
        let mut rest = ioc(Side::Sell, "0.1");
        rest.tif = Tif::Gtc;
        rest.limit_px = Some("120".parse().unwrap());
        for i in 0..1000 {
            sim.place(&feed, a, &rest)
                .unwrap_or_else(|e| panic!("order {i}: {e}"));
        }
        let err = sim.place(&feed, a, &rest).unwrap_err();
        assert!(format!("{err}").contains("open-order limit"), "{err}");
    }

    /// The docs' margin modes and the leverage bound: a strict-isolated or
    /// no-cross asset refuses cross leverage, and leverage cannot exceed the
    /// asset's max ("any integer between 1 and the max leverage").
    #[test]
    fn margin_modes_and_the_leverage_bound_refuse_by_name() {
        use crate::wire::MarginMode;
        let mut meta = meta_with("ETH", 10);
        meta.universe[0].margin_mode = Some(MarginMode::StrictIsolated);
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta);
        let (sim, a) = funded_sim(&feed, "1000");
        let setting = |mode, leverage| crate::sim::account::LeverageSetting { mode, leverage };
        let meta = feed.meta();
        let err = sim
            .set_leverage(
                a,
                "ETH",
                setting(crate::wire::LeverageType::Cross, 10),
                meta.as_ref(),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("does not allow cross"), "{err}");
        assert!(sim
            .set_leverage(
                a,
                "ETH",
                setting(crate::wire::LeverageType::Isolated, 10),
                meta.as_ref(),
            )
            .is_ok());
        let err = sim
            .set_leverage(
                a,
                "ETH",
                setting(crate::wire::LeverageType::Isolated, 999),
                meta.as_ref(),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("max leverage"), "{err}");
    }

    /// The docs' band: a limit more than 80 % from the oracle is refused by
    /// their own string.
    #[test]
    fn the_oracle_band_refuses_by_the_docs_string() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        // 300 is ~197 % above the oracle (101): outside the band.
        let mut far = ioc(Side::Buy, "0.2");
        far.limit_px = Some("300".parse().unwrap());
        let err = sim.order(&feed, a, &far).unwrap_err();
        assert!(
            format!("{err}").contains("Order price too far from oracle"),
            "{err}"
        );
        // 101 is inside the band.
        let mut ok = ioc(Side::Buy, "0.2");
        ok.limit_px = Some("101".parse().unwrap());
        assert!(sim.order(&feed, a, &ok).is_ok(), "inside the band fills");
    }

    /// The docs' max order value: $30M at ≥25× (a limit may be 10× that).
    #[test]
    fn the_order_value_cap_refuses() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 25));
        let (sim, a) = funded_sim(&feed, "1000");
        // 4M ETH at $100 = $400M > 10 × $30M.
        let mut huge = ioc(Side::Buy, "4000000");
        huge.tif = Tif::Gtc;
        huge.limit_px = Some("100".parse().unwrap());
        let err = sim.place(&feed, a, &huge).unwrap_err();
        assert!(format!("{err}").contains("exceeds the maximum"), "{err}");
    }

    /// The docs' tier-limit string: the resulting notional may not exceed the
    /// next tier down's lower bound at the position's leverage.
    #[test]
    fn the_margin_tier_limit_refuses() {
        let mut meta = meta_with("ETH", 40);
        meta.margin_tables.push((
            7,
            crate::wire::MarginTable {
                description: "test".into(),
                margin_tiers: vec![
                    crate::wire::MarginTier {
                        lower_bound: Decimal::ZERO,
                        max_leverage: 40,
                    },
                    crate::wire::MarginTier {
                        lower_bound: Decimal::from(150_000_000),
                        max_leverage: 20,
                    },
                ],
            },
        ));
        meta.universe[0].margin_table_id = Some(7);
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta);
        let (sim, a) = funded_sim(&feed, "1000");
        // 2M ETH at $100 = $200M > the 40× tier's $150M bound (and inside the
        // 10× limit cap, so the tier rule is the one that fires).
        let mut huge = ioc(Side::Buy, "2000000");
        huge.tif = Tif::Gtc;
        huge.limit_px = Some("100".parse().unwrap());
        let err = sim.place(&feed, a, &huge).unwrap_err();
        assert!(
            format!("{err}").contains("exceed margin tier limit"),
            "{err}"
        );
    }

    /// `updateIsolatedMargin` moves USDC between the balance and the
    /// position's margin; the isolated margin is the position's margin.
    #[test]
    fn isolated_margin_moves_onto_the_position() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        let before = sim.account(a).unwrap().unwrap().perp_balance;
        // $100 = 100_000_000 at the docs' 6 decimals.
        sim.update_isolated_margin(a, "ETH", 100_000_000, &feed).unwrap();
        let account = sim.account(a).unwrap().unwrap();
        let p = account.positions.get("ETH").unwrap();
        assert_eq!(p.isolated_margin, Decimal::from(100));
        assert_eq!(p.margin_used, Decimal::from(100));
        assert_eq!(p.leverage, crate::wire::LeverageType::Isolated);
        assert_eq!(account.perp_balance, before - Decimal::from(100));
        // And back off the position.
        sim.update_isolated_margin(a, "ETH", -40_000_000, &feed).unwrap();
        let account = sim.account(a).unwrap().unwrap();
        assert_eq!(account.positions.get("ETH").unwrap().isolated_margin, Decimal::from(60));
        assert_eq!(account.perp_balance, before - Decimal::from(60));
    }
    /// The nonce floor (docs): "the 100 highest nonces are stored per address.
    /// Every new transaction must have nonce larger than the smallest nonce in
    /// this set and also never have been used before." A nonce pushed out of
    /// the retained set must not be replayable.
    #[test]
    fn a_pruned_nonce_cannot_be_replayed() {
        let sim = Sim::memory();
        let signer = Address::repeat_byte(0xbb);
        let now = crate::now_ms();
        for i in 0..101u64 {
            sim.use_nonce(signer, now + i, now).unwrap();
        }
        // Only the 100 highest are kept: the first is below the floor now.
        let refused = sim.use_nonce(signer, now, now);
        assert!(refused.is_err(), "a nonce below the retained set is refused");
        assert!(refused.unwrap_err().to_string().contains("too low"));
        // The most recent is still "already used", not "too low".
        let used = sim.use_nonce(signer, now + 100, now);
        assert!(used.is_err());
        assert!(used.unwrap_err().to_string().contains("already used"));
        // A fresh nonce above the floor passes.
        sim.use_nonce(signer, now + 101, now).unwrap();
    }

    /// The venue rejects a fast cancel that refers to a trigger order (docs);
    /// a plain cancel of the same trigger succeeds.
    #[test]
    fn fast_cancel_refuses_trigger_orders() {
        let feed = feed_with_book(
            book("ETH", &[("100", "10")], &[("101", "10")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let mut t = ioc(Side::Buy, "1");
        t.tif = Tif::Gtc;
        t.limit_px = Some("101".parse().unwrap());
        t.trigger = Some(Trigger {
            is_market: false,
            trigger_px: "110".parse().unwrap(),
            tpsl: Tpsl::Sl,
        });
        let oid = sim.place(&feed, a, &t).unwrap().resting.expect("the trigger armed");
        let refused = sim.cancel(a, "ETH", oid, CancelSpeed::Fast);
        assert!(refused.is_err(), "a fast cancel refuses the trigger");
        assert!(refused.unwrap_err().to_string().contains("trigger"));
        sim.cancel(a, "ETH", oid, CancelSpeed::Normal).unwrap();
    }

    /// The trigger direction rule (docs, stated for the stop/take market
    /// orders): for a buy, a stop's trigger sits above the mid and a take's
    /// below; for a sell, the reverse. A mis-sided trigger would fire
    /// immediately, so the venue refuses it at placement.
    #[test]
    fn a_mis_sided_trigger_is_refused() {
        let feed = feed_with_book(
            book("ETH", &[("100", "10")], &[("101", "10")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let trigger = |side: Side, tpsl: Tpsl, px: &str| {
            let mut o = ioc(side, "1");
            o.tif = Tif::Gtc;
            o.limit_px = Some("100".parse().unwrap());
            o.trigger = Some(Trigger {
                is_market: true,
                trigger_px: px.parse().unwrap(),
                tpsl,
            });
            o
        };
        // The mid is (100 + 101) / 2 = 100.5.
        let refused = sim.place(&feed, a, &trigger(Side::Buy, Tpsl::Sl, "99"));
        assert!(refused.is_err(), "a stop buy below the mid is refused");
        assert!(refused.unwrap_err().to_string().contains("wrong side"));
        sim.place(&feed, a, &trigger(Side::Buy, Tpsl::Sl, "110")).unwrap();
        let refused = sim.place(&feed, a, &trigger(Side::Sell, Tpsl::Tp, "99"));
        assert!(refused.is_err(), "a take sell below the mid is refused");
        sim.place(&feed, a, &trigger(Side::Sell, Tpsl::Tp, "110")).unwrap();
    }

    /// The scheduled-cancel cap (docs): "The max number of triggers per day is
    /// 10. This trigger count is reset at 00:00 UTC." The count increments
    /// when one fires; the 11th schedule of the day is refused.
    #[test]
    fn scheduled_cancels_are_capped_per_day() {
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        sim.with_account(a, |acc| {
            acc.perp_balance = Decimal::from(100);
            Ok(())
        })
        .unwrap();
        let now = crate::now_ms();
        for i in 0..10u64 {
            let t = now + 6_000 + i * 1_000;
            sim.schedule_cancel(a, Some(t)).unwrap();
            sim.fire_scheduled_cancels(t).unwrap();
        }
        let refused = sim.schedule_cancel(a, Some(now + 60_000));
        assert!(refused.is_err(), "the 11th trigger of the day is refused");
        assert!(refused.unwrap_err().to_string().contains("per day"));
    }
}
