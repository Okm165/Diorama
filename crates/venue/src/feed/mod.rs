//! The market plane: a live cache of the venue's market data with scenario
//! overrides on top (plan §5).
//!
//! Two layers, one owner each:
//!
//! - [`MarketFeed`] holds the *cache* — the latest `meta`/`metaAndAssetCtxs`/
//!   `l2Book`/`allMids` snapshots from the REST poller and the WS client —
//!   with per-coin update stamps (the staleness metrics);
//! - [`Overrides`] holds the scenario's owned values (`crash` prices,
//!   `set_funding`, `set_book`) *and* the order lane's level consumptions
//!   ([`Consumption`] — the plan §4.5 COW analogue: a devnet order owns the
//!   sizes it took, per level, and the served book shows the remainder while
//!   the live feed keeps refreshing every other level), persisted to
//!   `feed.json` so a sim restart mid-scenario does not lose either.
//!
//! Every read goes through the composed view ([`MarketFeed::mark`] and
//! friends): a crash owns both mark and oracle, and `metaAndAssetCtxs` serves
//! the same owned numbers the account plane computes with — daloc must never
//! read a different mark than the sim used for margin.
pub mod budget;
pub mod metrics;
pub mod rest;
pub mod ws;

use crate::error::Result;
use crate::wire::{AllMids, AssetCtx, L2Book, Meta};
use budget::Budget;
use metrics::FeedMetrics;
use mirror_devnet::store;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The scenario's owned market values (persisted).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Overrides {
    /// `crash`: owned mark + oracle per coin.
    #[serde(default)]
    pub prices: BTreeMap<String, PriceOverride>,
    /// `set_funding`: owned hourly funding rate per coin.
    #[serde(default)]
    pub funding: BTreeMap<String, Decimal>,
    /// `set_book`: scripted books per coin.
    #[serde(default)]
    pub books: BTreeMap<String, L2Book>,
    /// The order lane's consumed levels (devnet-owned sizes).
    #[serde(default)]
    pub consumptions: Vec<Consumption>,
}

/// One consumed book level (plan §4.5: per-level ownership + consumption
/// deltas). Level-scoped: a live update that no longer carries the price
/// prunes it (I6 — pruned, never re-owned); the *mark is never moved* (I3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Consumption {
    pub coin: String,
    /// The wire side index: 0 = bids, 1 = asks (the order in `L2Book.levels`).
    pub side: u8,
    pub price: Decimal,
    /// The size the devnet took at this level (summed across orders).
    pub size: Decimal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub(crate) struct PriceOverride {
    pub mark: Decimal,
    pub oracle: Decimal,
}

/// The live cache (never persisted: it is a cache, and a stale file would
/// masquerade as fresh data).
#[derive(Default)]
struct Cache {
    meta: Option<Meta>,
    meta_at: Option<u64>,
    spot_meta: Option<crate::wire::SpotMeta>,
    ctxs: BTreeMap<String, AssetCtx>,
    ctx_at: BTreeMap<String, u64>,
    books: BTreeMap<String, L2Book>,
    book_at: BTreeMap<String, u64>,
    mids: BTreeMap<String, Decimal>,
    mids_at: Option<u64>,
    /// `perpsAtOpenInterestCap`'s last answer (the venue's cap list).
    pub(crate) oi_caps: Vec<String>,
}

pub struct MarketFeed {
    cache: RwLock<Cache>,
    overrides: RwLock<Overrides>,
    /// The rate budget meter (shared with the passthrough reads).
    pub(crate) budget: Budget,
    pub(crate) metrics: FeedMetrics,
    /// The configured markets (books are cached for these; the staleness
    /// gauges are per-coin over this set).
    pub markets: Vec<String>,
    /// The overrides' state file; `None` keeps them in memory only.
    path: Option<PathBuf>,
    /// The staleness drill: live writes are dropped until this deadline
    /// (0 = not stalled). The scripted plane is never gated.
    stalled_until_ms: AtomicU64,
}

impl MarketFeed {
    /// Load the persisted overrides beside the live cache.
    pub fn load(path: PathBuf, markets: Vec<String>) -> Result<Self> {
        let overrides = store::load::<Overrides>(&path, "feed overrides")?.unwrap_or_default();
        Ok(Self {
            cache: RwLock::new(Cache::default()),
            overrides: RwLock::new(overrides),
            budget: Budget::default(),
            metrics: FeedMetrics::new(&markets),
            markets,
            path: Some(path),
            stalled_until_ms: AtomicU64::new(0),
        })
    }

    /// An in-memory feed (tests).
    pub fn memory(markets: Vec<String>) -> Self {
        Self {
            cache: RwLock::new(Cache::default()),
            overrides: RwLock::new(Overrides::default()),
            budget: Budget::default(),
            metrics: FeedMetrics::new(&markets),
            markets,
            path: None,
            stalled_until_ms: AtomicU64::new(0),
        }
    }

    /// The staleness window every consumer shares (`readyz`, the order
    /// lane): live data older than this is stale — and stale is not tradable.
    pub const MAX_STALE_MS: u64 = 30_000;

    /// `stall`: hold the live market plane for `seconds` (0 clears) — the
    /// staleness drill. Live writes are dropped while stalled (a real stall
    /// serves nothing new); the control plane's scripted writes are never
    /// gated. Returns the deadline in ms since the epoch (0 = not stalled).
    pub(crate) fn stall(&self, seconds: u64) -> u64 {
        let until = if seconds == 0 {
            0
        } else {
            crate::now_ms().saturating_add(seconds.saturating_mul(1_000))
        };
        self.stalled_until_ms.store(until, Ordering::Relaxed);
        until
    }

    /// Whether the live plane is currently held (the staleness drill).
    pub(crate) fn stalled(&self) -> bool {
        let until = self.stalled_until_ms.load(Ordering::Relaxed);
        until != 0 && crate::now_ms() < until
    }

    /// The served book's age when it is live-sourced and older than `max_ms`:
    /// `None` for a scripted book (the scenario's present is never stale) and
    /// for a coin whose live base has not landed yet (that read is `Warming`,
    /// a different fault — absence, not staleness).
    pub fn book_stale_ms(&self, coin: &str, now_ms: u64, max_ms: u64) -> Option<u64> {
        if self.read_overrides().books.contains_key(coin) {
            return None;
        }
        self.stale_ms(coin, now_ms).filter(|age| *age > max_ms)
    }

    // ---- writes (the poller / WS client / control plane) ------------------

    pub(crate) fn put_meta(&self, meta: Meta, now_ms: u64) {
        if self.stalled() {
            tracing::debug!("live meta dropped: the feed is stalled (the staleness drill)");
            return;
        }
        let mut c = self.write();
        c.meta = Some(meta);
        c.meta_at = Some(now_ms);
    }

    pub(crate) fn put_spot_meta(&self, spot: crate::wire::SpotMeta) {
        self.write().spot_meta = Some(spot);
    }

    /// `perpsAtOpenInterestCap`: the coins at their cap (the venue's own
    /// list; the order lane refuses orders that would increase their OI).
    pub(crate) fn put_oi_caps(&self, caps: Vec<String>) {
        if self.stalled() {
            tracing::debug!("live OI caps dropped: the feed is stalled (the staleness drill)");
            return;
        }
        self.write().oi_caps = caps;
    }

    /// The cached cap list (empty until the first poll lands: the refusal is
    /// conservative — never a false cap, and the venue's own answer follows
    /// within a poll interval).
    pub fn open_interest_caps(&self) -> Vec<String> {
        self.read().oi_caps.clone()
    }

    pub(crate) fn put_ctx(&self, coin: String, ctx: AssetCtx, now_ms: u64) {
        if self.stalled() {
            tracing::debug!(coin = %coin, "live ctx dropped: the feed is stalled (the staleness drill)");
            return;
        }
        let mut c = self.write();
        c.ctx_at.insert(coin.clone(), now_ms);
        c.ctxs.insert(coin, ctx);
    }

    pub(crate) fn put_book(&self, book: L2Book, now_ms: u64) {
        if self.stalled() {
            tracing::debug!(coin = %book.coin, "live book dropped: the feed is stalled (the staleness drill)");
            return;
        }
        // I2/I6 are scoped to the *served* base: a live update refreshes
        // (and can prune against) the book only while live is what the coin
        // serves. A scripted book owns the coin until its hand-back — the
        // live feed neither refreshes nor prunes it (measured: an unscoped
        // prune dropped a scripted level's consumption on the next live
        // update, and the level reappeared).
        if !self.read_overrides().books.contains_key(&book.coin) {
            let (skipped, pruned) = self.prune_consumptions(&book);
            if skipped > 0 {
                self.metrics.add_owned_skips(skipped as u64);
            }
            if pruned > 0 {
                self.metrics.add_owned_pruned(pruned as u64);
                self.metrics.set_owned_levels(self.owned_levels());
            }
        }
        let mut c = self.write();
        c.book_at.insert(book.coin.clone(), now_ms);
        c.books.insert(book.coin.clone(), book);
    }

    /// Prune the coin's consumptions against the base it will serve (a live
    /// update, or a freshly scripted book): records whose price the base
    /// carries stay, the rest are pruned; returns (owned levels the base met,
    /// pruned records).
    fn prune_consumptions(&self, book: &L2Book) -> (usize, usize) {
        let mut o = self.write_overrides();
        let before = o.consumptions.len();
        let mut met = 0usize;
        o.consumptions.retain(|c| {
            if c.coin != book.coin {
                return true;
            }
            let live = book.levels[usize::from(c.side.min(1))]
                .iter()
                .any(|l| l.px == c.price);
            if live {
                met += 1;
            }
            live
        });
        (met, before - o.consumptions.len())
    }

    pub fn put_mids(&self, mids: BTreeMap<String, Decimal>, now_ms: u64) {
        if self.stalled() {
            tracing::debug!("live mids dropped: the feed is stalled (the staleness drill)");
            return;
        }
        let mut c = self.write();
        c.mids = mids;
        c.mids_at = Some(now_ms);
    }

    /// `crash`: own this coin's mark and oracle (live updates stop mattering
    /// until [`Self::clear_price`]).
    pub(crate) fn own_price(&self, coin: &str, price: PriceOverride) -> Result<()> {
        self.write_overrides().prices.insert(coin.to_string(), price);
        self.persist()
    }

    pub fn clear_price(&self, coin: &str) -> Result<()> {
        self.write_overrides().prices.remove(coin);
        self.persist()
    }

    pub fn own_funding(&self, coin: &str, rate: Decimal) -> Result<()> {
        self.write_overrides().funding.insert(coin.to_string(), rate);
        self.persist()
    }

    pub fn clear_funding(&self, coin: &str) -> Result<()> {
        self.write_overrides().funding.remove(coin);
        self.persist()
    }

    /// Record one consumed level (the order lane's ownership write): sizes at
    /// the same (coin, side, price) accumulate, exactly as a second order
    /// would take more of the same level.
    pub(crate) fn consume(&self, c: Consumption) -> Result<()> {
        {
            let mut o = self.write_overrides();
            match o
                .consumptions
                .iter_mut()
                .find(|x| x.coin == c.coin && x.side == c.side && x.price == c.price)
            {
                Some(x) => {
                    x.size = x
                        .size
                        .checked_add(c.size)
                        .ok_or(crate::error::Error::Overflow {
                            what: "consumed level size",
                        })?;
                }
                None => o.consumptions.push(c),
            }
        }
        self.metrics.set_owned_levels(self.owned_levels());
        self.persist()
    }

    /// The devnet-owned book level count (the `venue_sim_owned_levels` gauge).
    pub fn owned_levels(&self) -> u64 {
        self.read_overrides().consumptions.len() as u64
    }

    pub(crate) fn own_book(&self, book: &L2Book) -> Result<()> {
        {
            let mut o = self.write_overrides();
            o.books.insert(book.coin.clone(), book.clone());
        }
        // A new scripted base re-scopes the coin's consumptions: the deltas
        // for prices it still carries stay, the rest are pruned (I6 — pruned,
        // never re-owned).
        let (_, pruned) = self.prune_consumptions(book);
        if pruned > 0 {
            self.metrics.add_owned_pruned(pruned as u64);
        }
        self.metrics.set_owned_levels(self.owned_levels());
        self.persist()
    }

    pub fn clear_book(&self, coin: &str) -> Result<()> {
        {
            let mut o = self.write_overrides();
            o.books.remove(coin);
            // I4: the hand-back restores the live book — the coin's consumed
            // levels are part of what the devnet owned, so they drop with it.
            o.consumptions.retain(|c| c.coin != coin);
        }
        self.metrics.set_owned_levels(self.owned_levels());
        self.persist()
    }

    /// `reset`: drop every override (the live cache stays — it is the venue's,
    /// not the scenario's).
    pub fn clear_overrides(&self) -> Result<()> {
        *self.write_overrides() = Overrides::default();
        self.metrics.set_owned_levels(0);
        self.persist()
    }

    /// The number of scenario-owned feed values (`crash` prices + `set_funding`
    /// rates + `set_book` books) — the `venue_feed_overrides` gauge, computed
    /// from the overrides themselves so it cannot drift.
    pub fn owned_count(&self) -> u64 {
        let o = self.read_overrides();
        (o.prices.len() + o.funding.len() + o.books.len()) as u64
    }

    // ---- composed reads ---------------------------------------------------

    /// The coin's mark: the owned price wins, else the live context.
    pub fn mark(&self, coin: &str) -> Option<Decimal> {
        if let Some(p) = self.read_overrides().prices.get(coin) {
            return Some(p.mark);
        }
        self.read().ctxs.get(coin).map(|c| c.mark_px)
    }

    /// The coin's oracle price (funding and liquidations price from it).
    pub fn oracle(&self, coin: &str) -> Option<Decimal> {
        if let Some(p) = self.read_overrides().prices.get(coin) {
            return Some(p.oracle);
        }
        self.read().ctxs.get(coin).map(|c| c.oracle_px)
    }

    /// The coin's published hourly funding rate (overridable).
    pub fn funding(&self, coin: &str) -> Option<Decimal> {
        if let Some(rate) = self.read_overrides().funding.get(coin) {
            return Some(*rate);
        }
        self.read().ctxs.get(coin).map(|c| c.funding)
    }

    /// The coin's book: the scripted book wins; a scripted book is stamped
    /// `now` at serve time (it is the scenario's present, not a snapshot).
    pub(crate) fn book(&self, coin: &str, now_ms: u64) -> Option<L2Book> {
        let overrides = self.read_overrides();
        let base = match overrides.books.get(coin) {
            Some(b) => {
                let mut b = b.clone();
                b.time = now_ms;
                b
            }
            None => self.read().books.get(coin).cloned()?,
        };
        Some(apply_consumptions(base, &overrides.consumptions))
    }

    /// The perp universe + margin tables (the live meta; `None` until the
    /// first poll lands — a read that needs it is a 503, never a guess).
    pub(crate) fn meta(&self) -> Option<Meta> {
        self.read().meta.clone()
    }

    /// The spot token registry (`None` until the first poll lands).
    pub(crate) fn spot_meta(&self) -> Option<crate::wire::SpotMeta> {
        self.read().spot_meta.clone()
    }

    /// The spot token index for a coin name (`None` = not tracked).
    pub fn token_of(&self, coin: &str) -> Option<u32> {
        self.read()
            .spot_meta
            .as_ref()?
            .tokens
            .iter()
            .find(|t| t.name == coin)
            .map(|t| t.index)
    }

    /// `metaAndAssetCtxs` with the overrides applied: the served contexts are
    /// exactly what the account plane prices from.
    pub(crate) fn meta_and_ctxs(&self) -> Option<(Meta, Vec<AssetCtx>)> {
        let meta = self.meta()?;
        let overrides = self.read_overrides().clone();
        let c = self.read();
        let ctxs = meta
            .universe
            .iter()
            .filter_map(|asset| {
                let mut ctx = c.ctxs.get(&asset.name)?.clone();
                if let Some(p) = overrides.prices.get(&asset.name) {
                    ctx.mark_px = p.mark;
                    ctx.oracle_px = p.oracle;
                }
                if let Some(rate) = overrides.funding.get(&asset.name) {
                    ctx.funding = *rate;
                }
                Some(ctx)
            })
            .collect();
        Some((meta, ctxs))
    }

    /// `allMids` with the overrides applied.
    pub(crate) fn all_mids(&self) -> AllMids {
        let overrides = self.read_overrides().clone();
        let c = self.read();
        let mut mids = c.mids.clone();
        for (coin, p) in &overrides.prices {
            mids.insert(coin.clone(), p.mark);
        }
        AllMids(mids)
    }

    /// The prices the account plane prices a coin from (mark, oracle). A coin
    /// with no live context and no override is a fault at the read that needs
    /// it, never a zero (a zero mark would liquidate every position).
    pub fn required_prices(&self, coin: &str) -> Option<(Decimal, Decimal)> {
        Some((self.mark(coin)?, self.oracle(coin)?))
    }

    /// Milliseconds since the coin's last live context update (`None` = never
    /// seen). The readiness gate and the staleness metrics read this.
    pub fn stale_ms(&self, coin: &str, now_ms: u64) -> Option<u64> {
        let c = self.read();
        c.ctx_at.get(coin).map(|at| now_ms.saturating_sub(*at))
    }

    /// Publish the staleness gauges + budget use (the poller calls this per
    /// pass; the metrics endpoint renders the rest).
    pub fn publish_metrics(&self, now_ms: u64) {
        self.metrics.set_budget_used(self.budget.used());
        for coin in &self.markets {
            if let Some(stale) = self.stale_ms(coin, now_ms) {
                self.metrics.set_stale(coin, stale);
            }
        }
    }

    fn persist(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let overrides = self.read_overrides().clone();
        store::save(path, &overrides, "feed overrides")?;
        Ok(())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Cache> {
        self.cache.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Cache> {
        self.cache.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn read_overrides(&self) -> std::sync::RwLockReadGuard<'_, Overrides> {
        self.overrides.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_overrides(&self) -> std::sync::RwLockWriteGuard<'_, Overrides> {
        self.overrides.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The served book: the base book minus the consumed sizes — level-scoped, so
/// a level the devnet never touched serves the live size verbatim. A fully
/// consumed level leaves the book (the wire never carries a zero size); the
/// order count `n` stays the live level's (a consumption takes size, not
/// orders).
fn apply_consumptions(mut book: L2Book, consumptions: &[Consumption]) -> L2Book {
    for side in 0..2usize {
        for level in book.levels[side].iter_mut() {
            let taken: Decimal = consumptions
                .iter()
                .filter(|c| c.coin == book.coin && usize::from(c.side) == side && c.price == level.px)
                .map(|c| c.size)
                .sum();
            if taken > Decimal::ZERO {
                level.sz = (level.sz - taken).max(Decimal::ZERO);
            }
        }
        book.levels[side].retain(|l| l.sz > Decimal::ZERO);
    }
    book
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{test_ctx, Level};

    /// A scripted book owns the coin until its hand-back: live updates are
    /// not the served base, so they neither refresh nor prune its consumed
    /// levels (the measured bug: an unscoped prune dropped a scripted level's
    /// consumption on the next live update and the level reappeared).
    #[test]
    fn live_updates_leave_a_scripted_books_consumptions_alone() {
        let feed = MarketFeed::memory(vec!["BTC".into()]);
        let scripted = |px: &str| L2Book {
            coin: "BTC".into(),
            time: 1,
            levels: [
                vec![],
                vec![Level { px: px.parse().unwrap(), sz: Decimal::ONE, n: 1 }],
            ],
            spread: None,
        };
        feed.own_book(&scripted("100")).unwrap();
        feed.consume(Consumption {
            coin: "BTC".into(),
            side: 1,
            price: "100".parse().unwrap(),
            size: "0.5".parse().unwrap(),
        })
        .unwrap();
        // A live update that cannot carry the scripted price: no prune.
        feed.put_book(scripted("50"), 2);
        assert_eq!(feed.owned_levels(), 1, "the scripted consumption survives");
        assert_eq!(
            feed.book("BTC", 0).unwrap().levels[1][0].sz,
            "0.5".parse().unwrap(),
            "served = scripted − consumed"
        );
        // Re-scripting the book without the price re-scopes: pruned.
        feed.own_book(&scripted("50")).unwrap();
        assert_eq!(feed.owned_levels(), 0);
        assert_eq!(
            feed.book("BTC", 0).unwrap().levels[1][0].sz,
            "1".parse().unwrap()
        );
    }

    #[test]
    fn overrides_compose_over_the_cache() {
        let feed = MarketFeed::memory(vec!["BTC".into()]);
        feed.put_ctx("BTC".into(), test_ctx("100", "101"), 1_000);
        assert_eq!(feed.mark("BTC"), Some(Decimal::from(100)));
        assert_eq!(feed.oracle("BTC"), Some(Decimal::from(101)));

        feed.own_price(
            "BTC",
            PriceOverride {
                mark: "90".parse().unwrap(),
                oracle: "91".parse().unwrap(),
            },
        )
        .unwrap();
        assert_eq!(feed.mark("BTC"), Some(Decimal::from(90)));
        assert_eq!(feed.oracle("BTC"), Some(Decimal::from(91)));
        // A live update no longer changes the owned read.
        feed.put_ctx("BTC".into(), test_ctx("200", "201"), 2_000);
        assert_eq!(feed.mark("BTC"), Some(Decimal::from(90)));

        feed.clear_price("BTC").unwrap();
        assert_eq!(feed.mark("BTC"), Some(Decimal::from(200)));
    }

    #[test]
    fn scripted_book_wins_and_is_stamped_now() {
        let feed = MarketFeed::memory(vec!["BTC".into()]);
        let book = |px: &str| L2Book {
            coin: "BTC".into(),
            time: 1,
            levels: [
                vec![Level { px: px.parse().unwrap(), sz: Decimal::ONE, n: 1 }],
                vec![],
            ],
            spread: None,
        };
        feed.put_book(book("100"), 1_000);
        assert_eq!(feed.book("BTC", 5_000).unwrap().time, 1, "live book keeps its stamp");
        feed.own_book(&book("99")).unwrap();
        let served = feed.book("BTC", 5_000).unwrap();
        assert_eq!(served.time, 5_000, "scripted book is the scenario's present");
        assert_eq!(served.levels[0][0].px, Decimal::from(99));
        feed.clear_book("BTC").unwrap();
        assert_eq!(feed.book("BTC", 6_000).unwrap().levels[0][0].px, Decimal::from(100));
    }

    /// The stall holds the live plane (scripted writes still land) and the
    /// served-book freshness is live-only: a scripted book is never stale.
    #[test]
    fn a_stall_drops_live_updates_and_book_freshness_is_live_only() {
        let feed = MarketFeed::memory(vec!["BTC".into()]);
        let now = crate::now_ms();
        let book = |px: &str| L2Book {
            coin: "BTC".into(),
            time: 1,
            levels: [
                vec![Level { px: px.parse().unwrap(), sz: Decimal::ONE, n: 1 }],
                vec![],
            ],
            spread: None,
        };
        feed.put_ctx("BTC".into(), test_ctx("100", "101"), now);
        feed.put_book(book("100"), now);
        assert_eq!(feed.book_stale_ms("BTC", now, MarketFeed::MAX_STALE_MS), None);
        assert_eq!(
            feed.book_stale_ms("BTC", now + MarketFeed::MAX_STALE_MS + 1, MarketFeed::MAX_STALE_MS),
            Some(MarketFeed::MAX_STALE_MS + 1),
            "an aged live book is stale"
        );
        // The stall: live writes drop, scripted writes land.
        assert_eq!(feed.stall(30), now + 30_000);
        assert!(feed.stalled());
        feed.put_ctx("BTC".into(), test_ctx("999", "999"), now);
        feed.put_book(book("999"), now);
        assert_eq!(feed.mark("BTC"), Some(Decimal::from(100)), "the live ctx was dropped");
        assert_eq!(feed.book("BTC", now).unwrap().levels[0][0].px, Decimal::from(100));
        feed.own_book(&book("77")).unwrap();
        assert_eq!(
            feed.book("BTC", now).unwrap().levels[0][0].px,
            Decimal::from(77),
            "the scripted plane is never gated"
        );
        assert_eq!(feed.book_stale_ms("BTC", now + 60_000, MarketFeed::MAX_STALE_MS), None);
        // Clearing the stall lets the live plane back in.
        feed.stall(0);
        assert!(!feed.stalled());
        feed.put_ctx("BTC".into(), test_ctx("999", "999"), now);
        assert_eq!(feed.mark("BTC"), Some(Decimal::from(999)));
    }

    #[test]
    fn staleness_tracks_ctx_updates() {
        let feed = MarketFeed::memory(vec!["BTC".into(), "ETH".into()]);
        assert_eq!(feed.stale_ms("BTC", 10_000), None);
        feed.put_ctx("BTC".into(), test_ctx("100", "101"), 8_000);
        assert_eq!(feed.stale_ms("BTC", 10_000), Some(2_000));
        assert_eq!(feed.stale_ms("ETH", 10_000), None);
    }
}
