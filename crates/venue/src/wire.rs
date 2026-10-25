//! The Hyperliquid info API's wire model (measured live, plan §16.5).
//!
//! One request enum (the endpoint is a single `POST /info` tagged by `type`)
//! and the response structs daloc's services read — complete shapes, not
//! projections: every field the venue sends has a typed home, and the ones
//! daloc gates on (`positionValue` unsigned with the sign in `szi`,
//! `cumFunding.sinceOpen`, `leverage.type`, `totalRawUsd`) are named in the
//! rustdoc so a refactor cannot quietly drop them.
//!
//! Money and sizes are decimal *text* on this API; [`Decimal`] keeps them
//! exact (`serde-with-str`), because the served identity
//! `accountValue == totalRawUsd + Σ signed(positionValue)` is checked at
//! 1e-6 by the venue client — f64 arithmetic would fail it at the edges.
//! Fields the venue may null (`midPx` in thin books, `liquidationPx` on cross
//! positions) are `Option`.
//!
//! Every enum here is `#[non_exhaustive]`: this is the venue's vocabulary and
//! it may grow, so a consumer outside this crate must carry a default arm. Our
//! own taxonomies (errors, config, persisted state) stay exhaustive — a new
//! variant there *should* break every match at compile time.
use alloy_primitives::Address;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One `POST /info` request. Unknown types parse to [`InfoRequest::Unrouted`]
/// (hl_mock answered them as unrouted; the handler keeps that contract).
/// The docs' `oid` union for `orderStatus`: `uint64 or string` (a 16-byte hex
/// client order id).
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum OidOrCloid {
    Oid(u64),
    Cloid(alloy_primitives::B128),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
#[non_exhaustive]
pub(crate) enum InfoRequest {
    #[serde(rename = "clearinghouseState")]
    ClearinghouseState {
        user: Address,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dex: Option<String>,
    },
    #[serde(rename = "spotClearinghouseState")]
    SpotClearinghouseState { user: Address },
    #[serde(rename = "userAbstraction")]
    UserAbstraction { user: Address },
    #[serde(rename = "openOrders")]
    OpenOrders {
        user: Address,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dex: Option<String>,
    },
    #[serde(rename = "frontendOpenOrders")]
    FrontendOpenOrders {
        user: Address,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dex: Option<String>,
    },
    #[serde(rename = "orderStatus")]
    OrderStatus {
        user: Address,
        /// The docs' union: a u64 order id or a 16-byte hex cloid.
        oid: OidOrCloid,
    },
    #[serde(rename = "perpsAtOpenInterestCap")]
    PerpsAtOpenInterestCap {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dex: Option<String>,
    },
    #[serde(rename = "meta")]
    Meta {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dex: Option<String>,
    },
    #[serde(rename = "metaAndAssetCtxs")]
    MetaAndAssetCtxs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dex: Option<String>,
    },
    #[serde(rename = "perpDexs")]
    PerpDexs,
    #[serde(rename = "l2Book")]
    L2Book {
        coin: String,
        #[serde(rename = "nSigFigs", default, skip_serializing_if = "Option::is_none")]
        n_sig_figs: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mantissa: Option<u8>,
    },
    #[serde(rename = "marginTable")]
    MarginTable { id: u32 },
    #[serde(rename = "activeAssetData")]
    ActiveAssetData { user: Address, coin: String },
    #[serde(rename = "exchangeStatus")]
    ExchangeStatus,
    #[serde(rename = "spotMeta")]
    SpotMeta,
    #[serde(rename = "allMids")]
    AllMids {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dex: Option<String>,
    },
    #[serde(other)]
    Unrouted,
}

impl InfoRequest {
    /// The request's `type` tag: the unit test pins the map against the wire
    /// names (nothing in production needs the tags by name).
    #[cfg(test)]
    fn kind(&self) -> &'static str {
        match self {
            InfoRequest::ClearinghouseState { .. } => "clearinghouseState",
            InfoRequest::SpotClearinghouseState { .. } => "spotClearinghouseState",
            InfoRequest::UserAbstraction { .. } => "userAbstraction",
            InfoRequest::OpenOrders { .. } => "openOrders",
            InfoRequest::Meta { .. } => "meta",
            InfoRequest::MetaAndAssetCtxs { .. } => "metaAndAssetCtxs",
            InfoRequest::PerpDexs => "perpDexs",
            InfoRequest::FrontendOpenOrders { .. } => "frontendOpenOrders",
            InfoRequest::OrderStatus { .. } => "orderStatus",
            InfoRequest::PerpsAtOpenInterestCap { .. } => "perpsAtOpenInterestCap",
            InfoRequest::L2Book { .. } => "l2Book",
            InfoRequest::MarginTable { .. } => "marginTable",
            InfoRequest::ActiveAssetData { .. } => "activeAssetData",
            InfoRequest::ExchangeStatus => "exchangeStatus",
            InfoRequest::SpotMeta => "spotMeta",
            InfoRequest::AllMids { .. } => "allMids",
            InfoRequest::Unrouted => "unrouted",
        }
    }

    /// The perp dex this request scopes to (empty/absent = the first dex).
    /// Spot reads are only defined on the first dex.
    #[cfg(test)]
    fn dex(&self) -> &str {
        match self {
            InfoRequest::ClearinghouseState { dex, .. }
            | InfoRequest::OpenOrders { dex, .. }
            | InfoRequest::Meta { dex, .. }
            | InfoRequest::MetaAndAssetCtxs { dex, .. }
            | InfoRequest::AllMids { dex, .. } => dex.as_deref().unwrap_or(""),
            _ => "",
        }
    }
}

/// `marginSummary` (and `crossMarginSummary`): the figures daloc reads.
///
/// `totalRawUsd` is the operator's recall amount — the raw USDC balance with
/// the positions' cost basis removed (`balance − Σ szi·entryPx`). The served
/// identity holds by construction: `accountValue = totalRawUsd + Σ szi·mark`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarginSummary {
    pub account_value: Decimal,
    pub total_ntl_pos: Decimal,
    pub total_raw_usd: Decimal,
    pub total_margin_used: Decimal,
}

/// One position row. `positionValue` is unsigned on the wire; the side comes
/// from the sign of `szi` (daloc's exact rule). `liquidationPx` is present
/// for isolated positions only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Position {
    pub coin: String,
    #[serde(default)]
    pub entry_px: Option<Decimal>,
    pub leverage: Leverage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liquidation_px: Option<Decimal>,
    pub margin_used: Decimal,
    /// `Option` on the *parse* side only: hl_mock's control patches omit it
    /// (`VenuePositionState.maxLeverage` is optional); the sim always serves it.
    #[serde(default)]
    pub max_leverage: u32,
    pub position_value: Decimal,
    /// Same: the control patch shape has no `returnOnEquity`; the sim serves it.
    #[serde(default)]
    pub return_on_equity: Decimal,
    pub szi: Decimal,
    pub unrealized_pnl: Decimal,
    pub cum_funding: CumFunding,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Leverage {
    #[serde(rename = "type")]
    pub kind: LeverageType,
    pub value: u32,
    /// `marginUsed − positionValue` (measured exact on the live sample).
    #[serde(default)]
    pub raw_usd: Option<Decimal>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub(crate) enum LeverageType {
    Cross,
    Isolated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CumFunding {
    pub all_time: Decimal,
    pub since_open: Decimal,
    pub since_change: Decimal,
}

/// The `assetPositions` wrapper (`type: "oneWay"` today).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct AssetPosition {
    #[serde(rename = "type")]
    pub kind: PositionKind,
    pub position: Position,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum PositionKind {
    #[serde(rename = "oneWay")]
    OneWay,
}

/// `clearinghouseState`: the venue account read (daloc's equity + recall).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClearinghouseState {
    pub margin_summary: MarginSummary,
    pub cross_margin_summary: MarginSummary,
    pub cross_maintenance_margin_used: Decimal,
    pub withdrawable: Decimal,
    pub asset_positions: Vec<AssetPosition>,
    /// Milliseconds; the client's staleness gate is ≤6 s against this field.
    pub time: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpotClearinghouseState {
    pub balances: Vec<SpotBalance>,
    /// The venue's "available after maintenance", per token, as `[token, value]`
    /// pairs. Served for **unified** accounts — it is the input daloc's client
    /// reconciles (`available + maintenance == the collateral token's balance`);
    /// empty (and omitted) for standard ones, whose spot side is not read for
    /// equity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_to_available_after_maintenance: Vec<(u32, Decimal)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpotBalance {
    pub coin: String,
    pub token: u32,
    pub hold: Decimal,
    pub total: Decimal,
    pub entry_ntl: Decimal,
}


/// `meta`: the perp universe + margin tables. daloc's operator reads
/// `universe[].szDecimals` for the wire `sz` of unwind orders.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Meta {
    pub universe: Vec<AssetInfo>,
    /// `[[id, table], …]` on the wire (measured).
    #[serde(default)]
    pub margin_tables: Vec<(u32, MarginTable)>,
    #[serde(default)]
    pub collateral_token: u32,
}

impl Meta {
    /// The live asset entry for a coin — the one lookup (the maintenance
    /// tier, the max-leverage fallback, the order validators): `None` while
    /// the meta is warming or for a coin outside the tracked universe.
    pub(crate) fn asset(&self, coin: &str) -> Option<&AssetInfo> {
        self.universe.iter().find(|a| a.name == coin)
    }
}

/// The asset's margin mode (docs, margining: "strictIsolated" means margin
/// cannot be removed; the info endpoint: "noCross" means only isolated margin
/// allowed; `onlyIsolated` is deprecated — "Means either strictIsolated or
/// noCross"). Unknown future values parse as `Unknown` rather than failing
/// the whole meta.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum MarginMode {
    #[serde(rename = "strictIsolated")]
    StrictIsolated,
    #[serde(rename = "noCross")]
    NoCross,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssetInfo {
    pub name: String,
    pub sz_decimals: u8,
    pub max_leverage: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_table_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_mode: Option<MarginMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only_isolated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_delisted: Option<bool>,
}

/// `marginTable`: the leverage tiers. Maintenance margin is ½ the initial
/// margin of the tier the position's notional sits in (plan §16.3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarginTable {
    pub description: String,
    pub margin_tiers: Vec<MarginTier>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarginTier {
    pub lower_bound: Decimal,
    pub max_leverage: u32,
}

/// One asset's market context (`metaAndAssetCtxs[1][i]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssetCtx {
    /// The published hourly rate (⅛ of the 8 h rate).
    pub funding: Decimal,
    pub open_interest: Decimal,
    pub prev_day_px: Decimal,
    pub day_ntl_vlm: Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub premium: Option<Decimal>,
    pub oracle_px: Decimal,
    pub mark_px: Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mid_px: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact_pxs: Option<[Decimal; 2]>,
    #[serde(default)]
    pub day_base_vlm: Decimal,
}

/// The tests' `AssetCtx` fixture (funding ⅛e-7, marks/oracle from the args):
/// one definition — the feed tests, the sim tests and the account tests all
/// build on it.
#[cfg(test)]
pub(crate) fn test_ctx(mark: &str, oracle: &str) -> AssetCtx {
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
    }
}

/// `metaAndAssetCtxs`: a `[meta, ctxs]` tuple on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct MetaAndAssetCtxs(pub Meta, pub Vec<AssetCtx>);

/// `perpDexs`: `[null, …]` (the first dex is null). The sim serves the first
/// dex only until the multi-dex mechanics land (Phase 2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PerpDex {
    pub name: String,
    pub full_name: String,
    pub deployer: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle_updater: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_recipient: Option<Address>,
    /// Per-coin OI caps (the venue's credit terms; the sim's feed reads them).
    pub asset_to_streaming_oi_cap: Vec<(String, Decimal)>,
    /// The HIP-3 funding knobs the live dexes carry (measured 2026-10-23:
    /// `assetToFundingClamp` empty on `xyz`, the other two populated per coin);
    /// kept for wire completeness — no consumer reads them yet.
    #[serde(default)]
    pub asset_to_funding_clamp: Vec<(String, Decimal)>,
    #[serde(default)]
    pub asset_to_funding_interest_rate: Vec<(String, Decimal)>,
    #[serde(default)]
    pub asset_to_funding_multiplier: Vec<(String, Decimal)>,
    /// `[[action, [address…]]…]` — the dex's delegated admin surface
    /// (`registerAsset`, `setOracle`, …; measured live).
    #[serde(default)]
    pub sub_deployers: Vec<(String, Vec<Address>)>,
}

/// `l2Book`: exactly two sides, ≤20 levels each (daloc's gate). `n` is the
/// order count per level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct L2Book {
    pub coin: String,
    pub time: u64,
    pub levels: [Vec<Level>; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Level {
    pub px: Decimal,
    pub sz: Decimal,
    pub n: u64,
}

/// `activeAssetData`: per-coin leverage + trade limits for a user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActiveAssetData {
    pub user: Address,
    pub coin: String,
    pub leverage: Leverage,
    pub max_trade_szs: [Decimal; 2],
    pub available_to_trade: [Decimal; 2],
    pub mark_px: Decimal,
}

/// `exchangeStatus`: the venue's health probe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExchangeStatus {
    /// The venue serves `null` in normal operation; the sim always serves
    /// `null` (a chain halt is a real-venue fact, not a simulated one).
    pub special_statuses: Option<Vec<SpecialStatus>>,
    pub time: u64,
}

/// One degraded-subsystem note (shape pinned from the live API when set).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SpecialStatus {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
}

/// `allMids`: a plain `{coin: px}` map (HIP-3 keys are `"#<id>"`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct AllMids(pub BTreeMap<String, Decimal>);

/// `spotMeta`: the spot token registry (`token` indices the spot state keys
/// by) + the pair universe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SpotMeta {
    pub tokens: Vec<SpotToken>,
    pub universe: Vec<SpotPair>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpotToken {
    pub name: String,
    pub sz_decimals: u8,
    pub wei_decimals: u8,
    pub index: u32,
    #[serde(default)]
    pub is_canonical: bool,
    /// The token's EVM link; the live API serves `null` for unlinked tokens
    /// (the sim serves the cached live shape verbatim).
    pub evm_contract: Option<SpotEvmContract>,
    /// The venue's token id: a 16-byte hex string, distinct from the `index`
    /// the spot state keys by (measured on the live API).
    #[serde(default)]
    pub token_id: Option<String>,
    /// The token's display name; `null` on every live token (measured).
    #[serde(default)]
    pub full_name: Option<String>,
    /// The deployer's trading-fee share (HIP-1 tokens); a decimal string on
    /// the wire (`"0.0"` on the live sample).
    #[serde(default)]
    pub deployer_trading_fee_share: Decimal,
}

/// A token's EVM link (`evm_extra_wei_decimals` is measured: USDC carries −2).
/// The key is snake_case on the wire while the rest of the response is
/// camelCase (measured) — hence the explicit rename.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpotEvmContract {
    pub address: Address,
    #[serde(rename = "evm_extra_wei_decimals")]
    pub evm_extra_wei_decimals: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpotPair {
    /// `[base, quote]` token indices.
    pub tokens: [u32; 2],
    pub name: String,
    pub index: u32,
    #[serde(default)]
    pub is_canonical: bool,
}

/// `userAbstraction`: the account mode enum (daloc refuses
/// `portfolioMargin`/`dexAbstraction`; unknown users serve `default`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub(crate) enum UserAbstraction {
    #[default]
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "disabled")]
    Disabled,
    #[serde(rename = "unifiedAccount")]
    UnifiedAccount,
    #[serde(rename = "portfolioMargin")]
    PortfolioMargin,
    #[serde(rename = "dexAbstraction")]
    DexAbstraction,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_shapes_parse_through_their_types() {
        // Trimmed live samples (2026-10-23): every wire shape the sim serves
        // parses through its own type — the check that would have caught
        // `PerpDex`'s missing camelCase rename (it never parsed the live shape
        // until this test existed).
        let meta: MetaAndAssetCtxs = serde_json::from_str(
            r#"[
              {"universe":[{"szDecimals":5,"name":"BTC","maxLeverage":40,"marginTableId":56}],
               "marginTables":[[50,{"description":"","marginTiers":[{"lowerBound":"0.0","maxLeverage":50}]}]],
               "collateralToken":0},
              [{"funding":"0.0000125","openInterest":"43604.51494","prevDayPx":"82270.0",
                "dayNtlVlm":"2889867410.3497600555","premium":"-0.0000587373","oraclePx":"81719.8",
                "markPx":"81713.1","midPx":"81714.5","impactPxs":["81712.3","81715.0"],
                "dayBaseVlm":"35396.23357"}]
            ]"#,
        )
        .unwrap();
        assert_eq!(meta.0.universe[0].margin_table_id, Some(56));
        assert_eq!(meta.1[0].impact_pxs.unwrap()[1].to_string(), "81715.0");

        let book: L2Book = serde_json::from_str(
            r#"{"coin":"BTC","time":1792741463491,
                "levels":[[{"px":"81712.0","sz":"3.29777","n":16}],[{"px":"81713.0","sz":"13.62593","n":55}]]}"#,
        )
        .unwrap();
        assert_eq!(book.levels[0][0].n, 16);

        let table: MarginTable = serde_json::from_str(
            r#"{"description":"tiered 40x","marginTiers":[{"lowerBound":"0.0","maxLeverage":40},{"lowerBound":"150000000.0","maxLeverage":20}]}"#,
        )
        .unwrap();
        assert_eq!(table.margin_tiers[1].max_leverage, 20);

        let active: ActiveAssetData = serde_json::from_str(
            r#"{"user":"0x00000000000000000000000000000000000000a1","coin":"BTC",
                "leverage":{"type":"cross","value":20},"maxTradeSzs":["0.0","0.0"],
                "availableToTrade":["0.0","0.0"],"markPx":"81697.0"}"#,
        )
        .unwrap();
        assert_eq!(active.leverage.kind, LeverageType::Cross);

        let status: ExchangeStatus =
            serde_json::from_str(r#"{"specialStatuses":null,"time":1792741470785}"#).unwrap();
        assert!(status.special_statuses.is_none());

        let spot: SpotClearinghouseState = serde_json::from_str(r#"{"balances":[]}"#).unwrap();
        assert!(spot.balances.is_empty());
    }

    /// The served *names* are the venue's own: a snake_case key is silently
    /// nothing to a client, and a missing rename silently drops the live value
    /// on deserialize (measured: `SpotPair` served `is_canonical` and lost the
    /// live `isCanonical: true`). The parse test above cannot catch it —
    /// `#[serde(default)]` masks the missing field — so this asserts the names.
    #[test]
    fn served_field_names_are_the_venues() {
        let pair = SpotPair {
            tokens: [1, 0],
            name: "PURR/USDC".into(),
            index: 0,
            is_canonical: true,
        };
        let v = serde_json::to_value(&pair).unwrap();
        assert_eq!(v["isCanonical"], serde_json::json!(true));
        assert!(v.get("is_canonical").is_none(), "the venue's key is camelCase");
        let back: SpotPair = serde_json::from_value(serde_json::json!({
            "tokens": [1, 0], "name": "PURR/USDC", "index": 0, "isCanonical": true
        }))
        .unwrap();
        assert!(back.is_canonical, "the live key lands in the field");
        let token = SpotToken {
            name: "USDC".into(),
            sz_decimals: 8,
            wei_decimals: 8,
            index: 0,
            is_canonical: true,
            evm_contract: None,
            token_id: None,
            full_name: None,
            deployer_trading_fee_share: Decimal::ZERO,
        };
        let tv = serde_json::to_value(&token).unwrap();
        assert_eq!(tv["szDecimals"], serde_json::json!(8));
        assert_eq!(tv["isCanonical"], serde_json::json!(true));
        assert!(tv.get("deployerTradingFeeShare").is_some());
    }

    #[test]
    fn perp_dex_carries_the_measured_funding_knobs() {
        // Captured live 2026-10-23 from `perpDexs[1]` (the `xyz` dex): the
        // funding knobs and the delegated admin surface are part of the shape
        // (wire completeness — no consumer reads them yet).
        let raw = r#"{
            "name": "xyz",
            "fullName": "XYZ",
            "deployer": "0x0000000000000000000000000000000000000001",
            "oracleUpdater": "0x0000000000000000000000000000000000000002",
            "feeRecipient": "0x0000000000000000000000000000000000000003",
            "assetToStreamingOiCap": [["xyz:AAPL","200000000.0"]],
            "assetToFundingClamp": [],
            "assetToFundingInterestRate": [["xyz:EUR","0.0"]],
            "assetToFundingMultiplier": [["xyz:AAPL","0.5"]],
            "subDeployers": [["registerAsset",["0x0000000000000000000000000000000000000004"]]]
        }"#;
        let dex: PerpDex = serde_json::from_str(raw).unwrap();
        assert_eq!(dex.asset_to_funding_clamp.len(), 0);
        assert_eq!(dex.asset_to_funding_interest_rate[0].0, "xyz:EUR");
        assert_eq!(dex.asset_to_funding_multiplier[0].1.to_string(), "0.5");
        assert_eq!(dex.sub_deployers[0].0, "registerAsset");
        assert_eq!(dex.sub_deployers[0].1.len(), 1);
    }

    #[test]
    fn requests_parse_by_tag() {
        let r: InfoRequest =
            serde_json::from_str(r#"{"type":"clearinghouseState","user":"0x0000000000000000000000000000000000000001"}"#)
                .unwrap();
        assert!(matches!(r, InfoRequest::ClearinghouseState { .. }));
        assert_eq!(r.kind(), "clearinghouseState");
        assert_eq!(r.dex(), "");
        let r: InfoRequest = serde_json::from_str(r#"{"type":"l2Book","coin":"BTC","nSigFigs":5}"#).unwrap();
        assert!(matches!(r, InfoRequest::L2Book { n_sig_figs: Some(5), .. }));
        let r: InfoRequest = serde_json::from_str(r#"{"type":"userFills","user":"0x0000000000000000000000000000000000000001"}"#).unwrap();
        assert_eq!(r, InfoRequest::Unrouted);
    }

    /// The exact live sample (docs, measured): the identity daloc gates on
    /// holds numerically on the real wire shape.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the live sample fixture: one captured response through every field's identity check"
    )]
    fn live_sample_identity_holds() {
        let raw = r#"{
            "assetPositions": [{
                "position": {
                    "coin": "ETH",
                    "cumFunding": {"allTime": "514.085417", "sinceChange": "0.0", "sinceOpen": "0.0"},
                    "entryPx": "2986.3",
                    "leverage": {"rawUsd": "-95.059824", "type": "isolated", "value": 20},
                    "liquidationPx": "2866.26936529",
                    "marginUsed": "4.967826",
                    "maxLeverage": 50,
                    "positionValue": "100.02765",
                    "returnOnEquity": "-0.0026789",
                    "szi": "0.0335",
                    "unrealizedPnl": "-0.0134"
                },
                "type": "oneWay"
            }],
            "crossMaintenanceMarginUsed": "0.0",
            "crossMarginSummary": {
                "accountValue": "13104.514502",
                "totalMarginUsed": "0.0",
                "totalNtlPos": "0.0",
                "totalRawUsd": "13009.454678"
            },
            "marginSummary": {
                "accountValue": "13109.482328",
                "totalMarginUsed": "4.967826",
                "totalNtlPos": "100.02765",
                "totalRawUsd": "13009.454678"
            },
            "time": 1708622398623,
            "withdrawable": "13104.514502"
        }"#;
        let s: ClearinghouseState = serde_json::from_str(raw).unwrap();
        let p = &s.asset_positions[0].position;
        // The identity daloc gates on: accountValue == totalRawUsd + Σ
        // signed(positionValue), where the wire's positionValue is unsigned
        // and the sign comes from szi (`|szi|·mark` under the hood).
        let signed = if p.szi.is_sign_negative() {
            -p.position_value
        } else {
            p.position_value
        };
        assert_eq!(s.margin_summary.account_value, s.margin_summary.total_raw_usd + signed);
        // and equivalently via the derived mark: szi·mark = signed value.
        let mark = p.position_value / p.szi.abs();
        assert_eq!(p.szi * mark, signed);
        assert_eq!(p.unrealized_pnl, p.szi * (mark - p.entry_px.unwrap()));
        // cross accountValue = accountValue − isolated marginUsed (measured).
        assert_eq!(
            s.cross_margin_summary.account_value,
            s.margin_summary.account_value - p.margin_used
        );
        assert_eq!(s.withdrawable, s.cross_margin_summary.account_value);
        // rawUsd = marginUsed − positionValue (measured).
        assert_eq!(p.leverage.raw_usd.unwrap(), p.margin_used - p.position_value);
        // Decimals round-trip as strings, never as JSON floats.
        let out = serde_json::to_string(&s).unwrap();
        assert!(out.contains("\"accountValue\":\"13109.482328\""), "{out}");
    }

    /// A genuine excerpt of the live `spotMeta` (measured): the token keys are
    /// camelCase *except* `evm_extra_wei_decimals` (snake_case), and
    /// `evmContract` is null for tokens without an EVM link — both bit the
    /// poller live, so both stay pinned.
    #[test]
    fn spot_meta_parses_the_measured_mixed_case_shape() {
        let raw = r#"{
            "tokens": [
                {"name":"USDC","szDecimals":8,"weiDecimals":8,"index":0,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054","isCanonical":true,"evmContract":{"address":"0x6b9e773128f453f5c2c60935ee2de2cbc5390a24","evm_extra_wei_decimals":-2},"fullName":null,"deployerTradingFeeShare":"0.0"},
                {"name":"BYQYS","szDecimals":2,"weiDecimals":8,"index":1273,"tokenId":"0x09c9c5971dd28c23a28b1e39e794fcce","isCanonical":false,"evmContract":null,"fullName":"BYQYS","deployerTradingFeeShare":"0.0"}
            ],
            "universe": [{"tokens":[1,0],"name":"PURR/USDC","index":0,"isCanonical":true}]
        }"#;
        let spot: SpotMeta = serde_json::from_str(raw).unwrap();
        assert_eq!(spot.tokens.len(), 2);
        assert_eq!(spot.tokens[0].evm_contract.as_ref().unwrap().evm_extra_wei_decimals, -2);
        assert!(spot.tokens[1].evm_contract.is_none());
        assert_eq!(spot.tokens[1].index, 1273);
        assert_eq!(spot.universe[0].tokens, [1, 0]);
    }

    /// Requests never serialize `null` optionals: the venue rejects
    /// `{"type":"metaAndAssetCtxs","dex":null}` with a 422 (measured).
    #[test]
    fn requests_omit_null_optionals() {
        let req = InfoRequest::MetaAndAssetCtxs { dex: None };
        assert_eq!(serde_json::to_string(&req).unwrap(), r#"{"type":"metaAndAssetCtxs"}"#);
        let req = InfoRequest::L2Book { coin: "BTC".into(), n_sig_figs: None, mantissa: None };
        assert_eq!(serde_json::to_string(&req).unwrap(), r#"{"type":"l2Book","coin":"BTC"}"#);
        let req = InfoRequest::Meta { dex: Some("xyz".into()) };
        assert_eq!(serde_json::to_string(&req).unwrap(), r#"{"type":"meta","dex":"xyz"}"#);
    }
}

