//! The daloc-facing info API (8090): one `POST /info` endpoint, dispatched by
//! the request's `type`.
//!
//! COW rule: a **touched** account is answered from the sim; an **untouched**
//! one is passed through to the real venue (which serves its own empty shapes
//! for addresses it has never seen — daloc's "unconfigured accounts → empty
//! shape, 200 OK" contract holds either way). Market data is answered from
//! the composed feed, so a crashed mark is the same number the sim priced
//! with.
use super::ApiState;
use crate::error::{Error, Result};
use crate::sim::account;
use crate::wire::{
    ActiveAssetData, ClearinghouseState, ExchangeStatus, InfoRequest, Leverage, LeverageType,
    MarginSummary, MetaAndAssetCtxs, UserAbstraction,
};
use alloy_primitives::Address;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rust_decimal::Decimal;
use std::sync::Arc;

/// The readiness gate's freshness window (the plan's ≤6 s client gate, with
/// margin for a poll interval).
const READY_MAX_STALE_MS: u64 = 30_000;

pub async fn healthz(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    if state.sim.is_poisoned() {
        // The account plane refuses every read; a 200 here would hide an
        // outage from the orchestrator (the verifier's readiness-gap find).
        return (StatusCode::SERVICE_UNAVAILABLE, "sim state poisoned");
    }
    (StatusCode::OK, "ok")
}

pub(crate) async fn readyz(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    if state.sim.is_poisoned() {
        return (StatusCode::SERVICE_UNAVAILABLE, "sim state poisoned");
    }
    if state.ready(READY_MAX_STALE_MS) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "market plane warming up")
    }
}

/// `POST /exchange` — the venue's action surface. Only the `order` action is
/// modeled (plan §4.5/§5, Phase 2's first slice): it fills against the **real
/// book** — the live depth the feed caches — with per-level consumption, the
/// impact lane. The plan's identity (I3) holds by construction: the order
/// moves the *book*, never the *mark*; the mark moves only by an explicit
/// crash. Any other action tag is refused by name in the venue's own `err`
/// envelope (HTTP 200 — the shape their client parses). The signature is
/// accepted, never verified (the devnet has no keys — the anvil-impersonation
/// analogue), and the acting account is read from `user`.
pub(crate) async fn exchange(State(state): State<Arc<ApiState>>, body: Bytes) -> Result<Response> {
    let req: ExchangeBody = serde_json::from_slice(&body).map_err(|source| Error::BadRequest {
        reason: format!("invalid exchange request: {source}"),
    })?;
    let ExchangeAction::Order { orders, grouping } = req.action else {
        return Ok(Json(ExchangeResponse {
            status: "err",
            response: "venue-sim: only the order action is modeled (plan §4.5/§5, Phase 2); \
                       cancels, modifies, leverage updates, API wallets and multi-dex \
                       actions are not. See docs/INTEGRATION.md."
                .to_string(),
        })
        .into_response());
    };
    tracing::info!(
        user = %req.user,
        nonce = ?req.nonce,
        signed = req.signature.as_ref().is_some_and(Signature::is_shaped),
        sig_v = req.signature.as_ref().map(|s| s.v),
        vault = ?req.vault_address,
        group = ?grouping,
        orders = orders.len(),
        "exchange order action accepted (the signature is accepted, never verified)"
    );
    let meta = state.feed.meta();
    let mut statuses = Vec::with_capacity(orders.len());
    for action in &orders {
        let outcome = action
            .to_order(meta.as_ref())
            .and_then(|o| state.sim.order(&state.feed, req.user, &o));
        match outcome {
            Ok(fill) => statuses.push(OrderStatus::Filled(FilledInfo {
                total_sz: fill.total_sz.to_string(),
                avg_px: fill.avg_px.to_string(),
                oid: fill.oid,
            })),
            Err(e) => statuses.push(OrderStatus::Error(e.to_string())),
        }
    }
    Ok(Json(ExchangeOk {
        status: "ok",
        response: OrderResponseBody {
            r#type: "order",
            data: OrderData { statuses },
        },
    })
    .into_response())
}

/// The `/exchange` body: the venue's compact action shape. The devnet reads
/// the acting account from `user` (the real venue derives it from the
/// signature) and accepts `signature` without checking it.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ExchangeBody {
    pub action: ExchangeAction,
    pub user: Address,
    #[serde(default)]
    pub nonce: Option<u64>,
    #[serde(default)]
    pub signature: Option<Signature>,
    #[serde(default)]
    pub vault_address: Option<Address>,
}

/// The EIP-712 signature's fields (accepted, never verified): the devnet
/// reads only its presence and shape.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct Signature {
    pub r: String,
    pub s: String,
    pub v: u8,
}

impl Signature {
    /// Whether the signature is shaped like the venue's (nonempty `r`/`s`).
    /// The devnet never verifies it — the anvil-impersonation analogue.
    pub(crate) fn is_shaped(&self) -> bool {
        !self.r.is_empty() && !self.s.is_empty()
    }
}

/// The action, tagged by `type`. Only `order` is modeled; every other tag the
/// venue defines lands in `Unmodeled` and is refused by name.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum ExchangeAction {
    Order {
        orders: Vec<OrderAction>,
        #[serde(default)]
        grouping: Option<String>,
    },
    #[serde(other)]
    Unmodeled,
}

/// One compact order (`a` asset index, `b` is_buy, `p` limit price, `s` size,
/// `r` reduce-only, `t` type/time-in-force).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct OrderAction {
    pub a: u32,
    pub b: bool,
    #[serde(default)]
    pub p: Option<String>,
    pub s: String,
    #[serde(default)]
    pub r: bool,
    #[serde(default)]
    pub t: Option<OrderType>,
}

/// The order type: the venue's own shape (a limit with a time-in-force; a
/// "market" order is a limit at the touch with `Ioc`). Only IOC fills — a
/// Gtc/Alo remainder cannot rest in the devnet (refused by name in the lane).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OrderType {
    Limit { tif: String },
}

impl OrderAction {
    /// The compact action → the lane's order: the asset index resolves
    /// through the live meta (unknown = a named refusal), size and limit are
    /// decimal strings, and only IOC fills.
    fn to_order(&self, meta: Option<&crate::wire::Meta>) -> Result<crate::sim::order::Order> {
        let coin = meta
            .and_then(|m| m.universe.get(self.a as usize))
            .map(|a| a.name.clone())
            .ok_or_else(|| Error::BadRequest {
                reason: format!(
                    "unknown asset index {} (the live meta has not landed, or is shorter)",
                    self.a
                ),
            })?;
        let size: Decimal = self.s.parse().map_err(|_| Error::BadRequest {
            reason: format!("size `{}` is not a decimal", self.s),
        })?;
        let limit_px = match &self.p {
            Some(p) => Some(p.parse().map_err(|_| Error::BadRequest {
                reason: format!("limit price `{p}` is not a decimal"),
            })?),
            None => None,
        };
        let tif = match &self.t {
            None => crate::sim::order::Tif::Ioc,
            Some(OrderType::Limit { tif }) => match tif.as_str() {
                "Ioc" => crate::sim::order::Tif::Ioc,
                "Gtc" | "Alo" => crate::sim::order::Tif::Gtc,
                other => {
                    return Err(Error::BadRequest {
                        reason: format!("unknown tif `{other}`"),
                    })
                }
            },
        };
        Ok(crate::sim::order::Order {
            coin,
            is_buy: self.b,
            size,
            limit_px,
            tif,
            reduce_only: self.r,
        })
    }
}

/// The ok envelope (the venue's own shape; the per-order results inside).
#[derive(Debug, serde::Serialize)]
pub(crate) struct ExchangeOk {
    pub status: &'static str,
    pub response: OrderResponseBody,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct OrderResponseBody {
    pub r#type: &'static str,
    pub data: OrderData,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct OrderData {
    pub statuses: Vec<OrderStatus>,
}

/// One per-order status in the venue's own shape: `{"filled": {…}}` or
/// `{"error": "…"}`.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OrderStatus {
    Filled(FilledInfo),
    Error(String),
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FilledInfo {
    pub total_sz: String,
    pub avg_px: String,
    pub oid: u64,
}

/// The `/exchange` err envelope (the named refusal above; complete for the
/// err shape their client reads).
#[derive(Debug, serde::Serialize)]
pub(crate) struct ExchangeResponse {
    pub status: &'static str,
    pub response: String,
}

/// `POST /info`: parse the typed request, dispatch, or pass through.
pub async fn info(State(state): State<Arc<ApiState>>, body: Bytes) -> Result<Response> {
    // A malformed body is the caller's fault (400), not a server fault: the
    // verifier measured a 500 here, which reads as "the sim is broken".
    let req: InfoRequest = serde_json::from_slice(&body).map_err(|source| Error::BadRequest {
        reason: format!("invalid info request: {source}"),
    })?;
    match req {
        // hl_mock's contract for types it does not route.
        InfoRequest::Unrouted => Ok(Json("unrouted").into_response()),
        // ---- account-scoped: the sim's, or the venue's -------------------
        InfoRequest::ClearinghouseState { user, dex } => clearinghouse(&state, body, user, dex).await,
        InfoRequest::SpotClearinghouseState { user } => spot_state(&state, body, user).await,
        InfoRequest::UserAbstraction { user } => abstraction(&state, body, user).await,
        InfoRequest::OpenOrders { user, .. } => open_orders(&state, body, user).await,
        InfoRequest::ActiveAssetData { user, coin } => active_asset(&state, body, user, coin).await,
        // ---- market-scoped: the feed's -----------------------------------
        InfoRequest::Meta { dex } => meta(&dex, &state),
        InfoRequest::MetaAndAssetCtxs { dex } => meta_and_ctxs(&dex, &state),
        InfoRequest::PerpDexs => Ok(Json(vec![None::<crate::wire::PerpDex>]).into_response()),
        InfoRequest::L2Book { coin, .. } => l2_book(&coin, &state),
        InfoRequest::MarginTable { id } => margin_table(id, &state),
        InfoRequest::ExchangeStatus => exchange_status(&state),
        InfoRequest::SpotMeta => Ok(Json(state.feed.spot_meta().ok_or(Error::Warming)?).into_response()),
        InfoRequest::AllMids { .. } => Ok(Json(state.feed.all_mids()).into_response()),
    }
}

/// `clearinghouseState`: the sim's for a touched account (the first perp dex
/// only), the venue's verbatim otherwise.
async fn clearinghouse(
    state: &ApiState,
    body: Bytes,
    user: Address,
    dex: Option<String>,
) -> Result<Response> {
    if !dex.as_deref().unwrap_or("").is_empty() {
        // The sim serves the first perp dex only (perpDexs = [null]).
        return Ok(Json(empty_clearinghouse(state.sim.served_time(crate::now_ms())?)).into_response());
    }
    match clearinghouse_of(state, user)? {
        Some(served) => Ok(Json(served).into_response()),
        None => pass_through(state, &body).await,
    }
}

/// `spotClearinghouseState`: the unified reconciliation
/// (`available + maintenance == balance`), or the venue's verbatim.
async fn spot_state(state: &ApiState, body: Bytes, user: Address) -> Result<Response> {
    state.sim.evaluate(&state.feed)?;
    match state.sim.account(user)? {
        Some(account) => {
            let meta = state.feed.meta();
            Ok(Json(crate::wire::SpotClearinghouseState {
                balances: account::spot_balances(&account),
                token_to_available_after_maintenance: account::available_after_maintenance(
                    &account,
                    &state.feed,
                    meta.as_ref(),
                )?,
            })
            .into_response())
        }
        None => pass_through(state, &body).await,
    }
}

/// `userAbstraction`: the account's parseable mode, or the venue's verbatim.
async fn abstraction(state: &ApiState, body: Bytes, user: Address) -> Result<Response> {
    match state.sim.account(user)? {
        Some(account) => Ok(Json(account::mode_of(&account)).into_response()),
        None => pass_through(state, &body).await,
    }
}

/// `openOrders`: a touched account has no order machinery yet (Phase 2
/// matching), so its unwind list is empty; untouched accounts pass through.
async fn open_orders(state: &ApiState, body: Bytes, user: Address) -> Result<Response> {
    match state.sim.account(user)? {
        Some(_) => Ok(Json(Vec::<crate::wire::Order>::new()).into_response()),
        None => pass_through(state, &body).await,
    }
}

/// `activeAssetData`: the sim's derived row, or the venue's verbatim.
async fn active_asset(
    state: &ApiState,
    body: Bytes,
    user: Address,
    coin: String,
) -> Result<Response> {
    match state.sim.account(user)? {
        Some(account) => Ok(Json(active_asset_data(state, user, &account, &coin)?).into_response()),
        None => pass_through(state, &body).await,
    }
}

/// `meta`: the cached metadata for the default dex.
fn meta(dex: &Option<String>, state: &ApiState) -> Result<Response> {
    expect_default_dex(dex)?;
    Ok(Json(state.feed.meta().ok_or(Error::Warming)?).into_response())
}

/// `metaAndAssetCtxs`: the cached metadata + per-market ctxs, default dex.
fn meta_and_ctxs(dex: &Option<String>, state: &ApiState) -> Result<Response> {
    expect_default_dex(dex)?;
    let (meta, ctxs) = state.feed.meta_and_ctxs().ok_or(Error::Warming)?;
    Ok(Json(MetaAndAssetCtxs(meta, ctxs)).into_response())
}

/// `l2Book`: the cached or scripted book for one coin.
fn l2_book(coin: &str, state: &ApiState) -> Result<Response> {
    let now = crate::now_ms();
    let book = state.feed.book(coin, now).ok_or_else(|| Error::NotFound {
        what: format!("no book for `{coin}` (not a configured market and no scripted book)"),
    })?;
    Ok(Json(book).into_response())
}

/// `marginTable`: one table out of the cached metadata by id.
fn margin_table(id: u32, state: &ApiState) -> Result<Response> {
    let meta = state.feed.meta().ok_or(Error::Warming)?;
    let table = meta
        .margin_tables
        .iter()
        .find(|(tid, _)| *tid == id)
        .map(|(_, t)| t.clone())
        .ok_or_else(|| Error::NotFound {
            what: format!("margin table {id}"),
        })?;
    Ok(Json(table).into_response())
}

/// `exchangeStatus`: the served clock.
fn exchange_status(state: &ApiState) -> Result<Response> {
    Ok(Json(ExchangeStatus {
        special_statuses: None,
        time: state.sim.served_time(crate::now_ms())?,
    })
    .into_response())
}

/// The served `clearinghouseState` for a touched account: accrual *first*,
/// then the read (the verifier caught the reverse order: the served figures
/// lagged one accrual window behind the stored state).
pub(crate) fn clearinghouse_of(state: &ApiState, address: Address) -> Result<Option<ClearinghouseState>> {
    let now = crate::now_ms();
    let served_time = state.sim.served_time(now)?;
    state.sim.evaluate(&state.feed)?;
    let Some(account) = state.sim.account(address)? else {
        return Ok(None);
    };
    let meta = state.feed.meta();
    let served = account::clearinghouse(&account, &state.feed, meta.as_ref(), served_time)?;
    if !account::identity_holds(&served) {
        state
            .sim
            .metrics
            .identity_violations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::error!(
            address = %address,
            "served clearinghouseState failed the identity re-check; daloc's client gate would refuse it"
        );
    }
    Ok(Some(served))
}

/// The venue's own answer for an untouched account (verbatim passthrough).
async fn pass_through(state: &ApiState, body: &Bytes) -> Result<Response> {
    // Budget accounting (plan §18.3): a pass-through is an outbound venue
    // read, so it spends the conservative heavy weight (most info types weigh
    // 20; clearinghouseState/l2Book/allMids weigh 2) — the poller backs off
    // when test reads and its own polls add up. Not gated: refusing a caller's
    // read would look like a venue outage.
    state.feed.budget.try_spend(crate::feed::budget::weight::HEAVY);
    let resp = state.api.raw_info(body.to_vec()).await?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = resp.bytes().await?;
    Ok((status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response())
}

fn empty_clearinghouse(time: u64) -> ClearinghouseState {
    let zero = MarginSummary {
        account_value: Decimal::ZERO,
        total_ntl_pos: Decimal::ZERO,
        total_raw_usd: Decimal::ZERO,
        total_margin_used: Decimal::ZERO,
    };
    ClearinghouseState {
        margin_summary: zero.clone(),
        cross_margin_summary: zero,
        cross_maintenance_margin_used: Decimal::ZERO,
        withdrawable: Decimal::ZERO,
        asset_positions: Vec::new(),
        time,
    }
}

fn expect_default_dex(dex: &Option<String>) -> Result<()> {
    if dex.as_deref().unwrap_or("").is_empty() {
        return Ok(());
    }
    Err(Error::BadRequest {
        reason: "the sim serves the first perp dex only (multi-dex is Phase 2)".into(),
    })
}

/// `activeAssetData`: the coin's mark, the user's leverage, and the sizes the
/// withdrawable margin supports at that leverage.
fn active_asset_data(
    state: &ApiState,
    user: Address,
    account: &account::Account,
    coin: &str,
) -> Result<ActiveAssetData> {
    let mark = state.feed.mark(coin).ok_or_else(|| Error::NoMarket {
        coin: coin.to_string(),
    })?;
    let meta = state.feed.meta();
    let view = account::view(account, &state.feed, meta.as_ref())?;
    let leverage = account
        .positions
        .get(coin)
        .map(|p| Leverage {
            kind: p.leverage,
            value: p.leverage_value,
            raw_usd: None,
        })
        // A no-position coin serves the venue's own default: the live sample
        // measured `leverage: {type: "cross", value: 20}` for an account with
        // no position (2026-10-23, §16.5's probe) — not a fabrication.
        .unwrap_or(Leverage {
            kind: LeverageType::Cross,
            value: 20,
            raw_usd: None,
        });
    let available = view.withdrawable.max(Decimal::ZERO);
    let max_sz = if mark.is_zero() {
        Decimal::ZERO
    } else {
        available * Decimal::from(leverage.value) / mark
    };
    Ok(ActiveAssetData {
        user,
        coin: coin.to_string(),
        leverage,
        max_trade_szs: [max_sz, max_sz],
        available_to_trade: [available, available],
        mark_px: mark,
    })
}

/// `userAbstraction` for a sim account (the control read shares it).
pub(crate) fn abstraction_of(account: &account::Account) -> UserAbstraction {
    account::mode_of(account)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compact order maps onto the lane's order: the asset index resolves
    /// through the live meta, the strings parse, and the tif mapping is
    /// exactly IOC/resting.
    #[test]
    fn compact_orders_map_onto_the_lane() {
        let mut meta: crate::wire::Meta = serde_json::from_str(r#"{"universe": []}"#).unwrap();
        meta.universe.push(crate::wire::AssetInfo {
            name: "ETH".into(),
            sz_decimals: 4,
            max_leverage: 20,
            margin_table_id: None,
            only_isolated: None,
            is_delisted: None,
        });
        let action: OrderAction =
            serde_json::from_str(r#"{"a":0,"b":true,"p":"101.5","s":"2","t":{"limit":{"tif":"Ioc"}}}"#)
                .unwrap();
        let order = action.to_order(Some(&meta)).unwrap();
        assert_eq!(order.coin, "ETH");
        assert!(order.is_buy);
        assert_eq!(order.size, "2".parse().unwrap());
        assert_eq!(order.limit_px, Some("101.5".parse().unwrap()));
        assert_eq!(order.tif, crate::sim::order::Tif::Ioc);
        // An absent `t` (the market shape) is an IOC; a Gtc/Alo order maps to
        // the resting shape the lane refuses by name; an unknown tif is
        // refused here.
        let market: OrderAction = serde_json::from_str(r#"{"a":0,"b":false,"s":"1"}"#).unwrap();
        assert_eq!(market.to_order(Some(&meta)).unwrap().tif, crate::sim::order::Tif::Ioc);
        let gtc: OrderAction = serde_json::from_str(r#"{"a":0,"b":false,"s":"1","t":{"limit":{"tif":"Gtc"}}}"#).unwrap();
        assert_eq!(gtc.to_order(Some(&meta)).unwrap().tif, crate::sim::order::Tif::Gtc);
        let bad: OrderAction = serde_json::from_str(r#"{"a":0,"b":false,"s":"1","t":{"limit":{"tif":"Fok"}}}"#).unwrap();
        assert!(format!("{}", bad.to_order(Some(&meta)).unwrap_err()).contains("unknown tif"));
        // An index past the meta is a named refusal.
        let oob: OrderAction = serde_json::from_str(r#"{"a":7,"b":false,"s":"1"}"#).unwrap();
        assert!(format!("{}", oob.to_order(Some(&meta)).unwrap_err()).contains("unknown asset index"));
    }

    /// Every non-order action lands in `Unmodeled` (the named refusal path),
    /// and the filled status serializes in the venue's own shape.
    #[test]
    fn unmodeled_actions_and_the_fill_shape() {
        let body: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"cancel","cancels":[]},"user":"0x00000000000000000000000000000000000000e1"}"#,
        )
        .unwrap();
        assert!(matches!(body.action, ExchangeAction::Unmodeled));
        let ok = serde_json::to_value(ExchangeOk {
            status: "ok",
            response: OrderResponseBody {
                r#type: "order",
                data: OrderData {
                    statuses: vec![OrderStatus::Filled(FilledInfo {
                        total_sz: "1.5".into(),
                        avg_px: "101".into(),
                        oid: 3,
                    })],
                },
            },
        })
        .unwrap();
        assert_eq!(ok["status"], "ok");
        assert_eq!(ok["response"]["type"], "order");
        assert_eq!(ok["response"]["data"]["statuses"][0]["filled"]["totalSz"], "1.5");
        assert_eq!(ok["response"]["data"]["statuses"][0]["filled"]["oid"], 3);
        // The err envelope keeps the venue's shape (HTTP 200 in the handler).
        let err = serde_json::to_value(ExchangeResponse {
            status: "err",
            response: "venue-sim: only the order action is modeled".into(),
        })
        .unwrap();
        assert_eq!(err["status"], "err");
    }
}
