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
use super::ledger::{FillLiquidation, FillRecord, LiquidationMethod, Liquidity};
use super::validate::{validate_order, validate_trigger_side};
use super::Sim;
use crate::error::{Error, Result};
use crate::feed::dex::dex_of_coin;
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

    /// The reduce-only flag, converted from the wire once.
    pub(crate) fn reduce(&self) -> Reduce {
        if self.reduce_only {
            Reduce::Only
        } else {
            Reduce::Any
        }
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

/// The wire's `alwaysPlace` flag (the modify actions), converted once here:
/// with [`AlwaysPlace::RequiresCancel`] the cancel must succeed, the new order
/// must be non-trigger and an executable Gtc is overridden to Alo (docs); with
/// [`AlwaysPlace::Regardless`] the placement happens even if the cancel fails.
#[derive(Clone, Copy)]
pub(crate) enum AlwaysPlace {
    RequiresCancel,
    Regardless,
}

impl AlwaysPlace {
    /// The wire's `alwaysPlace` flag, converted once at the boundary.
    pub(crate) fn of(always: bool) -> Self {
        if always {
            AlwaysPlace::Regardless
        } else {
            AlwaysPlace::RequiresCancel
        }
    }
}

/// The `scheduleCancel` action's time: arm the dead-man's switch, or clear it
/// (the docs' absent-time form).
#[derive(Clone, Copy)]
pub(crate) enum ScheduleCancel {
    At(u64),
    Clear,
}

impl ScheduleCancel {
    /// The wire's optional `time`, converted once at the boundary.
    pub(crate) fn of(time: Option<u64>) -> Self {
        match time {
            Some(t) => ScheduleCancel::At(t),
            None => ScheduleCancel::Clear,
        }
    }
}

/// Whether an order may only reduce the position (the wire's `reduce_only`,
/// named): a reduce-only order frees margin instead of using it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reduce {
    Only,
    Any,
}

impl<'de> Deserialize<'de> for Reduce {
    /// Reads both spellings: the wire's `reduce_only` flag (the compact
    /// payloads and the pre-enum state files) and the enum's own name.
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
            Wire::Flag(true) => Ok(Reduce::Only),
            Wire::Flag(false) => Ok(Reduce::Any),
            Wire::Named(name) => match name.as_str() {
                "only" => Ok(Reduce::Only),
                "any" => Ok(Reduce::Any),
                other => Err(serde::de::Error::custom(format!(
                    "unknown reduce {other:?}"
                ))),
            },
        }
    }
}

/// Which way an order goes: the sim's own vocabulary. The wire keeps the
/// venue's `is_buy` boolean (what the consumer sends); [`Side::of`] is the one
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
    limit: usize,
) {
    // The order carries what filled; the terminal row copies it (one source,
    // never a caller-passed figure that can disagree with the order).
    let filled_sz = order.filled_sz;
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
    while terminal.len() > limit {
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

/// The venue's default for a coin the account never set a leverage on
/// (measured on the real `activeAssetData` of accounts with no position, on
/// every dex): cross at the asset's max leverage capped at 20x, and for an
/// isolated-only market (`noCross`/`strictIsolated`) isolated at that value.
pub(crate) fn default_leverage(
    coin: &str,
    meta: &crate::wire::Meta,
) -> super::account::LeverageSetting {
    /// The venue's default leverage cap for a new account's coin.
    const DEFAULT_LEVERAGE_CAP: u32 = 20;
    let max = super::account::max_leverage(coin, Some(meta)).unwrap_or(1);
    super::account::LeverageSetting {
        mode: if super::account::isolated_only(coin, Some(meta)) {
            crate::wire::LeverageType::Isolated
        } else {
            crate::wire::LeverageType::Cross
        },
        leverage: max.clamp(1, DEFAULT_LEVERAGE_CAP),
    }
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
    if matches!(mode, crate::wire::LeverageType::Cross) && super::account::isolated_only(coin, meta)
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
pub(crate) fn order_requirement(
    sz: Decimal,
    px: Decimal,
    leverage: u32,
    reduce: Reduce,
) -> Result<Decimal> {
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

/// The fee multiplier a builder-deployed (HIP-3) market charges over the base
/// tier: the deployer's `deployerFeeScale` `s` makes it `1 + s` below 1 and
/// `2 * s` from 1 up (the docs' HIP-3 fee rule, measured on real fills: scale
/// 1.0 charges twice the base), and a market in `growthMode` pays 10% of that
/// (the docs' 90% reduction: real xyz fills cost 0.2 of the base). The default
/// dex has no scale and charges the base tier.
pub(crate) fn dex_fee_multiplier(meta: Option<&crate::wire::Meta>, coin: &str) -> Decimal {
    let Some(asset) = meta.and_then(|m| m.universe.iter().find(|a| a.name == coin)) else {
        return Decimal::ONE;
    };
    let Some(scale) = asset
        .deployer_fee_scale
        .as_deref()
        .and_then(|s| s.parse::<Decimal>().ok())
    else {
        return Decimal::ONE;
    };
    let factor = if scale < Decimal::ONE {
        Decimal::ONE + scale
    } else {
        scale * Decimal::TWO
    };
    if asset.growth_mode.as_deref() == Some("enabled") {
        factor * Decimal::new(1, 1)
    } else {
        factor
    }
}

/// The venue's perp builder-fee cap (docs: 0.1%), in the wire's unit of
/// tenths of a basis point: an order may carry `f <= 100`.
pub(crate) const MAX_PERP_BUILDER_FEE: u64 = 100;

/// Tenths of a basis point per unit: the fee rate is `f / 100_000`.
const BUILDER_FEE_DENOMINATOR: u64 = 100_000;

/// The builder fee an order carries (docs: `{"b": address, "f": tenths of a
/// basis point}`): every fill of the order charges `notional * f / 100_000`
/// on top of the exchange fee. Stored on a resting order too, so a later
/// maker fill charges it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BuilderFee {
    pub builder: Address,
    pub f: u64,
}

impl BuilderFee {
    /// The charge a fill of `notional` owes this builder.
    pub(crate) fn charge(self, notional: Decimal) -> Result<BuilderCharge> {
        let rate = super::account::div(
            Decimal::from(self.f),
            Decimal::from(BUILDER_FEE_DENOMINATOR),
            "builder fee rate",
        )?;
        Ok(BuilderCharge {
            builder: self.builder,
            amount: super::account::mul(notional, rate, "builder fee")?,
        })
    }
}

/// One fill's builder fee, computed: debited from the filled account with the
/// exchange fee, credited to the builder's perp balance when the builder is a
/// simulated account, and recorded on the ledger fill as `builderFee`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BuilderCharge {
    pub builder: Address,
    pub amount: Decimal,
}

/// Who is placing, and the builder fee their order carries: the pair every
/// placement step needs, kept as one argument.
#[derive(Debug, Clone, Copy)]
struct Placer {
    address: Address,
    builder: Option<BuilderFee>,
}

/// A resting **spot** order's balance reservation: the `hold` the venue
/// serves in `spotClearinghouseState` while the order rests (a buy reserves
/// quote at its limit, a sell reserves the base it sells). Carried on the
/// order so a cancel can release it without re-resolving the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpotHold {
    pub token: u32,
    pub amount: Decimal,
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
    /// The reduce-only flag (`reduceOnly` in pre-enum state files; the alias
    /// and the tolerant `Reduce` deserializer migrate them).
    #[serde(alias = "reduceOnly", alias = "reduce_only")]
    pub reduce: Reduce,
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
    /// The builder fee the order was placed with: every later fill (a maker
    /// fill on the book or the tick, a fired trigger) charges it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builder: Option<BuilderFee>,
    /// The spot balance reservation (`None` on a perp order). Older state
    /// files read back with none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot_hold: Option<SpotHold>,
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
    /// The venue's `triggered`: a trigger order that fired. Its own oid ends
    /// here (the resulting market order carries the fill under its cloid); the
    /// real venue serves `triggered`, not `canceled`, for this order.
    Triggered,
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

/// What a partial take hands to its resting remainder: the order's own oid and
/// what already filled. The venue gives **one order one oid**, so the take's
/// fill and the remainder's resting order are the same order: `origSz` is the
/// whole order, `sz` the remainder, and `filled_sz`/`filled_ntl` carry the take
/// so a later terminal report has the order's true average price.
#[derive(Debug, Clone, Copy)]
struct RestCarry {
    oid: u64,
    filled_sz: Decimal,
    filled_ntl: Decimal,
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
        self.place_with_builder(feed, address, req, None)
    }

    /// `place` for an order that carries a builder fee: the approval and the
    /// cap are the caller's gate (`authorize_builder`); here the fee rides the
    /// order into its fills (the immediate take) and its resting remainder
    /// (the later maker fills).
    #[allow(clippy::too_many_lines)] // one placement decision: validate, take, rest the remainder, terminate
    pub(crate) fn place_with_builder(
        &self,
        feed: &MarketFeed,
        address: Address,
        req: &Order,
        builder: Option<BuilderFee>,
    ) -> Result<Placed> {
        let placer = Placer { address, builder };
        // A spot pair is a market with its own settlement: no leverage, no
        // funding, no positions — the fill moves tokens in the spot ledger.
        if let Some(pair) = feed.spot_pair(&req.coin) {
            return self.place_spot(feed, placer, req, &pair);
        }
        let meta = feed.meta_for_lookup(&req.coin);
        let position_szi = self.signed_position(address, &req.coin)?;
        let capped = cap_reduce_only(req, position_szi);
        let req = capped.as_ref().unwrap_or(req);
        validate_order(feed, meta.as_ref(), req, position_szi)?;
        self.pre_place_refusals(feed, address, req, meta.as_ref())?;
        // A trigger arms instead of walking.
        if let Some(trigger) = req.trigger {
            if trigger.trigger_px <= Decimal::ZERO {
                return Err(Error::bad_request("Invalid TP/SL price."));
            }
            validate_trigger_side(feed, req, &trigger)?;
            let px = req.limit_px.unwrap_or(trigger.trigger_px);
            self.require_order_margin(feed, address, req, (px, req.size))?;
            let oid = self.rest_order(placer, req, (px, req.size))?;
            self.metrics
                .triggers_armed_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(account = %address, coin = %req.coin, oid, "trigger order armed");
            return Ok(resting_only(oid));
        }
        // Post-only: a crossing order is canceled instead of matching (docs).
        if req.tif == Tif::Alo {
            let oid = self.place_alo(feed, placer, req)?;
            return Ok(resting_only(oid));
        }
        // A marketable GTC is refused whole before it trades a slice (see
        // `require_marketable_gtc_margin`).
        self.require_marketable_gtc_margin(feed, address, req)?;
        // The venue's self-trade rule: the taker's own crossing resting orders
        // are expired, not matched.
        self.expire_self_crossing(address, feed, req)?;
        // A Gtc/Alo that does not cross rests without a take (the venue's
        // behavior: nothing to match, so the order simply joins the book).
        let take = req.tif == Tif::Ioc || crosses(feed, req, &self.resting_orders(None)?);
        let fill = if take {
            Some(self.order_with_builder(feed, placer, req)?)
        } else {
            None
        };
        let filled_sz = fill.as_ref().map_or(Decimal::ZERO, |f| f.total_sz);
        let remainder = req.size - filled_sz;
        // **The remainder resumes the same order.** The venue gives one oid to
        // an order, so a marketable order's take and its resting remainder share
        // it. A fresh oid here split one order into two (measured 2026-11-23: a
        // marketable GTC filled under oid 629 and rested under 630, so
        // `orderStatus(629)` -- the oid the client was handed -- answered
        // `unknownOid`). The take's oid is the order's identity.
        let carry = match &fill {
            Some(f) => Some(RestCarry {
                oid: f.oid,
                filled_sz: f.total_sz,
                filled_ntl: super::account::mul(f.total_sz, f.avg_px, "filled notional")?,
            }),
            None => None,
        };
        // A remainder the account can no longer carry is canceled, never
        // reported as a failed order: the take is a fact. With the up-front
        // check above this is the rare intra-order case (a mark moved between
        // the check and the fill), and the client must not be told an order it
        // was filled on did not happen.
        let resting = match self.rest_remainder(feed, placer, req, remainder, carry) {
            Ok(resting) => resting,
            Err(error) if filled_sz > Decimal::ZERO => {
                tracing::warn!(
                    account = %address,
                    coin = %req.coin,
                    %error,
                    "resting remainder canceled: margin no longer available after the take"
                );
                None
            }
            Err(error) => return Err(error),
        };
        // **An order that never rests still ends** (see `record_terminal`).
        if resting.is_none() {
            if let Some(fill) = &fill {
                self.record_terminal(placer, req, fill, remainder)?;
            }
        }
        Ok(Placed { fill, resting })
    }

    /// **The whole order's margin is checked before it matches.** A marketable
    /// GTC fills part and rests the remainder; checking only the remainder after
    /// the take answered "Insufficient margin to place order." to a client that
    /// had already been filled (measured 2026-11-23: a 120-lot SNDK GTC took
    /// 11.988 and then failed). The venue refuses an order it cannot carry
    /// without trading any of it, so the full size is reserved at the order's
    /// own price up front. Only an opening GTC can take and rest: an IOC is
    /// checked on its fill, an ALO never crosses, a reduce-only needs no margin.
    fn require_marketable_gtc_margin(
        &self,
        feed: &MarketFeed,
        address: Address,
        req: &Order,
    ) -> Result<()> {
        if req.tif == Tif::Gtc && !req.reduce_only {
            if let Some(px) = req.limit_px {
                self.require_order_margin(feed, address, req, (px, req.size))?;
            }
        }
        Ok(())
    }

    /// Record the terminal state of an order that never rested: `Filled` when
    /// the take covered the order (or a GTC filled whole), `Canceled` for an
    /// IOC's unfilled remainder (or a remainder the account could not carry).
    /// The row is what `orderStatus` answers from and what `orderUpdates`
    /// diffs to push `filled`/`canceled`; without it the order is invisible to
    /// both after its fills.
    fn record_terminal(
        &self,
        placer: Placer,
        req: &Order,
        fill: &Fill,
        remainder: Decimal,
    ) -> Result<()> {
        let status = if remainder <= Decimal::ZERO {
            TerminalStatus::Filled
        } else {
            TerminalStatus::Canceled
        };
        let filled_ntl = super::account::mul(fill.total_sz, fill.avg_px, "filled notional")?;
        let order = RestingOrder {
            oid: fill.oid,
            account: placer.address,
            coin: req.coin.clone(),
            side: req.side(),
            px: req.limit_px.unwrap_or(fill.avg_px),
            sz: remainder.max(Decimal::ZERO),
            orig_sz: req.size,
            reduce: req.reduce(),
            tif: req.tif,
            cloid: req.cloid,
            trigger: req.trigger,
            placed_ms: crate::now_ms(),
            filled_sz: fill.total_sz,
            filled_ntl,
            builder: placer.builder,
            spot_hold: None,
        };
        let limit = self.terminal_orders;
        self.mutate_state(|st| {
            terminalize(&mut st.terminal, order, status, limit);
            Ok(())
        })
    }

    /// Spot taker fee: the venue's base tier is 0.070 % (perp taker is 0.045 %;
    /// spot pays the extra). Charged in the received token.
    fn spot_taker_fee() -> Decimal {
        Decimal::new(7, 4)
    }

    /// Spot maker fee: the venue's base tier (the maker side of the same
    /// schedule), charged in the received token. Shared with the passive
    /// (tick) maker path.
    pub(crate) fn spot_maker_fee() -> Decimal {
        Decimal::new(4, 4)
    }

    /// `place` for a **spot pair**: the same served book is the executable
    /// depth, but the fill moves tokens in the spot ledger instead of opening
    /// a position. An IOC takes at the taker fee; `Gtc`/`Alo` rest with their
    /// balance held (`spotClearinghouseState.hold`) and fill as makers at the
    /// maker fee when the book crosses them (the tick's passive spot lane).
    #[allow(
        clippy::too_many_lines,
        reason = "one placement path: validate, walk, settle, record, consume — read top to bottom in order"
    )]
    fn place_spot(
        &self,
        feed: &MarketFeed,
        placer: Placer,
        req: &Order,
        pair: &crate::wire::SpotPair,
    ) -> Result<Placed> {
        if req.trigger.is_some() {
            return Err(Error::bad_request(
                "Trigger orders are not served for spot on this mirror.",
            ));
        }
        if req.reduce_only {
            return Err(Error::bad_request(
                "Reduce-only is a perp order type; a spot sell spends the balance it owns.",
            ));
        }
        if req.size <= Decimal::ZERO {
            return Err(Error::bad_request("Order size must be positive."));
        }
        if let Some(px) = req.limit_px {
            if px <= Decimal::ZERO {
                return Err(Error::bad_request("Order price must be positive."));
            }
        }
        let base = feed
            .spot_token(pair.tokens[0])
            .ok_or(Error::Warming)?;
        let quote = feed
            .spot_token(pair.tokens[1])
            .ok_or(Error::Warming)?;
        if req.size.normalize().scale() > u32::from(base.sz_decimals) {
            return Err(Error::bad_request(format!(
                "Order size has more than {} decimals for {}.",
                base.sz_decimals, pair.name
            )));
        }
        // Post-only on a spot pair: the point of the order is the maker fee,
        // so an order that would take is refused (the venue's string, with the
        // bbo), never quietly filled as a taker. A non-crossing order rests
        // with its balance held, exactly like a Gtc remainder.
        if req.tif == Tif::Alo {
            let Some(px) = req.limit_px else {
                return Err(Error::bad_request(
                    "Post only order needs a limit price (a market order cannot rest).",
                ));
            };
            let own = self.resting_orders(Some(placer.address))?;
            if crosses(feed, req, &own) {
                let bbo = bbo(feed, &req.coin, req.side())
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "none".into());
                tracing::warn!(coin = %req.coin, bbo = %bbo, "post-only refused");
                return Err(Error::bad_request(format!(
                    "Post only order would have immediately matched, bbo was {bbo}."
                )));
            }
            let value = super::account::mul(req.size, px, "spot order value")?;
            if value < Decimal::from(10) {
                return Err(Error::bad_request(
                    "Order value is too small: the venue's minimum is 10 USD.",
                ));
            }
            let oid = self
                .rest_spot_remainder(placer, req, req.size, (&base, &quote), None)?
                .ok_or_else(|| Error::bad_request("Post only order could not rest."))?;
            return Ok(Placed {
                fill: None,
                resting: Some(oid),
            });
        }
        let book = feed.book(&req.coin, crate::now_ms()).ok_or(Error::Warming)?;
        // The venue's self-trade rule first (it releases holds via cancel):
        // the taker's own crossing resting orders are expired, not matched.
        self.expire_self_crossing(placer.address, feed, req)?;
        let resting: Vec<RestingOrder> = self
            .resting_orders(None)?
            .into_iter()
            .filter(|r| r.coin == req.coin)
            .collect();
        let (legs, filled, avg_px) = walk(&book, req, &resting)?;
        let notional = super::account::mul(filled, avg_px, super::account::FILL_NOTIONAL)?;
        // The venue's minimum order value is $10 (both planes), on the
        // **order**: a non-crossing Gtc takes nothing yet still rests, so its
        // value is its own size at its limit.
        let order_value = if filled > Decimal::ZERO {
            notional
        } else {
            super::account::mul(
                req.size,
                req.limit_px.unwrap_or(Decimal::ONE),
                "spot order value",
            )?
        };
        if order_value < Decimal::from(10) {
            return Err(Error::bad_request(
                "Order value is too small: the venue's minimum is 10 USD.",
            ));
        }
        let fee = super::account::mul(notional, Self::spot_taker_fee(), "spot taker fee")?;
        // The wire pair `fee`/`feeToken` is in the received token's units. A
        // resting (unfilled) order has no price and records no fill; keep the
        // quote figure there so the unfilled path never divides by zero.
        let fee_recorded = if filled.is_zero() {
            fee
        } else {
            super::account::spot_fee_in_received(fee, avg_px, req.side())?
        };
        let builder = placer.builder.map(|b| b.charge(notional)).transpose()?;
        let base_ref = crate::sim::TokenRef {
            index: base.index,
            coin: &base.name,
        };
        let quote_ref = crate::sim::TokenRef {
            index: quote.index,
            coin: &quote.name,
        };
        let effect = if filled > Decimal::ZERO {
            self.with_account_now(placer.address, feed, |account| {
                super::account::settle_spot_fill(
                    account,
                    base_ref,
                    quote_ref,
                    req.side(),
                    filled,
                    avg_px,
                    fee,
                )
            })?
        } else {
            super::ledger::FillEffect {
                start_position: Decimal::ZERO,
                closed_pnl: Decimal::ZERO,
                dir: "Resting",
            }
        };
        // The resting spot makers this take consumed: their token legs settle
        // here (holds released pro rata), never in the perp path.
        let maker_sz = self.settle_spot_makers(feed, req, &legs)?;
        let oid = self.next_oid()?;
        let facts = super::account::FillFacts {
            coin: req.coin.clone(),
            side: req.side(),
            size: filled,
            px: avg_px,
            leverage: 1,
            mode: crate::wire::LeverageType::Cross,
            reduce: Reduce::Any,
            fee: fee_recorded,
            builder,
        };
        if filled > Decimal::ZERO {
            self.record_fill(
                placer.address,
                &FillRecord {
                    liquidation: None,
                    facts: &facts,
                    effect,
                    oid,
                    cloid: req.cloid,
                    liquidity: Liquidity::Taker,
                    fee_token: if req.side() == Side::Buy {
                        &base.name
                    } else {
                        &quote.name
                    },
                },
            )?;
            for leg in &legs {
                if let Candidate::Live(c) = leg {
                    feed.consume(c.clone())?;
                }
            }
            self.credit_builder(builder, dex_of_coin(&req.coin))?;
            self.metrics
                .orders_filled_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.metrics
                .fees_charged_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.mark_dirty()?;
        let fill = (filled > Decimal::ZERO).then(|| Fill {
            total_sz: filled,
            avg_px,
            consumed: legs
                .iter()
                .filter_map(|leg| match leg {
                    Candidate::Live(c) => Some(c.clone()),
                    Candidate::Maker { .. } => None,
                })
                .collect(),
            oid,
        });
        // The remainder rests with its balance held: a buy reserves quote at
        // its limit, a sell reserves the base it sells (the venue's spot
        // `hold`, served in spotClearinghouseState).
        let remainder = req.size - filled - maker_sz;
        // The remainder continues the same order: the take's oid and fill ride
        // into it (see `RestCarry`).
        let carry = (filled > Decimal::ZERO).then_some(RestCarry {
            oid,
            filled_sz: filled,
            filled_ntl: notional,
        });
        let resting =
            self.rest_spot_remainder(placer, req, remainder, (&base, &quote), carry)?;
        Ok(Placed { fill, resting })
    }

    /// The maker legs a spot take consumed: each resting order's token legs
    /// settle against its own hold (a buy maker spends its reserved quote and
    /// receives the base net of the maker fee; a sell the reverse), its size
    /// shrinks, and a fully filled order leaves the book. Returns the size
    /// the resting side absorbed (it is not the taker's fill).
    #[allow(
        clippy::too_many_lines,
        reason = "one maker settlement path: shrink the order, settle the tokens, record the fill — read top to bottom"
    )]
    fn settle_spot_makers(
        &self,
        feed: &MarketFeed,
        req: &Order,
        legs: &[Candidate],
    ) -> Result<Decimal> {
        let pair = feed.spot_pair(&req.coin).ok_or(Error::Warming)?;
        let base = feed.spot_token(pair.tokens[0]).ok_or(Error::Warming)?;
        let quote = feed.spot_token(pair.tokens[1]).ok_or(Error::Warming)?;
        let mut absorbed = Decimal::ZERO;
        for leg in legs {
            let Candidate::Maker { oid, maker, px, sz } = leg else {
                continue;
            };
            absorbed += sz;
            let notional = super::account::mul(*sz, *px, "spot maker notional")?;
            // The order's own side decides the token flow; its traits are the
            // resting order's, not the taker's.
            let (side, hold, reduce, cloid, builder) = self.with_state(|st| {
                let r = st.resting.get_mut(oid).filter(|r| r.account == *maker).ok_or_else(|| {
                    Error::bad_request("the maker's resting order vanished mid-match")
                })?;
                r.sz -= sz;
                r.filled_sz += sz;
                r.filled_ntl += notional;
                let out = (r.side, r.spot_hold, r.reduce, r.cloid, r.builder);
                // The reservation follows the remaining size (the settlement
                // below releases the filled slice from the account's hold).
                if let Some(hold) = r.spot_hold {
                    r.spot_hold = Some(SpotHold {
                        token: hold.token,
                        amount: if r.side == Side::Buy {
                            super::account::mul(r.sz, r.px, "spot hold notional")?
                        } else {
                            r.sz
                        },
                    });
                }
                if r.sz <= Decimal::ZERO {
                    let done = st.resting.remove(oid).expect("just read");
                    terminalize(
                        &mut st.terminal,
                        done,
                        TerminalStatus::Filled,
                        self.terminal_orders,
                    );
                }
                Ok(out)
            })?;
            let fee = super::account::mul(notional, Self::spot_maker_fee(), "spot maker fee")?;
            let fee_recorded = super::account::spot_fee_in_received(fee, *px, side)?;
            let builder = builder.map(|b| b.charge(notional)).transpose()?;
            let effect = self.with_account_now(*maker, feed, |account| {
                super::account::settle_spot_maker(
                    account,
                    side,
                    hold,
                    *px,
                    notional,
                    *sz,
                    fee,
                    crate::sim::TokenRef { index: base.index, coin: &base.name },
                    crate::sim::TokenRef { index: quote.index, coin: &quote.name },
                )
            })?;
            let facts = super::account::FillFacts {
                coin: req.coin.clone(),
                side,
                size: *sz,
                px: *px,
                leverage: 1,
                mode: crate::wire::LeverageType::Cross,
                reduce,
                fee: fee_recorded,
                builder,
            };
            self.record_fill(
                *maker,
                &FillRecord {
                    liquidation: None,
                    facts: &facts,
                    effect,
                    oid: *oid,
                    cloid,
                    liquidity: Liquidity::Maker,
                    fee_token: if side == Side::Buy { &base.name } else { &quote.name },
                },
            )?;
            self.credit_builder(builder, dex_of_coin(&req.coin))?;
            self.metrics
                .orders_filled_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.metrics
                .fees_charged_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(absorbed)
    }

    /// Reserve the remainder's balance and rest it (a Gtc or post-only spot
    /// order). The tokens carry their served names: the reserve creates the
    /// entry when the account never held the token.
    #[allow(clippy::too_many_arguments)] // one hold-and-rest call: the pair's tokens and the carried take are distinct inputs
    fn rest_spot_remainder(
        &self,
        placer: Placer,
        req: &Order,
        remainder: Decimal,
        tokens: (&crate::wire::SpotToken, &crate::wire::SpotToken),
        carry: Option<RestCarry>,
    ) -> Result<Option<u64>> {
        if !matches!(req.tif, Tif::Gtc | Tif::Alo) || remainder <= Decimal::ZERO {
            return Ok(None);
        }
        let Some(px) = req.limit_px else {
            return Ok(None); // a market order cannot rest
        };
        let reserved = match req.side() {
            Side::Buy => super::account::mul(remainder, px, "spot hold notional")?,
            Side::Sell => remainder,
        };
        let token = match req.side() {
            Side::Buy => crate::sim::TokenRef { index: tokens.1.index, coin: &tokens.1.name },
            Side::Sell => crate::sim::TokenRef { index: tokens.0.index, coin: &tokens.0.name },
        };
        self.with_account(placer.address, |account| {
            super::account::spot_reserve(account, token, reserved)
        })?;
        let oid = self.rest_order_at(placer, req, (px, remainder), carry)?;
        self.with_state(|st| {
            if let Some(r) = st.resting.get_mut(&oid) {
                r.spot_hold = Some(SpotHold { token: token.index, amount: reserved });
            }
            Ok(())
        })?;
        Ok(Some(oid))
    }

    /// The pre-place refusals (reduce-only, the open-order limit, the
    /// open-interest cap, the margin-tier limit): each gate's reason is the
    /// venue's refusal envelope.
    fn pre_place_refusals(
        &self,
        feed: &MarketFeed,
        address: Address,
        req: &Order,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<()> {
        self.reduce_only_refusal(address, req)?;
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

    /// The venue's reduce-only placement rule (docs, error responses: `Order /
    /// ReduceOnly`): an order that cannot reduce the position it names is
    /// refused now rather than resting into a fill it could never make. A
    /// flat account has nothing to reduce, and the position's own side would
    /// increase it.
    fn reduce_only_refusal(&self, address: Address, req: &Order) -> Result<()> {
        if !req.reduce_only {
            return Ok(());
        }
        let old = self.position_szi(address, &req.coin)?;
        let signed = req.size * req.side().sign();
        if super::account::same_direction(old, signed) {
            tracing::warn!(coin = %req.coin, "order refused: reduce-only cannot reduce");
            return Err(Error::bad_request(
                "Reduce only order would increase position.",
            ));
        }
        Ok(())
    }

    /// The account's signed position in `coin` (0 when none): the reduce-only
    /// guard's and the passive-fill lane's shared read.
    pub(crate) fn position_szi(&self, address: Address, coin: &str) -> Result<Decimal> {
        Ok(self
            .account(address)?
            .and_then(|a| a.positions.get(coin).map(|p| p.szi))
            .unwrap_or(Decimal::ZERO))
    }

    /// The match-time re-margin: the maker's *current* available
    /// margin (value minus the positions' margin minus the account's other
    /// open orders' reservations) must cover the order's own requirement.
    pub(crate) fn remargin_ok(&self, feed: &MarketFeed, r: &RestingOrder) -> Result<bool> {
        let meta = feed.meta_for_lookup(&r.coin);
        let leverage = self.leverage_for(r.account, &r.coin, meta.as_ref())?;
        let requirement = order_requirement(r.sz, feed.mark_or(&r.coin, r.px), leverage, r.reduce)?;
        let Some(available) =
            self.available_for(feed, r.account, dex_of_coin(&r.coin), Some(r.oid))?
        else {
            // A resting order whose account vanished has nothing to re-margin
            // against: cancel it at match time, never fill it unmargined.
            return Ok(false);
        };
        Ok(available >= requirement)
    }

    /// A Gtc remainder rests (after the venue's order-margin check); anything
    /// else, or a filled order, rests nothing.
    #[allow(clippy::too_many_arguments)] // the market, the placer, the order, its remainder and the carried take are distinct inputs
    fn rest_remainder(
        &self,
        feed: &MarketFeed,
        placer: Placer,
        req: &Order,
        remainder: Decimal,
        carry: Option<RestCarry>,
    ) -> Result<Option<u64>> {
        if req.tif != Tif::Gtc || remainder <= Decimal::ZERO {
            return Ok(None);
        }
        let Some(px) = req.limit_px else {
            return Ok(None); // a market order cannot rest
        };
        self.require_order_margin(feed, placer.address, req, (px, remainder))?;
        Ok(Some(self.rest_order_at(placer, req, (px, remainder), carry)?))
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
        let meta = feed.meta_for_lookup(&req.coin);
        let leverage = self.leverage_for(address, &req.coin, meta.as_ref())?;
        let requirement =
            order_requirement(sz, feed.mark_or(&req.coin, px), leverage, Reduce::Any)?;
        let Some(available) = self.available_for(feed, address, dex_of_coin(&req.coin), None)?
        else {
            return Err(Error::bad_request(format!(
                "account {address} is not simulated (deal or seed it before placing orders)"
            )));
        };
        if available < requirement {
            return Err(Error::bad_request("Insufficient margin to place order."));
        }
        Ok(())
    }

    /// The account's free margin on one dex (the venue's `availableToTrade`):
    /// value minus the positions' margin minus the open orders' reservations
    /// at the marks, all of that dex. One home: the placement check, the
    /// match-time re-margin and the served `activeAssetData` read the same
    /// figure. `exclude_oid` skips one resting order (the re-margin's own);
    /// `None` = no account state.
    pub(crate) fn available_for(
        &self,
        feed: &MarketFeed,
        address: Address,
        dex: &str,
        exclude_oid: Option<u64>,
    ) -> Result<Option<Decimal>> {
        let Some(account) = self.account(address)? else {
            return Ok(None);
        };
        let v = super::account::view(&account, feed, dex)?;
        let reserved = self.reserved_margin(address, exclude_oid, feed, dex)?;
        // A unified account's spot collateral is margin even though the served
        // perp summary excludes it; `available_margin` is the one fold.
        let available = super::account::available_margin(&account, feed, dex, &v)?;
        Ok(Some(super::account::sub(
            available,
            reserved,
            "available margin",
        )?))
    }

    /// The account's open orders' reserved margin (their notional over the
    /// account's leverage, at the *mark* — the margining price; a warming mark
    /// falls back to the order's own price; reduce-only orders reserve
    /// nothing). `exclude_oid` skips the order under check; only the dex's own
    /// orders reserve against its ledger.
    pub(crate) fn reserved_margin(
        &self,
        account: Address,
        exclude_oid: Option<u64>,
        feed: &MarketFeed,
        dex: &str,
    ) -> Result<Decimal> {
        let mut total = Decimal::ZERO;
        for r in self.resting_orders(Some(account))? {
            if Some(r.oid) == exclude_oid || r.reduce == Reduce::Only || dex_of_coin(&r.coin) != dex
            {
                continue;
            }
            let leverage =
                self.leverage_for(account, &r.coin, feed.meta_for_lookup(&r.coin).as_ref())?;
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
    #[allow(clippy::too_many_lines)] // the walk and its liquidation-ledger record: one close
    pub(crate) fn liquidate_position(
        &self,
        feed: &MarketFeed,
        address: Address,
        coin: &str,
        size: Decimal,
    ) -> Result<Fill> {
        let meta = feed.meta_for_lookup(coin);
        let direction = self
            .account(address)?
            .and_then(|a| {
                a.positions
                    .get(coin)
                    .map(super::account::Position::direction)
            })
            .ok_or_else(|| Error::NotFound {
                what: format!("position {coin} for {address} (liquidation)"),
            })?;
        let req = Order {
            coin: coin.to_string(),
            is_buy: direction.entry_side().flip().is_buy(), // close the position
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
        let (legs, filled, avg_px) =
            walk_served_book(
                feed,
                meta.as_ref(),
                &req,
                &resting,
                self.signed_position(address, coin)?,
            )?;
        let leverage = self.leverage_for(address, coin, meta.as_ref())?;
        let facts = super::account::FillFacts::close(coin, req.side(), filled, avg_px, leverage);
        let effect = self.with_account_now(address, feed, |account| {
            super::account::apply_fill(account, &facts, meta.as_ref())
        })?;
        let oid = self.next_oid()?;
        self.record_fill(
            address,
            &FillRecord {
                liquidation: Some(FillLiquidation::new(
                    address,
                    feed.mark_or(coin, avg_px),
                    LiquidationMethod::Market,
                )),
                facts: &facts,
                effect,
                oid,
                cloid: None,
                liquidity: Liquidity::Taker,
                fee_token: &feed.fee_token_of_coin(coin),
            },
        )?;
        self.settle_legs(feed, &req, &legs, meta.as_ref())?;
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
    fn place_alo(&self, feed: &MarketFeed, placer: Placer, req: &Order) -> Result<u64> {
        let Some(px) = req.limit_px else {
            return Err(Error::bad_request(
                "Post only order needs a limit price (a market order cannot rest).",
            ));
        };
        let resting = self.resting_orders(Some(placer.address))?;
        if crosses(feed, req, &resting) {
            let bbo = bbo(feed, &req.coin, req.side())
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".into());
            tracing::warn!(coin = %req.coin, bbo = %bbo, "post-only refused");
            return Err(Error::bad_request(format!(
                "Post only order would have immediately matched, bbo was {bbo}."
            )));
        }
        self.rest_order(placer, req, (px, req.size))
    }

    /// `order`: the immediate take — fill `req` for `address` against the
    /// served book plus the devnet's resting orders. The taker pays the taker
    /// fee; each maker's account is settled at their own price with the maker
    /// fee (no opening probe: the venue reserves margin at placement).
    #[cfg(test)]
    pub(crate) fn order(&self, feed: &MarketFeed, address: Address, req: &Order) -> Result<Fill> {
        self.order_with_builder(
            feed,
            Placer {
                address,
                builder: None,
            },
            req,
        )
    }

    /// `order` for a placer whose order carries a builder fee: the fee is
    /// charged on this take's notional with the taker fee, and credited to
    /// the builder when it is a simulated account.
    #[allow(clippy::too_many_lines)] // one placement: validate, take, charge, credit
    fn order_with_builder(&self, feed: &MarketFeed, placer: Placer, req: &Order) -> Result<Fill> {
        let address = placer.address;
        let meta = feed.meta_for_lookup(&req.coin);
        let resting = self.resting_orders(None)?;
        // The plan's rule: resting orders are re-margined at match time (the
        // oracle moves after placement). A maker whose account cannot carry
        // the fill is canceled (the venue's `marginCanceled`), never filled.
        let resting = self.remargin_makers(feed, req, resting)?;
        let position_szi = self.signed_position(address, &req.coin)?;
        let (legs, filled, avg_px) = walk_served_book(feed, meta.as_ref(), req, &resting, position_szi)?;
        let setting = self.leverage_setting_for(address, &req.coin, meta.as_ref())?;
        let notional = super::account::mul(filled, avg_px, super::account::FILL_NOTIONAL)?;
        let facts = super::account::FillFacts {
            coin: req.coin.clone(),
            side: req.side(),
            size: filled,
            px: avg_px,
            leverage: setting.leverage,
            mode: setting.mode,
            reduce: req.reduce(),
            fee: super::account::mul(
                notional,
                taker_fee() * dex_fee_multiplier(meta.as_ref(), &req.coin),
                "taker fee",
            )?,
            builder: placer.builder.map(|b| b.charge(notional)).transpose()?,
        };
        let effect = self.land_fill(feed, address, &facts, meta.as_ref())?;
        self.credit_builder(facts.builder, dex_of_coin(&req.coin))?;
        let oid = self.next_oid()?;
        self.record_fill(
            address,
            &FillRecord {
                liquidation: None,
                facts: &facts,
                effect,
                oid,
                cloid: req.cloid,
                liquidity: Liquidity::Taker,
                fee_token: &feed.fee_token_of_coin(&req.coin),
            },
        )?;
        let consumed = self.settle_legs(feed, req, &legs, meta.as_ref())?;
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
                    let setting = self.leverage_setting_for(*maker, &req.coin, meta)?;
                    let notional = super::account::mul(*sz, *px, "maker notional")?;
                    let (reduce, cloid, builder) = self.resting_traits(*oid)?;
                    let facts = super::account::FillFacts {
                        coin: req.coin.clone(),
                        side: req.side().flip(),
                        size: *sz,
                        px: *px,
                        leverage: setting.leverage,
                        mode: setting.mode,
                        reduce,
                        fee: super::account::mul(
                            notional,
                            maker_fee() * dex_fee_multiplier(meta, &req.coin),
                            "maker fee",
                        )?,
                        builder: builder.map(|b| b.charge(notional)).transpose()?,
                    };
                    let effect = self.with_account_now(*maker, feed, |account| {
                        super::account::apply_fill(account, &facts, meta)
                    })?;
                    self.credit_builder(facts.builder, dex_of_coin(&req.coin))?;
                    self.record_fill(
                        *maker,
                        &FillRecord {
                            liquidation: None,
                            facts: &facts,
                            effect,
                            oid: *oid,
                            cloid,
                            liquidity: Liquidity::Maker,
                            fee_token: &feed.fee_token_of_coin(&req.coin),
                        },
                    )?;
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
                // The reservation follows the remaining size: a maker
                // settlement releases the filled slice from the account's
                // hold, so a cancel of the remainder must release only what
                // the order still covers.
                if let Some(hold) = order.spot_hold {
                    order.spot_hold = Some(SpotHold {
                        token: hold.token,
                        amount: if order.side == Side::Buy {
                            super::account::mul(order.sz, order.px, "spot hold notional")?
                        } else {
                            order.sz
                        },
                    });
                }
                return Ok(());
            }
            let Some(done) = st.resting.remove(&oid) else {
                return Ok(());
            };
            terminalize(
                &mut st.terminal,
                done,
                TerminalStatus::Filled,
                self.terminal_orders,
            );
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

    /// A resting order's reduce-only flag (a maker fill's reduce-only
    /// semantics), client id (its ledger entry's) and builder fee (a maker
    /// fill charges it too).
    fn resting_traits(&self, oid: u64) -> Result<RestingTraits> {
        Ok(self
            .state
            .lock()?
            .resting
            .get(&oid)
            .map_or((Reduce::Any, None, None), |r| {
                (r.reduce, r.cloid, r.builder)
            }))
    }

    /// Credit a fill's builder fee to the builder's ledger of the fill's dex
    /// (the fee is paid in that dex's collateral token). The builder is paid
    /// only when it is a simulated account (a touched address); an
    /// unsimulated builder is outside the sim, so the fee has already left the
    /// payer and is credited nowhere (the venue's builder wallet is not
    /// modeled). Never creates an account.
    pub(crate) fn credit_builder(&self, charge: Option<BuilderCharge>, dex: &str) -> Result<()> {
        let Some(charge) = charge else {
            return Ok(());
        };
        self.with_state(|st| {
            if let Some(account) = st.accounts.get_mut(&charge.builder) {
                account.credit_perp(dex, charge.amount, "builder fee credit")?;
            }
            Ok(())
        })?;
        self.touch();
        Ok(())
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
                // The mode switch moves margin to or from the coin's own dex
                // ledger (a disjoint borrow of the account's `perps`).
                let ledger = account
                    .perps
                    .entry(dex_of_coin(coin).to_string())
                    .or_default();
                switch_position_mode(&mut ledger.balance, p, mode, leverage)?;
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
    /// reduce the position (the venue's own string).
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
        let old = self
            .account(address)?
            .and_then(|a| a.positions.get(&req.coin).map(|p| p.szi))
            .unwrap_or(Decimal::ZERO);
        let signed = req.size * req.side().sign();
        let increases = super::account::same_direction(old, signed);
        let Some(caps) = feed.open_interest_caps_of(dex_of_coin(&req.coin)) else {
            // The cap set is unknown until the venue answers. Fail closed for
            // orders that could increase open interest; a risk-reducing order
            // cannot be the one the cap refuses.
            if !increases {
                return Ok(None);
            }
            return Ok(Some(
                "venue-sim: open-interest caps are not known yet (the market plane is warming); refusing an order that could increase open interest. See docs/INTEGRATION.md.",
            ));
        };
        if !caps.iter().any(|c| c == &req.coin) || !increases {
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
    fn rest_order(&self, placer: Placer, req: &Order, at: (Decimal, Decimal)) -> Result<u64> {
        self.rest_order_at(placer, req, at, None)
    }

    /// Rest an order. `carry` is `Some` when this continues a marketable order's
    /// partial take: the remainder keeps the order's oid, `origSz` stays the
    /// whole order, and the take's filled size/notional ride along (so the
    /// order's terminal average is its true one). `None` allocates a fresh oid
    /// for a brand-new resting order.
    fn rest_order_at(
        &self,
        placer: Placer,
        req: &Order,
        at: (Decimal, Decimal),
        carry: Option<RestCarry>,
    ) -> Result<u64> {
        let (px, sz) = at;
        let account = placer.address;
        let (oid, orig_sz, filled_sz, filled_ntl) = match carry {
            Some(c) => (c.oid, req.size, c.filled_sz, c.filled_ntl),
            None => (self.next_oid()?, sz, Decimal::ZERO, Decimal::ZERO),
        };
        let order = RestingOrder {
            oid,
            account,
            coin: req.coin.clone(),
            side: req.side(),
            px,
            sz,
            orig_sz,
            reduce: req.reduce(),
            tif: req.tif,
            cloid: req.cloid,
            trigger: req.trigger,
            placed_ms: crate::now_ms(),
            filled_sz,
            filled_ntl,
            builder: placer.builder,
            spot_hold: None,
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
    pub(crate) fn cancel(
        &self,
        account: Address,
        coin: &str,
        oid: u64,
        speed: CancelSpeed,
    ) -> Result<()> {
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

    /// A trigger that fires ends its own oid with the venue's `triggered`
    /// status (docs' order-status list); the fill belongs to the resulting
    /// market order, which carries the trigger's cloid. Serving `canceled`
    /// here would read as the user's cancel on every fired stop.
    pub(crate) fn mark_triggered(&self, account: Address, coin: &str, oid: u64) -> Result<()> {
        self.cancel_with(
            account,
            coin,
            oid,
            CancelMode {
                status: TerminalStatus::Triggered,
                speed: CancelSpeed::Normal,
            },
        )
    }

    /// The cancel's one home: move the resting order to the terminal history
    /// under the given status.
    fn cancel_with(&self, account: Address, coin: &str, oid: u64, mode: CancelMode) -> Result<()> {
        let CancelMode { status, speed } = mode;
        self.with_state(|st| match st.resting.get(&oid) {
            Some(r) if r.account == account && r.coin == coin => {
                if matches!(speed, CancelSpeed::Fast) && r.trigger.is_some() {
                    return Err(Error::bad_request(
                        "Fast cancel cannot cancel a trigger order.",
                    ));
                }
                let Some(done) = st.resting.remove(&oid) else {
                    return Ok(());
                };
                // A spot order's reservation releases with the order: the
                // hold is the account's, keyed by the token the order stored.
                if let Some(h) = done.spot_hold {
                    if let Some(account) = st.accounts.get_mut(&done.account) {
                        if let Some(entry) = account.spot.get_mut(&h.token) {
                            entry.hold -= h.amount;
                            if entry.hold < Decimal::ZERO {
                                entry.hold = Decimal::ZERO;
                            }
                        }
                    }
                }
                terminalize(
                    &mut st.terminal,
                    done,
                    status,
                    self.terminal_orders,
                );
                Ok(())
            }
            _ => Err(Error::bad_request(
                "Order was never placed, already canceled, or filled.",
            )),
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
    pub(crate) fn resting_by_oid(
        &self,
        account: Address,
        oid: u64,
    ) -> Result<Option<RestingOrder>> {
        self.with_state(|st| {
            Ok(st
                .resting
                .get(&oid)
                .filter(|r| r.account == account)
                .cloned())
        })
    }

    pub(crate) fn terminal_by_oid(
        &self,
        account: Address,
        oid: u64,
    ) -> Result<Option<TerminalOrder>> {
        self.with_state(|st| {
            Ok(st
                .terminal
                .get(&oid)
                .filter(|t| t.order.account == account)
                .cloned())
        })
    }

    /// The account's open orders and its terminal history: what
    /// `historicalOrders` serves (the handler puts them newest first).
    pub(crate) fn order_history(
        &self,
        account: Address,
    ) -> Result<(Vec<RestingOrder>, Vec<TerminalOrder>)> {
        self.with_state(|st| {
            Ok((
                st.resting
                    .values()
                    .filter(|r| r.account == account)
                    .cloned()
                    .collect(),
                st.terminal
                    .values()
                    .filter(|t| t.order.account == account)
                    .cloned()
                    .collect(),
            ))
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
    pub(crate) fn schedule_cancel(&self, account: Address, when: ScheduleCancel) -> Result<u64> {
        let now = crate::now_ms();
        let t = self.mutate_state(|st| match when {
            ScheduleCancel::Clear => {
                st.scheduled_cancels.remove(&account);
                Ok(0)
            }
            ScheduleCancel::At(t) => {
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
                    .map(|a| {
                        if a.schedule_fire_day == day {
                            a.schedule_fires
                        } else {
                            0
                        }
                    })
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
    ) -> Result<super::ledger::FillEffect> {
        let coin = facts.coin.clone();
        // The venue settles funding at the trade: accrue to now on the
        // *pre-fill* size, then the size change restarts since-change. The
        // all-or-nothing accrual leaves the stamp where it is when a position
        // lacks a live context (the shared rule).
        self.with_account_now(address, feed, |account| {
            let mut probe = account.clone();
            super::account::apply_fill(&mut probe, facts, meta)?;
            let dex = dex_of_coin(&coin);
            let view = super::account::view(&probe, feed, dex)?;
            // The post-fill check reads the same unified fold as placement:
            // the perp summary alone is zero for a unified account whose
            // collateral is entirely spot.
            let available = super::account::available_margin(&probe, feed, dex, &view)?;
            if available < Decimal::ZERO {
                // The venue's own string; the numbers ride the log (a client
                // matching on the vocabulary sees exactly the venue's).
                tracing::warn!(
                    account = %address,
                    coin,
                    needs = %view.margin_summary.total_margin_used,
                    worth = %view.margin_summary.account_value,
                    spot = ?probe.spot,
                    "order refused: insufficient margin"
                );
                return Err(Error::bad_request("Insufficient margin to place order."));
            }
            super::account::apply_fill(account, facts, meta)
        })
    }

    /// The leverage a fill uses: the position's own, else the account's
    /// per-coin setting, else the venue's default for a new position on a
    /// known coin ([`default_leverage`]). A missing live meta is warming and an
    /// unknown asset (or a zero max leverage) is a named refusal: the old 20×
    /// fallback fabricated a margin bound for a coin the venue never
    /// described.
    pub(crate) fn leverage_for(
        &self,
        address: Address,
        coin: &str,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<u32> {
        Ok(self.leverage_setting_for(address, coin, meta)?.leverage)
    }

    /// [`Self::leverage_for`] with the margin mode: a new position opens in
    /// the mode and at the leverage the account last set for the coin (the
    /// venue keeps the setting beyond the position), else the venue's default.
    pub(crate) fn leverage_setting_for(
        &self,
        address: Address,
        coin: &str,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<super::account::LeverageSetting> {
        let state = self.state.lock()?;
        let account = state.accounts.get(&address);
        if let Some(p) = account.and_then(|a| a.positions.get(coin)) {
            return Ok(super::account::LeverageSetting {
                mode: p.leverage,
                leverage: p.leverage_value,
            });
        }
        if let Some(s) = account.and_then(|a| a.leverage_settings.get(coin)) {
            return Ok(super::account::LeverageSetting {
                mode: s.mode,
                leverage: s.leverage.max(1),
            });
        }
        drop(state);
        let meta = meta.ok_or(crate::error::Error::Warming)?;
        let asset = meta
            .asset(coin)
            .ok_or_else(|| crate::error::Error::NotFound {
                what: format!("asset `{coin}` in the live meta (leverage)"),
            })?;
        if asset.max_leverage == 0 {
            return Err(crate::error::Error::NotFound {
                what: format!("a positive max leverage for `{coin}` in the live meta"),
            });
        }
        Ok(default_leverage(coin, meta))
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

/// A resting order's fill-time traits: reduce-only flag, client id, builder.
type RestingTraits = (Reduce, Option<alloy_primitives::B128>, Option<BuilderFee>);

/// One candidate the taker's walk can take: a live level (external liquidity,
/// recorded as a consumption) or a devnet resting order (an internal trade
/// that updates its maker's account).
#[derive(Debug, Clone)]
pub(crate) enum Candidate {
    Live(Consumption),
    Maker {
        oid: u64,
        maker: Address,
        px: Decimal,
        sz: Decimal,
    },
}

/// The best opposite price the served book shows (the `bbo` in the venue's
/// post-only refusal): the cheapest ask for a buy, the richest bid for a sell.
pub(crate) fn bbo(feed: &MarketFeed, coin: &str, side: Side) -> Option<Decimal> {
    let book = feed.book(coin, crate::now_ms())?;
    book.levels[side.book_index()].first().map(|l| l.px)
}

/// Whether `req` would cross anything the served book (live levels *and*
/// devnet resting orders) currently offers.
pub(crate) fn crosses(feed: &MarketFeed, req: &Order, resting: &[RestingOrder]) -> bool {
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

impl Sim {
    /// The account's signed position on the coin, or `None` when the account
    /// is unknown or flat — the input the minimum-value exception reads.
    fn signed_position(&self, address: Address, coin: &str) -> Result<Option<Decimal>> {
        Ok(self
            .account(address)?
            .and_then(|account| account.positions.get(coin).map(|p| p.szi)))
    }
}

/// The venue's reduce-only cap: an order may only ever reduce the position it
/// names, so a larger size is cut to it (the real venue's capped orders carry
/// `origSz` = requested and `sz` = what the position allowed). Without the cap
/// a flatten sized from a stale read — `size = balance` — walked the requested
/// size and the fill-time guard then refused the *whole* order, so a strategy
/// could not close until a fresh read happened to agree.
fn cap_reduce_only(req: &Order, position_szi: Option<Decimal>) -> Option<Order> {
    let szi = position_szi.filter(|szi| !szi.is_zero())?;
    if !req.reduce_only || req.size <= szi.abs() {
        return None;
    }
    Some(Order {
        size: szi.abs(),
        ..req.clone()
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
    position_szi: Option<Decimal>,
) -> Result<(Vec<Candidate>, Decimal, Decimal)> {
    let capped = cap_reduce_only(req, position_szi);
    let req = capped.as_ref().unwrap_or(req);
    validate_order(feed, meta, req, position_szi)?;
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

/// Where a merged candidate comes from: a live book level, or the devnet's
/// own resting order.
///
/// At an equal price the **live level goes first**: the venue's price-time
/// priority gives the orders already displayed precedence over one that rested
/// afterwards, and a devnet maker that joined an occupied price is behind that
/// queue. Measured 2026-11-22 on `@151`: a maker rested `0.1` behind a live ask
/// of `0.9755`, a `0.1` taker buy filled the **maker** (`crossed: false`) and
/// left the live level untouched — the mirror gave the maker a fill the real
/// queue could not. A maker that rested in an empty price (inside the spread)
/// is the one case where the book level at its price formed later; that maker
/// is then conservatively treated as behind (a sim must not manufacture fills).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    Book,
    Resting,
}

/// The merged opposite side, price-ordered: (px, origin, oid, maker, sz).
fn merge_candidates(
    book: &L2Book,
    req: &Order,
    resting: &[RestingOrder],
) -> Vec<(Decimal, Origin, u64, Address, Decimal)> {
    let side = req.side().book_index();
    let mut candidates: Vec<(Decimal, Origin, u64, Address, Decimal)> = book.levels[side]
        .iter()
        .map(|l| (l.px, Origin::Book, 0u64, Address::ZERO, l.sz))
        .collect();
    for r in resting
        .iter()
        .filter(|r| r.coin == book.coin && r.side != req.side() && r.trigger.is_none())
    {
        candidates.push((r.px, Origin::Resting, r.oid, r.account, r.sz));
    }
    candidates.sort_by(|a, b| {
        let by_px = match req.side() {
            Side::Buy => a.0.cmp(&b.0),
            Side::Sell => b.0.cmp(&a.0),
        };
        // Equal price: the live level first (it was displayed before the devnet
        // order rested), then the devnet makers in time order (oid ascending).
        by_px
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    candidates
}

/// Walk a book in price order: asks ascending for a buy, bids descending for
/// a sell; the limit stops the walk. Returns (the consumed candidates, the
/// filled size, the size-weighted average).
fn walk(
    book: &L2Book,
    req: &Order,
    resting: &[RestingOrder],
) -> Result<(Vec<Candidate>, Decimal, Decimal)> {
    let candidates = merge_candidates(book, req, resting);
    let mut remaining = req.size;
    let mut notional = Decimal::ZERO;
    let mut legs = Vec::new();
    for (px, origin, oid, maker, sz) in candidates {
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
        if origin == Origin::Resting {
            legs.push(Candidate::Maker {
                oid,
                maker,
                px,
                sz: take,
            });
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
        let feed = live_feed(book, meta);
        // The venue has answered with an empty cap set (known, nothing capped).
        feed.put_oi_caps(Vec::new());
        feed
    }

    /// The same feed before `perpsAtOpenInterestCap` has answered: the cap
    /// set is unknown.
    fn live_feed(book: L2Book, meta: Meta) -> MarketFeed {
        let feed = MarketFeed::memory(vec!["ETH".to_string()]);
        // Honest stamps: the lane's freshness gate reads the ctx age, and a
        // fixture stamped at the epoch would read as permanently stale.
        let now = crate::now_ms();
        feed.put_meta(meta, now);
        let mark = book.levels[1].first().map(|l| l.px).unwrap_or(Decimal::ONE);
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
                dex: None,
                amount: amount.parse().unwrap(),
            },
        )
        .unwrap();
        (sim, a)
    }

    /// An IOC order on ETH.
    fn ioc(side: Side, sz: &str) -> Order {
        Order {
            coin: "ETH".into(),
            is_buy: side.is_buy(),
            size: sz.parse().unwrap(),
            limit_px: None,
            tif: Tif::Ioc,
            reduce_only: false,
            cloid: None,
            trigger: None,
        }
    }

    /// The same, reduce-only.
    fn reduce(side: Side, sz: &str) -> Order {
        Order {
            reduce_only: true,
            ..ioc(side, sz)
        }
    }

    fn spot_token(name: &str, index: u32, sz_decimals: u8) -> crate::wire::SpotToken {
        crate::wire::SpotToken {
            name: name.into(),
            sz_decimals,
            wei_decimals: 8,
            index,
            is_canonical: true,
            evm_contract: None,
            token_id: None,
            full_name: None,
            deployer_trading_fee_share: Decimal::ZERO,
        }
    }

    /// A spot feed: `PURR/USDC` (pair index 0, tokens `[PURR=1, USDC=0]`), a
    /// two-sided book at 0.074/0.075 and the venue's `allMids` value.
    fn spot_feed() -> MarketFeed {
        let now = crate::now_ms();
        let feed = MarketFeed::memory(Vec::new()).with_spot_markets(vec!["PURR/USDC".into()]);
        feed.put_spot_meta(crate::wire::SpotMeta {
            tokens: vec![spot_token("USDC", 0, 8), spot_token("PURR", 1, 2)],
            universe: vec![crate::wire::SpotPair {
                tokens: [1, 0],
                name: "PURR/USDC".into(),
                index: 0,
                is_canonical: true,
            }],
        });
        feed.put_mids(
            std::collections::BTreeMap::from([("PURR/USDC".into(), Decimal::new(75, 3))]),
            now,
        );
        feed.put_book(
            book("PURR/USDC", &[("0.074", "5000")], &[("0.075", "5000")]),
            now,
        );
        feed
    }

    fn spot_order(side: Side, sz: &str, px: Option<&str>) -> Order {
        Order {
            coin: "PURR/USDC".into(),
            is_buy: side.is_buy(),
            size: sz.parse().unwrap(),
            limit_px: px.map(|p| p.parse().unwrap()),
            tif: Tif::Ioc,
            reduce_only: false,
            cloid: None,
            trigger: None,
        }
    }

    /// A spot account funded with `usdc` (spot token 0).
    fn spot_account(feed: &MarketFeed, usdc: &str) -> (Sim, Address) {
        let sim = Sim::memory();
        let a = Address::repeat_byte(0x5b);
        sim.deal(
            feed,
            &Deal {
                address: a,
                class: DealClass::Spot,
                coin: None,
                dex: None,
                amount: usdc.parse().unwrap(),
            },
        )
        .unwrap();
        (sim, a)
    }

    /// **A spot buy is a token trade, not a position.** The quote leaves the
    /// balance, the base arrives net of the fee (charged in the received
    /// token — the venue's spot rule), `entryNtl` carries the cost basis, and
    /// no perp position exists afterwards.
    #[test]
    fn a_spot_ioc_buy_settles_quote_into_base() {
        let feed = spot_feed();
        let (sim, a) = spot_account(&feed, "1000");
        let placed = sim
            .place(&feed, a, &spot_order(Side::Buy, "1000", Some("0.08")))
            .expect("the book has size at 0.075");
        let fill = placed.fill.expect("a taker fill");
        assert_eq!(fill.avg_px, Decimal::new(75, 3));
        assert_eq!(fill.total_sz, Decimal::from(1000));

        let account = sim.account(a).unwrap().expect("the account exists");
        let purr = account.spot.get(&1).expect("base credited");
        // 75 USD of notional at 0.070 % is 0.0525 USD = 0.7 PURR at 0.075.
        assert_eq!(purr.total, Decimal::from(1000) - Decimal::new(7, 1));
        assert_eq!(purr.entry_ntl, Decimal::from(75));
        let usdc = account.spot.get(&0).expect("quote exists");
        assert_eq!(usdc.total, Decimal::from(1000) - Decimal::from(75));
        assert!(
            account.positions.is_empty(),
            "a spot fill never opens a perp position"
        );
        let fill = account.fills.back().expect("the fill is in the ledger");
        assert_eq!(fill.dir, "Buy");
        assert_eq!(fill.fee_token, "PURR");
        // **The fee and its token agree in units.** 1000 PURR at 7 bp is 0.7 PURR
        // received-token units; recording the quote value (70 USDC) under `PURR`
        // made every spot buy's cost read ~price× too large (2026-11-22).
        assert_eq!(fill.fee.round_dp(6), Decimal::new(7, 1), "the recorded fee is 0.7 PURR, not 70 USDC");
        assert_eq!(fill.closed_pnl, Decimal::ZERO, "spot realizes nothing at the trade");
    }

    /// A sell spends the base it owns and credits the quote net of the fee;
    /// the cost basis shrinks pro rata.
    #[test]
    fn a_spot_ioc_sell_realizes_into_quote_and_shrinks_the_basis() {
        let feed = spot_feed();
        let (sim, a) = spot_account(&feed, "1000");
        sim.place(&feed, a, &spot_order(Side::Buy, "1000", Some("0.08")))
            .expect("buy first");
        sim.place(&feed, a, &spot_order(Side::Sell, "500", Some("0.07")))
            .expect("the bid has size at 0.074");

        let account = sim.account(a).unwrap().expect("the account exists");
        let purr = account.spot.get(&1).unwrap();
        // 999.3 PURR carried in (the buy's fee was charged in the base), less 500.
        assert_eq!(purr.total, Decimal::new(4993, 1));
        let expected_basis = (Decimal::from(75) * Decimal::new(4993, 1) / Decimal::new(9993, 1))
            .round_dp(6);
        assert_eq!(
            purr.entry_ntl.round_dp(6),
            expected_basis,
            "the basis leaves pro rata with the size"
        );
        let usdc = account.spot.get(&0).unwrap();
        // 500 × 0.074 = 37.0, fee 0.0259 → 36.9741 credited.
        assert_eq!(*usdc, {
            let mut expected = usdc.clone();
            expected.total = Decimal::from(1000) - Decimal::from(75) + Decimal::new(369_741, 4);
            expected
        });
        let fill = account.fills.back().expect("the fill is in the ledger");
        assert_eq!(fill.dir, "Sell");
        assert_eq!(fill.fee_token, "USDC");
    }

    /// A sell with no base balance is refused by name, not filled short (spot
    /// has no short leg).
    #[test]
    fn a_spot_sell_without_the_base_balance_is_refused() {
        let feed = spot_feed();
        let (sim, a) = spot_account(&feed, "1000");
        let err = sim
            .place(&feed, a, &spot_order(Side::Sell, "1000", Some("0.07")))
            .expect_err("no PURR to sell");
        assert!(err.to_string().contains("Insufficient PURR"), "{err}");
    }

    /// A non-crossing spot Gtc rests with its balance **held**: a buy
    /// reserves quote at its limit (the venue's spot `hold`), and a cancel
    /// releases it exactly.
    #[test]
    fn a_resting_spot_buy_holds_its_quote_and_cancel_releases_it() {
        let feed = spot_feed();
        let (sim, a) = spot_account(&feed, "1000");
        let mut req = spot_order(Side::Buy, "1000", Some("0.07"));
        req.tif = Tif::Gtc;
        let placed = sim.place(&feed, a, &req).expect("below the bid, it rests");
        let oid = placed.resting.expect("the remainder rests");
        let account = sim.account(a).unwrap().expect("the account");
        let usdc = account.spot.get(&0).unwrap();
        assert_eq!(usdc.total, Decimal::from(1000), "a hold reserves, it does not debit");
        assert_eq!(usdc.hold, Decimal::from(70), "1000 PURR at 0.070 reserves 70 USDC");
        sim.cancel(a, "PURR/USDC", oid, CancelSpeed::Normal).expect("cancel");
        let account = sim.account(a).unwrap().expect("the account");
        let usdc = account.spot.get(&0).unwrap();
        assert_eq!(usdc.hold, Decimal::ZERO, "the cancel releases the hold");
    }

    /// A resting spot sell is filled by another account's crossing IOC: the
    /// maker's hold converts into the quote proceeds net of the **maker** fee
    /// (0.040 %), its ledger carries dir Sell / feeToken USDC, and the taker
    /// receives the base net of the taker fee charged in the received token.
    #[test]
    fn a_resting_spot_sell_fills_as_the_maker_of_a_crossing_ioc() {
        let feed = spot_feed();
        let (sim, a) = spot_account(&feed, "1000");
        // A gets base to sell: buy 999.3 PURR first (IOC at the ask).
        sim.place(&feed, a, &spot_order(Side::Buy, "1000", Some("0.08")))
            .expect("the ask has size");
        // Rests inside the spread (below the 0.075 ask, above the 0.074 bid):
        // a worse price would never be hit.
        let mut sell = spot_order(Side::Sell, "500", Some("0.0745"));
        sell.tif = Tif::Gtc;
        let placed = sim.place(&feed, a, &sell).expect("inside the spread, it rests");
        let oid = placed.resting.expect("rests");
        let account = sim.account(a).unwrap().expect("the account");
        assert_eq!(account.spot.get(&1).unwrap().hold, Decimal::from(500));

        // B crosses it at the maker's price.
        let b = Address::repeat_byte(0x5c);
        sim.deal(
            &feed,
            &Deal {
                address: b,
                class: DealClass::Spot,
                coin: None,
                dex: None,
                amount: "1000".parse().unwrap(),
            },
        )
        .unwrap();
        let placed = sim
            .place(&feed, b, &spot_order(Side::Buy, "500", Some("0.0745")))
            .expect("crosses the resting sell");
        assert_eq!(placed.fill.expect("filled").total_sz, Decimal::from(500));

        // The maker: quote credited net of 0.040 % (37.25 × 0.9996 = 37.2351),
        // the base hold fully released into the fill, the order gone.
        let account = sim.account(a).unwrap().expect("the account");
        let purr = account.spot.get(&1).unwrap();
        let usdc = account.spot.get(&0).unwrap();
        assert_eq!(purr.hold, Decimal::ZERO);
        assert_eq!(usdc.total, Decimal::from(925) + Decimal::new(372_351, 4));
        assert_eq!(purr.total, Decimal::new(9993, 1) - Decimal::from(500));
        let fill = account.fills.back().unwrap();
        assert_eq!(fill.dir, "Sell");
        assert_eq!(fill.fee_token, "USDC");
        assert!(sim.resting_orders(None).unwrap().iter().all(|r| r.oid != oid));
    }

    /// A buy maker's base credit **creates** the balance entry: an account
    /// that never held the base gets its served name (`coin: "PURR"`), not an
    /// empty row — the maker path used to insert `{"coin": "", "token": 1}`
    /// (measured live 2026-11-23). The economics are the venue's: the maker
    /// buys 500 PURR at 0.0745, pays exactly the notional, and the 0.040 %
    /// fee arrives net in the received token (0.2 PURR).
    #[test]
    fn a_spot_maker_credit_names_the_token_it_creates() {
        let feed = spot_feed();
        let (sim, a) = spot_account(&feed, "1000");
        let mut buy = spot_order(Side::Buy, "500", Some("0.0745"));
        buy.tif = Tif::Alo;
        let placed = sim.place(&feed, a, &buy).expect("inside the spread, it rests");
        let oid = placed.resting.expect("rests");

        // B crosses it as a taker sell (it buys base off the book first).
        let b = Address::repeat_byte(0x5d);
        sim.deal(
            &feed,
            &Deal {
                address: b,
                class: DealClass::Spot,
                coin: None,
                dex: None,
                amount: "1000".parse().unwrap(),
            },
        )
        .unwrap();
        sim.place(&feed, b, &spot_order(Side::Buy, "1000", Some("0.08")))
            .expect("the ask has size");
        let placed = sim
            .place(&feed, b, &spot_order(Side::Sell, "500", Some("0.0745")))
            .expect("crosses the resting bid");
        assert_eq!(placed.fill.expect("filled").total_sz, Decimal::from(500));

        let account = sim.account(a).unwrap().expect("the account");
        let purr = account.spot.get(&1).expect("the credit creates the base entry");
        assert_eq!(purr.coin, "PURR", "the served name rides the credit");
        assert_eq!(purr.hold, Decimal::ZERO);
        // 500 − 37.25 × 0.0004 / 0.0745 = 499.8 PURR; USDC debited 37.25.
        assert_eq!(purr.total, Decimal::from(500) - Decimal::new(2, 1));
        let usdc = account.spot.get(&0).unwrap();
        assert_eq!(usdc.total, Decimal::from(1000) - Decimal::new(3_725, 2));
        let fill = account.fills.back().unwrap();
        assert_eq!(fill.dir, "Buy");
        assert_eq!(fill.fee_token, "PURR");
        assert!(sim.resting_orders(None).unwrap().iter().all(|r| r.oid != oid));
    }

    /// **Post-only on a spot pair is the maker's promise.** An ALO that would
    /// take is refused with the venue's string (it must never pay the 7 bp
    /// taker fee by accident), and a non-crossing one rests with its balance
    /// held exactly like a Gtc — with the same self-cross refusal.
    #[test]
    fn a_spot_post_only_order_refuses_to_take_and_rests_when_passive() {
        let feed = spot_feed();
        let (sim, a) = spot_account(&feed, "1000");
        let mut taking = spot_order(Side::Buy, "100", Some("0.075"));
        taking.tif = Tif::Alo;
        let err = sim.place(&feed, a, &taking).expect_err("the ask is 0.075");
        assert!(
            err.to_string()
                .contains("Post only order would have immediately matched"),
            "{err}"
        );
        let mut passive = spot_order(Side::Buy, "1000", Some("0.0745"));
        passive.tif = Tif::Alo;
        let placed = sim.place(&feed, a, &passive).expect("inside the spread it rests");
        assert!(placed.fill.is_none(), "a post-only order never takes");
        let oid = placed.resting.expect("it rests");
        let usdc = sim.account(a).unwrap().unwrap().spot.get(&0).cloned().unwrap();
        assert_eq!(usdc.hold, Decimal::new(745, 1), "1000 × 0.0745 held");
        // Crossing one's own resting buy is a match, so the sell is refused.
        let mut own_cross = spot_order(Side::Sell, "100", Some("0.0745"));
        own_cross.tif = Tif::Alo;
        let err = sim.place(&feed, a, &own_cross).expect_err("crosses its own buy");
        assert!(
            err.to_string()
                .contains("Post only order would have immediately matched"),
            "{err}"
        );
        assert!(
            sim.resting_by_oid(a, oid).unwrap().is_some(),
            "the refused sell did not disturb the resting buy"
        );
    }

    /// The venue's $10 minimum applies to spot too.
    #[test]
    fn a_spot_order_below_the_minimum_value_is_refused() {
        let feed = spot_feed();
        let (sim, a) = spot_account(&feed, "1000");
        let err = sim
            .place(&feed, a, &spot_order(Side::Buy, "10", Some("0.08")))
            .expect_err("10 PURR is 0.75 USD");
        assert!(err.to_string().contains("too small"), "{err}");
    }

    /// A fill's leverage is the position's, the account's setting, or the
    /// live asset's max — never a fabricated 20× when the meta is missing or
    /// the coin is unknown.
    #[test]
    fn leverage_requires_the_live_meta_for_a_flat_account() {
        let feed = feed_with_book(
            book("ETH", &[("100", "2")], &[("101", "2")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        assert_eq!(
            sim.leverage_for(a, "ETH", feed.meta().as_ref()).unwrap(),
            10
        );
        let err = sim
            .leverage_for(a, "NOPE", feed.meta().as_ref())
            .unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }), "{err}");
        let err = sim.leverage_for(a, "ETH", None).unwrap_err();
        assert!(matches!(err, Error::Warming), "{err}");
    }

    /// An order for an account the sim has never seen is refused by name: a
    /// resting order with no margin check would later fill unmargined.
    #[test]
    fn an_order_for_an_unsimulated_account_is_refused() {
        let feed = feed_with_book(
            book("ETH", &[("100", "2")], &[("101", "2")]),
            meta_with("ETH", 10),
        );
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        let order = Order {
            limit_px: Some("100".parse().unwrap()),
            tif: Tif::Gtc,
            ..ioc(Side::Buy, "1")
        };
        let err = sim.place(&feed, a, &order).unwrap_err();
        assert!(matches!(err, Error::BadRequest { .. }), "{err}");
        assert!(
            err.to_string().contains("not simulated"),
            "the refusal names the missing account: {err}"
        );
    }

    /// A resting order whose account vanished is never re-margined as if it
    /// had margin: the match-time check cancels it instead.
    #[test]
    fn a_resting_order_without_an_account_is_not_remargined() {
        let feed = feed_with_book(
            book("ETH", &[("100", "2")], &[("101", "2")]),
            meta_with("ETH", 10),
        );
        let sim = Sim::memory();
        let resting = RestingOrder {
            oid: 1,
            account: Address::repeat_byte(0xaa),
            coin: "ETH".into(),
            side: Side::Buy,
            px: "100".parse().unwrap(),
            sz: "1".parse().unwrap(),
            orig_sz: "1".parse().unwrap(),
            reduce: Reduce::Any,
            tif: Tif::Gtc,
            cloid: None,
            trigger: None,
            placed_ms: 0,
            filled_sz: Decimal::ZERO,
            filled_ntl: Decimal::ZERO,
            builder: None,
            spot_hold: None,
        };
        assert!(!sim.remargin_ok(&feed, &resting).unwrap());
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
            .order(
                &feed,
                a,
                &Order {
                    coin: "NOPE".into(),
                    ..ioc(Side::Buy, "1")
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }), "{err:?}");

        let err = sim
            .order(
                &feed,
                a,
                &Order {
                    coin: "SOL".into(),
                    ..ioc(Side::Buy, "1")
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::Warming), "{err:?}");
    }

    /// I3: the fill moves the book and the position, never the mark.
    #[test]
    fn a_fill_moves_the_book_never_the_mark() {
        let feed = feed_with_book(
            book("ETH", &[("100", "2")], &[("101", "2")]),
            meta_with("ETH", 10),
        );
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
        // The identity the consumer gates on stays exact.
        let served =
            crate::sim::account::clearinghouse(&account, &feed, "", 0)
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
        assert_eq!(
            served.levels[0][0].sz,
            "5".parse().unwrap(),
            "the bid is live"
        );
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
        let feed = feed_with_book(
            book("ETH", &[], &[("101", "1"), ("102", "1")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        // The live book refreshes 101 with a bigger size: the delta persists
        // (served = live − consumed) and the untouched 102 refreshes verbatim.
        feed.put_book(book("ETH", &[], &[("101", "3"), ("102", "7")]), 10);
        let served = feed.book("ETH", 0).unwrap();
        assert_eq!(
            served.levels[1][0].sz,
            "2".parse().unwrap(),
            "3 live − 1 consumed"
        );
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
        assert_eq!(
            feed.book("ETH", 0).unwrap().levels[1][0].sz,
            "1".parse().unwrap()
        );
        feed.clear_book("ETH").unwrap();
        assert_eq!(
            feed.book("ETH", 0).unwrap().levels[1][0].sz,
            "2".parse().unwrap()
        );
        assert_eq!(feed.owned_levels(), 0);
    }

    /// A reducing fill realizes its PnL into the perp balance.
    #[test]
    fn a_reduce_realizes_pnl() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        // The test is the PnL identity, not funding: the fills settle funding
        // (a sub-nano term with the fixture's tiny rate) — pin it to zero so
        // the assertion is exact.
        feed.own_funding("ETH", Decimal::ZERO, None).unwrap();
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "2")).unwrap();
        let before = sim.account(a).unwrap().unwrap().perp_balance("");
        // Sell 1 at 100: closed 1·(100 − 101) = −1.
        sim.order(&feed, a, &reduce(Side::Sell, "1")).unwrap();
        let after = sim.account(a).unwrap().unwrap().perp_balance("");
        // Realized −1, minus the reduce's own taker fee (100 notional × 4.5 bps).
        let fee = Decimal::from(100) * taker_fee();
        assert_eq!(
            after,
            before - Decimal::ONE - fee,
            "realized −1 net of the fee"
        );
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
        let feed = feed_with_book(
            book("ETH", &[("100", "1")], &[("101", "1")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let mut o = ioc(Side::Buy, "1");
        o.limit_px = Some("100.5".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(
            format!("{err}").contains("could not immediately match"),
            "{err}"
        );
        let mut o = ioc(Side::Sell, "1");
        o.limit_px = Some("100.5".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(
            format!("{err}").contains("could not immediately match"),
            "{err}"
        );
    }

    /// A stale live book refuses by name; a scripted book for the same coin
    /// is the scenario's present and fills.
    #[test]
    fn a_stale_live_book_refuses_but_a_scripted_one_fills() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        // Age the live book (and its ctx) past the shared window. The book's
        // own stamp is the gate: a REST-only book stall leaves the ctx lane
        // advancing, so a ctx-only age would not refuse the stale book.
        let old = crate::now_ms() - MarketFeed::MAX_STALE_MS - 1_000;
        feed.put_ctx("ETH".into(), crate::wire::test_ctx("100", "100"), old);
        feed.put_book(book("ETH", &[("100", "5")], &[("101", "5")]), old);
        let err = sim.order(&feed, a, &ioc(Side::Buy, "0.1")).unwrap_err();
        assert!(format!("{err}").contains("stale"), "{err}");
        // The scenario scripts the coin: its present is never stale.
        feed.own_book(&book("ETH", &[("100", "5")], &[("101", "5")]))
            .unwrap();
        let fill = sim.order(&feed, a, &ioc(Side::Buy, "0.1")).unwrap();
        assert_eq!(fill.total_sz, "0.1".parse().unwrap());
    }

    /// The venue settles funding at the trade: a fill accrues to now on the
    /// *pre-fill* size and restarts `cumFunding.sinceChange`.
    #[test]
    fn a_fill_settles_funding_and_restarts_since_change() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        // The shared fixture is rate-free (see `test_ctx`); this test is
        // about funding, so its ctx carries the published hourly rate.
        feed.put_ctx(
            "ETH".into(),
            crate::wire::funding_ctx("101", "101"),
            crate::now_ms(),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "0.1")).unwrap();
        // An hour passes, then a second fill: the pre-fill size pays the
        // hour, and the change restarts since-change.
        sim.advance(&feed, 3600, 1).unwrap();
        sim.order(&feed, a, &ioc(Side::Buy, "0.1")).unwrap();
        let p = sim
            .account(a)
            .unwrap()
            .unwrap()
            .positions
            .remove("ETH")
            .unwrap();
        assert_ne!(p.funding_all_time, Decimal::ZERO, "the hour was paid");
        assert_eq!(
            p.funding_since_change,
            Decimal::ZERO,
            "the size change restarts since-change"
        );
        assert_ne!(
            p.funding_since_open,
            Decimal::ZERO,
            "since-open keeps accruing"
        );
    }

    /// The venue's tick/lot rules and the $10 minimum refuse by name; an
    /// integer price of any width is always valid.
    #[test]
    fn tick_lot_and_minimum_refuse_by_name() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 4),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        // 5 decimals > szDecimals 4.
        let err = sim.order(&feed, a, &ioc(Side::Buy, "0.00001")).unwrap_err();
        assert!(format!("{err}").contains("szDecimals"), "{err}");
        // 3 decimals > 6 − 4: the venue's tick-size refusal.
        let mut o = ioc(Side::Buy, "1");
        o.limit_px = Some("100.123".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("divisible by tick size"), "{err}");
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

    /// The $10 minimum **exempts a reduce-only order that fully closes the
    /// position** (exchange docs: "an order that exactly closes a position
    /// with reduce-only bypasses this minimum"). Measured live 2026-11-22: a
    /// `0.006 ETH` long worth `$9.98` could not be sold — the mirror stranded a
    /// position the real venue lets out, and a dust liquidation would be
    /// refused by the book and escalate to the vault.
    #[test]
    fn a_reduce_only_full_close_below_the_minimum_is_accepted() {
        let feed = feed_with_book(
            book("ETH", &[("98.6", "5")], &[("98.8", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "100");
        // 0.1013 × 98.8 = 10.0104 ≥ $10 to open; at the bid it is 9.988.
        let mut open = ioc(Side::Buy, "0.1013");
        open.limit_px = Some("98.8".parse().unwrap());
        sim.order(&feed, a, &open).expect("the open clears the minimum");
        // A partial reduce-only is not a full close: the minimum still binds.
        let mut partial = ioc(Side::Sell, "0.05");
        partial.reduce_only = true;
        partial.limit_px = Some("98.6".parse().unwrap());
        let err = sim.order(&feed, a, &partial).unwrap_err();
        assert!(format!("{err}").contains("minimum"), "{err}");
        // A full close without the reduce-only flag is not the exempt shape.
        let mut close_any = ioc(Side::Sell, "0.1013");
        close_any.limit_px = Some("98.6".parse().unwrap());
        let err = sim.order(&feed, a, &close_any).unwrap_err();
        assert!(format!("{err}").contains("minimum"), "{err}");
        // The full reduce-only close is accepted and leaves the book flat.
        let mut close = ioc(Side::Sell, "0.1013");
        close.reduce_only = true;
        close.limit_px = Some("98.6".parse().unwrap());
        let fill = sim.order(&feed, a, &close).expect("the exempt close fills");
        assert_eq!(fill.total_sz, "0.1013".parse().unwrap());
        assert!(
            sim.account(a)
                .unwrap()
                .unwrap()
                .positions
                .get("ETH")
                .is_none_or(|p| p.szi.is_zero()),
            "the position is flat"
        );
    }

    /// **A reduce-only order larger than the position is capped to it, not
    /// refused.** The real venue caps the close (its status carries `origSz` =
    /// requested, `sz` = what the position allowed); the mirror used to walk
    /// the requested size and then refuse the whole fill
    /// (`fill.size > old.abs()`), so a flatten that sized from a stale read
    /// could never complete. Now the walk sees the capped size, the position
    /// closes exactly, and nothing flips.
    #[test]
    fn a_reduce_only_order_larger_than_the_position_is_capped_to_it() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "0.1")).unwrap();
        let mut oversized = reduce(Side::Sell, "0.5");
        oversized.limit_px = Some("100".parse().unwrap());
        let fill = sim
            .order(&feed, a, &oversized)
            .expect("the capped close fills");
        assert_eq!(
            fill.total_sz,
            "0.1".parse().unwrap(),
            "the venue caps at the position"
        );
        assert!(
            sim.account(a)
                .unwrap()
                .unwrap()
                .positions
                .get("ETH")
                .is_none_or(|p| p.szi.is_zero()),
            "flat, never short"
        );
        // The same oversize in the position's own direction still refuses:
        // capping must not turn an increase into a close.
        sim.order(&feed, a, &ioc(Side::Buy, "0.1")).unwrap();
        let err = sim
            .order(&feed, a, &reduce(Side::Buy, "0.5"))
            .unwrap_err();
        assert!(
            format!("{err}").contains("Reduce only order would increase position."),
            "{err}"
        );
    }

    /// **A marketable GTC is refused whole, before it trades a slice.** A GTC
    /// sized past the account's margin but crossing real depth used to take
    /// its fill and *then* fail the remainder's margin check, so the client saw
    /// "Insufficient margin to place order." after being filled (measured
    /// 2026-11-23: a 120-lot SNDK GTC took 11.988 and errored). The venue
    /// refuses an order it cannot carry without trading any of it: no fill, no
    /// position, the balance untouched.
    #[test]
    fn a_marketable_gtc_is_refused_whole_before_it_trades() {
        // $10 of margin at 10x carries ~$100 of ETH at ~101: ~0.99 units. The
        // ask depth is 5 units, so a 3-unit GTC crosses ~$303 and cannot be
        // carried whole -- but it would fill ~1 unit before hitting the wall.
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "10");
        let mut gtc = Order {
            coin: "ETH".into(),
            is_buy: true,
            size: "3".parse().unwrap(),
            limit_px: Some("101".parse().unwrap()),
            tif: Tif::Gtc,
            reduce_only: false,
            cloid: None,
            trigger: None,
        };
        let err = sim.place(&feed, a, &gtc).unwrap_err();
        assert!(
            format!("{err}").contains("Insufficient margin to place order."),
            "refused up front: {err}"
        );
        let account = sim.account(a).unwrap().unwrap();
        assert!(
            account.positions.get("ETH").is_none_or(|p| p.szi.is_zero()),
            "no slice traded before the refusal"
        );
        assert_eq!(
            account.perp_balance(""),
            "10".parse().unwrap(),
            "the balance is untouched"
        );
        // A GTC the account *can* carry the whole of still fills part and rests
        // the rest under one oid (the venue's rule, unchanged).
        gtc.size = "0.5".parse().unwrap();
        let placed = sim.place(&feed, a, &gtc).unwrap();
        assert!(placed.fill.is_some(), "the affordable GTC takes its slice");
    }

    /// **An order that never rests still ends, visibly.** A full IOC take left
    /// no terminal row, so `orderStatus` could not answer for it and
    /// `orderUpdates` never pushed `filled` (measured 2026-11-23: an IOC fill
    /// produced zero `orderUpdates` frames while its `userFills` fired). The
    /// venue records it terminal; the mirror now does too -- `Filled` for a
    /// full take, `Canceled` for an IOC's unfilled remainder.
    #[test]
    fn an_order_that_never_rests_is_terminal_and_visible() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let placed = sim.place(&feed, a, &ioc(Side::Buy, "0.1")).unwrap();
        let fill = placed.fill.expect("the IOC takes the visible depth");
        let terminal = sim
            .terminal_by_oid(a, fill.oid)
            .unwrap()
            .expect("a full IOC take is terminal, not invisible");
        assert_eq!(terminal.status, TerminalStatus::Filled);
        assert_eq!(terminal.filled_sz, "0.1".parse().unwrap());
        assert!(terminal.avg_px > Decimal::ZERO);
        // An IOC that only partly fills records the unfilled remainder as
        // canceled, with what did fill kept.
        sim.place(&feed, a, &ioc(Side::Sell, "0.1")).unwrap();
        let mut partial = ioc(Side::Buy, "6");
        partial.limit_px = Some("101".parse().unwrap());
        let placed = sim.place(&feed, a, &partial).unwrap();
        let fill = placed.fill.expect("it takes the visible depth");
        assert!(fill.total_sz > Decimal::ZERO && fill.total_sz < "6".parse().unwrap());
        let terminal = sim
            .terminal_by_oid(a, fill.oid)
            .unwrap()
            .expect("the partial IOC is terminal too");
        assert_eq!(terminal.status, TerminalStatus::Canceled);
        assert_eq!(terminal.filled_sz, fill.total_sz);
        assert_eq!(
            terminal.order.sz,
            "6".parse::<Decimal>().unwrap() - fill.total_sz,
            "the unfilled remainder is the canceled size"
        );
    }

    /// **One order, one oid — even when it takes part and rests the rest.** A
    /// marketable GTC used to fill under one oid and rest the remainder under a
    /// second (measured 2026-11-23: fill oid 629, resting oid 630), so
    /// `orderStatus(629)` — the oid handed back to the client — answered
    /// `unknownOid`. The order's take and its remainder are the same order: the
    /// oid is shared, `origSz` is the whole order, `sz` the remainder, and the
    /// carried fill makes the eventual terminal average the order's true one.
    #[test]
    #[allow(clippy::too_many_lines)] // one order's whole lifecycle: take, rest, second-account fill, terminal
    fn a_partial_take_and_its_remainder_are_one_order_one_oid() {
        // Ask depth 0.5 at 101: a 1.0 GTC buy takes 0.5 and rests 0.5.
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "0.5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let gtc = Order {
            coin: "ETH".into(),
            is_buy: true,
            size: "1".parse().unwrap(),
            limit_px: Some("101".parse().unwrap()),
            tif: Tif::Gtc,
            reduce_only: false,
            cloid: None,
            trigger: None,
        };
        let placed = sim.place(&feed, a, &gtc).unwrap();
        let fill = placed.fill.expect("it takes the top level");
        let rest_oid = placed.resting.expect("the remainder rests");
        assert_eq!(
            fill.oid, rest_oid,
            "the take and the remainder share the order's oid"
        );
        assert_eq!(fill.total_sz, "0.5".parse().unwrap());
        let resting = sim
            .resting_by_oid(a, rest_oid)
            .unwrap()
            .expect("orderStatus finds the order by the oid it was handed");
        assert_eq!(resting.sz, "0.5".parse().unwrap(), "the remainder");
        assert_eq!(resting.orig_sz, "1".parse().unwrap(), "the whole order");
        assert_eq!(resting.filled_sz, "0.5".parse().unwrap(), "the take carried");
        // Still open, so it is not terminal yet.
        assert!(sim.terminal_by_oid(a, rest_oid).unwrap().is_none());
        // Another account sells into A's resting remainder (a self-trade would
        // expire it, so the taker must be distinct). A's order fills the rest
        // and terminalizes under the SAME oid with the whole order's size.
        let b = Address::repeat_byte(0xbb);
        sim.deal(
            &feed,
            &Deal {
                address: b,
                class: DealClass::Perp,
                coin: None,
                dex: None,
                amount: "1000".parse().unwrap(),
            },
        )
        .unwrap();
        sim.place(&feed, b, &Order {
            coin: "ETH".into(),
            is_buy: false,
            size: "0.5".parse().unwrap(),
            limit_px: Some("101".parse().unwrap()),
            tif: Tif::Ioc,
            reduce_only: false,
            cloid: None,
            trigger: None,
        })
        .unwrap();
        let terminal = sim
            .terminal_by_oid(a, rest_oid)
            .unwrap()
            .expect("the filled remainder terminalizes under the same oid");
        assert_eq!(terminal.status, TerminalStatus::Filled);
        assert_eq!(terminal.filled_sz, "1".parse().unwrap(), "the whole order");
        assert_eq!(terminal.order.orig_sz, "1".parse().unwrap());
    }

    /// Every refusal is by name: resting orders, reduce-only that would not
    /// reduce, and an opening the account cannot margin.
    #[test]
    fn refusals_are_named() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
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
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
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
        // A reduce-only order reserves nothing (it needs a position to
        // reduce: open one first, then rest the close above the market).
        let opened = sim.order(&feed, a, &ioc(Side::Buy, "0.2")).unwrap();
        assert_eq!(opened.total_sz, "0.2".parse().unwrap());
        let mut close = reduce(Side::Sell, "0.2");
        close.tif = Tif::Gtc;
        close.limit_px = Some("105".parse().unwrap());
        assert!(
            sim.place(&feed, a, &close).is_ok(),
            "reduce-only rests free"
        );
    }

    /// The venue's reduce-only placement rule (docs' error responses, `Order /
    /// ReduceOnly`): an order that cannot reduce the position it names is
    /// refused now, with the venue's string — never a resting order that can
    /// only ever be refused at fill time.
    #[test]
    fn a_reduce_only_order_with_nothing_to_reduce_is_refused() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        // Flat: nothing to reduce, in either direction.
        for side in [Side::Buy, Side::Sell] {
            let mut o = reduce(side, "0.2");
            o.tif = Tif::Gtc;
            o.limit_px = Some("105".parse().unwrap());
            let err = sim.place(&feed, a, &o).unwrap_err();
            assert!(
                format!("{err}").contains("Reduce only order would increase position."),
                "{err}"
            );
        }
        // Long: the same side would increase it; the other side reduces it.
        sim.order(&feed, a, &ioc(Side::Buy, "0.2")).unwrap();
        let mut same_side = reduce(Side::Buy, "0.2");
        same_side.tif = Tif::Gtc;
        same_side.limit_px = Some("99".parse().unwrap());
        let err = sim.place(&feed, a, &same_side).unwrap_err();
        assert!(
            format!("{err}").contains("Reduce only order would increase position."),
            "{err}"
        );
        let mut closing = reduce(Side::Sell, "0.2");
        closing.tif = Tif::Gtc;
        closing.limit_px = Some("105".parse().unwrap());
        assert!(sim.place(&feed, a, &closing).is_ok(), "the close rests");
    }

    /// A resting reduce-only order is canceled, not refused on every tick,
    /// once its position is gone: the venue does not keep an order that can
    /// never reduce anything.
    #[test]
    fn a_resting_reduce_only_order_is_canceled_when_its_position_is_gone() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "0.2")).unwrap();
        // The close rests above the market (inside the ask, so a later book
        // move crosses it).
        let mut close = reduce(Side::Sell, "0.2");
        close.tif = Tif::Gtc;
        close.limit_px = Some("105".parse().unwrap());
        let oid = sim.place(&feed, a, &close).unwrap().resting.unwrap();
        // Another order closes the position while the close still rests.
        sim.order(&feed, a, &ioc(Side::Sell, "0.2")).unwrap();
        // The book crosses the resting close; the tick must cancel it.
        let crossed = book("ETH", &[("106", "5")], &[("108", "5")]);
        let crossed = feed_with_book(crossed, meta_with("ETH", 10));
        sim.run_resting(&crossed, crate::now_ms() + 1).unwrap();
        assert!(
            sim.resting_by_oid(a, oid).unwrap().is_none(),
            "a close with no position must not keep resting"
        );
    }

    /// **A unified account trades against its spot collateral.** The served
    /// perp summary carries the spot-funded margin as cash (the real venue's
    /// `hold + unrealized`), and the order lane's placement and post-fill
    /// checks count the unified spot USDC. Without this, the product's unified
    /// agent — whose note arrives in the spot class — cannot open its perp
    /// hedge at all.
    #[test]
    #[allow(clippy::too_many_lines)] // the two ledgers and the standard arm in one test
    fn a_unified_account_trades_against_its_spot_collateral() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        feed.put_spot_meta(crate::wire::SpotMeta {
            tokens: vec![crate::wire::SpotToken {
                name: "USDC".into(),
                sz_decimals: 8,
                wei_decimals: 8,
                index: 0,
                is_canonical: true,
                evm_contract: None,
                token_id: None,
                full_name: None,
                deployer_trading_fee_share: Decimal::ZERO,
            }],
            universe: vec![],
        });
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        sim.deal(
            &feed,
            &Deal {
                address: a,
                class: DealClass::Spot,
                coin: None,
                dex: None,
                amount: "1000".parse().unwrap(),
            },
        )
        .unwrap();
        sim.set_account(
            a,
            serde_json::from_str(r#"{"userAbstraction": "unifiedAccount"}"#).unwrap(),
        )
        .unwrap();

        // $50 notional at 10× needs $5; the spot collateral covers it.
        let fill = sim.order(&feed, a, &ioc(Side::Buy, "0.5")).unwrap();
        assert_eq!(fill.total_sz, "0.5".parse().unwrap());
        let account = sim.account(a).unwrap().unwrap();
        let view = super::super::account::view(&account, &feed, "").unwrap();
        let hold = super::super::account::unified_hold(&account, &feed, 0).unwrap();
        assert!(
            hold > Decimal::ZERO,
            "the position's margin is held on the spot token"
        );
        assert_eq!(
            view.margin_summary.account_value,
            view.perp_account_value + hold,
            "the served unified summary is the perp cash plus the spot-funded margin"
        );
        assert_eq!(
            account.spot.get(&0).map(|e| e.total).unwrap_or_default(),
            "1000".parse().unwrap(),
            "the spot collateral was not consumed by the perp fill"
        );

        // A standard account's spot holdings are not margin: the same balance
        // cannot open the same perp position.
        let sim = Sim::memory();
        let b = Address::repeat_byte(0xbb);
        sim.deal(
            &feed,
            &Deal {
                address: b,
                class: DealClass::Spot,
                coin: None,
                dex: None,
                amount: "1000".parse().unwrap(),
            },
        )
        .unwrap();
        let err = sim.order(&feed, b, &ioc(Side::Buy, "0.5")).unwrap_err();
        assert!(
            format!("{err}").contains("Insufficient margin to place order."),
            "{err}"
        );
    }

    /// The plan's rule: a resting order is re-margined at match time — a maker
    /// that cannot carry its fill is canceled (the venue's `marginCanceled`),
    /// never filled.
    #[test]
    #[allow(clippy::too_many_lines)] // one transition table: splitting it would hide the rule
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
                dex: None,
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
                dex: None,
                amount: "-990".parse().unwrap(),
            },
        )
        .unwrap();
        // The taker crosses at the maker's price.
        let mut cross = ioc(Side::Sell, "1");
        cross.limit_px = Some("100".parse().unwrap());
        let err = sim.place(&feed, taker, &cross).unwrap_err();
        assert!(
            format!("{err}").contains("could not immediately match"),
            "{err}"
        );
        assert!(
            sim.resting_orders(None)
                .unwrap()
                .iter()
                .all(|r| r.oid != oid),
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
    /// nor does the cancel show up in the trade feed." The taker's own
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
        let oid = sim
            .place(&feed, a, &rest)
            .unwrap()
            .resting
            .expect("a Gtc rests");
        // The same address sells through its own bid: expired, not filled;
        // the served bid behind it takes the fill.
        let sell = ioc(Side::Sell, "1");
        let fill = sim
            .place(&feed, a, &sell)
            .unwrap()
            .fill
            .expect("the served bid fills");
        assert_eq!(fill.total_sz, "1".parse().unwrap());
        assert_eq!(
            fill.avg_px,
            "98".parse().unwrap(),
            "behind the expired order"
        );
        assert!(
            sim.resting_orders(None)
                .unwrap()
                .iter()
                .all(|r| r.oid != oid),
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
        let oid = sim
            .place(&feed, a, &rest)
            .unwrap()
            .resting
            .expect("a Gtc rests");
        // The close: the account's own bid must be expired; the served bid
        // behind it takes the fill.
        let fill = sim
            .liquidate_position(&feed, a, "ETH", "1".parse().unwrap())
            .unwrap();
        assert_eq!(
            fill.avg_px,
            "98".parse().unwrap(),
            "behind the expired order"
        );
        assert!(
            sim.resting_orders(None)
                .unwrap()
                .iter()
                .all(|r| r.oid != oid),
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
        assert!(
            sim.place(&feed, a, &ok).is_ok(),
            "5 × 101 / 10 = 50.5 ≤ 100"
        );
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
        sim.update_isolated_margin(a, "ETH", 200_000_000, &feed)
            .unwrap();
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
        let oid = sim
            .place(&feed, a, &rest)
            .unwrap()
            .resting
            .expect("a Gtc rests");
        let before = sim.account(a).unwrap().unwrap().perp_balance("");
        // The meta flips to delisted (the operator's act; the feed carries it
        // live) — the next tick settles.
        let mut meta = meta_with("ETH", 10);
        meta.universe[0].is_delisted = Some(true);
        feed.put_meta(meta, crate::now_ms());
        sim.evaluate(&feed).unwrap();
        assert!(
            sim.resting_orders(None)
                .unwrap()
                .iter()
                .all(|r| r.oid != oid),
            "the open order is cancelled"
        );
        let account = sim.account(a).unwrap().unwrap();
        assert!(!account.positions.contains_key("ETH"), "settled flat");
        // Entry 101, the oracle 101 (the fixture's ctx): zero PnL, no fee.
        assert_eq!(account.perp_balance(""), before);
        let err = sim.place(&feed, a, &ioc(Side::Buy, "1")).unwrap_err();
        assert!(format!("{err}").contains("no new orders"), "{err}");
    }

    /// The docs' open-order limit (the rate-limits page): the 1000th resting
    /// order is the last; the next refuses by name (the volume bonus is not
    /// modeled — the named simplification above the rule).
    #[test]
    fn the_open_order_limit_refuses_by_name() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
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
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
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

    /// An unknown cap set is not "nothing is capped": the order lane refuses
    /// until the venue's `perpsAtOpenInterestCap` answer lands.
    #[test]
    fn an_unknown_open_interest_cap_set_refuses_orders() {
        let feed = live_feed(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 25),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let err = sim.place(&feed, a, &ioc(Side::Buy, "1")).unwrap_err();
        assert!(
            format!("{err}").contains("open-interest caps are not known"),
            "{err}"
        );
    }

    /// A known empty cap set allows the order (the venue said nothing is
    /// capped).
    #[test]
    fn a_known_empty_open_interest_cap_set_allows_orders() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 25),
        );
        assert_eq!(feed.open_interest_caps(), Some(Vec::new()));
        let (sim, a) = funded_sim(&feed, "1000");
        assert!(sim.place(&feed, a, &ioc(Side::Buy, "1")).is_ok());
    }

    /// The docs' max order value: $30M at ≥25× (a limit may be 10× that).
    #[test]
    fn the_order_value_cap_refuses() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 25),
        );
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
        // The venue's default leverage caps at 20x: the account sets the 40x.
        sim.set_leverage(
            a,
            "ETH",
            crate::sim::account::LeverageSetting {
                mode: crate::wire::LeverageType::Cross,
                leverage: 40,
            },
            feed.meta().as_ref(),
        )
        .unwrap();
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
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        let before = sim.account(a).unwrap().unwrap().perp_balance("");
        // $100 = 100_000_000 at the docs' 6 decimals.
        sim.update_isolated_margin(a, "ETH", 100_000_000, &feed)
            .unwrap();
        let account = sim.account(a).unwrap().unwrap();
        let p = account.positions.get("ETH").unwrap();
        assert_eq!(p.isolated_margin, Decimal::from(100));
        assert_eq!(p.margin_used, Decimal::from(100));
        assert_eq!(p.leverage, crate::wire::LeverageType::Isolated);
        assert_eq!(account.perp_balance(""), before - Decimal::from(100));
        // And back off the position.
        sim.update_isolated_margin(a, "ETH", -40_000_000, &feed)
            .unwrap();
        let account = sim.account(a).unwrap().unwrap();
        assert_eq!(
            account.positions.get("ETH").unwrap().isolated_margin,
            Decimal::from(60)
        );
        assert_eq!(account.perp_balance(""), before - Decimal::from(60));
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
        assert!(
            refused.is_err(),
            "a nonce below the retained set is refused"
        );
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
        let oid = sim
            .place(&feed, a, &t)
            .unwrap()
            .resting
            .expect("the trigger armed");
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
        sim.place(&feed, a, &trigger(Side::Buy, Tpsl::Sl, "110"))
            .unwrap();
        let refused = sim.place(&feed, a, &trigger(Side::Sell, Tpsl::Tp, "99"));
        assert!(refused.is_err(), "a take sell below the mid is refused");
        sim.place(&feed, a, &trigger(Side::Sell, Tpsl::Tp, "110"))
            .unwrap();
    }

    /// **A fired trigger ends its own oid with the venue's `triggered` status**,
    /// not `canceled` (which reads as the user's own cancel). The fill belongs
    /// to the resulting market order — which carries the trigger's cloid — so
    /// `orderStatus(trigger_oid)` is the venue's `triggered` and the cloid
    /// resolves to the fill (measured live 2026-11-23: the sim served
    /// `canceled` before this).
    #[test]
    fn a_fired_trigger_reports_the_triggered_status() {
        let feed = feed_with_book(
            book("ETH", &[("100", "10")], &[("101", "10")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(Side::Buy, "1")).unwrap(); // long 1 at 101
        let mut stop = ioc(Side::Sell, "1");
        stop.tif = Tif::Gtc;
        stop.limit_px = Some("100".parse().unwrap());
        stop.reduce_only = true;
        stop.cloid = Some("0x00000000000000000000000000000077".parse().unwrap());
        stop.trigger = Some(Trigger {
            is_market: true,
            trigger_px: "99".parse().unwrap(),
            tpsl: Tpsl::Sl,
        });
        let oid = sim.place(&feed, a, &stop).unwrap().resting.expect("the stop arms");
        // The mark crosses the stop: the next tick fires it.
        let now = crate::now_ms();
        feed.put_ctx(
            "ETH".into(),
            crate::wire::test_ctx("98", "98"),
            now,
        );
        sim.evaluate(&feed).unwrap();

        let terminal = sim
            .terminal_by_oid(a, oid)
            .unwrap()
            .expect("the trigger's own row");
        assert_eq!(terminal.status, TerminalStatus::Triggered);
        assert_eq!(
            crate::api::info::terminal_status_label(terminal.status),
            "triggered",
            "the served orderStatus vocabulary"
        );
        let fill = sim.user_fills(a).unwrap().pop().expect("the close filled");
        assert_eq!(fill.dir, "Close Long");
        assert_eq!(
            fill.cloid,
            Some("0x00000000000000000000000000000077".parse().unwrap()),
            "the resulting order carries the trigger's cloid"
        );
    }

    /// The scheduled-cancel cap (docs): "The max number of triggers per day is
    /// 10. This trigger count is reset at 00:00 UTC." The count increments
    /// when one fires; the 11th schedule of the day is refused.
    #[test]
    fn scheduled_cancels_are_capped_per_day() {
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        sim.with_account(a, |acc| {
            *acc.perp_balance_mut("") = Decimal::from(100);
            Ok(())
        })
        .unwrap();
        let now = crate::now_ms();
        for i in 0..10u64 {
            let t = now + 6_000 + i * 1_000;
            sim.schedule_cancel(a, ScheduleCancel::At(t)).unwrap();
            sim.fire_scheduled_cancels(t).unwrap();
        }
        let refused = sim.schedule_cancel(a, ScheduleCancel::At(now + 60_000));
        assert!(refused.is_err(), "the 11th trigger of the day is refused");
        assert!(refused.unwrap_err().to_string().contains("per day"));
    }

    /// Name-based validation/margin resolves a HIP-3 coin from the
    /// account-plane `margin_meta`, not the served default-dex meta: a
    /// `xyz:*` order no longer reads as "not in the venue's meta".
    #[test]
    fn an_xyz_coin_validates_from_the_account_plane_meta() {
        let feed = MarketFeed::memory(vec!["BTC".into(), "xyz:NVDA".into()]);
        let now = crate::now_ms();
        feed.put_meta(meta_with("BTC", 20), now);
        feed.put_ctx("xyz:NVDA".into(), crate::wire::test_ctx("200", "201"), now);
        feed.put_book(book("xyz:NVDA", &[("200", "2")], &[("201", "2")]), now);
        feed.put_oi_caps(Vec::new());
        feed.put_oi_caps_of("xyz", Vec::new());
        let (sim, a) = funded_sim(&feed, "10000");
        let order = Order {
            coin: "xyz:NVDA".into(),
            ..ioc(Side::Buy, "1")
        };
        // Without the dex's meta the name is genuinely outside the venue's
        // universe.
        let err = sim.place(&feed, a, &order).unwrap_err();
        assert!(err.to_string().contains("not in the venue's meta"), "{err}");
        // Folded in, the same order validates and fills on the xyz book, on
        // xyz's own ledger.
        feed.put_dex_meta("xyz".into(), meta_with("xyz:NVDA", 10), now);
        sim.deal(
            &feed,
            &Deal {
                address: a,
                class: DealClass::Perp,
                coin: None,
                dex: Some("xyz".into()),
                amount: Decimal::from(10_000),
            },
        )
        .unwrap();
        let placed = sim.place(&feed, a, &order).unwrap();
        assert_eq!(
            placed.fill.expect("the order fills").avg_px,
            Decimal::from(201)
        );
        let account = sim.account(a).unwrap().unwrap();
        assert_eq!(account.positions["xyz:NVDA"].szi, Decimal::ONE);
    }

    fn dec(text: &str) -> Decimal {
        text.parse().unwrap()
    }

    /// The perp balance of a simulated account.
    fn balance(sim: &Sim, who: Address) -> Decimal {
        sim.account(who).unwrap().unwrap().perp_balance("")
    }

    /// Fund `who` with `amount` USDC on the perp side.
    fn fund(sim: &Sim, feed: &MarketFeed, who: Address, amount: &str) {
        sim.deal(
            feed,
            &Deal {
                address: who,
                class: DealClass::Perp,
                coin: None,
                dex: None,
                amount: dec(amount),
            },
        )
        .unwrap();
    }

    /// The builder-fee gate: not approved, above the approval, above the perp
    /// cap, then accepted; a later approval overwrites (up or down) and 0
    /// revokes.
    #[test]
    fn the_builder_gate_follows_the_approval_and_the_cap() {
        let sim = Sim::memory();
        let owner = Address::repeat_byte(0xaa);
        let builder = Address::repeat_byte(0xbb);
        let refusal = |f: u64| {
            sim.authorize_builder(owner, builder, f)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(refusal(10), "Builder fee has not been approved.");
        assert_eq!(
            sim.max_builder_fee(owner, builder).unwrap(),
            None,
            "untouched"
        );

        sim.approve_builder_fee(owner, builder, 50).unwrap();
        assert_eq!(sim.max_builder_fee(owner, builder).unwrap(), Some(50));
        assert_eq!(
            sim.authorize_builder(owner, builder, 50).unwrap(),
            BuilderFee { builder, f: 50 }
        );
        assert!(
            refusal(51).contains("exceeds the 50 tenths of a basis point"),
            "{}",
            refusal(51)
        );
        // The perp cap is named even when the approval is larger (the agent
        // contract approves up to 1000, the venue's perp orders cap at 100).
        sim.approve_builder_fee(owner, builder, 1_000).unwrap();
        assert!(
            refusal(101).contains("perp builder fee cap of 100"),
            "{}",
            refusal(101)
        );
        assert!(sim.authorize_builder(owner, builder, 100).is_ok());
        // The venue sets the maximum: a later approval overwrites.
        sim.approve_builder_fee(owner, builder, 20).unwrap();
        assert_eq!(sim.max_builder_fee(owner, builder).unwrap(), Some(20));
        assert!(refusal(21).contains("exceeds the 20"));
        // Another builder is not approved; 0 revokes.
        let other = Address::repeat_byte(0xcc);
        assert_eq!(
            sim.authorize_builder(owner, other, 1)
                .unwrap_err()
                .to_string(),
            "Builder fee has not been approved."
        );
        sim.approve_builder_fee(owner, builder, 0).unwrap();
        assert_eq!(sim.max_builder_fee(owner, builder).unwrap(), Some(0));
        assert_eq!(refusal(1), "Builder fee has not been approved.");
    }

    /// A builder-deployed market's fee follows its deployer's scale (HIP-3):
    /// scale 1.0 doubles the base tier, a growth-mode market pays a tenth of
    /// that, a scale under 1 adds to it, and the default dex's coin (no scale)
    /// pays the base tier. Measured on real fills: xyz growth-mode coins cost
    /// 0.2 of the base, vntl's 2.0.
    #[test]
    fn a_builder_dex_fee_follows_the_deployers_scale_and_growth_mode() {
        let mut meta = meta_of(&[("a:X", 10), ("a:G", 10), ("a:LOW", 10), ("BASE", 10)]);
        meta.universe[0].deployer_fee_scale = Some("1.0".into());
        meta.universe[1].deployer_fee_scale = Some("1.0".into());
        meta.universe[1].growth_mode = Some("enabled".into());
        meta.universe[2].deployer_fee_scale = Some("0.5".into());
        let m = Some(&meta);
        assert_eq!(dex_fee_multiplier(m, "a:X"), dec("2"));
        assert_eq!(dex_fee_multiplier(m, "a:G"), dec("0.2"));
        assert_eq!(dex_fee_multiplier(m, "a:LOW"), dec("1.5"));
        assert_eq!(dex_fee_multiplier(m, "BASE"), dec("1"));
        assert_eq!(dex_fee_multiplier(None, "a:X"), dec("1"));
    }

    /// The taker fee on a growth-mode builder coin is the base fee times its
    /// multiplier, on the fill and on the account's balance alike.
    #[test]
    fn a_take_on_a_growth_mode_builder_coin_pays_the_scaled_fee() {
        let mut meta = meta_with("ETH", 10);
        meta.universe[0].deployer_fee_scale = Some("1.0".into());
        meta.universe[0].growth_mode = Some("enabled".into());
        let feed = feed_with_book(book("ETH", &[("100", "9")], &[("101", "9")]), meta);
        let (sim, a) = funded_sim(&feed, "1000");
        sim.place(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        let fee = dec("101") * taker_fee() * dec("0.2");
        assert_eq!(balance(&sim, a), dec("1000") - fee);
        assert_eq!(sim.user_fills(a).unwrap().remove(0).fee, fee);
    }

    /// An immediate take with a builder charges `notional * f / 100_000` on
    /// top of the taker fee, debits it from the placing account, credits a
    /// simulated builder, and the ledger fill carries `builderFee`. An order
    /// without a builder is exactly as before.
    #[test]
    fn a_builder_fee_is_charged_on_the_take_and_credited_to_the_builder() {
        let feed = feed_with_book(
            book("ETH", &[("100", "9")], &[("101", "9")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let builder = Address::repeat_byte(0xbb);
        fund(&sim, &feed, builder, "10");
        sim.approve_builder_fee(a, builder, 100).unwrap();
        let carried = sim.authorize_builder(a, builder, 50).unwrap();

        // A plain order first: no builder fee anywhere.
        sim.place(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        let taker = dec("101") * taker_fee();
        assert_eq!(balance(&sim, a), dec("1000") - taker);
        assert_eq!(balance(&sim, builder), dec("10"));
        let plain = sim.user_fills(a).unwrap().remove(0);
        assert_eq!(plain.builder_fee, None);
        assert!(serde_json::to_value(&plain)
            .unwrap()
            .get("builderFee")
            .is_none());

        // 2 at 101 = 202 notional; 50 tenths of a bp = 0.05% = 0.101.
        sim.place_with_builder(&feed, a, &ioc(Side::Buy, "2"), Some(carried))
            .unwrap();
        let fee_taker = dec("202") * taker_fee();
        let fee_builder = dec("0.101");
        assert_eq!(
            balance(&sim, a),
            dec("1000") - taker - fee_taker - fee_builder
        );
        assert_eq!(balance(&sim, builder), dec("10") + fee_builder);
        let fill = sim.user_fills(a).unwrap().remove(1);
        assert_eq!(fill.fee, fee_taker, "the exchange fee is unchanged");
        assert_eq!(fill.builder_fee, Some(fee_builder));
        assert_eq!(
            serde_json::to_value(&fill).unwrap()["builderFee"],
            "0.101",
            "served as a decimal string"
        );
    }

    /// A builder that is not a simulated account is not credited anywhere (and
    /// is not created), but the placing account still pays.
    #[test]
    fn an_unsimulated_builder_is_not_credited() {
        let feed = feed_with_book(
            book("ETH", &[("100", "9")], &[("101", "9")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let builder = Address::repeat_byte(0xcc);
        let carried = BuilderFee { builder, f: 100 };
        sim.place_with_builder(&feed, a, &ioc(Side::Buy, "1"), Some(carried))
            .unwrap();
        // 101 notional at 0.1% = 0.101.
        assert_eq!(
            balance(&sim, a),
            dec("1000") - dec("101") * taker_fee() - dec("0.101")
        );
        assert!(sim.account(builder).unwrap().is_none(), "never created");
    }

    /// A resting order keeps its builder: the maker fill that a later taker
    /// makes charges it (with the maker fee), and the taker, who carried none,
    /// pays none.
    #[test]
    fn a_resting_order_charges_its_builder_on_the_maker_fill() {
        // No served bids: the resting bid is the only liquidity.
        let feed = feed_with_book(book("ETH", &[], &[("101", "5")]), meta_with("ETH", 10));
        let (sim, maker) = funded_sim(&feed, "1000");
        let taker = Address::repeat_byte(0xdd);
        fund(&sim, &feed, taker, "1000");
        let builder = Address::repeat_byte(0xbb);
        fund(&sim, &feed, builder, "0");
        let carried = BuilderFee { builder, f: 20 };
        let rest = Order {
            limit_px: Some(dec("100")),
            tif: Tif::Gtc,
            ..ioc(Side::Buy, "1")
        };
        let oid = sim
            .place_with_builder(&feed, maker, &rest, Some(carried))
            .unwrap()
            .resting
            .unwrap();
        assert_eq!(
            sim.resting_orders(Some(maker)).unwrap()[0].builder,
            Some(carried),
            "the resting order keeps its builder"
        );
        assert_eq!(balance(&sim, maker), dec("1000"), "resting charges nothing");

        let hit = Order {
            limit_px: Some(dec("100")),
            ..ioc(Side::Sell, "1")
        };
        sim.place(&feed, taker, &hit).unwrap();
        // 100 notional: maker fee 0.015, builder fee 100 * 20 / 100000 = 0.02.
        assert_eq!(
            balance(&sim, maker),
            dec("1000") - dec("100") * maker_fee() - dec("0.02")
        );
        assert_eq!(balance(&sim, builder), dec("0.02"));
        let m = sim.user_fills(maker).unwrap().remove(0);
        assert_eq!((m.oid, m.crossed), (oid, false));
        assert_eq!(m.builder_fee, Some(dec("0.02")));
        let t = sim.user_fills(taker).unwrap().remove(0);
        assert_eq!(t.builder_fee, None, "the taker carried no builder");
        assert_eq!(balance(&sim, taker), dec("1000") - dec("100") * taker_fee());
    }

    /// The tick's passive fill (the live book crossing a resting order)
    /// charges the resting order's builder too.
    #[test]
    fn a_passive_tick_fill_charges_the_builder() {
        let feed = feed_with_book(
            book("ETH", &[("99", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let builder = Address::repeat_byte(0xbb);
        fund(&sim, &feed, builder, "0");
        let carried = BuilderFee { builder, f: 100 };
        let rest = Order {
            limit_px: Some(dec("100")),
            tif: Tif::Gtc,
            ..ioc(Side::Buy, "1")
        };
        sim.place_with_builder(&feed, a, &rest, Some(carried))
            .unwrap();
        feed.put_book(
            book("ETH", &[("99", "5")], &[("100", "5")]),
            crate::now_ms(),
        );
        sim.evaluate(&feed).unwrap();

        let fill = sim.user_fills(a).unwrap().remove(0);
        assert!(!fill.crossed);
        assert_eq!(fill.builder_fee, Some(dec("0.1")), "100 at 0.1%");
        assert_eq!(
            balance(&sim, a),
            dec("1000") - dec("100") * maker_fee() - dec("0.1")
        );
        assert_eq!(balance(&sim, builder), dec("0.1"));
    }

    /// The adversarial book drill (the roadmap's spoofing ask): a scripted
    /// overlay that vanishes can never fill a resting order, and a re-staged
    /// spoof fills exactly once. A passive fill consumes only the *served*
    /// book at tick time, and a hand-back to live ([`MarketFeed::clear_book`],
    /// I4) drops the coin's consumed deltas with the scenario-owned base — so
    /// a level that existed only while scripted cannot credit a phantom fill
    /// after it is gone, and the same price re-appearing live is fresh size.
    #[test]
    #[allow(clippy::too_many_lines)] // one drill: stage, fill, vanish, re-stage
    fn a_vanished_spoof_never_fills_a_resting_order() {
        // Real depth: bid 99. The maker rests a sell at 100, above the touch.
        let feed = feed_with_book(
            book("ETH", &[("99", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let rest = Order {
            limit_px: Some(dec("100")),
            tif: Tif::Gtc,
            ..ioc(Side::Sell, "1")
        };
        let oid = sim
            .place(&feed, a, &rest)
            .unwrap()
            .resting
            .expect("above the touch, it rests");

        // The spoof: a huge bid appears AT the maker's price. The next tick
        // fills the maker — the scenario owns the depth it staged.
        feed.own_book(&book("ETH", &[("100", "50")], &[("101", "5")])).unwrap();
        sim.evaluate(&feed).unwrap();
        let fills = sim.user_fills(a).expect("the account exists");
        assert_eq!(fills.len(), 1, "the spoofed level filled the maker once");
        assert_eq!(fills[0].px, dec("100"));
        assert!(!fills[0].crossed, "a passive fill is the maker side");

        // The spoof vanishes (hand-back to live): the bid is 99 again. No
        // further tick may fill anything at 100 — the level existed only
        // while scripted, and its consumed delta dropped with the overlay.
        feed.clear_book("ETH").unwrap();
        sim.evaluate(&feed).unwrap();
        sim.evaluate(&feed).unwrap();
        assert_eq!(
            sim.user_fills(a).expect("the account exists").len(),
            1,
            "a vanished spoof credits no phantom fill"
        );
        assert!(
            sim.resting_orders(None).unwrap().iter().all(|r| r.oid != oid),
            "the maker's order was consumed by its one real fill"
        );

        // The same price re-appearing **live** is fresh size, not the consumed
        // spoof: a new maker at 100 fills again against live depth exactly once.
        feed.put_book(
            book("ETH", &[("99", "5")], &[("101", "5")]),
            crate::now_ms(),
        );
        let rest2 = Order {
            limit_px: Some(dec("100")),
            tif: Tif::Gtc,
            ..ioc(Side::Sell, "1")
        };
        sim.place(&feed, a, &rest2).unwrap().resting.expect("rests");
        feed.put_book(
            book("ETH", &[("100", "50")], &[("101", "5")]),
            crate::now_ms(),
        );
        sim.evaluate(&feed).unwrap();
        let fills = sim.user_fills(a).expect("the account exists");
        assert_eq!(fills.len(), 2, "the live level filled the new maker once");
        assert_eq!(fills[0].px, dec("100"));
    }

    /// The 1:1 book symmetry: a resting maker's order is **in** the served
    /// book (the venue's `l2Book` shows every participant's resting orders),
    /// and a passive fill shrinks exactly that order — the live level at the
    /// same price is untouched (the maker *provides* liquidity; it consumes
    /// none). The old model consumed a live level as a display proxy for a
    /// fill of an invisible order: with the maker displayed, that proxy ate
    /// twice the liquidity one trade took.
    #[test]
    fn a_resting_maker_shows_in_the_served_book_and_its_fill_eats_only_itself() {
        let feed = feed_with_book(
            book("ETH", &[("99", "5")], &[("101", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let rest = Order {
            limit_px: Some(dec("100")),
            tif: Tif::Gtc,
            ..ioc(Side::Sell, "1")
        };
        sim.place(&feed, a, &rest)
            .unwrap()
            .resting
            .expect("above the touch, it rests");

        // The maker's order is part of the composed book: the ask side gains
        // a 100 @ 1 level the live feed never had.
        let base = feed.book("ETH", crate::now_ms()).expect("the book exists");
        let composed = crate::api::info::with_resting(base, &sim.resting_book("ETH"));
        let ask100 = composed.levels[1].iter().find(|l| l.px == dec("100"));
        assert_eq!(
            ask100.map(|l| (l.sz, l.n)),
            Some((dec("1"), 1)),
            "the resting maker is displayed liquidity"
        );

        // The live book crosses the maker. One tick fills it at its price;
        // the composed book then shows the maker gone and the live level at
        // 100 exactly as the feed delivered it (no consumed delta).
        feed.own_book(&book("ETH", &[("100", "5")], &[("101", "5")]))
            .unwrap();
        sim.evaluate(&feed).unwrap();
        assert_eq!(
            sim.user_fills(a).expect("the account exists").len(),
            1,
            "the cross filled the maker"
        );
        let base = feed.book("ETH", crate::now_ms()).expect("the book exists");
        let composed = crate::api::info::with_resting(base, &sim.resting_book("ETH"));
        assert!(
            !composed.levels[1].iter().any(|l| l.px == dec("100")),
            "the maker's order left the ask side with its fill"
        );
        assert_eq!(
            composed.levels[0].iter().find(|l| l.px == dec("100")).map(|l| l.sz),
            Some(dec("5")),
            "the live bid at the maker's price kept its full size: a maker fill consumes nothing"
        );
    }

    /// **A taker's flow consumes the live level before a devnet maker at the
    /// same price.** The queue at a price belongs to the orders already
    /// displayed; a maker that joined it is behind them. Measured 2026-11-22 on
    /// `@151`: a maker rested `0.1` behind a live ask of `0.9755`, a `0.1` taker
    /// buy filled the **maker** and left the live level untouched — a fill the
    /// real queue could not give.
    #[test]
    fn a_taker_consumes_the_live_level_before_a_maker_at_the_same_price() {
        let feed = feed_with_book(
            book("ETH", &[("99", "5")], &[("100", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let rest = Order {
            limit_px: Some(dec("100")),
            tif: Tif::Gtc,
            ..ioc(Side::Sell, "1")
        };
        let oid = sim.place(&feed, a, &rest).unwrap().resting.expect("joins the ask");
        let b = Address::repeat_byte(0x5d);
        sim.deal(
            &feed,
            &Deal {
                address: b,
                class: DealClass::Perp,
                coin: None,
                dex: None,
                amount: dec("1000"),
            },
        )
        .unwrap();
        let fill = sim
            .order(&feed, b, &ioc(Side::Buy, "1"))
            .expect("the live level fills it");
        assert_eq!(fill.total_sz, dec("1"));
        assert_eq!(fill.avg_px, dec("100"));
        assert!(
            sim.user_fills(a).unwrap().is_empty(),
            "the maker behind the displayed level is not filled by flow the level absorbed"
        );
        assert!(
            sim.resting_by_oid(a, oid).unwrap().is_some(),
            "and its order still rests"
        );
    }

    /// **A maker behind a live level does not fill on a cross smaller than the
    /// queue.** The crossing size is the opposite touch's size; only the part
    /// above the queue ahead (the displayed size at the maker's own price) can
    /// reach it.
    #[test]
    fn a_maker_behind_a_live_level_needs_a_cross_larger_than_the_queue() {
        let feed = feed_with_book(
            book("ETH", &[("99", "5")], &[("100", "5")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        let rest = Order {
            limit_px: Some(dec("100")),
            tif: Tif::Gtc,
            ..ioc(Side::Sell, "0.5")
        };
        sim.place(&feed, a, &rest).unwrap().resting.expect("joins the ask");
        // A 0.4 bid at 100 crosses, but the ask queue at 100 is still 5 deep:
        // the flow is smaller than what is ahead of the maker.
        feed.put_book(book("ETH", &[("100", "0.4")], &[("100", "5")]), crate::now_ms());
        sim.evaluate(&feed).unwrap();
        assert!(
            sim.user_fills(a).unwrap().is_empty(),
            "0.4 of flow cannot reach a maker behind a 5-deep level"
        );
        // 6 of flow leaves 1 above the queue: the maker fills fully.
        feed.put_book(book("ETH", &[("100", "6")], &[("100", "5")]), crate::now_ms());
        sim.evaluate(&feed).unwrap();
        let fills = sim.user_fills(a).unwrap();
        assert_eq!(fills.len(), 1, "the flow above the queue reaches the maker");
        assert_eq!(fills[0].sz, dec("0.5"));
        assert!(!fills[0].crossed);
    }

    /// The venue's default leverage for a coin the account never set (measured
    /// on real `activeAssetData`): cross at min(max, 20), isolated on an
    /// isolated-only market; a new position opens in the mode the account
    /// last set, funding an isolated one from the balance.
    #[test]
    #[allow(clippy::too_many_lines)] // one scenario
    fn a_new_position_opens_in_the_default_or_set_margin_mode() {
        use crate::wire::{LeverageType, MarginMode};
        let mut meta = meta_of(&[("BIG", 40), ("SMALL", 5), ("ISO", 10)]);
        meta.universe[2].margin_mode = Some(MarginMode::NoCross);
        meta.universe[2].only_isolated = Some(true);
        let d = |c: &str| default_leverage(c, &meta);
        assert_eq!(
            (d("BIG").mode, d("BIG").leverage),
            (LeverageType::Cross, 20)
        );
        assert_eq!(
            (d("SMALL").mode, d("SMALL").leverage),
            (LeverageType::Cross, 5)
        );
        assert_eq!(
            (d("ISO").mode, d("ISO").leverage),
            (LeverageType::Isolated, 10)
        );

        // An order after `updateLeverage` isolated opens an isolated position
        // whose margin left the balance (the venue's isolated open).
        let feed = feed_with_book(
            book("ETH", &[("100", "9")], &[("101", "9")]),
            meta_with("ETH", 25),
        );
        let (sim, a) = funded_sim(&feed, "1000");
        sim.set_leverage(
            a,
            "ETH",
            crate::sim::account::LeverageSetting {
                mode: LeverageType::Isolated,
                leverage: 5,
            },
            feed.meta().as_ref(),
        )
        .unwrap();
        sim.place(&feed, a, &ioc(Side::Buy, "2")).unwrap();
        let account = sim.account(a).unwrap().unwrap();
        let p = account.positions.get("ETH").unwrap();
        assert_eq!(p.leverage, LeverageType::Isolated);
        assert_eq!(p.leverage_value, 5);
        // 2 at 101 = 202 notional at 5x = 40.4 isolated margin.
        assert_eq!(p.isolated_margin, dec("40.4"));
        assert_eq!(p.margin_used, dec("40.4"));
        assert_eq!(
            account.perp_balance(""),
            dec("1000") - dec("40.4") - dec("202") * taker_fee()
        );
        // A second take adds its own initial margin; a full close returns it.
        sim.place(&feed, a, &ioc(Side::Buy, "1")).unwrap();
        let p = sim
            .account(a)
            .unwrap()
            .unwrap()
            .positions
            .get("ETH")
            .cloned()
            .unwrap();
        assert_eq!(p.isolated_margin, dec("60.6"));
        sim.place(&feed, a, &reduce(Side::Sell, "3")).unwrap();
        let account = sim.account(a).unwrap().unwrap();
        assert!(account.positions.is_empty());
        let charged = (dec("202") + dec("101") + dec("300")) * taker_fee();
        assert_eq!(
            account.perp_balance(""),
            dec("1000") - charged + dec("3") * (dec("100") - dec("101"))
        );
    }

    /// State written before builder fees existed still loads: no approvals,
    /// no builder on a resting order, no `builderFee` on a fill; and the new
    /// fields round-trip when set (and are omitted when not).
    #[test]
    #[allow(clippy::too_many_lines)] // one persistence round trip: splitting it would hide the field-to-line mapping
    fn an_old_state_file_loads_without_the_builder_fields() {
        let old = r#"{"accounts": {
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": {"perp_balance": "12"}}}"#;
        let mut state: crate::sim::SimState = serde_json::from_str(old).unwrap();
        state.migrate();
        let a = Address::repeat_byte(0xaa);
        assert!(state.accounts[&a].builder_approvals.is_empty());
        let json = serde_json::to_value(&state).unwrap();
        assert!(
            json["accounts"]["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
                .get("builder_approvals")
                .is_none(),
            "an account with no approvals persists exactly as before"
        );

        let builder = Address::repeat_byte(0xbb);
        state
            .accounts
            .get_mut(&a)
            .unwrap()
            .builder_approvals
            .insert(builder, 70);
        let resting = RestingOrder {
            oid: 1,
            account: a,
            coin: "ETH".into(),
            side: Side::Buy,
            px: dec("100"),
            sz: dec("1"),
            orig_sz: dec("1"),
            reduce: Reduce::Any,
            tif: Tif::Gtc,
            cloid: None,
            trigger: None,
            placed_ms: 0,
            filled_sz: Decimal::ZERO,
            filled_ntl: Decimal::ZERO,
            builder: None,
            spot_hold: None,
        };
        // A resting order without a builder omits the key; one written by an
        // older build (no key) reads back with none.
        let plain = serde_json::to_value(&resting).unwrap();
        assert!(plain.get("builder").is_none());
        let back: RestingOrder = serde_json::from_value(plain).unwrap();
        assert_eq!(back.builder, None);
        let with = RestingOrder {
            builder: Some(BuilderFee { builder, f: 5 }),
            ..resting
        };
        state.resting.insert(1, with);
        let text = serde_json::to_string(&state).unwrap();
        let mut loaded: crate::sim::SimState = serde_json::from_str(&text).unwrap();
        loaded.migrate();
        assert_eq!(loaded, state);
        assert_eq!(loaded.accounts[&a].builder_approvals[&builder], 70);
        assert_eq!(
            loaded.resting[&1].builder,
            Some(BuilderFee { builder, f: 5 })
        );
    }
    /// A resting spot sell is filled by the **live book** moving through its
    /// price (the tick's passive pass), not only by a client taker: the maker's
    /// hold converts into quote proceeds net of the maker fee, the live level
    /// is consumed, and the order leaves the book. No perp re-margin runs — the
    /// hold is the cover (before this lane the perp lookup refused every spot
    /// cross with a per-second WARN).
    #[test]
    fn a_resting_spot_sell_fills_when_the_live_book_crosses_it() {
        let feed = spot_feed();
        let (sim, a) = spot_account(&feed, "1000");
        // A gets base via an IOC at the ask (1000 PURR, fee in the received base).
        sim.place(&feed, a, &spot_order(Side::Buy, "1000", Some("0.08")))
            .expect("the ask has size");
        let mut sell = spot_order(Side::Sell, "500", Some("0.0745"));
        sell.tif = Tif::Gtc;
        let oid = sim
            .place(&feed, a, &sell)
            .expect("inside the spread, it rests")
            .resting
            .expect("rests");
        assert_eq!(
            sim.account(a).unwrap().unwrap().spot.get(&1).unwrap().hold,
            Decimal::from(500)
        );

        // The live book moves up through the maker's price.
        feed.put_book(
            book("PURR/USDC", &[("0.076", "5000")], &[("0.0765", "5000")]),
            crate::now_ms(),
        );
        sim.run_resting(&feed, crate::now_ms()).expect("the tick runs");

        let account = sim.account(a).unwrap().unwrap();
        let purr = account.spot.get(&1).unwrap();
        let usdc = account.spot.get(&0).unwrap();
        // 500 × 0.0745 = 37.25; the 0.040 % maker fee is charged in the
        // received token (USDC): 37.25 × 0.9996 = 37.2351.
        assert_eq!(usdc.total, Decimal::from(925) + Decimal::new(372_351, 4));
        assert_eq!(purr.total, Decimal::new(9993, 1) - Decimal::from(500));
        assert_eq!(purr.hold, Decimal::ZERO, "the hold converted into the fill");
        let fill = account.fills.back().unwrap();
        assert_eq!(fill.dir, "Sell");
        assert_eq!(fill.fee_token, "USDC");
        assert!(!fill.crossed, "a passive fill is the maker side");
        assert!(sim.resting_orders(None).unwrap().iter().all(|r| r.oid != oid));
    }

    /// A **partial** passive fill shrinks the reservation with the order: the
    /// remaining hold equals the remaining size, so a later cancel releases
    /// exactly what the account still has (never more — the pre-fix cancel
    /// released the original amount and clamped at zero, silently eating a
    /// later order's reservation on the same token).
    #[test]
    fn a_partial_passive_fill_shrinks_the_hold_with_the_order() {
        let feed = spot_feed();
        let (sim, a) = spot_account(&feed, "1000");
        sim.place(&feed, a, &spot_order(Side::Buy, "1000", Some("0.08")))
            .expect("the ask has size");
        let mut sell = spot_order(Side::Sell, "500", Some("0.0745"));
        sell.tif = Tif::Gtc;
        let oid = sim
            .place(&feed, a, &sell)
            .expect("rests")
            .resting
            .expect("rests");

        // The crossed level carries only 200: the maker fills partially.
        feed.put_book(
            book("PURR/USDC", &[("0.076", "200")], &[("0.0765", "5000")]),
            crate::now_ms(),
        );
        sim.run_resting(&feed, crate::now_ms()).expect("the tick runs");

        let account = sim.account(a).unwrap().unwrap();
        assert_eq!(account.spot.get(&1).unwrap().hold, Decimal::from(300), "hold follows the size");
        assert_eq!(sim.resting_orders(None).unwrap()[0].sz, Decimal::from(300));

        sim.cancel(a, "PURR/USDC", oid, CancelSpeed::Normal).expect("cancel");
        assert_eq!(
            sim.account(a).unwrap().unwrap().spot.get(&1).unwrap().hold,
            Decimal::ZERO,
            "the cancel releases exactly the remaining reservation"
        );
    }

}
