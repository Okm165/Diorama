//! The control API (8091): hl_mock's state surface plus the scenario
//! extensions (plan §7.3).
//!
//! Every request type is the full documented shape (fields optional where the
//! route has a default), and every response reports what moved — the
//! scenario's own accounting.
use super::ApiState;
use crate::error::{Error, Result};
use crate::feed::PriceOverride;
use crate::sim::account::Account;
use crate::sim::seed::{control_view, AccountSeed};
use crate::sim::DealClass;
use crate::wire::{ClearinghouseState, Level, L2Book, SpotBalance, UserAbstraction};
use alloy_primitives::Address;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub async fn health(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    if state.sim.is_poisoned() {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, super::SIM_POISONED);
    }
    (axum::http::StatusCode::OK, "ok")
}

/// A control write that names a coin the market plane does not track is a
/// refusal, not a silent success (the verifier's find: `set_funding NOTACOIN`
/// answered 200).
fn require_tracked(feed: &crate::feed::MarketFeed, coin: &str) -> Result<()> {
    if feed.mark(coin).is_some() {
        return Ok(());
    }
    Err(Error::NotFound {
        what: format!("coin `{coin}` (not in the venue's tracked markets)"),
    })
}

/// `/metrics`: the sim's + feed's gauges/counters (the engine's shared admin
/// slice is not this service's vocabulary).
pub async fn metrics(State(state): State<Arc<ApiState>>) -> String {
    let mut out = state.sim.metrics.render();
    out.push_str(&state.feed.metrics.render());
    // Computed at scrape time from the overrides themselves: an owned-count
    // gauge maintained by hand would drift the first time a mutation site
    // forgot it (plan §7.8's `venue_feed_overrides`).
    mirror_devnet::metrics::push_metric(
        &mut out,
        mirror_devnet::metrics::GAUGE,
        "venue_feed_overrides",
        "scenario-owned feed values (crash prices + funding + books)",
        state.feed.owned_count(),
    );
    if let Some(bridge) = &state.bridge {
        out.push_str(&bridge.render());
    }
    out
}

// ---- deal ----------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct DealRequest {
    pub address: Address,
    pub(crate) class: DealClass,
    #[serde(default)]
    pub coin: Option<String>,
    pub amount: Decimal,
}

impl From<DealRequest> for crate::sim::Deal {
    fn from(req: DealRequest) -> Self {
        Self {
            address: req.address,
            class: req.class,
            coin: req.coin,
            amount: req.amount,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct DealResponse {
    pub address: Address,
    pub class: DealClass,
    /// The affected balance after the mint (perp wallet or spot token).
    pub balance_after: Decimal,
}

pub(crate) async fn deal(State(state): State<Arc<ApiState>>, Json(req): Json<DealRequest>) -> Result<Json<DealResponse>> {
    let after = state.sim.deal(&state.feed, &req.clone().into())?;
    tracing::info!(address = %req.address, class = ?req.class, amount = %req.amount, after = %after, "sim deal");
    Ok(Json(DealResponse {
        address: req.address,
        class: req.class,
        balance_after: after,
    }))
}

// ---- order ---------------------------------------------------------------

/// The `order` route's body: the account plus the order's own fields (the
/// matching lane fills against the real book; plan §4.5).
#[derive(Debug, Deserialize)]
pub(crate) struct OrderRequest {
    pub address: Address,
    #[serde(flatten)]
    pub order: crate::sim::order::Order,
}

#[derive(Debug, Serialize)]
pub(crate) struct OrderResponse {
    pub address: Address,
    #[serde(flatten)]
    pub fill: crate::sim::order::Fill,
}

pub(crate) async fn order(State(state): State<Arc<ApiState>>, Json(req): Json<OrderRequest>) -> Result<Json<OrderResponse>> {
    let fill = state.sim.order(&state.feed, req.address, &req.order)?;
    tracing::info!(
        address = %req.address,
        coin = %req.order.coin,
        is_buy = req.order.is_buy,
        sz = %fill.total_sz,
        avg_px = %fill.avg_px,
        consumed = fill.consumed.len(),
        "sim order filled"
    );
    Ok(Json(OrderResponse { address: req.address, fill }))
}

// ---- advance -------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct AdvanceRequest {
    /// Total simulated seconds to jump (default: `blocks` seconds).
    #[serde(default)]
    pub seconds: Option<u64>,
    /// Liquidation checks inside the jump (default 1).
    #[serde(default)]
    pub n: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AdvanceResponse {
    pub advanced_ms: u64,
    pub blocks: u64,
    pub liquidations: Vec<LiquidationReport>,
}

#[derive(Debug, Serialize)]
pub(crate) struct LiquidationReport {
    pub positions_closed: Vec<String>,
    pub account_value: Decimal,
    pub maintenance: Decimal,
}

pub(crate) async fn advance(
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
pub(crate) struct CrashRequest {
    pub coin: String,
    /// Percent below the current mark; absent = hand the coin back to live
    /// updates.
    #[serde(default)]
    pub pct: Option<Decimal>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CrashResponse {
    pub coin: String,
    pub owned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mark: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle: Option<Decimal>,
}

/// `stall`: hold the live market plane for `seconds` (0 clears) — the
/// staleness drill. Live writes drop (the staleness gauges keep counting, so
/// `readyz` and the order lane see the stall exactly as they would a real
/// one); the scripted plane is never gated.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct StallRequest {
    pub seconds: u64,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct StallResponse {
    /// The deadline (ms since the epoch; 0 = not stalled).
    pub stalled_until_ms: u64,
}

pub(crate) async fn stall(State(state): State<Arc<ApiState>>, Json(req): Json<StallRequest>) -> Result<Json<StallResponse>> {
    let until = state.feed.stall(req.seconds);
    tracing::info!(seconds = req.seconds, stalled_until_ms = until, "live market plane stall set");
    Ok(Json(StallResponse {
        stalled_until_ms: until,
    }))
}

pub(crate) async fn crash(State(state): State<Arc<ApiState>>, Json(req): Json<CrashRequest>) -> Result<Json<CrashResponse>> {
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
                return Err(Error::bad_request(format!("crash pct {pct} must be in (0, 100)"),));
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
pub(crate) struct SetFundingRequest {
    pub coin: String,
    /// The hourly rate to own; absent = hand back to live.
    #[serde(default)]
    pub rate: Option<Decimal>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SetFundingResponse {
    pub coin: String,
    pub owned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<Decimal>,
}

pub(crate) async fn set_funding(
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
            require_tracked(&state.feed, &req.coin)?;
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
pub(crate) struct SetBookRequest {
    pub coin: String,
    /// The scripted levels (`[[bids], [asks]]`, ≤20 per side); absent = hand
    /// back to live depth.
    #[serde(default)]
    pub levels: Option<[Vec<Level>; 2]>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SetBookResponse {
    pub coin: String,
    pub owned: bool,
    pub levels: usize,
}

pub(crate) async fn set_book(
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
            require_tracked(&state.feed, &req.coin)?;
            for side in &levels {
                if side.len() > 20 {
                    return Err(Error::bad_request(format!("l2Book serves ≤20 levels per side (got {})", side.len()),));
                }
            }
            let book = L2Book {
                coin: req.coin.clone(),
                time: crate::now_ms(),
                levels,
                spread: None,
            };
            let count = book.levels[0].len() + book.levels[1].len();
            state.feed.own_book(&book)?;
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
pub(crate) struct SetAccountRequest {
    pub address: Address,
    pub state: AccountSeed,
}

/// The control read shape: the served clearinghouse fields + the spot side +
/// the mode (hl_mock-compatible; the same numbers `/info` serves).
#[derive(Debug, Serialize)]
pub struct StateResponse {
    #[serde(flatten)]
    pub(crate) clearinghouse: ClearinghouseState,
    pub(crate) balances: Vec<SpotBalance>,
    pub(crate) abstraction: UserAbstraction,
}

impl StateResponse {
    fn of(state: &ApiState, address: Address) -> Result<Self> {
        let clearinghouse = super::info::clearinghouse_of(state, address)?
            .ok_or_else(|| Error::NotFound {
                what: format!("{address} is not a simulated account"),
            })?;
        let account = state
            .sim
            .account(address)?
            .ok_or_else(|| Error::NotFound {
                what: format!("{address} is not a simulated account"),
            })?;
        Ok(Self {
            clearinghouse,
            balances: crate::sim::account::spot_balances(&account),
            abstraction: super::info::abstraction_of(&account),
        })
    }
}

pub(crate) async fn set_account(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<SetAccountRequest>,
) -> Result<Json<StateResponse>> {
    state.sim.set_account(req.address, req.state)?;
    Ok(Json(StateResponse::of(&state, req.address)?))
}

/// hl_mock's `POST /control/state/{address}`: the body is the state patch; the
/// answer is the resolved **control view** (the fixture where the scenario owns
/// it, the sim's derivation where it does not). `/info` keeps serving the live
/// view — the two are deliberately different: the control read is "what the
/// scenario set" (daloc's `resolved_equity` assertions read it), the info read
/// is the venue's live report.
pub(crate) async fn post_state(
    State(state): State<Arc<ApiState>>,
    Path(address): Path<Address>,
    Json(seed): Json<AccountSeed>,
) -> Result<Json<AccountSeed>> {
    state.sim.set_account(address, seed)?;
    control_state(&state, address).map(Json)
}

/// hl_mock's `GET /control/state/{address}`: the resolved control view. An
/// address the sim has never seen answers hl_mock's default (mode `disabled`,
/// the empty account) — a 404 would break the read-back pattern the harness
/// uses (`venue_account()` asserts success).
pub(crate) async fn get_state(
    State(state): State<Arc<ApiState>>,
    Path(address): Path<Address>,
) -> Result<Json<AccountSeed>> {
    control_state(&state, address).map(Json)
}

/// The resolved control view for `address` — `/control/state`'s one builder.
/// Read-time evaluation runs first, like every account-scoped read (§14.8).
fn control_state(state: &ApiState, address: Address) -> Result<AccountSeed> {
    state.sim.evaluate(&state.feed)?;
    let Some(account) = state.sim.account(address)? else {
        let empty = Account {
            mode: UserAbstraction::Disabled,
            ..Default::default()
        };
        return control_view(None, &empty, &state.feed, state.feed.meta().as_ref());
    };
    let fixture = state.sim.control_fixture(address)?;
    control_view(
        fixture.as_ref(),
        &account,
        &state.feed,
        state.feed.meta().as_ref(),
    )
}

// ---- reset / epoch_new ---------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct ResetResponse {
    pub epoch: u64,
    pub accounts: usize,
}

pub(crate) async fn reset(State(state): State<Arc<ApiState>>) -> Result<Json<ResetResponse>> {
    let epoch = state.sim.reset()?;
    state.feed.clear_overrides()?;
    // The reset blanks every simulated account while the chains keep their
    // history: a CCTP deposit already relayed into the sim is now missing
    // here (the bridge's cursor sits past it), and any account the engine's
    // shadow plane mirrors is gone. The pairing is restored by restarting the
    // bridge and re-pinning V (the same remedy the T re-pin carries) - say so
    // loudly, because nothing downstream can detect the divergence by itself.
    tracing::warn!(
        epoch,
        "sim reset: every simulated account is blank; the chains keep their history. \
         Restart the bridge and re-pin V to restore the pairing (docs/OPERATIONS.md, \
         the reset runbook)"
    );
    Ok(Json(ResetResponse { epoch, accounts: 0 }))
}

#[derive(Debug, Serialize)]
pub(crate) struct EpochNewResponse {
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
pub(crate) async fn epoch_new(State(state): State<Arc<ApiState>>) -> Result<Json<EpochNewResponse>> {
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
    // The engine's own wire type (mirror_devnet::admin), not a local copy:
    // the channel is ours on both ends, so the compiler keeps them consistent.
    let engine: mirror_devnet::admin::EpochResponse =
        serde_json::from_str(&text).map_err(|source| Error::Decode {
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
