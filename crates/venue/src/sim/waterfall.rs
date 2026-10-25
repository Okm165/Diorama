//! The venue's liquidation waterfall (docs): an account below its
//! maintenance requirement is first closed by market orders to the book
//! (20 % per attempt above $100k, the full size during the 30 s cooldown that
//! follows a partial); below ⅔ of the maintenance requirement the backstop
//! (the liquidator vault) takes the positions and margin over; a negative
//! account value goes to ADL — the opposite side, ranked by
//! `(mark/entry)·(notional/account_value)` at the previous mark, is closed
//! against the underwater account. The two scopes (`Leg`) are the docs'
//! independence rule: isolated liquidations touch only their own position and
//! margin, cross ones only the cross side.
use super::account::{self, Liquidation};
use super::{Account, Sim, BACKSTOP};
use crate::error::Result;
use crate::feed::MarketFeed;
use alloy_primitives::Address;
use rust_decimal::Decimal;
use std::sync::atomic::Ordering;

impl Sim {
    /// The venue's liquidation waterfall (docs): an account below its
    /// maintenance requirement is first closed by market orders to the book
    /// (20 % per attempt above $100k, the full size during the 30 s cooldown
    /// that follows a partial); below ⅔ of the maintenance requirement the
    /// backstop (the liquidator vault) takes the positions and margin over
    /// (the maintenance is not returned); a negative account value goes to
    /// ADL — the opposite side, ranked by `(mark/entry)·(notional/account_value)`
    /// at the previous mark, is closed against the underwater account.
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
        let mut closed = Vec::new();
        // The cross leg: the venue's own cross figure (the total minus the
        // isolated margins — the served identity, measured live) against the
        // cross maintenance.
        let cross = rescue_step(
            v.cross_margin_summary.account_value,
            v.cross_maintenance_margin_used,
        )?;
        closed.extend(self.rescue_leg(&pass, Leg::Cross, cross)?);
        // The isolated legs: each position alone — its isolated margin plus
        // its unrealized pnl (docs: "isolated positions will apply unrealized
        // pnl as additional margin for the open position") against its own
        // maintenance.
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
            let rescue = rescue_step(equity, facts.maintenance)?;
            closed.extend(self.rescue_leg(&pass, Leg::Isolated(coin), rescue)?);
        }
        if closed.is_empty() {
            return Ok(None);
        }
        self.liquidated(closed, v.margin_summary.account_value)
    }

    /// One leg's rescue: the step run over the leg's own scope.
    fn rescue_leg(&self, pass: &Pass<'_>, leg: Leg<'_>, rescue: Rescue) -> Result<Vec<String>> {
        match rescue {
            Rescue::Solvent => Ok(Vec::new()),
            Rescue::Adl => self.adl(pass, leg),
            Rescue::Backstop => self.backstop(pass, leg),
            Rescue::Book => self.liquidate_via_book(pass, leg),
        }
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
        let in_cooldown = pass.now_ms < cooldown;
        let mut closed = Vec::new();
        let mut partial = false;
        for (coin, p) in &pass.account.positions {
            if !leg.holds(coin, p) {
                continue;
            }
            let (did_close, was_partial) = self.liquidate_one(pass, (coin, p), in_cooldown)?;
            partial |= was_partial;
            if did_close {
                closed.push(coin.clone());
            }
        }
        if partial {
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
        in_cooldown: bool,
    ) -> Result<(bool, bool)> {
        let (coin, p) = position;
        let Some(mark) = pass.feed.mark(coin) else {
            return Ok((false, false));
        };
        let notional = account::mul(p.szi.abs(), mark, "liquidation notional")?;
        let (size, was_partial) = attempt_size(p.szi.abs(), notional, in_cooldown)?;
        if size.is_zero() {
            return Ok((false, was_partial));
        }
        match self.liquidate_position(pass.feed, pass.address, coin, size) {
            Ok(fill) => {
                tracing::warn!(account = %pass.address, coin = %coin, sz = %fill.total_sz, avg_px = %fill.avg_px, partial = was_partial, "liquidation: market close");
                Ok((true, was_partial))
            }
            Err(e) => {
                tracing::warn!(account = %pass.address, coin = %coin, error = %e, "liquidation close refused (no liquidity?)");
                Ok((false, was_partial))
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
            let mut held = false;
            let coins: Vec<String> = me.positions.keys().cloned().collect();
            for coin in coins {
                let Some(p) = me.positions.get(&coin) else { continue };
                if !leg.holds(&coin, p) {
                    continue;
                }
                held = true;
                let taken = p.isolated_margin;
                let mark = pass.feed.mark(&coin).unwrap_or(p.entry_px);
                let facts = account::FillFacts {
                    coin: coin.clone(),
                    is_buy: p.is_long(),
                    size: p.szi.abs(),
                    px: mark,
                    leverage: p.leverage_value,
                    reduce_only: false,
                    fee: Decimal::ZERO,
                };
                if let Err(e) = account::apply_fill(&mut vault, &facts, pass.feed.meta().as_ref()) {
                    tracing::warn!(account = %pass.address, coin = %coin, error = %e, "backstop transfer refused");
                    continue;
                }
                me.positions.remove(&coin);
                margin = account::add(margin, taken, "backstop margin")?;
                closed.push(coin);
            }
            // The cross margin moves with the cross backstop (docs); if every
            // cross transfer was refused, nothing moves at all — the log names
            // each refusal.
            if matches!(leg, Leg::Cross) && (!held || !closed.is_empty()) {
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
            let Some(prev_mark) =
                self.with_state(|state| Ok(state.prev_marks.get(coin).copied()))?
            else {
                tracing::warn!(account = %pass.address, coin = %coin, "ADL skipped: no previous mark recorded yet");
                continue;
            };
            let remaining = p.szi.abs();
            let my_side_long = p.is_long();
            let mut ranked = self.adl_ranking(
                pass.feed,
                pass.address,
                (coin, prev_mark, my_side_long),
                meta.as_ref(),
            )?;
            ranked.sort_by_key(|r| std::cmp::Reverse(r.0));
            let mut left = remaining;
            for (_, other, size, _entry) in ranked {
                if left <= Decimal::ZERO {
                    break;
                }
                let take = left.min(size);
                // The counterparty closes at the previous mark; the underwater
                // account's side closes against it at the same price.
                let own =
                    account::FillFacts::close(coin, !my_side_long, take, prev_mark, p.leverage_value);
                let theirs = account::FillFacts::close(coin, my_side_long, take, prev_mark, 1);
                self.adl_close_pair(pass.address, other, (&own, &theirs), meta.as_ref())?;
                tracing::warn!(account = %pass.address, other = %other, coin = %coin, sz = %take, px = %prev_mark, "ADL close at the previous mark");
                left = left.saturating_sub(take);
            }
            closed.push(coin.clone());
        }
        Ok(closed)
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
        target: (&str, Decimal, bool), // (coin, previous mark, the underwater side)
        meta: Option<&crate::wire::Meta>,
    ) -> Result<Vec<(Decimal, Address, Decimal, Decimal)>> {
        let (coin, prev_mark, my_side_long) = target;
        let mut ranked: Vec<(Decimal, Address, Decimal, Decimal)> = Vec::new();
        self.with_state(|st| {
            for (other, oa) in st.accounts.iter() {
                if *other == address {
                    continue;
                }
                let Some(op) = oa.positions.get(coin) else { continue };
                if op.is_long() == my_side_long || op.szi.is_zero() {
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

/// The waterfall's step for one underwater leg.
#[derive(Clone, Copy)]
enum Rescue {
    /// At or above the maintenance requirement: nothing to do.
    Solvent,
    /// Negative equity: ADL against the opposite side.
    Adl,
    /// Below the backstop ratio (`3·equity < 2·maintenance`): the vault takes
    /// the positions over.
    Backstop,
    /// Otherwise: market orders to the book.
    Book,
}

/// The waterfall's step from the two readings (docs: ADL when underwater, the
/// vault below the backstop ratio, the book otherwise).
fn rescue_step(equity: Decimal, maintenance: Decimal) -> Result<Rescue> {
    if equity >= maintenance {
        return Ok(Rescue::Solvent);
    }
    if equity < Decimal::ZERO {
        return Ok(Rescue::Adl);
    }
    let backstop = account::mul(equity, Decimal::from(3), "backstop check")?
        < account::mul(maintenance, Decimal::from(2), "backstop check")?;
    Ok(if backstop {
        Rescue::Backstop
    } else {
        Rescue::Book
    })
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

/// The liquidation attempt's size (docs): above the $100k partial threshold
/// only 20 % goes per attempt; during the 30 s cooldown after a partial, the
/// full size. Returns (size, was_partial).
pub(super) fn attempt_size(full: Decimal, notional: Decimal, in_cooldown: bool) -> Result<(Decimal, bool)> {
    const PARTIAL_THRESHOLD: Decimal = Decimal::from_parts(100_000, 0, 0, false, 0);
    if notional > PARTIAL_THRESHOLD && !in_cooldown {
        return Ok((account::mul(full, Decimal::new(2, 1), "partial size")?, true));
    }
    Ok((full, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::feed_with;

    /// The waterfall's attempt sizes (docs): 20 % above the $100k partial
    /// threshold, the full size below it and during the 30 s cooldown.
    #[test]
    fn liquidation_attempts_follow_the_partial_rules() {
        let full = Decimal::from(10);
        let (sz, partial) = attempt_size(full, Decimal::from(120_000), false).unwrap();
        assert!(partial, "above the threshold the attempt is partial");
        assert_eq!(sz, Decimal::from(2));
        let (sz, partial) = attempt_size(full, Decimal::from(120_000), true).unwrap();
        assert!(!partial, "the cooldown switches to the full size");
        assert_eq!(sz, full);
        let (sz, partial) = attempt_size(full, Decimal::from(50_000), false).unwrap();
        assert!(!partial, "below the threshold the attempt is full");
        assert_eq!(sz, full);
    }

    /// A meta with the given assets (the waterfall's maintenance tiers read
    /// it).
    fn meta_of(assets: &[(&str, u32)]) -> crate::wire::Meta {
        let mut m: crate::wire::Meta = serde_json::from_str(r#"{"universe": []}"#).unwrap();
        for (coin, max_leverage) in assets {
            m.universe.push(crate::wire::AssetInfo {
                name: (*coin).into(),
                sz_decimals: 4,
                max_leverage: *max_leverage,
                margin_table_id: None,
                only_isolated: None,
                is_delisted: None,
                margin_mode: None,
            });
        }
        m
    }

    /// A meta with one asset.
    fn meta_with(coin: &str, max_leverage: u32) -> crate::wire::Meta {
        meta_of(&[(coin, max_leverage)])
    }

    /// The waterfall's fixture: one account holding a single ETH position.
    fn put_account(sim: &Sim, address: Address, facts: (Decimal, Decimal, Decimal, u32)) {
        let (balance, entry, szi, leverage) = facts;
        sim.with_account(address, |acc| {
            acc.perp_balance = balance;
            acc.positions.insert(
                "ETH".into(),
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

    /// A negative account value goes to ADL: the opposite side (ranked by
    /// (mark/entry)·(notional/account_value)) is closed at the previous mark,
    /// and the platform stays solvent (docs).
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
        // BTC isolated: margin 1, maintenance 100/10/2 = 5 → 3·1 < 2·5: the vault.
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
        // 3·3 < 2·5 → the vault.
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
                crate::sim::account::FillFacts::close("ETH", false, Decimal::ONE, Decimal::from(100), 10);
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
                crate::sim::account::FillFacts::close("ETH", false, Decimal::ONE, Decimal::from(100), 10);
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
        sim.set_leverage(a, "ETH", account::LeverageSetting { is_cross: false, leverage: 10 }, None)
            .unwrap();
        let acc = sim.account(a).unwrap().unwrap();
        assert_eq!(
            acc.positions.get("ETH").map(|p| p.isolated_margin),
            Some(Decimal::from(10)),
            "the initial margin is parked on the position"
        );
        assert_eq!(acc.perp_balance, Decimal::from(40));
        sim.set_leverage(a, "ETH", account::LeverageSetting { is_cross: true, leverage: 10 }, None)
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
