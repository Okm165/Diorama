//! The matching lane's first slice (plan §4.5/§5, Phase 2): a devnet order
//! fills against the **real book** — the live depth the feed caches — with
//! per-level consumption (the COW analogue). The plan's identity (I3): the
//! order moves the *book*, never the *mark*; the pricing oracle moves only by
//! an explicit crash. The trader's own state moves the way the venue's would:
//! size and size-weighted entry update, a reducing fill realizes its PnL into
//! the perp balance (the measured identity `accountValue == totalRawUsd + Σ
//! signed positionValue` stays exact), and the initial margin follows
//! `|szi|·entry/leverage`.
//!
//! Scope, named rather than implied: only the IOC shape fills (a Gtc
//! remainder would have to rest — refused by name, Phase 2), the signature is
//! accepted but never verified (the devnet has no keys — the
//! anvil-impersonation analogue), and the depth is the served book's (the live
//! book minus what the devnet already consumed, so a second order sees the
//! first one's impact).
use super::Sim;
use crate::error::{Error, Result};
use crate::feed::{Consumption, MarketFeed};
use crate::wire::L2Book;
use alloy_primitives::Address;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

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

/// Move a finished order into the bounded terminal history (`orderStatus`
/// answers for filled and canceled orders, as the venue's does).
fn terminalize(
    terminal: &mut std::collections::BTreeMap<u64, TerminalOrder>,
    order: RestingOrder,
    status: TerminalStatus,
    filled_sz: Decimal,
) {
    let avg = if order.filled_sz.is_zero() {
        Decimal::ZERO
    } else {
        order.filled_ntl / order.filled_sz
    };
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
    pub is_buy: bool,
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

impl Sim {
    /// `place`: the venue's order entry, end to end — validations, the
    /// immediate take against the served book *and* the devnet's own resting
    /// orders (fees on both sides), the remainder resting (Gtc/Alo), or the
    /// trigger armed (no walk; the tick fires it when the mark crosses).
    pub(crate) fn place(&self, feed: &MarketFeed, address: Address, req: &Order) -> Result<Placed> {
        let meta = feed.meta();
        validate_order(feed, meta.as_ref(), req)?;
        if let Some(reason) = self.oi_cap_refusal(feed, address, req)? {
            tracing::warn!(coin = %req.coin, "order refused: open interest cap");
            return Err(Error::bad_request(reason));
        }
        // A trigger arms instead of walking.
        if let Some(trigger) = req.trigger {
            if trigger.trigger_px <= Decimal::ZERO {
                return Err(Error::bad_request("Invalid TP/SL price."));
            }
            let px = req.limit_px.unwrap_or(trigger.trigger_px);
            let oid = self.rest_order(address, req, px, req.size)?;
            self.metrics
                .triggers_armed_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(account = %address, coin = %req.coin, oid, "trigger order armed");
            return Ok(Placed {
                fill: None,
                resting: Some(oid),
            });
        }
        // Post-only: a crossing order is canceled instead of matching (docs).
        if req.tif == Tif::Alo {
            let oid = self.place_alo(feed, address, req)?;
            return Ok(Placed {
                fill: None,
                resting: Some(oid),
            });
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
        let resting = if req.tif == Tif::Gtc && remainder > Decimal::ZERO {
            match req.limit_px {
                Some(px) => Some(self.rest_order(address, req, px, remainder)?),
                None => None, // a market order cannot rest
            }
        } else {
            None
        };
        Ok(Placed { fill, resting })
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
            let bbo = bbo(feed, &req.coin, req.is_buy)
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
        let (legs, filled, avg_px) = walk_served_book(feed, meta.as_ref(), req, &resting)?;
        let leverage = self.leverage_for(address, &req.coin, meta.as_ref())?;
        let notional = super::account::mul(filled, avg_px, "fill notional")?;
        let facts = super::account::FillFacts {
            coin: req.coin.clone(),
            is_buy: req.is_buy,
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
        legs: &[Leg],
        meta: Option<&crate::wire::Meta>,
    ) -> Result<Vec<Consumption>> {
        let mut consumed = Vec::new();
        for leg in legs {
            match leg {
                Leg::Live(c) => {
                    feed.consume(c.clone())?;
                    consumed.push(c.clone());
                }
                Leg::Maker { oid, maker, px, sz } => {
                    let leverage = self.leverage_for(*maker, &req.coin, meta)?;
                    let notional = super::account::mul(*sz, *px, "maker notional")?;
                    let facts = super::account::FillFacts {
                        coin: req.coin.clone(),
                        is_buy: !req.is_buy,
                        size: *sz,
                        px: *px,
                        leverage,
                        reduce_only: self.resting_flag(*oid)?,
                        fee: super::account::mul(notional, maker_fee(), "maker fee")?,
                    };
                    let clock = self.clock(crate::now_ms())?;
                    self.with_account(*maker, |account| {
                        super::account::accrue_funding(account, feed, clock)?;
                        super::account::apply_fill(account, &facts)
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
        let mut st = self.state.lock()?;
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
    }

    /// The taker's own crossing resting orders are expired (the venue's
    /// self-trade rule: expire-maker, never a self-match).
    fn expire_self_crossing(&self, account: Address, feed: &MarketFeed, req: &Order) -> Result<()> {
        let mine = self.resting_orders(Some(account))?;
        for r in mine {
            if r.coin != req.coin || r.is_buy == req.is_buy || r.trigger.is_some() {
                continue;
            }
            let crosses = match req.limit_px {
                None => true,
                Some(limit) => {
                    if req.is_buy {
                        r.px <= limit
                    } else {
                        r.px >= limit
                    }
                }
            };
            if crosses {
                let _ = feed; // (the feed is the caller's context; no read needed)
                tracing::info!(account = %account, oid = r.oid, "self-crossing resting order expired (expire-maker)");
                self.cancel(account, &r.coin, r.oid)?;
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
        is_cross: bool,
        leverage: u32,
    ) -> Result<()> {
        if leverage == 0 {
            return Err(Error::bad_request("leverage must be at least 1"));
        }
        let max = self
            .state
            .lock()?
            .accounts
            .get(&address)
            .and_then(|a| a.leverage_settings.get(coin).map(|s| s.leverage))
            .unwrap_or(0);
        let _ = max;
        self.with_account(address, |account| {
            account.leverage_settings.insert(
                coin.to_string(),
                super::account::LeverageSetting { is_cross, leverage },
            );
            if let Some(p) = account.positions.get_mut(coin) {
                p.leverage = if is_cross {
                    crate::wire::LeverageType::Cross
                } else {
                    crate::wire::LeverageType::Isolated
                };
                p.leverage_value = leverage;
                p.margin_used = super::account::mul(p.szi.abs(), p.entry_px, "margin notional")?
                    / Decimal::from(leverage);
            }
            Ok(())
        })
    }

    /// The venue's OI-cap refusal: a coin at its cap accepts only orders that
    /// reduce the position (plan §5's own string).
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
        let signed = if req.is_buy { req.size } else { -req.size };
        let increases =
            old.is_zero() || super::account::sign(old) == super::account::sign(signed);
        Ok(increases.then_some(
            "Order would increase open interest while open interest is capped",
        ))
    }

    /// The tick's passive fill (`fill_resting`, one implementation).
    pub(crate) fn fill_resting_pub(&self, oid: u64, sz: Decimal, px: Decimal) -> Result<()> {
        self.fill_resting(oid, sz, px)
    }

    /// Every resting order, or one account's (`openOrders`).
    pub(crate) fn resting_orders(&self, account: Option<Address>) -> Result<Vec<RestingOrder>> {
        let st = self.state.lock()?;
        Ok(st
            .resting
            .values()
            .filter(|r| account.is_none_or(|a| r.account == a))
            .cloned()
            .collect())
    }

    /// Insert a resting order under a fresh oid.
    fn rest_order(&self, account: Address, req: &Order, px: Decimal, sz: Decimal) -> Result<u64> {
        let oid = self.next_oid()?;
        let order = RestingOrder {
            oid,
            account,
            coin: req.coin.clone(),
            is_buy: req.is_buy,
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
        {
            let mut st = self.state.lock()?;
            st.resting.insert(oid, order);
        }
        self.mark_dirty()?;
        tracing::info!(account = %account, coin = %req.coin, oid, px = %px, sz = %sz, "order resting");
        Ok(oid)
    }

    /// `cancel`: the venue's own refusal when the order is unknown to the
    /// caller (`Order was never placed, already canceled, or filled.`).
    pub(crate) fn cancel(&self, account: Address, coin: &str, oid: u64) -> Result<()> {
        let mut st = self.state.lock()?;
        match st.resting.get(&oid) {
            Some(r) if r.account == account && r.coin == coin => {
                let Some(done) = st.resting.remove(&oid) else {
                    return Ok(());
                };
                st.terminal.insert(
                    oid,
                    TerminalOrder {
                        order: done,
                        status: TerminalStatus::Canceled,
                        filled_sz: Decimal::ZERO,
                        avg_px: Decimal::ZERO,
                        status_ms: crate::now_ms(),
                },
                );
                drop(st);
                self.mark_dirty()?;
                Ok(())
            }
            _ => Err(Error::bad_request(
                "Order was never placed, already canceled, or filled.",
            )),
        }
    }

    /// `cancelByCloid`: the same, addressed by the client id.
    pub(crate) fn cancel_by_cloid(
        &self,
        account: Address,
        coin: &str,
        cloid: alloy_primitives::B128,
    ) -> Result<()> {
        let oid = {
            let st = self.state.lock()?;
            st.resting
                .values()
                .find(|r| r.account == account && r.coin == coin && r.cloid == Some(cloid))
                .map(|r| r.oid)
        };
        match oid {
            Some(oid) => self.cancel(account, coin, oid),
            None => Err(Error::bad_request(
                "Order was never placed, already canceled, or filled.",
            )),
        }
    }

    /// The resting order a status read may find (open), or its terminal
    /// record (filled/canceled).
    pub(crate) fn resting_by_oid(&self, account: Address, oid: u64) -> Result<Option<RestingOrder>> {
        Ok(self
            .state
            .lock()?
            .resting
            .get(&oid)
            .filter(|r| r.account == account)
            .cloned())
    }

    pub(crate) fn terminal_by_oid(&self, account: Address, oid: u64) -> Result<Option<TerminalOrder>> {
        Ok(self
            .state
            .lock()?
            .terminal
            .get(&oid)
            .filter(|t| t.order.account == account)
            .cloned())
    }

    pub(crate) fn resting_by_cloid(
        &self,
        account: Address,
        cloid: alloy_primitives::B128,
    ) -> Result<Option<RestingOrder>> {
        Ok(self
            .state
            .lock()?
            .resting
            .values()
            .find(|r| r.account == account && r.cloid == Some(cloid))
            .cloned())
    }

    pub(crate) fn terminal_by_cloid(
        &self,
        account: Address,
        cloid: alloy_primitives::B128,
    ) -> Result<Option<TerminalOrder>> {
        Ok(self
            .state
            .lock()?
            .terminal
            .values()
            .find(|t| t.order.account == account && t.order.cloid == Some(cloid))
            .cloned())
    }

    /// `scheduleCancel`: a dead-man's switch — at `time` (ms) every resting
    /// order of the account is canceled (the tick enforces it).
    pub(crate) fn schedule_cancel(&self, account: Address, time_ms: Option<u64>) -> Result<u64> {
        let now = crate::now_ms();
        let mut st = self.state.lock()?;
        match time_ms {
            None => {
                st.scheduled_cancels.remove(&account);
                drop(st);
                self.mark_dirty()?;
                Ok(0)
            }
            Some(t) => {
                if t < now + 5_000 {
                    return Err(Error::bad_request(
                        "Scheduled cancel time must be at least 5 seconds in the future.",
                    ));
                }
                st.scheduled_cancels.insert(account, t);
                drop(st);
                self.mark_dirty()?;
                Ok(t)
            }
        }
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
        let clock = self.clock(crate::now_ms())?;
        self.with_account(address, |account| {
            super::account::accrue_funding(account, feed, clock)?;
            let mut probe = account.clone();
            super::account::apply_fill(&mut probe, facts)?;
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
            super::account::apply_fill(account, facts)
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
            .and_then(|m| m.universe.iter().find(|a| a.name == coin))
            .map(|a| a.max_leverage)
            .filter(|l| *l > 0)
            .unwrap_or(20))
    }

    /// The next order id (the venue's own numbering; monotonic across the
    /// scenario, persisted with the state).
    fn next_oid(&self) -> Result<u64> {
        let mut state = self.state.lock()?;
        state.next_oid = state.next_oid.saturating_add(1);
        Ok(state.next_oid)
    }
}

/// Walk the served book (the live depth minus what the devnet already
/// consumed) and validate the request: only IOC fills, positive size, real
/// depth within the limit.
/// One candidate the taker's walk can take: a live level (external liquidity,
/// recorded as a consumption) or a devnet resting order (an internal trade
/// that updates its maker's account).
#[derive(Debug, Clone)]
pub(crate) enum Leg {
    Live(Consumption),
    Maker { oid: u64, maker: Address, px: Decimal, sz: Decimal },
}

/// The best opposite price the served book shows (the `bbo` in the venue's
/// post-only refusal): the cheapest ask for a buy, the richest bid for a sell.
pub(crate) fn bbo(feed: &MarketFeed, coin: &str, is_buy: bool) -> Option<Decimal> {
    let book = feed.book(coin, crate::now_ms())?;
    let side = usize::from(is_buy);
    book.levels[side].first().map(|l| l.px)
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
        let side = usize::from(req.is_buy);
        if let Some(level) = book.levels[side].first() {
            let crosses = if req.is_buy { level.px <= limit } else { level.px >= limit };
            if crosses {
                return true;
            }
        }
    }
    resting.iter().any(|r| {
        r.coin == req.coin && r.is_buy != req.is_buy && {
            if req.is_buy {
                r.px <= limit
            } else {
                r.px >= limit
            }
        }
    })
}

/// The shared order validations — size, tick/lot, the $10 minimum, and the
/// freshness window — used by the walk and every resting path.
pub(crate) fn validate_order(
    feed: &MarketFeed,
    meta: Option<&crate::wire::Meta>,
    req: &Order,
) -> Result<()> {
    if req.size <= Decimal::ZERO {
        return Err(Error::bad_request("order size must be > 0"));
    }
    validate_tick_lot(meta, req)?;
    validate_min_notional(feed, req)?;
    // The freshness discipline (stale ≠ absent): a live-sourced book older
    // than the shared window is not tradable — the fill would price off a
    // snapshot the venue has already left. A scripted book is the scenario's
    // present and never stale.
    if let Some(age) = feed.book_stale_ms(&req.coin, crate::now_ms(), MarketFeed::MAX_STALE_MS) {
        tracing::warn!(coin = %req.coin, age_ms = age, "order refused: stale book");
        return Err(Error::bad_request(format!(
            "the {} book is stale ({age} ms since the last live update); trading on stale data is refused",
            req.coin
        )));
    }
    Ok(())
}

fn walk_served_book(
    feed: &MarketFeed,
    meta: Option<&crate::wire::Meta>,
    req: &Order,
    resting: &[RestingOrder],
) -> Result<(Vec<Leg>, Decimal, Decimal)> {
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

/// The venue's tick/lot rules (plan §5, docs §16.3): a size carries at most
/// the asset's `szDecimals` decimals; a price at most 5 significant figures
/// and at most `6 − szDecimals` decimals (an integer price is always valid).
fn validate_tick_lot(meta: Option<&crate::wire::Meta>, req: &Order) -> Result<()> {
    let Some(asset) = meta.and_then(|m| m.universe.iter().find(|a| a.name == req.coin)) else {
        return Ok(());
    };
    let sz_dec = u32::from(asset.sz_decimals);
    if req.size.scale() > sz_dec {
        return Err(Error::bad_request(format!(
            "size {} has more than {sz_dec} decimals ({}'s szDecimals)",
            req.size, req.coin
        )));
    }
    let Some(px) = req.limit_px else {
        return Ok(());
    };
    if px.fract().is_zero() {
        return Ok(()); // an integer price is always valid
    }
    let max_dec = 6u32.saturating_sub(sz_dec);
    if px.scale() > max_dec {
        // The venue's tick-size refusal (its tick is 10^(6 − szDecimals)).
        tracing::warn!(coin = %req.coin, price = %px, max_dec, "order refused: tick size");
        return Err(Error::bad_request("Price must be divisible by tick size."));
    }
    if significant_digits(px) > 5 {
        tracing::warn!(coin = %req.coin, price = %px, "order refused: significant figures");
        return Err(Error::bad_request(
            "Order price has more than 5 significant figures.",
        ));
    }
    Ok(())
}

/// The venue's minimum order value: a perp order's notional (size × the
/// limit, else the mark) is at least $10. The refusal is the venue's own
/// string from the exchange docs' error sample ("Order must have minimum
/// value of $10."), so a client matching on it sees the real message.
fn validate_min_notional(feed: &MarketFeed, req: &Order) -> Result<()> {
    let notional = if let Some(px) = req.limit_px {
        super::account::mul(req.size, px, "order notional")?
    } else {
        let mark = feed.mark(&req.coin).ok_or(Error::Warming)?;
        super::account::mul(req.size, mark, "order notional")?
    };
    if notional < Decimal::from(10) {
        return Err(Error::bad_request("Order must have minimum value of $10."));
    }
    Ok(())
}

/// A price's significant digits (trailing zeros are not significant —
/// `normalize` strips them).
fn significant_digits(px: Decimal) -> u32 {
    let mut m = px.normalize().mantissa().unsigned_abs();
    let mut n = 0;
    while m > 0 {
        n += 1;
        m /= 10;
    }
    n
}

/// Walk a book in price order: asks ascending for a buy, bids descending for
/// a sell; the limit stops the walk. Returns (filled size, size-weighted
/// average, the consumed levels).
/// The merged opposite side, price-ordered: (px, is_maker, oid, maker, sz).
/// At the same price the devnet's own resting orders go first (they are
/// time-stamped; the live level's age is unknowable).
fn merge_candidates(
    book: &L2Book,
    req: &Order,
    resting: &[RestingOrder],
) -> Vec<(Decimal, bool, u64, Address, Decimal)> {
    let side = usize::from(req.is_buy);
    let mut candidates: Vec<(Decimal, bool, u64, Address, Decimal)> = book.levels[side]
        .iter()
        .map(|l| (l.px, false, 0u64, Address::ZERO, l.sz))
        .collect();
    for r in resting
        .iter()
        .filter(|r| r.coin == book.coin && r.is_buy != req.is_buy && r.trigger.is_none())
    {
        candidates.push((r.px, true, r.oid, r.account, r.sz));
    }
    candidates.sort_by(|a, b| {
        let by_px = if req.is_buy { a.0.cmp(&b.0) } else { b.0.cmp(&a.0) };
        by_px.then_with(|| b.1.cmp(&a.1))
    });
    candidates
}

fn walk(book: &L2Book, req: &Order, resting: &[RestingOrder]) -> Result<(Vec<Leg>, Decimal, Decimal)> {
    let side = usize::from(req.is_buy);
    let candidates = merge_candidates(book, req, resting);
    let mut remaining = req.size;
    let mut notional = Decimal::ZERO;
    let mut legs = Vec::new();
    for (px, is_maker, oid, maker, sz) in candidates {
        if remaining <= Decimal::ZERO {
            break;
        }
        if let Some(limit) = req.limit_px {
            let within = if req.is_buy { px <= limit } else { px >= limit };
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
            super::account::mul(take, px, "fill notional")?,
            "fill total",
        )?;
        if is_maker {
            legs.push(Leg::Maker { oid, maker, px, sz: take });
        } else {
            legs.push(Leg::Live(Consumption {
                coin: book.coin.clone(),
                side: side as u8,
                price: px,
                size: take,
            }));
        }
    }
    let filled = req.size - remaining;
    let avg_px = if filled.is_zero() {
        Decimal::ZERO
    } else {
        notional / filled
    };
    Ok((legs, filled, avg_px))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{Deal, DealClass};
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

    /// A one-asset meta.
    fn meta_with(coin: &str, max_leverage: u32) -> Meta {
        let mut m: Meta = serde_json::from_str(r#"{"universe": []}"#).unwrap();
        m.universe.push(crate::wire::AssetInfo {
            name: coin.into(),
            sz_decimals: 4,
            max_leverage,
            margin_table_id: None,
            only_isolated: None,
            is_delisted: None,
        });
        m
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
    fn ioc(is_buy: bool, size: &str) -> Order {
        Order {
            coin: "ETH".into(),
            is_buy,
            size: size.parse().unwrap(),
            limit_px: None,
            tif: Tif::Ioc,
            reduce_only: false,
            cloid: None,
            trigger: None,
        }
    }

    /// The same, reduce-only.
    fn reduce(is_buy: bool, size: &str) -> Order {
        Order {
            reduce_only: true,
            ..ioc(is_buy, size)
        }
    }

    /// I3: the fill moves the book and the position, never the mark.
    #[test]
    fn a_fill_moves_the_book_never_the_mark() {
        let feed = feed_with_book(book("ETH", &[("100", "2")], &[("101", "2")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        let mark_before = feed.mark("ETH").unwrap();

        let fill = sim.order(&feed, a, &ioc(true, "1.5")).unwrap();
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
        sim.order(&feed, a, &ioc(true, "1")).unwrap();
        let served = feed.book("ETH", 0).unwrap();
        assert_eq!(served.levels[1].len(), 1, "the consumed ask left the book");
        assert_eq!(served.levels[1][0].px, "102".parse().unwrap());
        assert_eq!(served.levels[0][0].sz, "5".parse().unwrap(), "the bid is live");
        assert_eq!(feed.owned_levels(), 1);
        // A second order walks into the next level.
        let fill = sim.order(&feed, a, &ioc(true, "1")).unwrap();
        assert_eq!(fill.avg_px, "102".parse().unwrap());
        assert_eq!(feed.owned_levels(), 2);
    }

    /// I2/I6: a live update refreshes the unowned levels, keeps the owned
    /// delta, and prunes the records the market moved past.
    #[test]
    fn live_updates_keep_the_delta_and_prune_stale_records() {
        let feed = feed_with_book(book("ETH", &[], &[("101", "1"), ("102", "1")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(true, "1")).unwrap();
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
        sim.order(&feed, a, &ioc(true, "1")).unwrap();
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
        sim.order(&feed, a, &ioc(true, "2")).unwrap();
        let before = sim.account(a).unwrap().unwrap().perp_balance;
        // Sell 1 at 100: closed 1·(100 − 101) = −1.
        sim.order(&feed, a, &reduce(false, "1")).unwrap();
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
        let mut o = ioc(true, "1");
        o.limit_px = Some("100.5".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("could not immediately match"), "{err}");
        let mut o = ioc(false, "1");
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
        let err = sim.order(&feed, a, &ioc(true, "0.1")).unwrap_err();
        assert!(format!("{err}").contains("stale"), "{err}");
        // The scenario scripts the coin: its present is never stale.
        feed.own_book(&book("ETH", &[("100", "5")], &[("101", "5")])).unwrap();
        let fill = sim.order(&feed, a, &ioc(true, "0.1")).unwrap();
        assert_eq!(fill.total_sz, "0.1".parse().unwrap());
    }

    /// The venue settles funding at the trade: a fill accrues to now on the
    /// *pre-fill* size and restarts `cumFunding.sinceChange`.
    #[test]
    fn a_fill_settles_funding_and_restarts_since_change() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(true, "0.1")).unwrap();
        // An hour passes (the fixture's ctx carries a nonzero funding rate),
        // then a second fill: the pre-fill size pays the hour, and the change
        // restarts since-change.
        sim.advance(&feed, 3600, 1).unwrap();
        sim.order(&feed, a, &ioc(true, "0.1")).unwrap();
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
        let err = sim.order(&feed, a, &ioc(true, "0.00001")).unwrap_err();
        assert!(format!("{err}").contains("szDecimals"), "{err}");
        // 3 decimals > 6 − 4: the venue's tick-size refusal.
        let mut o = ioc(true, "1");
        o.limit_px = Some("100.123".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(
            format!("{err}").contains("divisible by tick size"),
            "{err}"
        );
        // 6 significant figures.
        let mut o = ioc(true, "1");
        o.limit_px = Some("12345.6".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("significant"), "{err}");
        // 1 × 5 = 5 < the $10 minimum.
        let mut o = ioc(true, "1");
        o.limit_px = Some("5".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("minimum"), "{err}");
        // 0.1 × 101 = 10.1 ≥ the minimum: valid.
        let mut o = ioc(true, "0.1");
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
        let mut gtc = ioc(true, "0.2");
        gtc.tif = Tif::Gtc;
        gtc.limit_px = Some("99".parse().unwrap());
        let placed = sim.place(&feed, a, &gtc).unwrap();
        assert!(placed.resting.is_some(), "a Gtc order rests: {placed:?}");
        // Post-only above the market: the venue's own refusal, with the bbo.
        let mut alo = ioc(true, "0.2");
        alo.tif = Tif::Alo;
        alo.limit_px = Some("102".parse().unwrap());
        let err = sim.place(&feed, a, &alo).unwrap_err();
        assert!(
            format!("{err}").contains("Post only order would have immediately matched"),
            "{err}"
        );
        // A reduce-only order with nothing to reduce.
        let err = sim.order(&feed, a, &reduce(false, "1")).unwrap_err();
        assert!(
            format!("{err}").contains("Reduce only order would increase position"),
            "{err}"
        );
        // An unfunded opening: the numbers ride in the refusal.
        let b = Address::repeat_byte(0xbb);
        let err = sim.order(&feed, b, &ioc(true, "5")).unwrap_err();
        assert!(
            format!("{err}").contains("Insufficient margin to place order."),
            "{err}"
        );
    }
}
