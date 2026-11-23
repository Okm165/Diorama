//! The self-advance automaton: the evaluation the 1 Hz tick and
//! every account-scoped read share — funding accrues, delistings settle, the
//! resting book's passes run (scheduled cancels, armed triggers, passive
//! fills), the waterfall checks every account, the marks are recorded and the
//! state persists when something moved. The passes are exercised through the
//! lanes they act on (their tests live with the lanes: order.rs, waterfall.rs,
//! account.rs).
use super::account::{self, Liquidation};
use super::funding;
use super::ledger::{FillRecord, Liquidity};
use super::Sim;
use crate::error::{Error, Result};
use crate::feed::{MarketFeed, Prices};
use alloy_primitives::Address;
use rust_decimal::Decimal;
use std::sync::atomic::Ordering;

impl Sim {
    /// The delisting settlement (docs: "When an asset is delisted, all
    /// positions are settled and open orders are cancelled ... After
    /// settlement, no new orders will be accepted"): each delisted market's
    /// open orders are cancelled and every position is closed at the served
    /// oracle — the docs' settlement price is the 1 h TWAP spot oracle, a
    /// named simplification; no fee is charged (a settlement is not a trade).
    ///
    /// Every dex's delisted coins settle (a builder dex's `xyz:TSLA` as much
    /// as the default dex's).
    fn settle_delisted(&self, feed: &MarketFeed) -> Result<()> {
        for coin in feed.delisted_coins() {
            let Some(oracle) = feed.oracle(&coin) else {
                continue;
            };
            // The meta lists every delisted coin of its dex; the lookup only
            // fails if the registry changed under us (skip the coin).
            let Some(meta) = feed.meta_for_coin(&coin) else {
                continue;
            };
            if !self.has_market_state(&coin)? {
                continue;
            }
            self.cancel_market_orders(&coin)?;
            self.settle_market_positions(feed, &coin, oracle, &meta)?;
        }
        Ok(())
    }

    /// Whether any resting order or account position references `coin`: a
    /// delisted coin nobody touches costs the tick one lookup, not a state
    /// copy per coin (a venue lists hundreds of delisted coins).
    fn has_market_state(&self, coin: &str) -> Result<bool> {
        self.with_state(|st| {
            Ok(st.resting.values().any(|r| r.coin == coin)
                || st
                    .accounts
                    .values()
                    .any(|a| a.positions.get(coin).is_some_and(|p| !p.szi.is_zero())))
        })
    }

    /// The delisting's first clause: "open orders are cancelled".
    fn cancel_market_orders(&self, coin: &str) -> Result<()> {
        for r in self.resting_orders(None)? {
            if r.coin == coin {
                self.cancel(
                    r.account,
                    coin,
                    r.oid,
                    crate::sim::order::CancelSpeed::Normal,
                )?;
            }
        }
        Ok(())
    }

    /// The delisting's second clause: "all positions are settled" — closed at
    /// the served oracle (the docs' 1 h TWAP spot oracle, simplified).
    fn settle_market_positions(
        &self,
        feed: &MarketFeed,
        coin: &str,
        oracle: Decimal,
        meta: &crate::wire::Meta,
    ) -> Result<()> {
        for address in self.account_addresses()? {
            let Some(account) = self.account(address)? else {
                continue;
            };
            let Some(position) = account.positions.get(coin) else {
                continue;
            };
            let size = position.szi.abs();
            if size.is_zero() {
                continue;
            }
            let facts = account::FillFacts::close(
                coin,
                if position.szi.is_sign_negative() {
                    crate::sim::order::Side::Buy
                } else {
                    crate::sim::order::Side::Sell
                },
                size,
                oracle,
                position.leverage_value.max(1),
            );
            let effect = self.with_account_now(address, feed, |account| {
                account::apply_fill(account, &facts, Some(meta))
            })?;
            self.record_fill(
                address,
                &FillRecord {
                    liquidation: None,
                    facts: &facts,
                    effect,
                    oid: 0,
                    cloid: None,
                    liquidity: Liquidity::Taker,
                    fee_token: &feed.fee_token_of_coin(coin),
                },
            )?;
            tracing::info!(account = %address, coin = %coin, %size, %oracle, "delisted position settled at the oracle");
        }
        Ok(())
    }

    /// The §14.8 evaluation, one implementation for the 1 Hz tick and every
    /// account-scoped read: accrue funding to now, settle delistings, run the
    /// resting book's passes, check every account against its maintenance
    /// requirement (the waterfall), record the marks and persist if anything
    /// moved — so the consumer's set-and-read tests see live liquidation behavior
    /// without ever calling `advance` (funding steps at the hour). A warming
    /// market plane (no meta yet) is a no-op, not a fault storm.
    pub fn evaluate(&self, feed: &MarketFeed) -> Result<Vec<Liquidation>> {
        if feed.meta().is_none() {
            return Ok(Vec::new());
        }
        let now = crate::now_ms();
        self.accrue_all(feed, now)?;
        self.settle_delisted(feed)?;
        self.run_resting(feed, now)?;
        let liquidated = self.check_all(feed, now)?;
        self.record_marks(feed)?;
        self.save_if_dirty()?;
        Ok(liquidated)
    }

    /// The resting-book pass (the 1 Hz automaton): scheduled cancels fire,
    /// armed triggers whose mark crossed execute, and resting orders the live
    /// book has crossed fill at their own price (they are the maker; the live
    /// levels they take are consumed — the impact lane).
    pub(crate) fn run_resting(&self, feed: &MarketFeed, now_ms: u64) -> Result<()> {
        self.fire_scheduled_cancels(now_ms)?;
        self.fire_triggers(feed)?;
        self.passive_fills(feed, now_ms)
    }

    /// (a) `scheduleCancel`: the dead-man's switch cancels everything due.
    pub(crate) fn fire_scheduled_cancels(&self, now_ms: u64) -> Result<()> {
        let due: Vec<Address> = self.with_state(|st| {
            Ok(st
                .scheduled_cancels
                .iter()
                .filter(|(_, t)| **t <= now_ms)
                .map(|(a, _)| *a)
                .collect())
        })?;
        for account in due {
            let mine = self.resting_orders(Some(account))?;
            let n = mine.len();
            for r in mine {
                let _ = self.cancel(
                    account,
                    &r.coin,
                    r.oid,
                    crate::sim::order::CancelSpeed::Normal,
                );
            }
            self.with_state(|st| {
                st.scheduled_cancels.remove(&account);
                if let Some(acc) = st.accounts.get_mut(&account) {
                    let day = now_ms / 86_400_000;
                    if acc.schedule_fire_day != day {
                        acc.schedule_fire_day = day;
                        acc.schedule_fires = 0;
                    }
                    acc.schedule_fires = acc.schedule_fires.saturating_add(1);
                }
                Ok(())
            })?;
            tracing::info!(account = %account, canceled = n, "scheduled cancel fired (dead-man's switch)");
        }
        Ok(())
    }

    /// (b) Triggers: fire when the mark crosses the trigger price; the
    /// trigger leaves the book first (a failed execution cancels it, as the
    /// venue's does — never a silent re-arm).
    fn fire_triggers(&self, feed: &MarketFeed) -> Result<()> {
        for r in self.resting_orders(None)? {
            let Some(t) = r.trigger else { continue };
            let Some(Prices { mark, .. }) = feed.required_prices(&r.coin) else {
                continue;
            };
            let fires = match (r.side, t.tpsl) {
                (crate::sim::order::Side::Sell, crate::sim::order::Tpsl::Tp) => {
                    mark >= t.trigger_px
                }
                (crate::sim::order::Side::Sell, crate::sim::order::Tpsl::Sl) => {
                    mark <= t.trigger_px
                }
                (crate::sim::order::Side::Buy, crate::sim::order::Tpsl::Tp) => mark <= t.trigger_px,
                (crate::sim::order::Side::Buy, crate::sim::order::Tpsl::Sl) => mark >= t.trigger_px,
            };
            if !fires {
                continue;
            }
            let _ = self.cancel(
                r.account,
                &r.coin,
                r.oid,
                crate::sim::order::CancelSpeed::Normal,
            );
            let order = crate::sim::order::Order {
                coin: r.coin.clone(),
                is_buy: r.side.is_buy(),
                size: r.sz,
                limit_px: if t.is_market { None } else { Some(r.px) },
                tif: crate::sim::order::Tif::Ioc,
                reduce_only: r.reduce == crate::sim::order::Reduce::Only,
                cloid: r.cloid,
                trigger: None,
            };
            match self.place_with_builder(feed, r.account, &order, r.builder) {
                Ok(placed) => {
                    self.metrics
                        .triggers_fired_total
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::info!(account = %r.account, coin = %r.coin, oid = r.oid, filled = placed.fill.as_ref().map(|f| f.total_sz.to_string()).unwrap_or_default(), "trigger fired");
                }
                Err(e) => {
                    tracing::warn!(account = %r.account, coin = %r.coin, oid = r.oid, error = %e, "trigger execution refused; the trigger is canceled");
                }
            }
        }
        Ok(())
    }

    /// (c) Passive fills: a resting order the live book has crossed fills at
    /// its own price (the maker side). The maker *provides* the liquidity, so
    /// it consumes no live level: the fill's book effect is the resting
    /// order's remaining size shrinking, and that size is part of the served
    /// book (`with_resting` — the venue's `l2Book` shows every participant's
    /// resting orders). The old consumption here was a display proxy from
    /// when the sim's resting book was invisible: eating a live level "showed"
    /// the fill. With the maker's order displayed, that proxy double-counted
    /// (−2·take of displayed liquidity for one trade) and destroyed liquidity
    /// nobody took.
    fn passive_fills(&self, feed: &MarketFeed, now_ms: u64) -> Result<()> {
        for r in self.resting_orders(None)? {
            if r.trigger.is_some() {
                continue;
            }
            let Some(book) = feed.book(&r.coin, now_ms) else {
                continue;
            };
            let opposite = r.side.book_index();
            let Some(best) = book.levels[opposite].first() else {
                continue;
            };
            let crossed = match r.side {
                crate::sim::order::Side::Buy => best.px <= r.px,
                crate::sim::order::Side::Sell => best.px >= r.px,
            };
            if !crossed {
                continue;
            }
            // The queue ahead on the maker's **own** side: a displayed level at
            // its price was there before it rested, so the flow that reaches
            // that price must consume it first (measured 2026-11-22: a maker
            // behind a 0.9755 ask filled from a 0.1 taker buy while the level
            // stayed untouched). The crossing size is the opposite touch's
            // size; only the part above the queue ahead can reach the maker.
            // `book_index` is the side a taker of the maker's side consumes
            // (a buy maker consumes the asks); the queue ahead is on the maker's
            // **own** side.
            let own = 1 - r.side.book_index();
            let ahead = book.levels[own]
                .iter()
                .filter(|level| level.px == r.px)
                .map(|level| level.sz)
                .max()
                .unwrap_or(Decimal::ZERO);
            let reachable = best.sz - ahead;
            let take = r.sz.min(reachable.max(Decimal::ZERO));
            if take <= Decimal::ZERO {
                continue;
            }
            // A reduce-only maker can only ever fill against the position it
            // names: the size is capped to it, and an order whose position is
            // gone (closed while it rested) or flipped is canceled — the
            // venue does not keep an order that can never reduce anything.
            let Some(take) = self.reduce_only_take(&r, take)? else {
                continue;
            };
            // A spot resting order reserves tokens, not margin: the hold *is*
            // the cover, so no perp re-margin runs and the fill moves the spot
            // ledger. (Before this branch the perp re-margin looked the pair up
            // in the perp meta and refused every spot cross — a per-second WARN
            // and a maker that could never be filled by the live book, measured
            // 2026-11-21.)
            if feed.spot_pair(&r.coin).is_some() {
                if let Err(e) = self.passive_spot_fill(&r, take, feed) {
                    tracing::warn!(account = %r.account, coin = %r.coin, oid = r.oid, error = %e, "passive spot fill refused");
                }
                continue;
            }
            // The same re-margin rule as the taker path: a maker that cannot
            // carry the fill is canceled, not filled.
            if let Err(e) = self.remargin_one(feed, &r) {
                tracing::warn!(account = %r.account, oid = r.oid, error = %e, "passive re-margin check failed");
                continue;
            }
            if let Err(e) = self.passive_fill(&r, take, feed) {
                tracing::warn!(account = %r.account, coin = %r.coin, oid = r.oid, error = %e, "passive fill refused");
            }
        }
        Ok(())
    }

    /// A reduce-only maker's fill size against its own position: capped to
    /// the position it names, or `None` (the order canceled) when that
    /// position is gone or flipped — never a per-tick refusal of an order
    /// that can never reduce anything.
    fn reduce_only_take(
        &self,
        r: &crate::sim::order::RestingOrder,
        take: Decimal,
    ) -> Result<Option<Decimal>> {
        if r.reduce != crate::sim::order::Reduce::Only {
            return Ok(Some(take));
        }
        let held = self.position_szi(r.account, &r.coin)?;
        if crate::sim::account::same_direction(held, r.side.sign() * r.sz) {
            let _ = self.cancel(
                r.account,
                &r.coin,
                r.oid,
                crate::sim::order::CancelSpeed::Normal,
            );
            return Ok(None);
        }
        Ok(Some(take.min(held.abs())))
    }

    /// One spot passive fill: the maker's token legs settle against the
    /// order's own hold (a buy spends reserved quote and receives the base net
    /// of the maker fee; a sell the reverse) and the resting size shrinks. No
    /// live-level consumption — the maker provides the liquidity (see
    /// `passive_fills`). Mirrors the perp path's maker economics — the same
    /// ledger record — with tokens instead of a position.
    #[allow(
        clippy::too_many_lines,
        reason = "one spot maker settlement: tokens, ledger, level, shrink — read top to bottom"
    )]
    fn passive_spot_fill(
        &self,
        r: &crate::sim::order::RestingOrder,
        take: Decimal,
        feed: &MarketFeed,
    ) -> Result<()> {
        let pair = feed.spot_pair(&r.coin).ok_or(Error::Warming)?;
        let base = feed.spot_token(pair.tokens[0]).ok_or(Error::Warming)?;
        let quote = feed.spot_token(pair.tokens[1]).ok_or(Error::Warming)?;
        let notional = account::mul(take, r.px, "spot maker notional")?;
        let fee = account::mul(notional, Self::spot_maker_fee(), "spot maker fee")?;
        let fee_recorded = account::spot_fee_in_received(fee, r.px, r.side)?;
        let builder = r.builder.map(|b| b.charge(notional)).transpose()?;
        let effect = self.with_account_now(r.account, feed, |account| {
            account::settle_spot_maker(
                account,
                r.side,
                r.spot_hold,
                r.px,
                notional,
                take,
                fee,
                crate::sim::TokenRef { index: base.index, coin: &base.name },
                crate::sim::TokenRef { index: quote.index, coin: &quote.name },
            )
        })?;
        let facts = account::FillFacts {
            coin: r.coin.clone(),
            side: r.side,
            size: take,
            px: r.px,
            leverage: 1,
            mode: crate::wire::LeverageType::Cross,
            reduce: r.reduce,
            fee: fee_recorded,
            builder,
        };
        self.record_fill(
            r.account,
            &FillRecord {
                liquidation: None,
                facts: &facts,
                effect,
                oid: r.oid,
                cloid: r.cloid,
                liquidity: Liquidity::Maker,
                fee_token: if r.side == crate::sim::order::Side::Buy {
                    &base.name
                } else {
                    &quote.name
                },
            },
        )?;
        self.credit_builder(builder, crate::feed::dex::dex_of_coin(&r.coin))?;
        self.fill_resting_pub(r.oid, take, r.px)?;
        self.metrics.orders_filled_total.fetch_add(1, Ordering::Relaxed);
        self.metrics.fees_charged_total.fetch_add(1, Ordering::Relaxed);
        tracing::info!(account = %r.account, coin = %r.coin, oid = r.oid, sz = %take, px = %r.px, "resting spot order filled passively");
        Ok(())
    }

    /// The passive path's re-margin check: probe the maker's post-fill
    /// account; on breach, cancel the resting order (the venue's
    /// `marginCanceled`) and report it so the caller skips the fill.
    fn remargin_one(&self, feed: &MarketFeed, r: &crate::sim::order::RestingOrder) -> Result<()> {
        if self.remargin_ok(feed, r)? {
            return Ok(());
        }
        tracing::warn!(account = %r.account, oid = r.oid, "resting order canceled at match time (marginCanceled)");
        self.margin_cancel(r.account, &r.coin, r.oid)
    }

    /// One passive fill: the maker's account update (their price, the maker
    /// fee) plus the resting size's reduce. No live-level consumption — the
    /// maker provides the liquidity (see `passive_fills`).
    fn passive_fill(
        &self,
        r: &crate::sim::order::RestingOrder,
        take: Decimal,
        feed: &MarketFeed,
    ) -> Result<()> {
        let meta = feed.meta_for_lookup(&r.coin);
        let setting = self.leverage_setting_for(r.account, &r.coin, meta.as_ref())?;
        let notional = crate::sim::account::mul(take, r.px, "maker notional")?;
        let facts = crate::sim::account::FillFacts {
            coin: r.coin.clone(),
            side: r.side,
            size: take,
            px: r.px,
            leverage: setting.leverage,
            mode: setting.mode,
            reduce: r.reduce,
            fee: crate::sim::account::mul(
                notional,
                crate::sim::order::maker_fee()
                    * crate::sim::order::dex_fee_multiplier(meta.as_ref(), &r.coin),
                "maker fee",
            )?,
            builder: r.builder.map(|b| b.charge(notional)).transpose()?,
        };
        let effect = self.with_account_now(r.account, feed, |account| {
            crate::sim::account::apply_fill(account, &facts, meta.as_ref())
        })?;
        self.credit_builder(facts.builder, crate::feed::dex::dex_of_coin(&r.coin))?;
        self.record_fill(
            r.account,
            &FillRecord {
                liquidation: None,
                facts: &facts,
                effect,
                oid: r.oid,
                cloid: r.cloid,
                liquidity: Liquidity::Maker,
                fee_token: &feed.fee_token_of_coin(&r.coin),
            },
        )?;
        self.fill_resting_pub(r.oid, take, r.px)?;
        tracing::info!(account = %r.account, coin = %r.coin, oid = r.oid, sz = %take, px = %r.px, "resting order filled passively");
        Ok(())
    }

    /// Accrue funding for every account (the caller holds no lock).
    pub(super) fn accrue_all(&self, feed: &MarketFeed, now_ms: u64) -> Result<()> {
        let clock = self.clock(now_ms)?;
        let mut payments = 0u64;
        self.with_state(|state| {
            for account in state.accounts.values_mut() {
                payments += funding::accrue_funding(account, feed, clock)? as u64;
            }
            Ok(())
        })?;
        if payments > 0 {
            self.metrics
                .funding_payments_total
                .fetch_add(payments, Ordering::Relaxed);
            self.dirty.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Record the marks the ADL step reads as "the previous mark".
    pub(super) fn record_marks(&self, feed: &MarketFeed) -> Result<()> {
        self.with_state(|st| {
            // Every dex's coins: a builder dex's ADL close prices from the
            // same previous mark.
            for coin in feed.all_coins() {
                if let Some(mark) = feed.mark(&coin) {
                    st.prev_marks.insert(coin, mark);
                }
            }
            Ok(())
        })
    }
}
