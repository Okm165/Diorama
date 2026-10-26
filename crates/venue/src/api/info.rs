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
/// The wire's order-type names (the venue's own vocabulary): one spelling.
const LIMIT: &str = "Limit";
const MARKET: &str = "Market";
const TRIGGER: &str = "Trigger";

/// The wire's order-type name: a trigger rests as `Trigger`.
fn order_type(has_trigger: bool) -> &'static str {
    if has_trigger { TRIGGER } else { LIMIT }
}

pub async fn healthz(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    if let Some(response) = super::sim_gate(&state) {
        return response;
    }
    (StatusCode::OK, "ok")
}

pub(crate) async fn readyz(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    if let Some(response) = super::sim_gate(&state) {
        return response;
    }
    if state.ready(crate::feed::MarketFeed::MAX_STALE_MS) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "market plane warming up")
    }
}

/// `POST /info`: parse the typed request, dispatch, or pass through.
pub async fn info(State(state): State<Arc<ApiState>>, body: Bytes) -> Result<Response> {
    // A malformed body is the caller's fault (400), not a server fault: the
    // verifier measured a 500 here, which reads as "the sim is broken".
    let req: InfoRequest = serde_json::from_slice(&body).map_err(|source| Error::bad_request(format!("invalid info request: {source}"),))?;
    match req {
        // hl_mock's contract for types it does not route.
        InfoRequest::Unrouted => Ok(Json("unrouted").into_response()),
        // ---- account-scoped: the sim's, or the venue's -------------------
        InfoRequest::ClearinghouseState { user, dex } => clearinghouse(&state, body, user, dex).await,
        InfoRequest::SpotClearinghouseState { user } => spot_state(&state, body, user).await,
        InfoRequest::UserAbstraction { user } => abstraction(&state, body, user).await,
        InfoRequest::OpenOrders { user, .. } => open_orders(&state, body, user).await,
        InfoRequest::FrontendOpenOrders { user, .. } => {
            frontend_open_orders(&state, body, user).await
        }
        InfoRequest::OrderStatus { user, oid } => order_status(&state, body, user, oid).await,
        InfoRequest::PerpsAtOpenInterestCap { .. } => {
            Ok(Json(state.feed.open_interest_caps()).into_response())
        }
        InfoRequest::ActiveAssetData { user, coin } => active_asset(&state, body, user, coin).await,
        // ---- market-scoped: the feed's -----------------------------------
        InfoRequest::Meta { dex } => meta(&dex, &state),
        InfoRequest::MetaAndAssetCtxs { dex } => meta_and_ctxs(&dex, &state),
        InfoRequest::PerpDexs => Ok(Json(vec![None::<crate::wire::PerpDex>]).into_response()),
        InfoRequest::L2Book {
            coin,
            n_sig_figs,
            mantissa,
        } => l2_book(&coin, n_sig_figs, mantissa, &state),
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
    simulated(state, &body, user, |account| {
        let meta = state.feed.meta();
        Ok(Json(crate::wire::SpotClearinghouseState {
            balances: account::spot_balances(account),
            token_to_available_after_maintenance: account::available_after_maintenance(
                account,
                &state.feed,
                meta.as_ref(),
            )?,
        })
        .into_response())
    })
    .await
}

/// `userAbstraction`: the account's parseable mode, or the venue's verbatim.
async fn abstraction(state: &ApiState, body: Bytes, user: Address) -> Result<Response> {
    simulated(state, &body, user, |account| {
        Ok(Json(account::mode_of(account)).into_response())
    })
    .await
}

/// One `openOrders` row (the docs' shape: coin, limitPx, oid, side, sz,
/// timestamp; a resting order the devnet's book holds).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenOrder {
    pub coin: String,
    pub limit_px: String,
    pub oid: u64,
    /// "A" (ask) or "B" (bid) — the venue's own letters.
    pub side: &'static str,
    pub sz: String,
    pub timestamp: u64,
}

/// One `frontendOpenOrders` row (the docs' fuller shape).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FrontendOpenOrder {
    pub coin: String,
    pub is_position_tpsl: bool,
    pub is_trigger: bool,
    pub limit_px: String,
    pub oid: u64,
    pub order_type: &'static str,
    pub orig_sz: String,
    pub reduce_only: bool,
    pub side: &'static str,
    pub sz: String,
    pub timestamp: u64,
    /// "N/A" for a plain order, "Triggered"/"Stop …" for triggers (the
    /// venue's own vocabulary; ours names the condition).
    pub trigger_condition: String,
    pub trigger_px: String,
}

fn side_letter(side: crate::sim::order::Side) -> &'static str {
    match side {
        crate::sim::order::Side::Buy => "B",
        crate::sim::order::Side::Sell => "A",
    }
}

fn trigger_condition(order: &crate::sim::order::RestingOrder) -> String {
    match &order.trigger {
        None => "N/A".into(),
        Some(t) => {
            let kind = match t.tpsl {
                crate::sim::order::Tpsl::Tp => "Take Profit",
                crate::sim::order::Tpsl::Sl => "Stop",
            };
            let dir = match order.side {
                crate::sim::order::Side::Buy => MARKET,
                crate::sim::order::Side::Sell => LIMIT,
            };
            format!("{kind} {dir} {}({})", if t.is_market { MARKET } else { LIMIT }, t.trigger_px)
        }
    }
}

/// `openOrders`: the devnet's resting book for a touched account (the venue's
/// own row shape); untouched accounts pass through.
async fn open_orders(state: &ApiState, body: Bytes, user: Address) -> Result<Response> {
    simulated(state, &body, user, |_| {
        let rows: Vec<OpenOrder> = state
            .sim
            .resting_orders(Some(user))?
            .into_iter()
            .map(|r| OpenOrder {
                coin: r.coin.clone(),
                limit_px: r.px.to_string(),
                oid: r.oid,
                side: side_letter(r.side),
                sz: r.sz.to_string(),
                timestamp: r.placed_ms,
            })
            .collect();
        Ok(Json(rows).into_response())
    })
    .await
}

/// `frontendOpenOrders`: the same rows with the frontend's extra fields.
async fn frontend_open_orders(state: &ApiState, body: Bytes, user: Address) -> Result<Response> {
    simulated(state, &body, user, |_| {
        let rows: Vec<FrontendOpenOrder> = state
            .sim
            .resting_orders(Some(user))?
            .into_iter()
            .map(|r| frontend_row(&r))
            .collect();
        Ok(Json(rows).into_response())
    })
    .await
}

/// One order's status row inside `orderStatus` (the docs' `order` object).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatusOrderInner {
    pub coin: String,
    pub side: &'static str,
    pub limit_px: String,
    pub sz: String,
    pub oid: u64,
    pub timestamp: u64,
    pub trigger_condition: String,
    pub is_trigger: bool,
    pub trigger_px: String,
    /// TP/SL children (the devnet never creates them; the shape is the docs').
    pub children: Vec<ChildOrder>,
    pub is_position_tpsl: bool,
    pub reduce_only: bool,
    pub order_type: &'static str,
    pub orig_sz: String,
    pub tif: &'static str,
    pub cloid: Option<alloy_primitives::B128>,
}

/// A TP/SL child (docs' `children` element shape).
#[derive(Debug, serde::Serialize)]
pub(crate) struct ChildOrder {
    pub order: StatusOrderInner,
    pub r#type: &'static str,
}

/// The `orderStatus` wrapper (docs: `{order, status, statusTimestamp}`).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatusOrder {
    pub order: StatusOrderInner,
    pub status: &'static str,
    pub status_timestamp: u64,
}

/// The `orderStatus` response: the docs' two shapes.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub(crate) enum OrderStatusResponse {
    /// Boxed: the order row dwarfs the unknown marker (clippy's large-variant
    /// rule; the enum rides every orderStatus response).
    Order { order: Box<StatusOrder> },
    UnknownOid,
}

fn tif_name(tif: crate::sim::order::Tif) -> &'static str {
    match tif {
        crate::sim::order::Tif::Ioc => "Ioc",
        crate::sim::order::Tif::Gtc => "Gtc",
        crate::sim::order::Tif::Alo => "Alo",
    }
}

/// One resting order's frontend-row fields: the single mapping the two served
/// shapes share (`frontendOpenOrders` uses it whole; `orderStatus` extends it
/// with its own fields).
fn frontend_row(r: &crate::sim::order::RestingOrder) -> FrontendOpenOrder {
    FrontendOpenOrder {
        coin: r.coin.clone(),
        is_position_tpsl: false,
        is_trigger: r.trigger.is_some(),
        limit_px: r.px.to_string(),
        oid: r.oid,
        order_type: order_type(r.trigger.is_some()),
        orig_sz: r.orig_sz.to_string(),
        reduce_only: matches!(r.reduce, crate::sim::order::Reduce::Only),
        side: side_letter(r.side),
        sz: r.sz.to_string(),
        timestamp: r.placed_ms,
        trigger_condition: trigger_condition(r),
        trigger_px: r
            .trigger
            .map(|t| t.trigger_px.to_string())
            .unwrap_or_else(|| "0.0".into()),
    }
}

/// The status row: the frontend row's fields (one mapping for both shapes)
/// plus the status-only extras.
fn status_from_frontend(
    front: FrontendOpenOrder,
    sz: String,
    tif: &'static str,
    cloid: Option<alloy_primitives::B128>,
) -> StatusOrderInner {
    StatusOrderInner {
        coin: front.coin,
        side: front.side,
        limit_px: front.limit_px,
        sz,
        oid: front.oid,
        timestamp: front.timestamp,
        trigger_condition: front.trigger_condition,
        is_trigger: front.is_trigger,
        trigger_px: front.trigger_px,
        children: Vec::new(),
        is_position_tpsl: front.is_position_tpsl,
        reduce_only: front.reduce_only,
        order_type: front.order_type,
        orig_sz: front.orig_sz,
        tif,
        cloid,
    }
}

fn status_inner(r: &crate::sim::order::RestingOrder, sz: String) -> StatusOrderInner {
    status_from_frontend(frontend_row(r), sz, tif_name(r.tif), r.cloid)
}

/// The terminal status label (the docs' orderStatus strings).
fn terminal_status_label(status: crate::sim::order::TerminalStatus) -> &'static str {
    match status {
        crate::sim::order::TerminalStatus::Filled => "filled",
        crate::sim::order::TerminalStatus::Canceled => "canceled",
        crate::sim::order::TerminalStatus::MarginCanceled => "marginCanceled",
    }
}

/// `orderStatus`: open from the book, terminal from the history, else the
/// docs' `unknownOid`.
async fn order_status(state: &ApiState, body: Bytes, user: Address, oid: crate::wire::OidOrCloid) -> Result<Response> {
    if state.sim.account(user)?.is_none() {
        return pass_through(state, &body).await;
    }
    let found = match oid {
        crate::wire::OidOrCloid::Oid(o) => match state.sim.resting_by_oid(user, o)? {
            Some(r) => Some((
                status_inner(&r, r.sz.to_string()),
                "open",
                r.placed_ms,
            )),
            None => state.sim.terminal_by_oid(user, o)?.map(|t| {
                (
                    status_inner(&t.order, t.order.sz.to_string()),
                    terminal_status_label(t.status),
                    t.status_ms,
                )
            }),
        },
        crate::wire::OidOrCloid::Cloid(c) => match state.sim.resting_by_cloid(user, c)? {
            Some(r) => Some((status_inner(&r, r.sz.to_string()), "open", r.placed_ms)),
            None => state.sim.terminal_by_cloid(user, c)?.map(|t| {
                (
                    status_inner(&t.order, t.order.sz.to_string()),
                    terminal_status_label(t.status),
                    t.status_ms,
                )
            }),
        },
    };
    Ok(match found {
        Some((order, status, status_timestamp)) => Json(OrderStatusResponse::Order {
            order: Box::new(StatusOrder {
                order,
                status,
                status_timestamp,
            }),
        })
        .into_response(),
        None => Json(OrderStatusResponse::UnknownOid).into_response(),
    })
}

/// `activeAssetData`: the sim's derived row, or the venue's verbatim.
async fn active_asset(
    state: &ApiState,
    body: Bytes,
    user: Address,
    coin: String,
) -> Result<Response> {
    simulated(state, &body, user, |account| {
        Ok(Json(active_asset_data(state, user, account, &coin)?).into_response())
    })
    .await
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

/// `l2Book`: the cached or scripted book for one coin, aggregated per the
/// request's parameters (docs: `nSigFigs ∈ {2,3,4,5}`, `mantissa ∈ {1,2,5}`
/// only with `nSigFigs = 5`). A parameter outside the documented set is a
/// named 400 — never silently ignored.
fn l2_book(
    coin: &str,
    n_sig_figs: Option<u8>,
    mantissa: Option<u8>,
    state: &ApiState,
) -> Result<Response> {
    book_params_ok(n_sig_figs, mantissa)?;
    let now = crate::now_ms();
    let Some(book) = state.feed.book(coin, now) else {
        // The venue's own absence shape (measured live: `l2Book` for an
        // unknown coin answers HTTP 200 `null`). A *configured* market with
        // no book yet is a different fault — the plane is warming (503).
        return if state.feed.markets.iter().any(|m| m == coin) {
            Err(Error::Warming)
        } else {
            Ok(Json(None::<crate::wire::L2Book>).into_response())
        };
    };
    Ok(Json(aggregate_book(book, n_sig_figs, mantissa.unwrap_or(1))).into_response())
}

/// The documented parameter set, refused by name otherwise.
fn book_params_ok(n_sig_figs: Option<u8>, mantissa: Option<u8>) -> Result<()> {
    if let Some(n) = n_sig_figs {
        if !(2..=5).contains(&n) {
            return Err(Error::bad_request(format!(
                "nSigFigs {n} is not in {{2,3,4,5}}"
            )));
        }
    }
    match (n_sig_figs, mantissa) {
        (None, Some(_)) => Err(Error::bad_request(
            "mantissa requires nSigFigs = 5 (docs)",
        )),
        (Some(5), Some(m)) if !matches!(m, 1 | 2 | 5) => Err(Error::bad_request(format!(
            "mantissa {m} is not in {{1,2,5}}"
        ))),
        (Some(n), Some(_)) if n != 5 => Err(Error::bad_request(
            "mantissa requires nSigFigs = 5 (docs)",
        )),
        _ => Ok(()),
    }
}

/// The venue's aggregation: prices round to `n` significant figures (the tick
/// is `mantissa × 10^(integer digits − n)`), levels at the same rounded price
/// merge (sizes and counts sum, saturating — a merge never panics), and each
/// side keeps its order (bids descending, asks ascending).
fn aggregate_book(book: crate::wire::L2Book, n_sig_figs: Option<u8>, mantissa: u8) -> crate::wire::L2Book {
    let Some(n) = n_sig_figs else {
        return book;
    };
    let mut out = book;
    for side in 0..2usize {
        let mut merged: std::collections::BTreeMap<Decimal, (Decimal, u64)> =
            std::collections::BTreeMap::new();
        for level in &out.levels[side] {
            let px = round_to_sig_figs(level.px, n, mantissa);
            let entry = merged.entry(px).or_insert((Decimal::ZERO, 0));
            entry.0 = entry.0.saturating_add(level.sz);
            entry.1 = entry.1.saturating_add(level.n);
        }
        let mut levels: Vec<crate::wire::Level> = merged
            .into_iter()
            .map(|(px, (sz, n))| crate::wire::Level { px, sz, n })
            .collect();
        if side == 0 {
            levels.reverse();
        }
        out.levels[side] = levels;
    }
    out
}

/// Round a price to `n` significant figures on a `mantissa`-scaled tick.
fn round_to_sig_figs(px: Decimal, n: u8, mantissa: u8) -> Decimal {
    if px <= Decimal::ZERO {
        return px;
    }
    let norm = px.normalize();
    let int_digits = norm.mantissa().unsigned_abs().ilog10() as i32 + 1 - norm.scale() as i32;
    let step_exp = int_digits - i32::from(n);
    // 10^step_exp by repeated multiply: exact for the magnitudes a book
    // carries; beyond them the price is served unrounded (a bounded,
    // documented degradation rather than a panic).
    let ten = Decimal::from(10u64);
    let mut step = Decimal::ONE;
    for _ in 0..step_exp.abs().min(28) {
        step = if step_exp >= 0 {
            step.saturating_mul(ten)
        } else {
            step.saturating_mul(Decimal::new(1, 1))
        };
    }
    let step = step.saturating_mul(Decimal::from(mantissa));
    if step.is_zero() {
        return px;
    }
    (px / step).round().saturating_mul(step)
}

/// The venue's implicit default table (id 1): the metadata never lists it,
/// but the real API serves it (measured live 2026-10-25:
/// `{"description":"","marginTiers":[{"lowerBound":"0.0","maxLeverage":1}]}`),
/// so the simulator must too.
pub(crate) fn default_margin_table() -> crate::wire::MarginTable {
    crate::wire::MarginTable {
        description: String::new(),
        margin_tiers: vec![crate::wire::MarginTier {
            lower_bound: Decimal::new(0, 1),
            max_leverage: 1,
        }],
    }
}

/// `marginTable`: one table out of the cached metadata by id, with the
/// venue's implicit default served for id 1.
fn margin_table(id: u32, state: &ApiState) -> Result<Response> {
    if id == 1 {
        return Ok(Json(default_margin_table()).into_response());
    }
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
/// The per-user read's guard: a simulated account computes the answer; an
/// unconfigured one is passed through to the live venue verbatim (the
/// overlay's discipline — the sim's own state, or the venue's answer, never a
/// guess). One shape for every per-user read.
async fn simulated(
    state: &ApiState,
    body: &Bytes,
    user: Address,
    compute: impl FnOnce(&crate::sim::account::Account) -> Result<Response>,
) -> Result<Response> {
    match state.sim.account(user)? {
        Some(account) => compute(&account),
        None => pass_through(state, body).await,
    }
}

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
    Ok((
        status,
        [(header::CONTENT_TYPE, mirror_anvil::JSON_CONTENT_TYPE)],
        bytes,
    )
        .into_response())
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
    Err(Error::bad_request(
        "the sim serves the first perp dex only (multi-dex is deferred; see docs/INTEGRATION.md)",
    ))
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
    // `availableToTrade` is the free margin (value minus used minus the open
    // orders' reservations) — the same figure the placement check reads — not
    // `withdrawable`, which the docs' transfer rule bounds.
    let available = state
        .sim
        .available_for(&state.feed, user, None, meta.as_ref())?
        .unwrap_or(Decimal::ZERO)
        .max(Decimal::ZERO);
    let max_sz = account::ratio_or(
        account::mul(available, Decimal::from(leverage.value), "max size")?,
        mark,
        Decimal::ZERO,
    );
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

    /// The book aggregation follows the docs: prices round to `n` significant
    /// figures, levels at the same rounded price merge, and out-of-set
    /// parameters refuse by name.
    #[test]
    fn l2_book_aggregation_follows_the_docs() {
        let level = |px: &str, sz: &str, n: u64| crate::wire::Level {
            px: px.parse().unwrap(),
            sz: sz.parse().unwrap(),
            n,
        };
        let book = crate::wire::L2Book {
            coin: "ETH".into(),
            time: 0,
            levels: [
                vec![level("100.4", "1", 2), level("100.9", "2", 1)],
                vec![level("106.0", "1", 1)],
            ],
            spread: None,
        };
        // n=2: 100.4 and 100.9 both round to 100 (merged: sz 3, n 3);
        // 106 rounds to 110 (half away from zero on the step of 10).
        let a = aggregate_book(book.clone(), Some(2), 1);
        assert_eq!(a.levels[0].len(), 1, "same rounded price merges");
        assert_eq!(a.levels[0][0].px, "100".parse().unwrap());
        assert_eq!(a.levels[0][0].sz, "3".parse().unwrap());
        assert_eq!(a.levels[0][0].n, 3);
        assert_eq!(a.levels[1][0].px, "110".parse().unwrap());
        // No params: verbatim, both sides in their own order.
        let c = aggregate_book(book, None, 1);
        assert_eq!(c.levels[0].len(), 2);
        assert_eq!(c.levels[0][0].px, "100.4".parse().unwrap());
        // The documented parameter set, refused by name otherwise.
        assert!(book_params_ok(Some(2), None).is_ok());
        assert!(book_params_ok(Some(5), Some(5)).is_ok());
        assert!(book_params_ok(Some(6), None).is_err());
        assert!(book_params_ok(None, Some(5)).is_err());
        assert!(book_params_ok(Some(4), Some(5)).is_err());
        assert!(book_params_ok(Some(5), Some(3)).is_err());
    }

    /// The implicit default table's served body, pinned against the real
    /// API's measured answer (live 2026-10-25) — the venue serves it for
    /// `marginTable` id 1 although the metadata never lists id 1.
    #[test]
    fn the_default_margin_table_matches_the_venue() {
        let body = serde_json::to_value(default_margin_table()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "description": "",
                "marginTiers": [{"lowerBound": "0.0", "maxLeverage": 1}]
            })
        );
    }
}
