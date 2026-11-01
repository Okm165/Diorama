//! The control API (8091): hl_mock's state surface plus the scenario
//! extensions.
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
use crate::wire::{ClearinghouseState, L2Book, Level, SpotBalance, UserAbstraction};
use alloy_primitives::Address;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub async fn health(State(state): State<Arc<ApiState>>) -> axum::response::Response {
    if let Err(refusal) = super::sim_gate(&state) {
        return refusal.into_response();
    }
    (axum::http::StatusCode::OK, "ok").into_response()
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
    // The endpoint pools' own health, one series per configured endpoint.
    // Without this a pool can be cooling every endpoint while the aggregate
    // feed counters still look plausible; the label names the URL that failed.
    render_pool(&mut out, "venue_rest", state.api.pool());
    if let Some(pool) = &state.ws_pool {
        render_pool(&mut out, "venue_ws", pool);
    }
    // Computed at scrape time from the overrides themselves: an owned-count
    // gauge maintained by hand would drift the first time a mutation site
    // forgot it (`venue_feed_overrides`).
    diorama_devnet::metrics::push_metric(
        &mut out,
        diorama_devnet::metrics::GAUGE,
        "venue_feed_overrides",
        "scenario-owned feed values (crash prices + funding + books)",
        state.feed.owned_count(),
    );
    if let Some(bridge) = &state.bridge {
        out.push_str(&bridge.render());
    }
    out
}

/// One pool's per-endpoint health. `healthy` is the pool's own admission
/// decision at scrape time; `failures` is the current exponential-backoff
/// level; `last_ok_ms` is the last success (0 = never).
fn render_pool(out: &mut String, prefix: &str, pool: &diorama_anvil::EndpointPool) {
    let now = diorama_anvil::now_ms();
    for (index, endpoint) in pool.snapshot().iter().enumerate() {
        let url = pool.url(index);
        let labels = &[("endpoint", url)][..];
        let healthy = u64::from(endpoint.down_until_ms <= now);
        let healthy_name = format!("{prefix}_endpoint_healthy");
        let failures_name = format!("{prefix}_endpoint_failures");
        let last_ok_name = format!("{prefix}_endpoint_last_ok_ms");
        diorama_devnet::metrics::push_gauge(
            out,
            &healthy_name,
            "endpoint is currently admitted by the pool",
            labels,
            healthy,
        );
        diorama_devnet::metrics::push_gauge(
            out,
            &failures_name,
            "current consecutive failure level (exponential backoff)",
            labels,
            u64::from(endpoint.failures),
        );
        diorama_devnet::metrics::push_gauge(
            out,
            &last_ok_name,
            "unix milliseconds of the last successful answer (0 = never)",
            labels,
            endpoint.last_ok_ms,
        );
    }
}

// ---- deal ----------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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

pub(crate) async fn deal(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<DealRequest>,
) -> Result<Json<DealResponse>> {
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
/// matching lane fills against the real book). The order's fields
/// are spelled out rather than flattened: a typo'd key must refuse by name —
/// this is the one verb where a silently ignored field would execute the
/// wrong order (the venue's own wire order stays tolerant on `/exchange`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrderRequest {
    pub address: Address,
    pub coin: String,
    pub is_buy: bool,
    pub size: Decimal,
    /// The limit price (absent = market: fill whatever the book gives).
    #[serde(default)]
    pub limit_px: Option<Decimal>,
    #[serde(default)]
    pub tif: crate::sim::order::Tif,
    #[serde(default)]
    pub reduce_only: bool,
    /// The client order id (echoed; it also addresses cancels/modifies and
    /// resting orders, as the venue's does).
    #[serde(default)]
    pub cloid: Option<alloy_primitives::B128>,
    /// A trigger order: armed instead of walked, fired by the tick when the
    /// mark crosses.
    #[serde(default)]
    pub trigger: Option<crate::sim::order::Trigger>,
}

impl From<OrderRequest> for crate::sim::order::Order {
    fn from(req: OrderRequest) -> Self {
        // Exhaustive: a new order field breaks this build rather than
        // silently dropping the control route's copy of it.
        Self {
            coin: req.coin,
            is_buy: req.is_buy,
            size: req.size,
            limit_px: req.limit_px,
            tif: req.tif,
            reduce_only: req.reduce_only,
            cloid: req.cloid,
            trigger: req.trigger,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct OrderResponse {
    pub address: Address,
    #[serde(flatten)]
    pub fill: crate::sim::order::Fill,
}

pub(crate) async fn order(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<OrderRequest>,
) -> Result<Json<OrderResponse>> {
    let address = req.address;
    let order: crate::sim::order::Order = req.into();
    let fill = state.sim.order(&state.feed, address, &order)?;
    // The fill's provenance, so every fill is self-explaining: the levels it
    // walked (the consumed sizes — the COW analogue) and the served book's
    // own source — `live@<age>` proves the depth was the live venue's,
    // `scripted+...` names a scenario-owned base with its live remainder.
    let mut levels: Vec<String> = fill
        .consumed
        .iter()
        .take(8)
        .map(|c| format!("{}x{}", c.price, c.size))
        .collect();
    let more = fill.consumed.len().saturating_sub(levels.len());
    if more > 0 {
        levels.push(format!("+{more}"));
    }
    let book = {
        let live = match state.feed.stale_ms(&order.coin, crate::now_ms()) {
            Some(age) => format!("live@{age}ms"),
            None => "live@warming".to_string(),
        };
        if state.feed.scripted_book(&order.coin) {
            format!("scripted+{live}")
        } else {
            live
        }
    };
    tracing::info!(
        address = %address,
        coin = %order.coin,
        is_buy = order.side().is_buy(),
        sz = %fill.total_sz,
        avg_px = %fill.avg_px,
        consumed = fill.consumed.len(),
        levels = %levels.join(" "),
        book = %book,
        "sim order filled"
    );
    Ok(Json(OrderResponse { address, fill }))
}

// ---- advance -------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    let blocks = req.n.unwrap_or(1);
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
#[serde(deny_unknown_fields)]
pub(crate) struct PriceMoveRequest {
    pub coin: String,
    /// The move's size in percent; absent = hand the coin back to live
    /// updates.
    #[serde(default)]
    pub pct: Option<Decimal>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PriceMoveResponse {
    pub coin: String,
    pub owned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mark: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle: Option<Decimal>,
}

/// Which way the scenario's price move goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PriceMove {
    /// Down: the crash primitive.
    Crash,
    /// Up: the squeeze (a short's adverse side — the crash's mirror).
    Rally,
}

impl PriceMove {
    /// The verb the messages carry.
    fn verb(self) -> &'static str {
        match self {
            PriceMove::Crash => "crash",
            PriceMove::Rally => "rally",
        }
    }

    /// The moved price: below the current mark for a crash, above for a
    /// rally.
    fn apply(self, mark: Decimal, pct: Decimal) -> Decimal {
        match self {
            PriceMove::Crash => mark * (Decimal::ONE - pct / Decimal::ONE_HUNDRED),
            PriceMove::Rally => mark * (Decimal::ONE + pct / Decimal::ONE_HUNDRED),
        }
    }
}

/// `stall`: hold the live market plane for `seconds` (0 clears) — the
/// staleness drill. Live writes drop (the staleness gauges keep counting, so
/// `readyz` and the order lane see the stall exactly as they would a real
/// one); the scripted plane is never gated.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StallRequest {
    pub seconds: u64,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct StallResponse {
    /// The deadline (ms since the epoch; 0 = not stalled).
    pub stalled_until_ms: u64,
}

pub(crate) async fn stall(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<StallRequest>,
) -> Result<Json<StallResponse>> {
    let until = state.feed.stall(req.seconds);
    tracing::info!(
        seconds = req.seconds,
        stalled_until_ms = until,
        "live market plane stall set"
    );
    Ok(Json(StallResponse {
        stalled_until_ms: until,
    }))
}

pub(crate) async fn crash(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<PriceMoveRequest>,
) -> Result<Json<PriceMoveResponse>> {
    move_price(&state.feed, req, PriceMove::Crash).map(Json)
}

/// `/control/rally` — the crash's mirror: the mark and oracle move *up* by
/// `pct` (a squeeze), so a short's adverse side — and a long's upside — is
/// exercisable too. Absent `pct` hands the coin back, the crash's own clear.
pub(crate) async fn rally(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<PriceMoveRequest>,
) -> Result<Json<PriceMoveResponse>> {
    move_price(&state.feed, req, PriceMove::Rally).map(Json)
}

/// The scenario's price move, one home for both verbs: own the coin's mark
/// and oracle at the moved price, or hand it back.
fn move_price(
    feed: &crate::feed::MarketFeed,
    req: PriceMoveRequest,
    dir: PriceMove,
) -> Result<PriceMoveResponse> {
    match req.pct {
        None => {
            feed.clear_price(&req.coin)?;
            Ok(PriceMoveResponse {
                coin: req.coin,
                owned: false,
                mark: None,
                oracle: None,
            })
        }
        Some(pct) => {
            if pct <= Decimal::ZERO || pct >= Decimal::ONE_HUNDRED {
                return Err(Error::bad_request(format!(
                    "{} pct {pct} must be in (0, 100)",
                    dir.verb()
                )));
            }
            let mark = feed.mark(&req.coin).ok_or_else(|| Error::NoMarket {
                coin: req.coin.clone(),
            })?;
            let moved = dir.apply(mark, pct);
            let price = PriceOverride {
                mark: moved,
                oracle: moved,
            };
            feed.own_price(&req.coin, price)?;
            // A price move owns the **whole perp market**, not only the
            // mark/oracle. The liquidation lane closes a position against the
            // book, so leaving the book at the old price let a 55% rally
            // liquidate a short at the pre-shock price: the account value was
            // unchanged and the product saw a vanished hedge with intact cash
            // (measured 2026-10-29, the `0xde2c` 55% rally). Shift every level
            // by the same mark ratio, preserving the book's own basis to the
            // mark; `basis-widen`/`basis-tighten` remain the book-only moves.
            if let Some(mut book) = feed.book(&req.coin, crate::now_ms()) {
                let ratio = moved / mark;
                for side in &mut book.levels {
                    for level in side {
                        level.px *= ratio;
                    }
                }
                book.time = crate::now_ms();
                feed.own_book(&book)?;
            }
            tracing::warn!(coin = %req.coin, pct = %pct, mark = %moved, move = dir.verb(), "sim price move: mark, oracle and book owned");
            Ok(PriceMoveResponse {
                coin: req.coin,
                owned: true,
                mark: Some(moved),
                oracle: Some(moved),
            })
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
            // The venue's published rate is clamped to ±4 %/h. A scenario
            // outside that band is not a market state the real venue can
            // produce; admitting it would let a stress test price a risk the
            // venue itself cannot reach.
            let cap = Decimal::new(4, 2);
            if rate.abs() > cap {
                return Err(Error::bad_request(format!(
                    "funding rate {rate} is outside the venue's ±4 %/h band"
                )));
            }
            state.feed.own_funding(&req.coin, rate)?;
            tracing::info!(coin = %req.coin, rate = %rate, "control: funding override");
            Ok(Json(SetFundingResponse {
                coin: req.coin,
                owned: true,
                rate: Some(rate),
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
                    return Err(Error::bad_request(format!(
                        "l2Book serves ≤20 levels per side (got {})",
                        side.len()
                    )));
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
#[serde(deny_unknown_fields)]
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
        let clearinghouse =
            super::info::clearinghouse_of(state, address)?.ok_or_else(|| Error::NotFound {
                what: format!("{address} is not a simulated account"),
            })?;
        let account = state.sim.account(address)?.ok_or_else(|| Error::NotFound {
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

#[derive(Debug, Serialize)]
pub(crate) struct ClearAccountResponse {
    pub address: String,
    /// Whether an account existed and was removed.
    pub removed: bool,
    /// True when the request was a `dry_run`: nothing was removed.
    pub dry_run: bool,
}

/// `POST /control/clear_account/{address}`: remove one account entirely —
/// fixture, balances and positions. The explicit recovery door for a control
/// patch that must not survive into the next measurement. Destructive, so it
/// requires `{"confirm":"clear_account"}`; a bare probe is refused.
pub(crate) async fn clear_account(
    State(state): State<Arc<ApiState>>,
    Path(address): Path<Address>,
    Json(request): Json<ConfirmRequest>,
) -> Result<Json<ClearAccountResponse>> {
    require_confirm(&request, "clear_account")?;
    if request.dry_run {
        return Ok(Json(ClearAccountResponse {
            address: format!("{address:#x}"),
            removed: false,
            dry_run: true,
        }));
    }
    let removed = state.sim.clear_account(address)?;
    Ok(Json(ClearAccountResponse {
        address: format!("{address:#x}"),
        removed,
        dry_run: false,
    }))
}

/// `POST /control/clear_fixture/{address}`: drop the control-fixture
/// provenance while keeping the account's balances and positions. Use this
/// after a **setup** patch whose state is the intended live state; a balance
/// patch is cleared with `clear_account` and re-funded.
pub(crate) async fn clear_fixture(
    State(state): State<Arc<ApiState>>,
    Path(address): Path<Address>,
    Json(request): Json<ConfirmRequest>,
) -> Result<Json<ClearAccountResponse>> {
    require_confirm(&request, "clear_fixture")?;
    if request.dry_run {
        return Ok(Json(ClearAccountResponse {
            address: format!("{address:#x}"),
            removed: false,
            dry_run: true,
        }));
    }
    let removed = state.sim.clear_control_fixture(address)?;
    Ok(Json(ClearAccountResponse {
        address: format!("{address:#x}"),
        removed,
        dry_run: false,
    }))
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
/// Read-time evaluation runs first, like every account-scoped read.
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

/// The explicit confirmation every destructive control route requires.
///
/// A route-existence probe is a POST, and a POST to a handler with no body
/// extractor is the action. The measured incident (2026-10-28 03:18:40Z): a
/// read-only sweep POSTed `{}` to every `/control/*` route, and `reset` +
/// `epoch_new` blanked every simulated account and re-pinned V. The control
/// plane must therefore refuse a request that did not name the action it
/// intends. `dry_run` answers the same question without mutating anything.
#[derive(Debug, Deserialize)]
pub(crate) struct ConfirmRequest {
    /// Must equal the route's action (`"reset"` or `"epoch_new"`).
    pub confirm: String,
    /// Report the current epoch without resetting or re-pinning.
    #[serde(default)]
    pub dry_run: bool,
}

fn require_confirm(request: &ConfirmRequest, action: &str) -> Result<()> {
    if request.confirm != action {
        return Err(Error::bad_request(format!(
            "refusing {action}: send {{\"confirm\":\"{action}\"}}; a bare POST must not mutate the scenario"
        )));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub(crate) struct ResetResponse {
    pub epoch: u64,
    pub accounts: usize,
    /// True when the request was a `dry_run`: nothing was blanked.
    pub dry_run: bool,
}

pub(crate) async fn reset(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<ConfirmRequest>,
) -> Result<Json<ResetResponse>> {
    require_confirm(&request, "reset")?;
    if request.dry_run {
        return Ok(Json(ResetResponse {
            epoch: state.sim.epoch()?,
            accounts: 0,
            dry_run: true,
        }));
    }
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
    Ok(Json(ResetResponse {
        epoch,
        accounts: 0,
        dry_run: false,
    }))
}

#[derive(Debug, Serialize)]
pub(crate) struct EpochNewResponse {
    pub epoch: u64,
    /// The engine accepted the re-pin (`POST {engine_admin}/admin/epoch`).
    pub engine_repinned: bool,
    /// The fork source the engine re-pinned to (the engine's answer).
    pub engine_source: String,
    /// True when the request was a `dry_run`: nothing was blanked or re-pinned.
    pub dry_run: bool,
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
pub(crate) async fn epoch_new(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<ConfirmRequest>,
) -> Result<Json<EpochNewResponse>> {
    require_confirm(&request, "epoch_new")?;
    if request.dry_run {
        return Ok(Json(EpochNewResponse {
            epoch: state.sim.epoch()?,
            engine_repinned: false,
            engine_source: "dry-run".into(),
            dry_run: true,
        }));
    }
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
    // The engine's own wire type (diorama_devnet::admin), not a local copy:
    // the channel is ours on both ends, so the compiler keeps them consistent.
    let engine: diorama_devnet::admin::EpochResponse =
        serde_json::from_str(&text).map_err(|source| Error::Decode {
            what: "engine epoch response",
            source,
        })?;
    tracing::warn!(epoch, source = %engine.source, "sim epoch_new: scenario blanked, V re-pinned");
    Ok(Json(EpochNewResponse {
        epoch,
        engine_repinned: engine.reset,
        engine_source: engine.source,
        dry_run: false,
    }))
}

#[cfg(test)]
mod confirm_tests {
    use super::*;

    /// The measured incident: a route probe POSTed `{}` to every control route
    /// and `reset`/`epoch_new` executed. A request that does not name the
    /// action must be refused before any state is touched.
    #[test]
    fn a_bare_probe_cannot_confirm_a_destructive_action() {
        let bare = ConfirmRequest {
            confirm: String::new(),
            dry_run: false,
        };
        assert!(require_confirm(&bare, "reset").is_err());
        assert!(require_confirm(&bare, "epoch_new").is_err());

        let wrong = ConfirmRequest {
            confirm: "reset".into(),
            dry_run: false,
        };
        assert!(require_confirm(&wrong, "epoch_new").is_err());

        let right = ConfirmRequest {
            confirm: "reset".into(),
            dry_run: false,
        };
        assert!(require_confirm(&right, "reset").is_ok());
    }
}
