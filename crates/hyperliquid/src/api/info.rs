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

/// `POST /exchange` — the matching lane. Plan §18.4's verdict: no service
/// submits HL actions (the borrower signs client-side), so the lane is not
/// required for the suite and stays a research lane; positions come from the
/// control API instead. The refusal is *named* and uses HL's own envelope
/// (`{"status":"err","response":"…"}`, HTTP 200 — the shape their client
/// already parses), so a redesign probing the lane learns why it is absent,
/// not just that it is.
pub(crate) async fn exchange() -> Json<ExchangeResponse> {
    Json(ExchangeResponse {
        status: "err",
        response: "venue-sim: the matching lane is not implemented (plan §18.4: no service \
                   submits HL actions; positions come from the control API). \
                   See docs/INTEGRATION.md.",
    })
}

/// The `/exchange` envelope (the refusal above; complete for the err shape
/// their client reads).
#[derive(Debug, serde::Serialize)]
pub(crate) struct ExchangeResponse {
    pub status: &'static str,
    pub response: &'static str,
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

    /// The matching lane's refusal is HL's own envelope, never a 404 (plan
    /// §18.4): a client probing `/exchange` parses `status`/`response`.
    #[tokio::test]
    async fn exchange_refuses_in_hls_envelope() {
        let body = serde_json::to_value(exchange().await.0).expect("serialize");
        assert_eq!(body["status"], "err");
        let response = body["response"].as_str().expect("a response string");
        assert!(response.contains("matching lane"), "{response}");
        assert!(response.contains("18.4"), "{response}");
    }
}
