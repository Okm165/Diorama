//! The venue's liquidation waterfall (docs): an account below its
//! maintenance requirement is first attempted to be entirely closed by
//! market orders to the book (20 % per attempt above $100k, the full size
//! during the 30 s cooldown that follows a partial); what the book cannot act
//! on escalates — below ⅔ of the maintenance requirement the backstop (the
//! liquidator vault) takes the positions and margin over, and a negative
//! account value goes to ADL: the opposite side, ranked by
//! `(mark/entry)·(notional/account_value)` at the previous mark, is closed
//! against the underwater account. The two scopes (`Leg`) are the docs'
//! independence rule: isolated liquidations touch only their own position and
//! margin, cross ones only the cross side. The thin venue's boundary: the
//! ADL's opposite side is the sim's accounts, not the real venue's whole
//! book — an empty ranking leaves the position for the next tick (named in
//! the log).
use super::account::{self, Liquidation};
use super::{Account, Sim, BACKSTOP};
use crate::error::Result;
use crate::feed::MarketFeed;
use alloy_primitives::Address;
use rust_decimal::Decimal;
use std::sync::atomic::Ordering;

/// Whether any of the account's positions held for the leg: the cross-margin
/// rule needs "nothing to take" (the margin moves) apart from "taken and
/// refused" (nothing moves at all).
#[derive(PartialEq, Eq)]
enum Held {
    Any,
    None,
}

/// One coin's ADL outcome: only an acted-on coin is a close (the report feeds
/// the control response, the metrics and the facts).
#[derive(PartialEq, Eq)]
enum AdlOutcome {
    Acted,
    Idle,
}

impl Sim {
    /// The venue's liquidation waterfall (docs): an account below its
    /// maintenance requirement is first attempted through the book (20 % per
    /// attempt above $100k, the full size during the 30 s cooldown that
    /// follows a partial); what the book cannot act on escalates — below ⅔ of
    /// the maintenance requirement the backstop (the liquidator vault) takes
    /// the positions and margin over (the maintenance is not returned), and a
    /// negative account value goes to ADL — the opposite side, ranked by
    /// `(mark/entry)·(notional/account_value)` at the previous mark, is
    /// closed against the underwater account.
    pub fn check_all(&self, feed: &MarketFeed, now_ms: u64) -> Result<Vec<Liquidation>> {
        // The maintenance is tiered from the live meta; with none (the feed
        // still warming) the requirement reads zero and no liquidation fires —
        // the pass fails open, never liquidating on an unknown requirement.
        let accounts = self.account_snapshot()?;
        let mut out = Vec::new();
        for (address, account) in accounts {
            if let Some(record) = self.check_account(feed, address, &account, now_ms)? {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// The accounts snapshot (the decision loop mutates state, so it iterates
    /// a copy).
    fn account_snapshot(&self) -> Result<Vec<(Address, Account)>> {
        self.with_state(|st| {
            Ok(st
                .accounts
                .iter()
                .map(|(a, acc)| (*a, acc.clone()))
                .collect())
        })
    }

    /// One account's pass through the waterfall: the snapshot being checked,
    /// the wall clock and the feed — what every step of the walk needs.
    ///
    /// One account's waterfall: the cross side and every isolated position
    /// are checked *independently* (docs: "Liquidations in that asset do not
    /// affect other isolated positions or cross positions. Similarly, cross
    /// liquidations or other isolated liquidations do not affect the original
    /// isolated position"; "the only inputs to the computation are the
    /// isolated margin and the notional value of the isolated position").
    /// Each leg that needs rescuing runs the same step — ADL, the vault, the
    /// book — over its own positions and margin. One record per pass, the
    /// acted-on coins aggregated.
    fn check_account(
        &self,
        feed: &MarketFeed,
        address: Address,
        account: &Account,
        now_ms: u64,
    ) -> Result<Option<Liquidation>> {
        if account.positions.is_empty() {
            return Ok(None);
        }
        let meta = feed.meta();
        let v = account::view(account, feed, meta.as_ref())?;
        let pass = Pass {
            feed,
            address,
            account,
            now_ms,
        };
        let mut closed = self.check_cross_leg(&pass, &v)?;
        closed.extend(self.check_isolated_legs(&pass, &v, account)?);
        if closed.is_empty() {
            return Ok(None);
        }
        self.liquidated(closed, v.margin_summary.account_value)
    }

    /// The cross leg: the venue's own cross figure (the total minus the
    /// isolated margins — the served identity, measured live) against the
    /// cross maintenance.
    fn check_cross_leg(&self, pass: &Pass<'_>, v: &account::AccountView) -> Result<Vec<String>> {
        let equity = v.cross_margin_summary.account_value;
        let maintenance = v.cross_maintenance_margin_used;
        self.rescue_leg(
            pass,
            Leg::Cross,
            rescue_step(equity, maintenance),
            (equity, maintenance),
        )
    }

    /// The isolated legs: each position alone — its isolated margin plus its
    /// unrealized pnl (docs: "isolated positions will apply unrealized pnl as
    /// additional margin for the open position") against its own maintenance.
    fn check_isolated_legs(
        &self,
        pass: &Pass<'_>,
        v: &account::AccountView,
        account: &Account,
    ) -> Result<Vec<String>> {
        let mut closed = Vec::new();
        for (coin, p) in &account.positions {
            if p.leverage != crate::wire::LeverageType::Isolated {
                continue;
            }
            let Some(facts) = v.facts.get(coin) else { continue };
            let equity = account::add(
                p.isolated_margin,
                p.unrealized_pnl(facts.mark)?,
                "isolated equity",
            )?;
            closed.extend(self.rescue_leg(
                pass,
                Leg::Isolated(coin),
                rescue_step(equity, facts.maintenance),
                (equity, facts.maintenance),
            )?);
        }
        Ok(closed)
    }

    /// One leg's rescue (docs): below the maintenance requirement the
    /// positions are first attempted to be entirely closed by market orders
    /// to the book — for every below-maintenance account, a negative value
    /// included. What the book *cannot act on at all* escalates: a still
    /// negative value goes to ADL ("the users on the opposite side ... are
    /// closed at the previous mark price against the now underwater user"),
    /// and a non-negative value below ⅔ of the maintenance requirement goes
    /// to the vault ("without successful liquidation through the book, a
    /// backstop liquidation happens"). A book that *did* act — a partial —
    /// waits out the cooldown: the next attempt is full-size, exactly the
    /// docs' partial-liquidation rule.
    ///
    /// `figures` is the leg's pre-attempt `(equity, maintenance)`; it is only
    /// read on the escalate path, where the book's attempt changed nothing.
    fn rescue_leg(
        &self,
        pass: &Pass<'_>,
        leg: Leg<'_>,
        rescue: Rescue,
        figures: (Decimal, Decimal),
    ) -> Result<Vec<String>> {
        let Rescue::Book = rescue else {
            return Ok(Vec::new());
        };
        let closed = self.liquidate_via_book(pass, leg)?;
        if !closed.is_empty() {
            return Ok(closed);
        }
        let (equity, maintenance) = figures;
        if equity < Decimal::ZERO {
            return self.adl(pass, leg);
        }
        let backstop = account::mul(equity, Decimal::from(3), "backstop check")?
            < account::mul(maintenance, Decimal::from(2), "backstop check")?;
        if backstop {
            return self.backstop(pass, leg);
        }
        Ok(Vec::new())
    }

    /// Step 1: market orders to the book. Above the $100k partial threshold
    /// only 20 % goes per attempt; after any partial, a 30 s cooldown makes
    /// every attempt the full size (docs). Partial fills by liquidity are
    /// expected — the remainder stays with the account.
    fn liquidate_via_book(&self, pass: &Pass<'_>, leg: Leg<'_>) -> Result<Vec<String>> {
        const COOLDOWN_MS: u64 = 30_000;
        let cooldown = self.with_state(|state| {
            Ok(state.liq_cooldown_until.get(&pass.address).copied().unwrap_or(0))
        })?;
        let cooldown = if pass.now_ms < cooldown {
            PartialCooldown::Active
        } else {
            PartialCooldown::Elapsed
        };
        let mut closed = Vec::new();
        let mut fill = Fill::Whole;
        for (coin, p) in &pass.account.positions {
            if !leg.holds(coin, p) {
                continue;
            }
            let outcome = self.liquidate_one(pass, (coin, p), cooldown)?;
            fill = fill.or(outcome.fill());
            if matches!(outcome, CloseOutcome::Landed { .. }) {
                closed.push(coin.clone());
            }
        }
        if matches!(fill, Fill::Partial) {
            let until = pass.now_ms.saturating_add(COOLDOWN_MS);
            self.with_state(|state| {
                state.liq_cooldown_until.insert(pass.address, until);
                Ok(())
            })?;
            self.metrics.partial_liquidations_total.fetch_add(1, Ordering::Relaxed);
        }
        Ok(closed)
    }

    /// One position's attempt inside the waterfall: `(closed, partial)`. The
    /// close may be refused (no liquidity), and a partial attempt sets the
    /// cooldown either way — the caller ORs the flag before it looks at the
    /// outcome.
    fn liquidate_one(
        &self,
        pass: &Pass<'_>,
        position: (&str, &account::Position),
        cooldown: PartialCooldown,
    ) -> Result<CloseOutcome> {
        let (coin, p) = position;
        let Some(mark) = pass.feed.mark(coin) else {
            return Ok(CloseOutcome::Refused {
                fill: Fill::Whole,
            });
        };
        let notional = account::mul(p.szi.abs(), mark, "liquidation notional")?;
        let (size, filled) = match attempt_size(p.szi.abs(), notional, cooldown)? {
            Attempt::Full(size) => (size, Fill::Whole),
            Attempt::Partial(size) => (size, Fill::Partial),
        };
        if size.is_zero() {
            return Ok(CloseOutcome::Refused { fill: filled });
        }
        match self.liquidate_position(pass.feed, pass.address, coin, size) {
            Ok(fill) => {
                tracing::warn!(account = %pass.address, coin = %coin, sz = %fill.total_sz, avg_px = %fill.avg_px, filled = ?filled, "liquidation: market close");
                Ok(CloseOutcome::Landed { fill: filled })
            }
            Err(e) => {
                tracing::warn!(account = %pass.address, coin = %coin, error = %e, "liquidation close refused (no liquidity?)");
                Ok(CloseOutcome::Refused { fill: filled })
            }
        }
    }

    /// Step 2: the backstop (the liquidator vault) takes the positions and
    /// margin over — the maintenance is not returned, so the trader ends at
    /// zero equity (docs). The vault's entry is the mark.
    fn backstop(&self, pass: &Pass<'_>, leg: Leg<'_>) -> Result<Vec<String>> {
        let closed = self.vault_take(pass, leg)?;
        self.metrics.backstop_liquidations_total.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(account = %pass.address, closed = closed.len(), "backstop liquidation: the vault took the position(s)");
        Ok(closed)
    }

    /// The vault takes the leg's positions over at the mark (same side,
    /// entered there — its own book starts flat) and the leg's margin with it
    /// (docs: a cross backstop transfers the cross positions and the cross
    /// margin; an isolated backstop transfers that position and its isolated
    /// margin — "the user's cross margin and positions are untouched").
    /// Returns the coins taken; a refused transfer stays with the account and
    /// is named.
    fn vault_take(&self, pass: &Pass<'_>, leg: Leg<'_>) -> Result<Vec<String>> {
        let mut closed = Vec::new();
        self.with_state(|st| {
            let mut me = st.accounts.remove(&pass.address).unwrap_or_default();
            let mut vault = st.accounts.remove(&BACKSTOP).unwrap_or_default();
            let mut margin = Decimal::ZERO;
            let mut held = Held::None;
            let coins: Vec<String> = me.positions.keys().cloned().collect();
            for coin in coins {
                let holds = me
                    .positions
                    .get(&coin)
                    .map(|p| leg.holds(&coin, p))
                    .unwrap_or(false);
                if !holds {
                    continue;
                }
                held = Held::Any;
                if let Some(taken) =
                    vault_take_coin(&mut vault, &mut me, pass.address, &coin, pass.feed)
                {
                    margin = account::add(margin, taken, "backstop margin")?;
                    closed.push(coin);
                }
            }
            // The cross margin moves with the cross backstop (docs); if every
            // cross transfer was refused, nothing moves at all — the log names
            // each refusal.
            if matches!(leg, Leg::Cross) && (held == Held::None || !closed.is_empty()) {
                margin = account::add(margin, std::mem::take(&mut me.perp_balance), "backstop margin")?;
            }
            vault.perp_balance = account::add(vault.perp_balance, margin, "backstop balance")?;
            st.accounts.insert(pass.address, me);
            st.accounts.insert(BACKSTOP, vault);
            Ok(closed)
        })
    }

    /// Step 3: ADL — the platform must stay solvent. The opposite side is
    /// ranked by `(mark/entry)·(notional/account_value)` and closed at the
    /// *previous* mark against the underwater account (docs).
    fn adl(&self, pass: &Pass<'_>, leg: Leg<'_>) -> Result<Vec<String>> {
        let meta = pass.feed.meta();
        let mut closed = Vec::new();
        for (coin, p) in &pass.account.positions {
            if !leg.holds(coin, p) {
                continue;
            }
            if matches!(self.adl_coin(pass, coin, p, meta.as_ref())?, AdlOutcome::Acted) {
                closed.push(coin.clone());
            }
        }
        Ok(closed)
    }

    /// One coin's ADL: rank the opposite side and close it against the
    /// underwater account at the previous mark. Answers whether anything was
    /// acted on — only an acted-on coin is a close: the report feeds the
    /// control response, the metrics and the facts (found by the local daloc
    /// run: the unconditional push claimed a close that never happened).
    fn adl_coin(
        &self,
        pass: &Pass<'_>,
        coin: &str,
        p: &account::Position,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<AdlOutcome> {
        let Some(prev_mark) = self.with_state(|state| Ok(state.prev_marks.get(coin).copied()))? else {
            tracing::warn!(account = %pass.address, coin = %coin, "ADL skipped: no previous mark recorded yet");
            return Ok(AdlOutcome::Idle);
        };
        let remaining = p.szi.abs();
        let direction = p.direction();
        let mut ranked = self.adl_ranking(
            pass.feed,
            pass.address,
            AdlTarget {
                coin,
                prev_mark,
                direction,
            },
            meta,
        )?;
        ranked.sort_by_key(|r| std::cmp::Reverse(r.0));
        if ranked.is_empty() {
            // The thin venue's opposite side is empty: the real venue's ADL
            // ranks the whole book's other side, which always exists. Nothing
            // closes — the attempt is retried next tick.
            tracing::warn!(account = %pass.address, coin = %coin, "ADL found no counterparties (the thin venue's opposite side is empty); the position stays");
            return Ok(AdlOutcome::Idle);
        }
        let mut left = remaining;
        for (_, other, size, _entry) in ranked {
            if left <= Decimal::ZERO {
                break;
            }
            let take = left.min(size);
            // The counterparty closes at the previous mark; the underwater
            // account's side closes against it at the same price.
            let mine = direction.entry_side();
            let own = account::FillFacts::close(coin, mine.flip(), take, prev_mark, p.leverage_value);
            let theirs = account::FillFacts::close(coin, mine, take, prev_mark, 1);
            self.adl_close_pair(pass.address, other, (&own, &theirs), meta)?;
            tracing::warn!(account = %pass.address, other = %other, coin = %coin, sz = %take, px = %prev_mark, "ADL close at the previous mark");
            left = left.saturating_sub(take);
        }
        Ok(if left < remaining {
            AdlOutcome::Acted
        } else {
            AdlOutcome::Idle
        })
    }

    /// Close both sides of one ADL fill — the underwater account's own side
    /// and the ranked counterparty's against it, at the same price — counted
    /// as one close.
    fn adl_close_pair(
        &self,
        address: Address,
        other: Address,
        fills: (&account::FillFacts, &account::FillFacts),
        meta: Option<&crate::wire::Meta>,
    ) -> Result<()> {
        let (own, theirs) = fills;
        self.with_account(address, |a| account::apply_fill(a, own, meta))?;
        self.with_account(other, |a| account::apply_fill(a, theirs, meta))?;
        self.metrics.adl_closes_total.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// The ADL ranking: the opposite side, `(mark/entry)·(notional/account_value)`
    /// at the previous mark, descending (docs).
    fn adl_ranking(
        &self,
        feed: &MarketFeed,
        address: Address,
        target: AdlTarget<'_>,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<Vec<(Decimal, Address, Decimal, Decimal)>> {
        let AdlTarget {
            coin,
            prev_mark,
            direction,
        } = target;
        let mut ranked: Vec<(Decimal, Address, Decimal, Decimal)> = Vec::new();
        self.with_state(|st| {
            for (other, oa) in st.accounts.iter() {
                if *other == address {
                    continue;
                }
                let Some(op) = oa.positions.get(coin) else { continue };
                if op.direction() == direction || op.szi.is_zero() {
                    continue;
                }
                let ov = account::view(oa, feed, meta)?;
                if ov.margin_summary.account_value <= Decimal::ZERO {
                    continue;
                }
                let ratio = account::mul(
                    account::ratio_or(prev_mark, op.entry_px, prev_mark),
                    account::mul(op.szi.abs(), prev_mark, "adl notional")?
                        / ov.margin_summary.account_value,
                    "adl rank",
                )?;
                ranked.push((ratio, *other, op.szi.abs(), op.entry_px));
            }
            Ok(())
        })?;
        Ok(ranked)
    }

    /// One liquidation's tail: counted and marked dirty (saved), with the
    /// `Liquidation` the caller returns. `account_value` is the figure at the
    /// close — zero for the book/ADL paths, the view's number for the backstop.
    fn liquidated(
        &self,
        positions_closed: Vec<String>,
        account_value: Decimal,
    ) -> Result<Option<Liquidation>> {
        self.metrics.liquidations_total.fetch_add(1, Ordering::Relaxed);
        self.mark_dirty()?;
        Ok(Some(Liquidation {
            positions_closed,
            account_value,
            maintenance: Decimal::ZERO,
        }))
    }
}

/// How much of a liquidation leg the close filled (the cooldown ORs it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    Whole,
    Partial,
}

impl Fill {
    /// Either attempt's fill: partial absorbs (the caller's OR).
    fn or(self, other: Fill) -> Fill {
        match (self, other) {
            (Fill::Whole, Fill::Whole) => Fill::Whole,
            _ => Fill::Partial,
        }
    }
}

/// One leg's liquidation attempt: the close either landed (the market order
/// filled) or was refused (no liquidity) — and the attempt may have been
/// partial either way, so the fill rides both variants (the caller ORs it
/// into the cooldown before it looks at the outcome).
#[derive(Debug, Clone, Copy)]
enum CloseOutcome {
    Landed { fill: Fill },
    Refused { fill: Fill },
}

impl CloseOutcome {
    /// The attempt's fill.
    fn fill(self) -> Fill {
        match self {
            CloseOutcome::Landed { fill } | CloseOutcome::Refused { fill } => fill,
        }
    }
}

/// The waterfall's entry step for one underwater leg.
#[derive(Clone, Copy)]
enum Rescue {
    /// At or above the maintenance requirement: nothing to do.
    Solvent,
    /// Below the maintenance requirement: attempt the book, escalating what
    /// the book cannot act on (see `rescue_leg`).
    Book,
}

/// The waterfall's entry decision (docs: "the positions are first attempted
/// to be entirely closed by sending market orders to the book" — for every
/// account below its maintenance requirement, a negative value included).
/// One position's backstop transfer: the vault takes it at the mark (the
/// same side, entered there) and the position's own isolated margin moves
/// with it. A refused transfer names itself and moves nothing — the
/// position stays for the next pass.
fn vault_take_coin(
    vault: &mut Account,
    me: &mut Account,
    address: Address,
    coin: &str,
    feed: &MarketFeed,
) -> Option<Decimal> {
    let (taken, direction, size, entry_px, leverage) = me.positions.get(coin).map(|p| {
        (
            p.isolated_margin,
            p.direction(),
            p.szi.abs(),
            p.entry_px,
            p.leverage_value,
        )
    })?;
    let facts = account::FillFacts {
        coin: coin.to_string(),
        // The vault takes the position on the same side (entered at the mark).
        side: direction.entry_side(),
        size,
        px: feed.mark_or(coin, entry_px),
        leverage,
        reduce: crate::sim::order::Reduce::Any,
        fee: Decimal::ZERO,
    };
    if let Err(e) = account::apply_fill(vault, &facts, feed.meta().as_ref()) {
        tracing::warn!(account = %address, coin = %coin, error = %e, "backstop transfer refused");
        return None;
    }
    me.positions.remove(coin);
    Some(taken)
}

fn rescue_step(equity: Decimal, maintenance: Decimal) -> Rescue {
    if equity >= maintenance {
        Rescue::Solvent
    } else {
        Rescue::Book
    }
}

/// The ADL ranking's target: the leg being closed (its coin, the previous
/// mark, and the direction the underwater position holds).
#[derive(Debug, Clone, Copy)]
struct AdlTarget<'a> {
    coin: &'a str,
    prev_mark: Decimal,
    direction: account::Direction,
}

/// One account's walk through the waterfall: the snapshot the check reads,
/// the wall clock the cooldown compares and the feed every step prices from.
struct Pass<'a> {
    feed: &'a MarketFeed,
    address: Address,
    account: &'a Account,
    now_ms: u64,
}

/// The two independent scopes of an account's waterfall (docs: "Liquidations
/// in that asset do not affect other isolated positions or cross positions.
/// Similarly, cross liquidations or other isolated liquidations do not affect
/// the original isolated position."): the cross side — its positions and the
/// cross balance — or one isolated position with its isolated margin.
#[derive(Clone, Copy)]
enum Leg<'a> {
    Cross,
    Isolated(&'a str),
}

impl Leg<'_> {
    /// Whether this leg owns the position.
    fn holds(&self, coin: &str, p: &account::Position) -> bool {
        match self {
            Leg::Cross => p.leverage == crate::wire::LeverageType::Cross,
            Leg::Isolated(c) => p.leverage == crate::wire::LeverageType::Isolated && *c == coin,
        }
    }
}

/// Whether the 30 s cooldown after a partial liquidation has elapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PartialCooldown {
    Active,
    Elapsed,
}

/// The liquidation attempt's size: the full position, or the partial share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Attempt {
    Full(Decimal),
    Partial(Decimal),
}

/// The liquidation attempt's size (docs): above the $100k partial threshold
/// only 20 % goes per attempt; during the 30 s cooldown after a partial, the
/// full size.
pub(super) fn attempt_size(
    full: Decimal,
    notional: Decimal,
    cooldown: PartialCooldown,
) -> Result<Attempt> {
    const PARTIAL_THRESHOLD: Decimal = Decimal::from_parts(100_000, 0, 0, false, 0);
    if notional > PARTIAL_THRESHOLD && matches!(cooldown, PartialCooldown::Elapsed) {
        return Ok(Attempt::Partial(account::mul(
            full,
            Decimal::new(2, 1),
            "partial size",
        )?));
    }
    Ok(Attempt::Full(full))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{feed_with, meta_of, meta_with};

    /// The waterfall's attempt sizes (docs): 20 % above the $100k partial
    /// threshold, the full size below it and during the 30 s cooldown.
    #[test]
    fn liquidation_attempts_follow_the_partial_rules() {
        let full = Decimal::from(10);
        let Attempt::Partial(sz) =
            attempt_size(full, Decimal::from(120_000), PartialCooldown::Elapsed).unwrap()
        else {
            panic!("above the threshold the attempt is partial");
        };
        assert_eq!(sz, Decimal::from(2));
        let Attempt::Full(sz) =
            attempt_size(full, Decimal::from(120_000), PartialCooldown::Active).unwrap()
        else {
            panic!("the cooldown switches to the full size");
        };
        assert_eq!(sz, full);
        let Attempt::Full(sz) =
            attempt_size(full, Decimal::from(50_000), PartialCooldown::Elapsed).unwrap()
        else {
            panic!("below the threshold the attempt is full");
        };
        assert_eq!(sz, full);
    }

    /// The waterfall's fixture: one account holding a single position.
    fn put_position(
        sim: &Sim,
        address: Address,
        coin: &str,
        facts: (Decimal, Decimal, Decimal, u32),
    ) {
        let (balance, entry, szi, leverage) = facts;
        sim.with_account(address, |acc| {
            acc.perp_balance = balance;
            acc.positions.insert(
                coin.into(),
                crate::sim::account::Position {
                    szi,
                    entry_px: entry,
                    leverage: crate::wire::LeverageType::Cross,
                    leverage_value: leverage,
                    margin_used: Decimal::ZERO,
                    isolated_margin: Decimal::ZERO,
                    funding_all_time: Decimal::ZERO,
                    funding_since_open: Decimal::ZERO,
                    funding_since_change: Decimal::ZERO,
                },
            );
            Ok(())
        })
        .unwrap();
    }

    /// The ETH shorthand the older fixtures use.
    fn put_account(sim: &Sim, address: Address, facts: (Decimal, Decimal, Decimal, u32)) {
        put_position(sim, address, "ETH", facts);
    }

    /// Below ⅔ of the maintenance requirement the backstop takes the position
    /// and the margin over: the vault holds the same side, the trader ends at
    /// zero equity (docs).
    #[test]
    fn the_backstop_takes_the_position_below_two_thirds() {
        let feed = feed_with(&[("ETH", "40", "40")]);
        feed.put_meta(meta_with("ETH", 10), crate::now_ms());
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        // Equity 0 (10 + (40 − 50)), maintenance 40/10/2 = 2, ⅔ of it 1.33.
        put_account(&sim, a, (Decimal::from(10), Decimal::from(50), Decimal::ONE, 10));
        let out = sim.check_all(&feed, crate::now_ms()).unwrap();
        assert_eq!(out.len(), 1, "one liquidation record");
        let a_acc = sim.account(a).unwrap().unwrap();
        assert!(a_acc
            .positions
            .get("ETH")
            .map(|p| p.szi.is_zero())
            .unwrap_or(true));
        assert_eq!(a_acc.perp_balance, Decimal::ZERO, "the maintenance is not returned");
        let vault = sim.account(BACKSTOP).unwrap().expect("the vault is touched");
        assert_eq!(
            vault.positions.get("ETH").map(|p| p.szi),
            Some(Decimal::ONE),
            "the vault takes the same side over"
        );
        assert_eq!(
            sim.metrics
                .backstop_liquidations_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    /// A negative account value goes to ADL — after the book's attempt
    /// (the fixture has no book, so it cannot act): the opposite side
    /// (ranked by (mark/entry)·(notional/account_value)) is closed at the
    /// previous mark, and the platform stays solvent (docs).
    #[test]
    fn adl_closes_the_opposite_side_at_the_previous_mark() {
        let feed = feed_with(&[("ETH", "100", "100")]);
        feed.put_meta(meta_with("ETH", 10), crate::now_ms());
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa); // long 1 @ 100, thin
        let b = Address::repeat_byte(0xbb); // short 1 @ 100, in profit
        put_account(&sim, a, (Decimal::from(10), Decimal::from(100), Decimal::ONE, 10));
        put_account(&sim, b, (Decimal::from(10), Decimal::from(100), -Decimal::ONE, 10));
        // The previous mark is 90; the mark then falls to 80: A's equity = −10.
        feed.put_ctx("ETH".into(), crate::wire::test_ctx("90", "90"), crate::now_ms());
        sim.record_marks(&feed).unwrap();
        feed.put_ctx("ETH".into(), crate::wire::test_ctx("80", "80"), crate::now_ms());
        let out = sim.check_all(&feed, crate::now_ms()).unwrap();
        assert_eq!(out.len(), 1, "one liquidation record");
        let a_acc = sim.account(a).unwrap().unwrap();
        assert!(a_acc
            .positions
            .get("ETH")
            .map(|p| p.szi.is_zero())
            .unwrap_or(true));
        // A closed at 90: (90 − 100)·1 = −10 → balance 0 (solvent, no bad debt).
        assert_eq!(a_acc.perp_balance, Decimal::ZERO);
        let b_acc = sim.account(b).unwrap().unwrap();
        assert!(b_acc
            .positions
            .get("ETH")
            .map(|p| p.szi.is_zero())
            .unwrap_or(true));
        // B bought back at the previous mark 90: (100 − 90)·1 = +10.
        assert_eq!(b_acc.perp_balance, Decimal::from(20));
        assert_eq!(
            sim.metrics
                .adl_closes_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }
    /// The docs' sequence, first step: below the maintenance requirement the
    /// book is attempted — a negative value included ("the positions are
    /// first attempted to be entirely closed by sending market orders to the
    /// book"). With liquidity the close lands at the book, the account ends
    /// negative, and ADL is not reached (found by the local daloc run: the
    /// negative account skipped the book and stuck in ADL with no
    /// counterparties, while the docs' first step would have closed it).
    #[test]
    fn a_negative_value_is_book_closed_first() {
        let now = crate::now_ms();
        let feed = MarketFeed::memory(vec!["BTC".to_string()]);
        feed.put_meta(meta_with("BTC", 10), now);
        feed.put_ctx("BTC".into(), crate::wire::test_ctx("80", "80"), now);
        let level = |px: &str, sz: &str| crate::wire::Level {
            px: px.parse().unwrap(),
            sz: sz.parse().unwrap(),
            n: 1,
        };
        feed.put_book(
            crate::wire::L2Book {
                coin: "BTC".into(),
                time: now,
                levels: [vec![level("80", "2")], vec![level("81", "2")]],
                spread: None,
            },
            now,
        );
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa); // long 1 @ 100, balance 10 → equity −10
        put_position(&sim, a, "BTC", (Decimal::from(10), Decimal::from(100), Decimal::ONE, 10));
        let out = sim.check_all(&feed, now).unwrap();
        assert_eq!(out.len(), 1, "one liquidation record");
        assert_eq!(out[0].positions_closed, vec!["BTC".to_string()]);
        let acc = sim.account(a).unwrap().unwrap();
        assert!(acc.positions.is_empty(), "the book closed the position");
        // The close at the book's 80: (80 − 100)·1 = −20 → balance −10.
        assert_eq!(acc.perp_balance, Decimal::from(-10));
        assert_eq!(
            sim.metrics
                .adl_closes_total
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "ADL is not reached: the book acted"
        );
    }

    /// The waterfall's fixture for an isolated leg: one isolated position
    /// carrying its own margin (the mode switch parks it there).
    fn put_isolated(
        sim: &Sim,
        address: Address,
        coin: &str,
        facts: (Decimal, Decimal, u32, Decimal),
    ) {
        let (entry, szi, leverage, margin) = facts;
        sim.with_account(address, |acc| {
            acc.positions.insert(
                coin.into(),
                crate::sim::account::Position {
                    szi,
                    entry_px: entry,
                    leverage: crate::wire::LeverageType::Isolated,
                    leverage_value: leverage,
                    margin_used: margin,
                    isolated_margin: margin,
                    funding_all_time: Decimal::ZERO,
                    funding_since_open: Decimal::ZERO,
                    funding_since_change: Decimal::ZERO,
                },
            );
            Ok(())
        })
        .unwrap();
    }

    /// The legs are independent (docs: "Liquidations in that asset do not
    /// affect other isolated positions or cross positions"): an isolated
    /// position below its own maintenance goes to the vault alone — "the
    /// user's cross margin and positions are untouched".
    #[test]
    fn an_isolated_leg_goes_to_the_vault_alone() {
        let feed = feed_with(&[("ETH", "100", "100"), ("BTC", "100", "100")]);
        feed.put_meta(meta_of(&[("ETH", 10), ("BTC", 10)]), crate::now_ms());
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        put_account(&sim, a, (Decimal::from(10), Decimal::from(100), Decimal::ONE, 10));
        // BTC isolated: margin 1, maintenance 100/10/2 = 5 → 3·1 < 2·5: the
        // vault (the fixture has no book, so the book cannot act first).
        put_isolated(&sim, a, "BTC", (Decimal::from(100), Decimal::ONE, 10, Decimal::ONE));
        let out = sim.check_all(&feed, crate::now_ms()).unwrap();
        assert_eq!(out.len(), 1, "one liquidation record");
        assert_eq!(out[0].positions_closed, vec!["BTC".to_string()]);
        let acc = sim.account(a).unwrap().unwrap();
        assert_eq!(
            acc.positions.get("ETH").map(|p| p.szi),
            Some(Decimal::ONE),
            "the cross side is untouched"
        );
        assert_eq!(acc.perp_balance, Decimal::from(10), "the cross balance is untouched");
        assert!(!acc.positions.contains_key("BTC"), "the isolated position is gone");
        let vault = sim.account(BACKSTOP).unwrap().expect("the vault is touched");
        assert_eq!(vault.positions.get("BTC").map(|p| p.szi), Some(Decimal::ONE));
        assert_eq!(vault.perp_balance, Decimal::ONE, "the isolated margin moved with it");
    }

    /// A cross backstop transfers the cross positions and the cross margin
    /// only (docs: "the trader's cross positions and cross margin are all
    /// transferred"); the trader keeps the isolated position and its margin —
    /// "if the trader has no isolated positions, the trader ends up with zero
    /// account equity" implies the isolated margin stays otherwise.
    #[test]
    fn the_cross_backstop_leaves_isolated_positions() {
        let feed = feed_with(&[("ETH", "100", "100"), ("BTC", "100", "100")]);
        feed.put_meta(meta_of(&[("ETH", 10), ("BTC", 10)]), crate::now_ms());
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        // Cross: balance 3, ETH long 1 @ 100 → cross value 3 < 5 and
        // 3·3 < 2·5 → the vault (no book in the fixture: the book cannot
        // act first).
        put_account(&sim, a, (Decimal::from(3), Decimal::from(100), Decimal::ONE, 10));
        // BTC isolated, margin 6: 6 >= maintenance 5 → solvent.
        put_isolated(&sim, a, "BTC", (Decimal::from(100), Decimal::ONE, 10, Decimal::from(6)));
        let out = sim.check_all(&feed, crate::now_ms()).unwrap();
        assert_eq!(out.len(), 1, "one liquidation record");
        assert_eq!(out[0].positions_closed, vec!["ETH".to_string()]);
        let acc = sim.account(a).unwrap().unwrap();
        assert!(!acc.positions.contains_key("ETH"), "the vault took the cross position");
        assert_eq!(acc.perp_balance, Decimal::ZERO, "the cross margin moved with it");
        let btc = acc.positions.get("BTC").expect("the isolated position stays");
        assert_eq!(btc.isolated_margin, Decimal::from(6), "its margin stays with it");
        let vault = sim.account(BACKSTOP).unwrap().expect("the vault is touched");
        assert_eq!(vault.positions.get("ETH").map(|p| p.szi), Some(Decimal::ONE));
        assert!(
            !vault.positions.contains_key("BTC"),
            "the vault did not touch the isolated leg"
        );
    }

    /// Closing an isolated position returns its margin to the cross balance:
    /// the margin is the trader's collateral, only the position is gone.
    #[test]
    fn closing_an_isolated_position_returns_its_margin() {
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        put_isolated(&sim, a, "ETH", (Decimal::from(100), Decimal::ONE, 10, Decimal::from(4)));
        sim.with_account(a, |acc| {
            let facts =
                crate::sim::account::FillFacts::close(
                    "ETH",
                    crate::sim::order::Side::Sell,
                    Decimal::ONE,
                    Decimal::from(100),
                    10,
                );
            crate::sim::account::apply_fill(acc, &facts, None)
        })
        .unwrap();
        let acc = sim.account(a).unwrap().unwrap();
        assert!(acc.positions.is_empty(), "the position is closed");
        assert_eq!(acc.perp_balance, Decimal::from(4), "the isolated margin came back");
    }

    /// A strict isolated asset (docs: "the margin cannot be removed. Margin is
    /// proportionally removed as the position is closed"): removal is refused
    /// by name, and a half close releases half the margin.
    #[test]
    fn strict_isolated_margin_is_not_removable_and_releases_proportionally() {
        let feed = feed_with(&[("ETH", "100", "100")]);
        let mut meta = meta_with("ETH", 10);
        meta.universe[0].only_isolated = Some(true);
        feed.put_meta(meta, crate::now_ms());
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        put_isolated(&sim, a, "ETH", (Decimal::from(100), Decimal::from(2), 10, Decimal::from(8)));
        let refused = sim.update_isolated_margin(a, "ETH", -1_000_000, &feed);
        assert!(refused.is_err(), "strict isolated margin cannot be removed");
        assert_eq!(
            sim.account(a).unwrap().unwrap().positions["ETH"].isolated_margin,
            Decimal::from(8),
            "the refused removal left the margin alone"
        );
        // A half close releases half the margin (4), keeping the other 4.
        sim.with_account(a, |acc| {
            let facts =
                crate::sim::account::FillFacts::close(
                    "ETH",
                    crate::sim::order::Side::Sell,
                    Decimal::ONE,
                    Decimal::from(100),
                    10,
                );
            crate::sim::account::apply_fill(acc, &facts, feed.meta().as_ref())
        })
        .unwrap();
        let acc = sim.account(a).unwrap().unwrap();
        let p = acc.positions.get("ETH").expect("half the position remains");
        assert_eq!(p.szi, Decimal::ONE);
        assert_eq!(p.isolated_margin, Decimal::from(4), "released proportionally");
        assert_eq!(acc.perp_balance, Decimal::from(4), "the released half landed in the cross balance");
    }

    /// `updateLeverage` moves the initial margin across the boundary: to
    /// isolated parks it on the position, back to cross returns it.
    #[test]
    fn switching_to_isolated_parks_and_back_returns_the_margin() {
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        put_account(&sim, a, (Decimal::from(50), Decimal::from(100), Decimal::ONE, 10));
        sim.set_leverage(
            a,
            "ETH",
            account::LeverageSetting {
                mode: crate::wire::LeverageType::Isolated,
                leverage: 10,
            },
            None,
        )
            .unwrap();
        let acc = sim.account(a).unwrap().unwrap();
        assert_eq!(
            acc.positions.get("ETH").map(|p| p.isolated_margin),
            Some(Decimal::from(10)),
            "the initial margin is parked on the position"
        );
        assert_eq!(acc.perp_balance, Decimal::from(40));
        sim.set_leverage(
            a,
            "ETH",
            account::LeverageSetting {
                mode: crate::wire::LeverageType::Cross,
                leverage: 10,
            },
            None,
        )
            .unwrap();
        let acc = sim.account(a).unwrap().unwrap();
        assert_eq!(
            acc.positions.get("ETH").map(|p| p.isolated_margin),
            Some(Decimal::ZERO)
        );
        assert_eq!(acc.perp_balance, Decimal::from(50), "the margin came back");
    }

    /// The measured served identities with an isolated leg (probe: two live
    /// accounts): the total carries the isolated margin as cash —
    /// `accountValue = totalRawUsd + Σ sign·positionValue` holds with it
    /// included — the cross value is the total minus the margin, and the
    /// cross raw subtracts the margin and adds the isolated signed value.
    #[test]
    fn the_isolated_margin_rides_the_totals() {
        let feed = feed_with(&[("ETH", "100", "100"), ("BTC", "100", "100")]);
        feed.put_meta(meta_of(&[("ETH", 10), ("BTC", 10)]), crate::now_ms());
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        put_account(&sim, a, (Decimal::from(50), Decimal::from(100), Decimal::ONE, 10));
        put_isolated(&sim, a, "BTC", (Decimal::from(100), Decimal::ONE, 10, Decimal::from(6)));
        let acc = sim.account(a).unwrap().unwrap();
        let v = crate::sim::account::view(&acc, &feed, feed.meta().as_ref()).unwrap();
        // Total: cash 50 + 6, cost basis 200, signed 200 → value 56, raw −144.
        assert_eq!(v.margin_summary.account_value, Decimal::from(56));
        assert_eq!(v.margin_summary.total_raw_usd, Decimal::from(-144));
        assert_eq!(
            v.margin_summary.total_raw_usd + Decimal::from(200),
            v.margin_summary.account_value,
            "the client's identity holds with the isolated margin in the total"
        );
        // Cross: 56 − 6 = 50; raw −144 − 6 + 100 = −50.
        assert_eq!(v.cross_margin_summary.account_value, Decimal::from(50));
        assert_eq!(v.cross_margin_summary.total_raw_usd, Decimal::from(-50));
        let state =
            crate::sim::account::clearinghouse(&acc, &feed, feed.meta().as_ref(), 0).unwrap();
        assert!(crate::sim::account::identity_holds(&state));
    }
}
