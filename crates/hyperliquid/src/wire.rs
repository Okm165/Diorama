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
use alloy_primitives::Address;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One `POST /info` request. Unknown types parse to [`InfoRequest::Unrouted`]
/// (hl_mock answered them as unrouted; the handler keeps that contract).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum InfoRequest {
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
    /// The request's `type` tag, for logs and the unrouted refusal.
    pub fn kind(&self) -> &'static str {
        match self {
            InfoRequest::ClearinghouseState { .. } => "clearinghouseState",
            InfoRequest::SpotClearinghouseState { .. } => "spotClearinghouseState",
            InfoRequest::UserAbstraction { .. } => "userAbstraction",
            InfoRequest::OpenOrders { .. } => "openOrders",
            InfoRequest::Meta { .. } => "meta",
            InfoRequest::MetaAndAssetCtxs { .. } => "metaAndAssetCtxs",
            InfoRequest::PerpDexs => "perpDexs",
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
    pub fn dex(&self) -> &str {
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
pub struct MarginSummary {
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
pub struct Position {
    pub coin: String,
    pub entry_px: Option<Decimal>,
    pub leverage: Leverage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liquidation_px: Option<Decimal>,
    pub margin_used: Decimal,
    pub max_leverage: u32,
    pub position_value: Decimal,
    pub return_on_equity: Decimal,
    pub szi: Decimal,
    pub unrealized_pnl: Decimal,
    pub cum_funding: CumFunding,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Leverage {
    #[serde(rename = "type")]
    pub kind: LeverageType,
    pub value: u32,
    /// `marginUsed − positionValue` (measured exact on the live sample).
    pub raw_usd: Option<Decimal>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LeverageType {
    Cross,
    Isolated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CumFunding {
    pub all_time: Decimal,
    pub since_open: Decimal,
    pub since_change: Decimal,
}

/// The `assetPositions` wrapper (`type: "oneWay"` today).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetPosition {
    #[serde(rename = "type")]
    pub kind: PositionKind,
    pub position: Position,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PositionKind {
    #[serde(rename = "oneWay")]
    OneWay,
}

/// `clearinghouseState`: the venue account read (daloc's equity + recall).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClearinghouseState {
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
pub struct SpotClearinghouseState {
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
pub struct SpotBalance {
    pub coin: String,
    pub token: u32,
    pub hold: Decimal,
    pub total: Decimal,
    pub entry_ntl: Decimal,
}

/// One open order. The sim has no order machinery yet (Phase 2 matching), so
/// it serves the empty list; the type is complete for the day it does.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub coin: String,
    pub oid: u64,
    pub side: Side,
    pub sz: Decimal,
    pub limit_px: Decimal,
    pub timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_trigger: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_px: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_position_tpsl: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orig_sz: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tif: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloid: Option<String>,
}

/// `A` = ask/sell, `B` = bid/buy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side {
    #[serde(rename = "A")]
    Ask,
    #[serde(rename = "B")]
    Bid,
}

/// `meta`: the perp universe + margin tables. daloc's operator reads
/// `universe[].szDecimals` for the wire `sz` of unwind orders.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub universe: Vec<AssetInfo>,
    /// `[[id, table], …]` on the wire (measured).
    #[serde(default)]
    pub margin_tables: Vec<(u32, MarginTable)>,
    #[serde(default)]
    pub collateral_token: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssetInfo {
    pub name: String,
    pub sz_decimals: u8,
    pub max_leverage: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_table_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only_isolated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_delisted: Option<bool>,
}

/// `marginTable`: the leverage tiers. Maintenance margin is ½ the initial
/// margin of the tier the position's notional sits in (plan §16.3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MarginTable {
    pub description: String,
    pub margin_tiers: Vec<MarginTier>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MarginTier {
    pub lower_bound: Decimal,
    pub max_leverage: u32,
}

/// One asset's market context (`metaAndAssetCtxs[1][i]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssetCtx {
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

/// `metaAndAssetCtxs`: a `[meta, ctxs]` tuple on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetaAndAssetCtxs(pub Meta, pub Vec<AssetCtx>);

/// `perpDexs`: `[null, …]` (the first dex is null). The sim serves the first
/// dex only until the multi-dex mechanics land (Phase 2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerpDex {
    pub name: String,
    pub full_name: String,
    pub deployer: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle_updater: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_recipient: Option<Address>,
    pub asset_to_streaming_oi_cap: Vec<(String, Decimal)>,
}

/// `l2Book`: exactly two sides, ≤20 levels each (daloc's gate). `n` is the
/// order count per level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct L2Book {
    pub coin: String,
    pub time: u64,
    pub levels: [Vec<Level>; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Level {
    pub px: Decimal,
    pub sz: Decimal,
    pub n: u64,
}

/// `activeAssetData`: per-coin leverage + trade limits for a user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveAssetData {
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
pub struct ExchangeStatus {
    /// The venue serves `null` in normal operation; the sim always serves
    /// `null` (a chain halt is a real-venue fact, not a simulated one).
    pub special_statuses: Option<Vec<SpecialStatus>>,
    pub time: u64,
}

/// One degraded-subsystem note (shape pinned from the live API when set).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpecialStatus {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
}

/// `allMids`: a plain `{coin: px}` map (HIP-3 keys are `"#<id>"`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AllMids(pub BTreeMap<String, Decimal>);

/// `spotMeta`: the spot token registry (`token` indices the spot state keys
/// by) + the pair universe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpotMeta {
    pub tokens: Vec<SpotToken>,
    pub universe: Vec<SpotPair>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SpotToken {
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
pub struct SpotEvmContract {
    pub address: Address,
    #[serde(rename = "evm_extra_wei_decimals")]
    pub evm_extra_wei_decimals: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpotPair {
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
pub enum UserAbstraction {
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

