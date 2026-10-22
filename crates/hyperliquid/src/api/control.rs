//! The control API (8091): hl_mock's state surface plus the scenario
//! extensions (plan §7.3).
//!
//! Every request type is the full documented shape (fields optional where the
//! route has a default), and every response reports what moved — the
//! scenario's own accounting.
use super::ApiState;
use crate::error::{Error, Result};
use crate::feed::PriceOverride;
use crate::sim::account::{Account, AccountSeed};
use crate::sim::DealClass;
use crate::wire::{ClearinghouseState, Level, L2Book, SpotBalance, UserAbstraction};
use alloy_primitives::Address;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub async fn health() -> impl IntoResponse {
    (axum::http::StatusCode::OK, "ok")
}

/// `/metrics`: the sim's + feed's gauges/counters (the engine's shared admin
/// slice is not this service's vocabulary).
pub async fn metrics(State(state): State<Arc<ApiState>>) -> String {
    let mut out = state.sim.metrics.render();
    out.push_str(&state.feed.metrics.render());
    if let Some(bridge) = &state.bridge {
        out.push_str(&bridge.render());
    }
    out
}

// ---- deal ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DealRequest {
    pub address: Address,
    pub class: DealClass,
    #[serde(default)]
    pub coin: Option<String>,
    pub amount: Decimal,
}

#[derive(Debug, Serialize)]
pub struct DealResponse {
    pub address: Address,
    pub class: DealClass,
    /// The affected balance after the mint (perp wallet or spot token).
    pub balance_after: Decimal,
}

pub async fn deal(State(state): State<Arc<ApiState>>, Json(req): Json<DealRequest>) -> Result<Json<DealResponse>> {
    let after = state
        .sim
        .deal(&state.feed, req.address, req.class, req.coin.as_deref(), req.amount)?;
    tracing::info!(address = %req.address, class = ?req.class, amount = %req.amount, after = %after, "sim deal");
    Ok(Json(DealResponse {
        address: req.address,
        class: req.class,
        balance_after: after,
    }))
}

// ---- advance -------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AdvanceRequest {
    /// Total simulated seconds to jump (default: `blocks` seconds).
    #[serde(default)]
    pub seconds: Option<u64>,
    /// Liquidation checks inside the jump (default 1).
    #[serde(default)]
    pub n: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct AdvanceResponse {
    pub advanced_ms: u64,
    pub blocks: u64,
    pub liquidations: Vec<LiquidationReport>,
}

#[derive(Debug, Serialize)]
pub struct LiquidationReport {
    pub positions_closed: Vec<String>,
    pub account_value: Decimal,
    pub maintenance: Decimal,
}

pub async fn advance(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<AdvanceRequest>,
) -> Result<Json<AdvanceResponse>> {
    let blocks = req.n.unwrap_or(1).max(1);
    let seconds = req.seconds.unwrap_or(blocks);
    let outcome = state.sim.advance(&state.feed, seconds, blocks)?;
    Ok(Json(AdvanceResponse {
        advanced_ms: outcome.advanced_ms,
        blocks: outcome.blocks,
        liquidations: outcome
            .liquidations
            .into_iter()
            .map(|l| LiquidationReport {
                positions_closed: l.positions_closed,
                account_value: l.account_value,
                maintenance: l.maintenance,
            })
            .collect(),
    }))
}

// ---- crash / set_funding / set_book --------------------------------------

#[derive(Debug, Deserialize)]
pub struct CrashRequest {
    pub coin: String,
    /// Percent below the current mark; absent = hand the coin back to live
    /// updates.
    #[serde(default)]
    pub pct: Option<Decimal>,
}

#[derive(Debug, Serialize)]
pub struct CrashResponse {
    pub coin: String,
    pub owned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mark: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle: Option<Decimal>,
}

pub async fn crash(State(state): State<Arc<ApiState>>, Json(req): Json<CrashRequest>) -> Result<Json<CrashResponse>> {
    let feed = &state.feed;
    match req.pct {
        None => {
            feed.clear_price(&req.coin)?;
            Ok(Json(CrashResponse {
                coin: req.coin,
                owned: false,
                mark: None,
                oracle: None,
            }))
        }
        Some(pct) => {
            if pct <= Decimal::ZERO || pct >= Decimal::ONE_HUNDRED {
                return Err(Error::BadRequest {
                    reason: format!("crash pct {pct} must be in (0, 100)"),
                });
            }
            let mark = feed.mark(&req.coin).ok_or_else(|| Error::NoMarket {
                coin: req.coin.clone(),
            })?;
            let crashed = mark * (Decimal::ONE - pct / Decimal::ONE_HUNDRED);
            let price = PriceOverride {
                mark: crashed,
                oracle: crashed,
            };
            feed.own_price(&req.coin, price)?;
            tracing::warn!(coin = %req.coin, pct = %pct, mark = %crashed, "sim crash: mark and oracle owned");
            Ok(Json(CrashResponse {
                coin: req.coin,
                owned: true,
                mark: Some(crashed),
                oracle: Some(crashed),
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SetFundingRequest {
    pub coin: String,
    /// The hourly rate to own; absent = hand back to live.
    #[serde(default)]
    pub rate: Option<Decimal>,
}

#[derive(Debug, Serialize)]
pub struct SetFundingResponse {
    pub coin: String,
    pub owned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<Decimal>,
}

pub async fn set_funding(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<SetFundingRequest>,
) -> Result<Json<SetFundingResponse>> {
    match req.rate {
        None => {
            state.feed.clear_funding(&req.coin)?;
            Ok(Json(SetFundingResponse {
                coin: req.coin,
                owned: false,
                rate: None,
            }))
        }
        Some(rate) => {
            state.feed.own_funding(&req.coin, rate)?;
            Ok(Json(SetFundingResponse {
                coin: req.coin,
                owned: true,
                rate: Some(rate),
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SetBookRequest {
    pub coin: String,
    /// The scripted levels (`[[bids], [asks]]`, ≤20 per side); absent = hand
    /// back to live depth.
    #[serde(default)]
    pub levels: Option<[Vec<Level>; 2]>,
}

#[derive(Debug, Serialize)]
pub struct SetBookResponse {
    pub coin: String,
    pub owned: bool,
    pub levels: usize,
}

pub async fn set_book(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<SetBookRequest>,
) -> Result<Json<SetBookResponse>> {
    match req.levels {
        None => {
            state.feed.clear_book(&req.coin)?;
            Ok(Json(SetBookResponse {
                coin: req.coin,
                owned: false,
                levels: 0,
            }))
        }
        Some(levels) => {
            for side in &levels {
                if side.len() > 20 {
                    return Err(Error::BadRequest {
                        reason: format!("l2Book serves ≤20 levels per side (got {})", side.len()),
                    });
                }
            }
            let book = L2Book {
                coin: req.coin.clone(),
                time: crate::now_ms(),
                levels,
                spread: None,
            };
            let count = book.levels[0].len() + book.levels[1].len();
            state.feed.own_book(book)?;
            Ok(Json(SetBookResponse {
                coin: req.coin,
                owned: true,
                levels: count,
            }))
        }
    }
}

// ---- accounts ------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SetAccountRequest {
    pub address: Address,
    pub state: AccountSeed,
}

/// The control read shape: the served clearinghouse fields + the spot side +
/// the mode (hl_mock-compatible; the same numbers `/info` serves).
#[derive(Debug, Serialize)]
pub struct StateResponse {
    #[serde(flatten)]
    pub clearinghouse: ClearinghouseState,
    pub balances: Vec<SpotBalance>,
    pub abstraction: UserAbstraction,
}

impl StateResponse {
    fn of(state: &ApiState, account: &Account) -> Result<Self> {
        let clearinghouse = super::info::clearinghouse_of(state, Address::ZERO, account)?;
        Ok(Self {
            clearinghouse,
            balances: crate::sim::account::spot_balances(account),
            abstraction: super::info::abstraction_of(account),
        })
    }
}

pub async fn set_account(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<SetAccountRequest>,
) -> Result<Json<StateResponse>> {
    state.sim.set_account(req.address, req.state)?;
    let account = state
        .sim
        .account(req.address)?
        .ok_or_else(|| Error::NotFound { what: "the account just seeded".into() })?;
    Ok(Json(StateResponse::of(&state, &account)?))
}

/// hl_mock's `POST /control/state/{address}`: the body is the state seed.
pub async fn post_state(
    State(state): State<Arc<ApiState>>,
    Path(address): Path<Address>,
    Json(seed): Json<AccountSeed>,
) -> Result<Json<StateResponse>> {
    state.sim.set_account(address, seed)?;
    let account = state
        .sim
        .account(address)?
        .ok_or_else(|| Error::NotFound { what: "the account just seeded".into() })?;
    Ok(Json(StateResponse::of(&state, &account)?))
}

/// hl_mock's `GET /control/state/{address}` (404 when untouched — the read
/// that would otherwise go to the real venue).
pub async fn get_state(
    State(state): State<Arc<ApiState>>,
    Path(address): Path<Address>,
) -> Result<Json<StateResponse>> {
    let account = state
        .sim
        .account(address)?
        .ok_or_else(|| Error::NotFound {
            what: format!("{address} is not a simulated account (untouched reads go to the venue)"),
        })?;
    Ok(Json(StateResponse::of(&state, &account)?))
}

// ---- reset / epoch_new ---------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ResetResponse {
    pub epoch: u64,
    pub accounts: usize,
}

pub async fn reset(State(state): State<Arc<ApiState>>) -> Result<Json<ResetResponse>> {
    let epoch = state.sim.reset()?;
    state.feed.clear_overrides()?;
    Ok(Json(ResetResponse { epoch, accounts: 0 }))
}

#[derive(Debug, Serialize)]
pub struct EpochNewResponse {
    pub epoch: u64,
    /// The engine accepted the re-pin (`POST {engine_admin}/admin/epoch`).
    pub engine_repinned: bool,
    /// The fork source the engine re-pinned to (the engine's answer).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_source: Option<String>,
}

/// `epoch_new`: blank the scenario and re-pin V through the engine's
/// `/admin/epoch` (the driver's canary then runs the epoch rule). Refuses
/// when `engine_admin_url` is not configured — a reset that silently left V
/// on the old base would be the worst kind of success.
///
/// The engine call goes first: if the re-pin fails, nothing has changed yet
/// (a 502 means "retry the whole thing"), and a retry after a success is
/// idempotent on both sides (reset blanks an already-blank scenario; the
/// engine re-pins to the then-current latest).
pub async fn epoch_new(State(state): State<Arc<ApiState>>) -> Result<Json<EpochNewResponse>> {
    let Some(base) = &state.engine_admin else {
        return Err(Error::NotConfigured {
            what: "services.sim.engine_admin_url (needed to re-pin V)",
        });
    };
    let url = format!("{}/admin/epoch", base.trim_end_matches('/'));
    let resp = state.http.post(&url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(Error::ApiStatus {
            status: status.as_u16(),
            body: text,
        });
    }
    let epoch = state.sim.reset()?;
    state.feed.clear_overrides()?;
    #[derive(Deserialize)]
    struct EngineEpoch {
        reset: bool,
        source: String,
    }
    let engine: EngineEpoch = serde_json::from_str(&text).map_err(|source| Error::Decode {
        what: "engine epoch response",
        source,
    })?;
    tracing::warn!(epoch, source = %engine.source, "sim epoch_new: scenario blanked, V re-pinned");
    Ok(Json(EpochNewResponse {
        epoch,
        engine_repinned: engine.reset,
        engine_source: Some(engine.source),
    }))
}
