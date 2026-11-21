//! The perp dex registry: the venue's dexes in `perpDexs` order (index 0 is
//! the default dex, named `""`), each with its own universe, collateral token
//! and asset-id range, plus the one resolver between asset ids and coin names.
//!
//! Facts (measured on the real venue, 2026-11-16, and the official SDK):
//!
//! - a builder dex's coins are `dex:COIN` (`xyz:TSLA`); the default dex's
//!   coins carry no prefix;
//! - the default dex's asset id is the coin's index in its universe; a
//!   builder dex at `perpDexs` position `i` numbers its coins
//!   `100000 + i * 10000 + index_in_that_dex_universe` (`xyz:XYZ100` is
//!   `110000`).
use crate::wire::{Meta, PerpDex};

/// The `dex` value that selects every dex at once (`allMids`, `openOrders`,
/// `frontendOpenOrders` accept it; the other info types answer 500).
pub(crate) const ALL_DEXS: &str = "ALL_DEXS";

/// The first builder dex's asset-id base is `BUILDER_ASSET_BASE +
/// BUILDER_ASSET_STRIDE` (`perpDexs` index 1).
const BUILDER_ASSET_BASE: u32 = 100_000;
/// The asset-id span reserved per builder dex.
const BUILDER_ASSET_STRIDE: u32 = 10_000;

/// The dex a coin belongs to: the prefix before `:` for a builder coin
/// (`xyz:TSLA` -> `xyz`), `""` (the default dex) for a plain coin.
pub(crate) fn dex_of_coin(coin: &str) -> &str {
    coin.split_once(':').map_or("", |(dex, _)| dex)
}

/// The asset-id base of the dex at `perpDexs` position `index`: 0 for the
/// default dex, `100000 + index * 10000` for a builder dex. `None` on
/// arithmetic overflow (an index no real venue reaches).
fn asset_base(index: usize) -> Option<u32> {
    if index == 0 {
        return Some(0);
    }
    let index = u32::try_from(index).ok()?;
    index
        .checked_mul(BUILDER_ASSET_STRIDE)?
        .checked_add(BUILDER_ASSET_BASE)
}

/// One perp dex: its place in `perpDexs`, its universe and the collateral
/// token its positions margin against.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PerpDexInfo {
    /// `""` for the default dex.
    pub name: String,
    /// The position in `perpDexs` (0 = the default dex).
    pub index: usize,
    /// The dex meta's `collateralToken` (0 = USDC).
    pub collateral_token: u32,
    /// The asset id of the dex's first coin.
    pub asset_base: u32,
    /// The raw `perpDexs` record; `None` for the default dex (the venue
    /// serves `null` there) and for a dex whose record did not parse.
    pub raw: Option<PerpDex>,
    /// The dex's universe and margin tables.
    pub meta: Meta,
}

impl PerpDexInfo {
    pub(crate) fn new(
        index: usize,
        name: String,
        raw: Option<PerpDex>,
        meta: Meta,
    ) -> Option<Self> {
        Some(Self {
            asset_base: asset_base(index)?,
            collateral_token: meta.collateral_token,
            name,
            index,
            raw,
            meta,
        })
    }

    /// The coin at `asset` (an asset id in this dex's range), if it exists.
    fn coin_of_asset(&self, asset: u32) -> Option<String> {
        let offset = asset.checked_sub(self.asset_base)?;
        self.meta
            .universe
            .get(offset as usize)
            .map(|a| a.name.clone())
    }
}

/// The venue's dexes in `perpDexs` order.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DexRegistry {
    dexs: Vec<PerpDexInfo>,
}

impl DexRegistry {
    /// A registry over entries whose positions are already assigned (the
    /// feed builds it from its cached metas, keyed by `perpDexs` position).
    pub(crate) fn from_dexs(dexs: Vec<PerpDexInfo>) -> Self {
        Self { dexs }
    }

    /// A registry holding only the default dex (tests, and the feed before
    /// `perpDexs` has landed).
    #[cfg(test)]
    pub(crate) fn of_default(meta: Meta) -> Self {
        let mut registry = Self::default();
        registry.put_meta("", meta);
        registry
    }

    /// Build from `perpDexs` and the index-aligned `allPerpMetas`. `None` on
    /// a length mismatch (the two answers are from different moments: keep
    /// the previous registry rather than misalign a dex with another's
    /// universe).
    #[allow(dead_code)] // the allPerpMetas refresh lands with the staggered builder-dex poller
    pub(crate) fn from_api(dexs: Vec<(String, Option<PerpDex>)>, metas: Vec<Meta>) -> Option<Self> {
        if dexs.len() != metas.len() || dexs.is_empty() {
            return None;
        }
        let built: Option<Vec<PerpDexInfo>> = dexs
            .into_iter()
            .zip(metas)
            .enumerate()
            .map(|(index, ((name, raw), meta))| PerpDexInfo::new(index, name, raw, meta))
            .collect();
        let built = built?;
        // Index 0 is the default dex by definition: a registry whose first
        // entry is named is not `perpDexs`.
        built.first().filter(|d| d.name.is_empty())?;
        Some(Self { dexs: built })
    }

    /// Replace the named dex's meta, or append the dex when it is new (the
    /// default dex first). Used by the poller's per-dex meta refresh and by
    /// the tests' fixtures; the live registry is rebuilt by [`Self::from_api`].
    #[allow(dead_code)] // the fixtures use it; the feed rebuilds via `from_dexs`
    pub(crate) fn put_meta(&mut self, name: &str, meta: Meta) {
        if let Some(dex) = self.dexs.iter_mut().find(|d| d.name == name) {
            dex.collateral_token = meta.collateral_token;
            dex.meta = meta;
            return;
        }
        let index = self.dexs.len();
        if let Some(info) = PerpDexInfo::new(index, name.to_string(), None, meta) {
            self.dexs.push(info);
        }
    }

    /// Replace the named dex's meta only when the dex is known; `true` when
    /// it was.
    #[allow(dead_code)] // the allPerpMetas refresh lands with the staggered builder-dex poller
    pub(crate) fn refresh_meta(&mut self, name: &str, meta: Meta) -> bool {
        match self.dexs.iter_mut().find(|d| d.name == name) {
            Some(dex) => {
                dex.collateral_token = meta.collateral_token;
                dex.meta = meta;
                true
            }
            None => false,
        }
    }

    /// Every dex, `perpDexs` order.
    #[allow(dead_code)] // read by the tests; the push lanes ask per coin list
    pub(crate) fn dexs(&self) -> &[PerpDexInfo] {
        &self.dexs
    }

    pub(crate) fn dex(&self, name: &str) -> Option<&PerpDexInfo> {
        self.dexs.iter().find(|d| d.name == name)
    }

    /// The meta of the dex that owns `coin` (by prefix), when it lists it.
    #[allow(dead_code)] // the margin-table lookup adopts it with the multi-dex serving phase
    pub(crate) fn meta_for_coin(&self, coin: &str) -> Option<&Meta> {
        let meta = &self.dex(dex_of_coin(coin))?.meta;
        meta.asset(coin).map(|_| meta)
    }

    /// The coin's asset id: its dex's base plus its index in that dex's
    /// universe. `None` for a coin no dex lists.
    #[allow(dead_code)] // reached through the tests; the order paths resolve ids, not names
    pub(crate) fn asset_id(&self, coin: &str) -> Option<u32> {
        let dex = self.dex(dex_of_coin(coin))?;
        let position = dex.meta.universe.iter().position(|a| a.name == coin)?;
        dex.asset_base.checked_add(u32::try_from(position).ok()?)
    }

    /// The one asset-id resolver (the order, cancel, leverage and CoreWriter
    /// paths all call it). An id below the builder range indexes the default
    /// dex's universe (the plain index the venue's own clients send for the
    /// default dex); `100000 + i * 10000 + n` is coin `n` of the dex at
    /// `perpDexs` position `i`. Anything else (spot ids, a dex that is not
    /// listed, an index past a universe) is `None`.
    pub(crate) fn coin_of_asset(&self, asset: u32) -> Option<String> {
        if asset < BUILDER_ASSET_STRIDE {
            return self.dex("")?.coin_of_asset(asset);
        }
        let past_base = asset.checked_sub(BUILDER_ASSET_BASE)?;
        let index = (past_base / BUILDER_ASSET_STRIDE) as usize;
        // `100000..109999` would be dex 0 with a builder base: not an id.
        if index == 0 {
            return None;
        }
        // The feed's registry is sparse while the configured dexes' metas
        // land one by one: the entry carries its own `perpDexs` position.
        self.dexs
            .iter()
            .find(|d| d.index == index)?
            .coin_of_asset(asset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::meta_of;

    fn two_dexs() -> DexRegistry {
        let mut xyz = meta_of(&[("xyz:XYZ100", 20), ("xyz:TSLA", 10)]);
        xyz.collateral_token = 360;
        DexRegistry::from_api(
            vec![(String::new(), None), ("xyz".into(), None)],
            vec![meta_of(&[("BTC", 40), ("ETH", 25)]), xyz],
        )
        .expect("aligned")
    }

    #[test]
    fn the_dex_of_a_coin_is_its_prefix() {
        assert_eq!(dex_of_coin("BTC"), "");
        assert_eq!(dex_of_coin("xyz:TSLA"), "xyz");
        assert_eq!(dex_of_coin("@107"), "");
    }

    #[test]
    fn the_default_dex_resolves_by_plain_index() {
        let r = two_dexs();
        assert_eq!(r.coin_of_asset(0).as_deref(), Some("BTC"));
        assert_eq!(r.coin_of_asset(1).as_deref(), Some("ETH"));
        assert_eq!(r.asset_id("ETH"), Some(1));
        // An index past the default universe is unknown, not a wrap.
        assert_eq!(r.coin_of_asset(2), None);
    }

    #[test]
    fn a_builder_dex_resolves_by_its_perp_dexs_position() {
        let r = two_dexs();
        assert_eq!(r.coin_of_asset(110_000).as_deref(), Some("xyz:XYZ100"));
        assert_eq!(r.coin_of_asset(110_001).as_deref(), Some("xyz:TSLA"));
        assert_eq!(r.asset_id("xyz:XYZ100"), Some(110_000));
        assert_eq!(r.asset_id("xyz:TSLA"), Some(110_001));
        assert_eq!(r.dex("xyz").unwrap().collateral_token, 360);
        assert_eq!(r.dex("xyz").unwrap().asset_base, 110_000);
        assert_eq!(r.dex("").unwrap().asset_base, 0);
        assert_eq!(
            r.meta_for_coin("xyz:TSLA").map(|m| m.collateral_token),
            Some(360)
        );
        assert!(r.meta_for_coin("xyz:NOPE").is_none());
        assert!(r.meta_for_coin("BTC").is_some());
    }

    #[test]
    fn an_unknown_asset_id_resolves_to_nothing() {
        let r = two_dexs();
        // Past the builder dex's universe, a dex position that is not listed,
        // the spot range, and the unused 100000 block.
        assert_eq!(r.coin_of_asset(110_002), None);
        assert_eq!(r.coin_of_asset(120_000), None);
        assert_eq!(r.coin_of_asset(10_000), None);
        assert_eq!(r.coin_of_asset(100_000), None);
        assert_eq!(r.coin_of_asset(u32::MAX), None);
        assert_eq!(r.asset_id("zzz:COIN"), None);
        assert_eq!(r.asset_id("DOGE"), None);
        assert_eq!(DexRegistry::default().coin_of_asset(0), None);
    }

    #[test]
    fn the_registry_keeps_perp_dexs_order_and_refuses_a_misaligned_pair() {
        let r = two_dexs();
        let names: Vec<&str> = r.dexs().iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["", "xyz"]);
        assert_eq!(r.dexs()[1].index, 1);
        // A metas answer of a different length is not aligned with perpDexs.
        assert!(DexRegistry::from_api(
            vec![(String::new(), None), ("xyz".into(), None)],
            vec![meta_of(&[("BTC", 40)])],
        )
        .is_none());
        // The first entry must be the default dex.
        assert!(
            DexRegistry::from_api(vec![("xyz".into(), None)], vec![meta_of(&[])]).is_none(),
            "index 0 is the default dex"
        );
    }

    #[test]
    fn a_fixture_dex_appends_after_the_default() {
        let mut r = DexRegistry::of_default(meta_of(&[("BTC", 40)]));
        r.put_meta("xyz", meta_of(&[("xyz:TSLA", 10)]));
        assert_eq!(r.coin_of_asset(110_000).as_deref(), Some("xyz:TSLA"));
        // A refresh never invents a dex.
        assert!(!r.refresh_meta("nope", meta_of(&[])));
        assert!(r.refresh_meta("xyz", meta_of(&[("xyz:TSLA", 10), ("xyz:AAPL", 10)])));
        assert_eq!(r.coin_of_asset(110_001).as_deref(), Some("xyz:AAPL"));
    }
}
