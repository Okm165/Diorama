//! The market plane: a live cache of the venue's market data with scenario
//! overrides on top (plan §5).
//!
//! Two layers, one owner each:
//!
//! - [`MarketFeed`] holds the *cache* — the latest `meta`/`metaAndAssetCtxs`/
//!   `l2Book`/`allMids` snapshots from the REST poller and the WS client —
//!   with per-coin update stamps (the staleness metrics);
//! - [`Overrides`] holds the scenario's owned values (`crash` prices,
//!   `set_funding`, `set_book`), persisted to `feed.json` so a sim restart
//!   mid-scenario does not lose the scenario.
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
        }
    }

    // ---- writes (the poller / WS client / control plane) ------------------

    pub(crate) fn put_meta(&self, meta: Meta, now_ms: u64) {
        let mut c = self.write();
        c.meta = Some(meta);
        c.meta_at = Some(now_ms);
    }

    pub(crate) fn put_spot_meta(&self, spot: crate::wire::SpotMeta) {
        self.write().spot_meta = Some(spot);
    }

    pub(crate) fn put_ctx(&self, coin: String, ctx: AssetCtx, now_ms: u64) {
        let mut c = self.write();
        c.ctx_at.insert(coin.clone(), now_ms);
        c.ctxs.insert(coin, ctx);
    }

    pub(crate) fn put_book(&self, book: L2Book, now_ms: u64) {
        let mut c = self.write();
        c.book_at.insert(book.coin.clone(), now_ms);
        c.books.insert(book.coin.clone(), book);
    }

    pub fn put_mids(&self, mids: BTreeMap<String, Decimal>, now_ms: u64) {
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

    pub(crate) fn own_book(&self, book: L2Book) -> Result<()> {
        self.write_overrides().books.insert(book.coin.clone(), book);
        self.persist()
    }

    pub fn clear_book(&self, coin: &str) -> Result<()> {
        self.write_overrides().books.remove(coin);
        self.persist()
    }

    /// `reset`: drop every override (the live cache stays — it is the venue's,
    /// not the scenario's).
    pub fn clear_overrides(&self) -> Result<()> {
        *self.write_overrides() = Overrides::default();
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
        if let Some(b) = self.read_overrides().books.get(coin) {
            let mut b = b.clone();
            b.time = now_ms;
            return Some(b);
        }
        self.read().books.get(coin).cloned()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{test_ctx, Level};

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
        feed.own_book(book("99")).unwrap();
        let served = feed.book("BTC", 5_000).unwrap();
        assert_eq!(served.time, 5_000, "scripted book is the scenario's present");
        assert_eq!(served.levels[0][0].px, Decimal::from(99));
        feed.clear_book("BTC").unwrap();
        assert_eq!(feed.book("BTC", 6_000).unwrap().levels[0][0].px, Decimal::from(100));
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
