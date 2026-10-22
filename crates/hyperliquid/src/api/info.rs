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

pub async fn readyz(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    if state.sim.is_poisoned() {
        return (StatusCode::SERVICE_UNAVAILABLE, "sim state poisoned");
    }
    if state.ready(READY_MAX_STALE_MS) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "market plane warming up")
    }
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
        InfoRequest::ClearinghouseState { user, dex } => {
            if !dex.as_deref().unwrap_or("").is_empty() {
                // The sim serves the first perp dex only (perpDexs = [null]).
                return Ok(Json(empty_clearinghouse(state.sim.served_time(crate::now_ms())?)).into_response());
            }
            match clearinghouse_of(&state, user)? {
                Some(state_body) => Ok(Json(state_body).into_response()),
                None => pass_through(&state, &body).await,
            }
        }
        InfoRequest::SpotClearinghouseState { user } => match state.sim.account(user)? {
            Some(account) => Ok(Json(crate::wire::SpotClearinghouseState {
                balances: account::spot_balances(&account),
            })
            .into_response()),
            None => pass_through(&state, &body).await,
        },
        InfoRequest::UserAbstraction { user } => match state.sim.account(user)? {
            Some(account) => Ok(Json(account::mode_of(&account)).into_response()),
            None => pass_through(&state, &body).await,
        },
        InfoRequest::OpenOrders { user, .. } => match state.sim.account(user)? {
            // No order machinery yet (Phase 2 matching): a touched account has
            // no open orders, and daloc's unwind list is empty.
            Some(_) => Ok(Json(Vec::<crate::wire::Order>::new()).into_response()),
            None => pass_through(&state, &body).await,
        },
        InfoRequest::ActiveAssetData { user, coin } => match state.sim.account(user)? {
            Some(account) => Ok(Json(active_asset_data(&state, user, &account, &coin)?).into_response()),
            None => pass_through(&state, &body).await,
        },
        // ---- market-scoped: the feed's -----------------------------------
        InfoRequest::Meta { dex } => {
            expect_default_dex(&dex)?;
            Ok(Json(state.feed.meta().ok_or(Error::Warming)?).into_response())
        }
        InfoRequest::MetaAndAssetCtxs { dex } => {
            expect_default_dex(&dex)?;
            let (meta, ctxs) = state.feed.meta_and_ctxs().ok_or(Error::Warming)?;
            Ok(Json(MetaAndAssetCtxs(meta, ctxs)).into_response())
        }
        InfoRequest::PerpDexs => Ok(Json(vec![None::<crate::wire::PerpDex>]).into_response()),
        InfoRequest::L2Book { coin, .. } => {
            let now = crate::now_ms();
            let book = state.feed.book(&coin, now).ok_or_else(|| Error::NotFound {
                what: format!("no book for `{coin}` (not a configured market and no scripted book)"),
            })?;
            Ok(Json(book).into_response())
        }
        InfoRequest::MarginTable { id } => {
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
        InfoRequest::ExchangeStatus => Ok(Json(ExchangeStatus {
            special_statuses: None,
            time: state.sim.served_time(crate::now_ms())?,
        })
        .into_response()),
        InfoRequest::SpotMeta => Ok(Json(state.feed.spot_meta().ok_or(Error::Warming)?).into_response()),
        InfoRequest::AllMids { .. } => Ok(Json(state.feed.all_mids()).into_response()),
    }
}

/// The served `clearinghouseState` for a touched account: accrual *first*,
/// then the read (the verifier caught the reverse order: the served figures
/// lagged one accrual window behind the stored state).
pub fn clearinghouse_of(state: &ApiState, address: Address) -> Result<Option<ClearinghouseState>> {
    let now = crate::now_ms();
    let served_time = state.sim.served_time(now)?;
    state.sim.accrue_all(&state.feed, now)?;
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
