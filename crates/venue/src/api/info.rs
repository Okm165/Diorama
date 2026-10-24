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
    if state.ready(crate::feed::MarketFeed::MAX_STALE_MS) {
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
    let req: ExchangeBody = serde_json::from_slice(&body).map_err(|source| Error::bad_request(format!("invalid exchange request: {source}"),))?;
    // The noop action (docs): no side effects, the venue's no-data success —
    // but it still consumes its nonce, as the venue's does.
    if matches!(req.action, ExchangeAction::Noop) {
        if let (Some(user), Some(nonce)) = (req.user, req.nonce) {
            if let Err(e) = state.sim.use_nonce(user, nonce, crate::now_ms()) {
                return Ok(err_envelope(e.to_string()).into_response());
            }
        }
        return Ok(noop_response(&req));
    }
    let user = match acting_user(&state, &req) {
        Ok(user) => user,
        Err(response) => return Ok(*response),
    };
    Ok(dispatch(&state, user, &body, &req))
}

/// The docs' pre-validation pass: `Some(reason)` refuses the whole payload
/// with one error (the venue duplicates it n times for their clients).
fn prevalidate(state: &Arc<ApiState>, orders: &[OrderAction]) -> Option<String> {
    if orders.is_empty() {
        return Some("venue-sim: an empty order batch is rejected (docs: pre-validation)".into());
    }
    let meta = state.feed.meta();
    for action in orders {
        let Ok(order) = action.to_order(meta.as_ref()) else {
            continue; // a per-order mapping error keeps its own status
        };
        if order.trigger.is_some() && !order.reduce_only {
            return Some(
                "venue-sim: TP/SL orders must be reduce-only (docs: pre-validation)".into(),
            );
        }
        if let Err(e) = crate::sim::order::validate_order(&state.feed, meta.as_ref(), &order) {
            let reason = e.to_string();
            if reason.contains("too far from oracle") {
                return Some(reason);
            }
        }
    }
    None
}

/// The asset-index actions (`updateLeverage`, `updateIsolatedMargin`): resolve
/// the coin, run the sim call, answer in the venue's envelope.
fn asset_action<F>(state: &Arc<ApiState>, asset: u32, run: F) -> Response
where
    F: FnOnce(&str) -> std::result::Result<(), crate::error::Error>,
{
    match coin_of(state.feed.meta().as_ref(), asset) {
        Some(coin) => match run(&coin) {
            Ok(()) => default_ok(),
            Err(e) => err_envelope(e.to_string()).into_response(),
        },
        None => err_envelope(format!("unknown asset index {asset}")).into_response(),
    }
}

/// The action dispatch (its own fn: the handler is the parse + the preamble).
fn dispatch(state: &Arc<ApiState>, user: Address, body: &Bytes, req: &ExchangeBody) -> Response {
    match &req.action {
        ExchangeAction::Order { .. } => order_action(state, user, body, req),
        ExchangeAction::Cancel { cancels, fast } => {
            let fast = *fast;
            flagged("f", fast, || cancel_action(state, user, cancels, fast.unwrap_or(false)))
        }
        ExchangeAction::CancelByCloid { cancels, fast } => {
            let fast = *fast;
            flagged("f", fast, || {
                cancel_by_cloid_action(state, user, cancels, fast.unwrap_or(false))
            })
        }
        ExchangeAction::Modify {
            oid,
            order,
            always_place,
        } => modify_action(state, user, oid, order, always_place.unwrap_or(false)),
        ExchangeAction::BatchModify {
            modifies,
            always_place,
        } => batch_modify(state, user, modifies, always_place.unwrap_or(false)),
        ExchangeAction::UpdateIsolatedMargin { asset, ntli, .. } => {
            let meta = state.feed.meta();
            asset_action(state, *asset, |coin| {
                state.sim.update_isolated_margin(user, coin, *ntli, meta.as_ref())
            })
        }
        ExchangeAction::UpdateLeverage {
            asset,
            is_cross,
            leverage,
        } => asset_action(state, *asset, |coin| {
            state.sim.set_leverage(user, coin, *is_cross, *leverage)
        }),
        ExchangeAction::ScheduleCancel { time } => match state.sim.schedule_cancel(user, *time) {
            Ok(_) => default_ok(),
            Err(e) => err_envelope(e.to_string()).into_response(),
        },
        ExchangeAction::Noop => noop_response(req),
        ExchangeAction::Unmodeled => err_envelope(unmodeled_reason(body)).into_response(),
    }
}

/// The acting account + the venue's nonce rule (plan §5: the 100 highest per
/// signer, window (T − 2 d, T + 1 d), no reuse). `Err` carries the ready
/// err-envelope response.
fn acting_user(
    state: &Arc<ApiState>,
    req: &ExchangeBody,
) -> std::result::Result<Address, Box<Response>> {
    let Some(user) = req.user else {
        return Err(Box::new(
            err_envelope(
                "venue-sim: an action must name the acting account in `user` — the devnet has \
                 no keys to derive it from the signature (plan §17). See docs/INTEGRATION.md."
                    .to_string(),
            )
            .into_response(),
        ));
    };
    if let Some(nonce) = req.nonce {
        if let Err(e) = state.sim.use_nonce(user, nonce, crate::now_ms()) {
            return Err(Box::new(err_envelope(e.to_string()).into_response()));
        }
    }
    Ok(user)
}

/// The docs' `isCross` defaults to true when omitted.
fn default_true() -> bool {
    true
}

/// The docs' "must be skipped if false" rule for the `f`/`a` flags: a present
/// `false` is rejected at the hash, so it is a named refusal here.
fn flag_omitted(name: &str, flag: Option<bool>) -> Result<(), String> {
    match flag {
        Some(false) => Err(format!(
            "venue-sim: the `{name}` flag must be omitted when false (docs: actions hashed \
             with {name}: false are rejected). See docs/INTEGRATION.md."
        )),
        _ => Ok(()),
    }
}

/// Run `action` only when the flag is omitted (`flag_omitted`); a present
/// flag's refusal is the ready err-envelope response. The one shape both
/// cancel arms share.
fn flagged(name: &str, flag: Option<bool>, action: impl FnOnce() -> Response) -> Response {
    match flag_omitted(name, flag) {
        Ok(()) => action(),
        Err(reason) => err_envelope(reason).into_response(),
    }
}

/// Modify every leg; the first refusal is the response (the venue answers a
/// batch all-or-nothing).
fn batch_modify(
    state: &Arc<ApiState>,
    user: Address,
    modifies: &[ModifyItem],
    always: bool,
) -> Response {
    for m in modifies {
        if let Err(e) = modify_one(state, user, &m.oid, &m.order, always) {
            return err_envelope(e.to_string()).into_response();
        }
    }
    default_ok()
}

/// The venue's no-data success, built once.
fn default_ok() -> Response {
    Json(ExchangeOk {
        status: "ok",
        response: DefaultBody { r#type: "default" },
    })
    .into_response()
}

/// The `order` action (its own fn so the dispatcher stays small).
fn order_action(state: &Arc<ApiState>, user: Address, body: &Bytes, req: &ExchangeBody) -> Response {
    let (orders, grouping) = match order_parts(body, &req.action) {
        Ok(parts) => parts,
        Err(reason) => return err_envelope(reason).into_response(),
    };
    tracing::info!(
        user = %user,
        nonce = ?req.nonce,
        expires_after = ?req.expires_after,
        signed = req.signature.as_ref().is_some_and(Signature::is_shaped),
        sig_v = req.signature.as_ref().map(|s| s.v),
        vault = ?req.vault_address,
        group = ?grouping,
        orders = orders.len(),
        "exchange order action accepted (the signature is accepted, never verified)"
    );
    let meta = state.feed.meta();
    // The docs' pre-validation class: one error for the whole payload
    // (empty batches, non-reduce-only TP/SL, an order too far from the
    // reference price) — every status carries it, as their clients expect.
    if let Some(reason) = prevalidate(state, orders) {
        let statuses = orders
            .iter()
            .map(|_| OrderStatus::Error(reason.clone()))
            .collect();
        return order_response(statuses);
    }
    let statuses = orders
        .iter()
        .map(|action| fill_status(state, user, meta.as_ref(), action))
        .collect();
    order_response(statuses)
}

/// The order action's `{"status":"ok","response":{"type":"order","data":…}}`
/// envelope (the shape their client parses).
fn order_response(statuses: Vec<OrderStatus>) -> Response {
    Json(ExchangeOk {
        status: "ok",
        response: OrderResponseBody {
            r#type: "order",
            data: OrderData { statuses },
        },
    })
    .into_response()
}

/// The unmodeled tag's refusal, naming the tag from the raw body.
fn unmodeled_reason(body: &Bytes) -> String {
    let tag = serde_json::from_slice::<ActionTag>(body)
        .map(|t| t.action.r#type)
        .unwrap_or_else(|_| "unknown".into());
    format!(
        "venue-sim: the `{tag}` action is not modeled (plan §4.5/§5, Phase 2). See \
         docs/INTEGRATION.md."
    )
}

/// The asset index → coin map (one home; `None` = unknown index).
fn coin_of(meta: Option<&crate::wire::Meta>, asset: u32) -> Option<String> {
    meta?.universe.get(asset as usize).map(|a| a.name.clone())
}

/// `cancel`: per-entry statuses in the venue's own shape.
fn cancel_action(
    state: &ApiState,
    user: Address,
    cancels: &[CancelAction],
    fast: bool,
) -> Response {
    if cancels.is_empty() {
        // The venue's batched actions answer an error for an empty payload
        // (the order action's pre-validation carries the same rule).
        return err_envelope("venue-sim: an empty cancel batch is rejected (docs: pre-validation)".to_string())
            .into_response();
    }
    let meta = state.feed.meta();
    let statuses = cancels
        .iter()
        .map(|c| match coin_of(meta.as_ref(), c.a) {
            Some(coin) => match state.sim.cancel(user, &coin, c.o, fast) {
                Ok(()) => CancelStatus::Success(SuccessStatus),
                Err(e) => CancelStatus::Error(Box::new(ErrorStatus { error: e.to_string() })),
            },
            None => CancelStatus::Error(Box::new(ErrorStatus {
                error: format!("unknown asset index {}", c.a),
            })),
        })
        .collect();
    Json(ExchangeOk {
        status: "ok",
        response: CancelResponseBody {
            r#type: "cancel",
            data: CancelData { statuses },
        },
    })
    .into_response()
}

/// `cancelByCloid`: the same, addressed by client id.
fn cancel_by_cloid_action(
    state: &ApiState,
    user: Address,
    cancels: &[CancelByCloidAction],
    fast: bool,
) -> Response {
    if cancels.is_empty() {
        return err_envelope("venue-sim: an empty cancel batch is rejected (docs: pre-validation)".to_string())
            .into_response();
    }
    let meta = state.feed.meta();
    let statuses = cancels
        .iter()
        .map(|c| match coin_of(meta.as_ref(), c.asset) {
            Some(coin) => match state.sim.cancel_by_cloid(user, &coin, c.cloid, fast) {
                Ok(()) => CancelStatus::Success(SuccessStatus),
                Err(e) => CancelStatus::Error(Box::new(ErrorStatus { error: e.to_string() })),
            },
            None => CancelStatus::Error(Box::new(ErrorStatus {
                error: format!("unknown asset index {}", c.asset),
            })),
        })
        .collect();
    Json(ExchangeOk {
        status: "ok",
        response: CancelResponseBody {
            r#type: "cancel",
            data: CancelData { statuses },
        },
    })
    .into_response()
}

/// `modify` (single): cancel + place, with the docs' `always_place` rules.
fn modify_action(
    state: &ApiState,
    user: Address,
    oid: &crate::wire::OidOrCloid,
    order: &OrderAction,
    always_place: bool,
) -> Response {
    match modify_one(state, user, oid, order, always_place) {
        Ok(()) => default_ok(),
        Err(e) => err_envelope(e.to_string()).into_response(),
    }
}

/// One modify: the cancel of the old order, then the placement. With
/// `always_place = false` the cancel must succeed, the new order must be
/// non-trigger, and an executable Gtc is overridden to Alo (docs); with
/// `always_place = true` the placement happens regardless.
fn modify_one(
    state: &ApiState,
    user: Address,
    oid: &crate::wire::OidOrCloid,
    order: &OrderAction,
    always_place: bool,
) -> Result<()> {
    let meta = state.feed.meta();
    let canceled = match oid {
        crate::wire::OidOrCloid::Oid(o) => {
            // The oid's coin is the order's own asset (the docs' `a`).
            let coin = coin_of(meta.as_ref(), order.a).ok_or_else(|| {
                Error::bad_request(format!("unknown asset index {}", order.a))
            })?;
            state.sim.cancel(user, &coin, *o, false)
        }
        crate::wire::OidOrCloid::Cloid(c) => {
            let coin = coin_of(meta.as_ref(), order.a).ok_or_else(|| {
                Error::bad_request(format!("unknown asset index {}", order.a))
            })?;
            state.sim.cancel_by_cloid(user, &coin, *c, false)
        }
    };
    if !always_place {
        canceled?;
    }
    let mut mapped = order.to_order(meta.as_ref())?;
    if !always_place {
        if mapped.trigger.is_some() {
            return Err(Error::bad_request(
                "always_place=false requires a non-trigger order (docs)",
            ));
        }
        if mapped.tif == crate::sim::order::Tif::Gtc {
            // The docs' override: a Gtc that could execute becomes post-only.
            mapped.tif = crate::sim::order::Tif::Alo;
        }
    }
    state.sim.place(&state.feed, user, &mapped)?;
    Ok(())
}

/// The noop's answer: the venue's no-data success envelope, logged in the
/// same shape every exchange call is.
fn noop_response(req: &ExchangeBody) -> Response {
    tracing::info!(
        user = ?req.user,
        nonce = ?req.nonce,
        signed = req.signature.as_ref().is_some_and(Signature::is_shaped),
        "exchange noop accepted (the signature is accepted, never verified)"
    );
    Json(ExchangeOk {
        status: "ok",
        response: DefaultBody { r#type: "default" },
    })
    .into_response()
}

/// The action's orders after the boundary checks, or the refusal reason the
/// err envelope carries: the unmodeled tag (read alone so the reason *names*
/// it) and the builder fee (fees are a research lane — a fill that silently
/// skipped the fee would lie).
fn order_parts<'a>(
    body: &Bytes,
    action: &'a ExchangeAction,
) -> Result<(&'a [OrderAction], Option<Grouping>), String> {
    let ExchangeAction::Order {
        orders,
        grouping,
        builder,
    } = action
    else {
        let tag = serde_json::from_slice::<ActionTag>(body)
            .map(|t| t.action.r#type)
            .unwrap_or_else(|_| "unknown".into());
        return Err(format!(
            "venue-sim: only the order and noop actions are modeled (plan §4.5/§5, Phase 2); \
             `{tag}` is not. See docs/INTEGRATION.md."
        ));
    };
    if let Some(builder) = builder {
        return Err(format!(
            "venue-sim: builder fees are not modeled (fees are a research lane, plan §16.3) \
             — the order would fill without charging {} its {} tenths of a basis point. See \
             docs/INTEGRATION.md.",
            builder.b, builder.f
        ));
    }
    Ok((orders, *grouping))
}

/// The venue's `err` envelope (HTTP 200 — the shape their client parses).
fn err_envelope(response: String) -> Json<ExchangeResponse> {
    Json(ExchangeResponse {
        status: "err",
        response,
    })
}

/// One order's status: the fill, or its refusal, in the venue's own shapes.
fn fill_status(
    state: &ApiState,
    user: Address,
    meta: Option<&crate::wire::Meta>,
    action: &OrderAction,
) -> OrderStatus {
    if let Some(cloid) = action.c {
        tracing::info!(
            cloid = %cloid,
            asset = action.a,
            "order carries a client id (echoed; nothing rests, so there is no id to key)"
        );
    }
    match action
        .to_order(meta)
        .and_then(|o| state.sim.place(&state.feed, user, &o))
    {
        Ok(placed) => match (placed.fill, placed.resting) {
            (Some(fill), _) => OrderStatus::Filled(FilledInfo {
                total_sz: fill.total_sz.to_string(),
                avg_px: fill.avg_px.to_string(),
                oid: fill.oid,
            }),
            (None, Some(oid)) => OrderStatus::Resting(RestingInfo { oid }),
            (None, None) => OrderStatus::Error(
                "order produced neither a fill nor a rest (an internal state)".into(),
            ),
        },
        Err(e) => OrderStatus::Error(e.to_string()),
    }
}

/// The `/exchange` body: the venue's compact action shape. The devnet reads
/// the acting account from `user` (the real venue derives it from the
/// signature) and accepts `signature` without checking it.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExchangeBody {
    pub action: ExchangeAction,
    /// The acting account. The real venue derives it from the signature; the
    /// devnet has no keys, so it reads the field (the documented analogue).
    /// Optional in the body so the SDK's `noop` (which carries no `user`)
    /// parses; an order without it refuses by name.
    #[serde(default)]
    pub user: Option<Address>,
    #[serde(default)]
    pub nonce: Option<u64>,
    #[serde(default)]
    pub signature: Option<Signature>,
    #[serde(default)]
    pub vault_address: Option<Address>,
    /// The optional staleness guard (docs: a timestamp in milliseconds after
    /// which the action is rejected). Accepted, never enforced — the devnet
    /// has no mempool to go stale in.
    #[serde(default)]
    pub expires_after: Option<u64>,
}

/// The action's tag, read alone so a refusal can *name* the unmodeled action
/// (`serde(other)` keeps the tag out of the typed enum by construction).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ActionTag {
    pub action: ActionTagInner,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ActionTagInner {
    pub r#type: String,
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

/// The action, tagged by `type`. `order` and `noop` are modeled; every other
/// tag the venue defines lands in `Unmodeled` and is refused by name.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum ExchangeAction {
    /// The no-operation action (docs): signature-authentication testing with
    /// no side effects. Answers the venue's no-data success envelope.
    Noop,
    Order {
        orders: Vec<OrderAction>,
        /// The batch's grouping (docs: `"na" | "normalTpsl" | "positionTpsl"`).
        #[serde(default)]
        grouping: Option<Grouping>,
        /// The optional builder fee (docs: `{"b": address, "f": tenths of a
        /// basis point}`): refused by name — fees are a research lane, and a
        /// fill that silently skips the fee would lie.
        #[serde(default)]
        builder: Option<Builder>,
    },
    /// `{"type":"cancel","cancels":[{"a":asset,"o":oid}]}` (docs); the `f`
    /// fast flag must be omitted when false, so a present `false` refuses.
    Cancel {
        cancels: Vec<CancelAction>,
        #[serde(default, rename = "f")]
        fast: Option<bool>,
    },
    /// `{"type":"cancelByCloid","cancels":[{"asset":N,"cloid":"0x…"}]}`.
    CancelByCloid {
        cancels: Vec<CancelByCloidAction>,
        #[serde(default, rename = "f")]
        fast: Option<bool>,
    },
    /// `{"type":"modify","oid":N|cloid,"order":{…},"a":always_place}`.
    Modify {
        oid: crate::wire::OidOrCloid,
        order: OrderAction,
        #[serde(default, rename = "a")]
        always_place: Option<bool>,
    },
    /// `{"type":"batchModify","modifies":[…],"a":always_place}`.
    BatchModify {
        modifies: Vec<ModifyItem>,
        #[serde(default, rename = "a")]
        always_place: Option<bool>,
    },
    /// `{"type":"scheduleCancel","time":ms}` (the dead-man's switch).
    ScheduleCancel {
        #[serde(default)]
        time: Option<u64>,
    },
    /// `{"type":"updateIsolatedMargin","asset":N,"isBuy":bool,"ntli":int}`
    /// (docs: 6-decimals USDC, signed).
    UpdateIsolatedMargin {
        asset: u32,
        /// The docs' payload carries the position side; the sim resolves the
        /// position by coin (one per coin), so it is informational.
        #[serde(default, rename = "isBuy")]
        _is_buy: bool,
        ntli: i64,
    },
    /// `{"type":"updateLeverage","asset":N,"isCross":bool,"leverage":N}`.
    UpdateLeverage {
        asset: u32,
        #[serde(default = "default_true")]
        is_cross: bool,
        leverage: u32,
    },
    #[serde(other)]
    Unmodeled,
}

/// One `cancel` entry (docs: `a` asset, `o` oid).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct CancelAction {
    pub a: u32,
    pub o: u64,
}

/// One `cancelByCloid` entry (docs: `asset`, `cloid`).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct CancelByCloidAction {
    pub asset: u32,
    pub cloid: alloy_primitives::B128,
}

/// One `batchModify` entry (docs).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ModifyItem {
    pub oid: crate::wire::OidOrCloid,
    pub order: OrderAction,
}

/// One compact order (`a` asset index, `b` is_buy, `p` limit price, `s` size,
/// `r` reduce-only, `t` type/time-in-force, `c` cloid).
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
    /// The client order id (docs: an optional 128-bit hex string). Accepted
    /// and echoed in the trace; nothing rests, so there is no id to match on.
    #[serde(default)]
    pub c: Option<alloy_primitives::B128>,
}

/// The order type: the venue's own shapes (docs). Only IOC limit fills — a
/// Gtc/Alo remainder cannot rest and a trigger order has no trigger engine in
/// the devnet (both refused by name).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OrderType {
    Limit {
        tif: String,
    },
    #[serde(rename_all = "camelCase")]
    Trigger {
        is_market: bool,
        trigger_px: String,
        tpsl: String,
    },
}

/// The batch grouping (docs: `"na" | "normalTpsl" | "positionTpsl"`).
#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub(crate) enum Grouping {
    #[serde(rename = "na")]
    Na,
    #[serde(rename = "normalTpsl")]
    NormalTpsl,
    #[serde(rename = "positionTpsl")]
    PositionTpsl,
}

/// The optional builder fee (docs: `{"b": address, "f": tenths of a basis
/// point}`).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct Builder {
    pub b: Address,
    pub f: u64,
}

impl OrderAction {
    /// The compact action → the lane's order: the asset index resolves
    /// through the live meta (unknown = a named refusal), size and limit are
    /// decimal strings, and only IOC fills.
    fn to_order(&self, meta: Option<&crate::wire::Meta>) -> Result<crate::sim::order::Order> {
        let coin = meta
            .and_then(|m| m.universe.get(self.a as usize))
            .map(|a| a.name.clone())
            .ok_or_else(|| Error::bad_request(format!(
                    "unknown asset index {} (the live meta has not landed, or is shorter)",
                    self.a
                ),))?;
        let size: Decimal = self.s.parse().map_err(|_| Error::bad_request(format!("size `{}` is not a decimal", self.s),))?;
        let limit_px = match &self.p {
            Some(p) => Some(p.parse().map_err(|_| Error::bad_request(format!("limit price `{p}` is not a decimal"),))?),
            None => None,
        };
        let (tif, trigger) = self.type_of()?;
        Ok(crate::sim::order::Order {
            coin,
            is_buy: self.b,
            size,
            limit_px,
            tif,
            reduce_only: self.r,
            cloid: self.c,
            trigger,
        })
    }

    /// The order type: the venue's own shapes (`t.limit.tif` or `t.trigger`).
    fn type_of(&self) -> Result<(crate::sim::order::Tif, Option<crate::sim::order::Trigger>)> {
        let mut trigger = None;
        let tif = match &self.t {
            None => crate::sim::order::Tif::Ioc,
            Some(OrderType::Trigger {
                is_market,
                trigger_px,
                tpsl,
            }) => {
                let px: Decimal = trigger_px.parse().map_err(|_| {
                    Error::bad_request(format!("trigger price `{trigger_px}` is not a decimal"))
                })?;
                let tpsl = match tpsl.as_str() {
                    "tp" => crate::sim::order::Tpsl::Tp,
                    "sl" => crate::sim::order::Tpsl::Sl,
                    other => {
                        return Err(Error::bad_request(format!(
                            "unknown tpsl `{other}` (docs: \"tp\" | \"sl\")"
                        )))
                    }
                };
                trigger = Some(crate::sim::order::Trigger {
                    is_market: *is_market,
                    trigger_px: px,
                    tpsl,
                });
                // A trigger does not walk now; its tif is irrelevant (the
                // execution uses the order's limit price, or a market IOC).
                crate::sim::order::Tif::Ioc
            }
            Some(OrderType::Limit { tif }) => {
                crate::sim::order::Tif::from_wire(tif).ok_or_else(|| Error::bad_request(format!("unknown tif `{tif}`"),))?
            }
        };
        Ok((tif, trigger))
    }
}

/// The ok body for the `order` action (the venue's own shape; the per-order
/// results inside).
#[derive(Debug, serde::Serialize)]
pub(crate) struct OrderResponseBody {
    pub r#type: &'static str,
    pub data: OrderData,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct OrderData {
    pub statuses: Vec<OrderStatus>,
}

/// One per-order status in the venue's own shape: `{"filled": {…}}`, the
/// `resting` shape, or `{"error": "…"}`.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OrderStatus {
    Filled(FilledInfo),
    /// The venue's third status: the order rests on the book.
    Resting(RestingInfo),
    Error(String),
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RestingInfo {
    pub oid: u64,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FilledInfo {
    pub total_sz: String,
    pub avg_px: String,
    pub oid: u64,
}

/// The ok envelope (the venue's own shape; the body varies by action).
#[derive(Debug, serde::Serialize)]
pub(crate) struct ExchangeOk<T> {
    pub status: &'static str,
    pub response: T,
}

/// The venue's no-data success body (`noop` and friends: `{"type":"default"}`).
#[derive(Debug, serde::Serialize)]
pub(crate) struct DefaultBody {
    pub r#type: &'static str,
}

/// One cancel's status: the docs' literal `"success"` string or an error
/// object (the venue's own two shapes).
#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum CancelStatus {
    Success(SuccessStatus),
    /// Boxed: the error string is far larger than the success marker.
    Error(Box<ErrorStatus>),
}

/// The docs' success literal.
#[derive(Debug)]
pub(crate) struct SuccessStatus;

impl serde::Serialize for SuccessStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str("success")
    }
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct ErrorStatus {
    pub error: String,
}

/// The cancel ok body (docs: `{"type":"cancel","data":{"statuses":[…]}}`).
#[derive(Debug, serde::Serialize)]
pub(crate) struct CancelResponseBody {
    pub r#type: &'static str,
    pub data: CancelData,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct CancelData {
    pub statuses: Vec<CancelStatus>,
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

fn side_letter(is_buy: bool) -> &'static str {
    if is_buy {
        "B"
    } else {
        "A"
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
            let dir = if order.is_buy { "Market" } else { "Limit" };
            format!("{kind} {dir} {}({})", if t.is_market { "Market" } else { "Limit" }, t.trigger_px)
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
                side: side_letter(r.is_buy),
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
            .map(|r| FrontendOpenOrder {
                coin: r.coin.clone(),
                is_position_tpsl: false,
                is_trigger: r.trigger.is_some(),
                limit_px: r.px.to_string(),
                oid: r.oid,
                order_type: if r.trigger.is_some() {
                    "Trigger"
                } else {
                    "Limit"
                },
                orig_sz: r.orig_sz.to_string(),
                reduce_only: r.reduce_only,
                side: side_letter(r.is_buy),
                sz: r.sz.to_string(),
                timestamp: r.placed_ms,
                trigger_condition: trigger_condition(&r),
                trigger_px: r
                    .trigger
                    .map(|t| t.trigger_px.to_string())
                    .unwrap_or_else(|| "0.0".into()),
            })
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

fn status_inner(r: &crate::sim::order::RestingOrder, sz: String) -> StatusOrderInner {
    StatusOrderInner {
        coin: r.coin.clone(),
        side: side_letter(r.is_buy),
        limit_px: r.px.to_string(),
        sz,
        oid: r.oid,
        timestamp: r.placed_ms,
        trigger_condition: trigger_condition(r),
        is_trigger: r.trigger.is_some(),
        trigger_px: r
            .trigger
            .map(|t| t.trigger_px.to_string())
            .unwrap_or_else(|| "0.0".into()),
        children: Vec::new(),
        is_position_tpsl: false,
        reduce_only: r.reduce_only,
        order_type: if r.trigger.is_some() {
            "Trigger"
        } else {
            "Limit"
        },
        orig_sz: r.orig_sz.to_string(),
        tif: tif_name(r.tif),
        cloid: r.cloid,
    }
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
    Err(Error::bad_request("the sim serves the first perp dex only (multi-dex is Phase 2)",))
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

    /// The compact order maps onto the lane's order: the asset index resolves
    /// through the live meta, the strings parse, and the tif mapping is
    /// exactly IOC/resting.
    /// A one-asset meta (ETH at index 0, szDecimals 4).
    fn eth_meta() -> crate::wire::Meta {
        let mut meta: crate::wire::Meta = serde_json::from_str(r#"{"universe": []}"#).unwrap();
        meta.universe.push(crate::wire::AssetInfo {
            name: "ETH".into(),
            sz_decimals: 4,
            max_leverage: 20,
            margin_table_id: None,
            only_isolated: None,
            is_delisted: None,
        });
        meta
    }

    #[test]
    fn compact_orders_map_onto_the_lane() {
        let meta = eth_meta();
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

    /// The complete wire shapes parse (cloid as alloy's `B128`, the trigger
    /// order, `expiresAfter`, the builder) and the boundary refusals name
    /// what they refused.
    #[test]
    fn complete_shapes_and_named_refusals() {
        // The unmodeled tag is read alone and named in the reason.
        let body = Bytes::from(
            r#"{"action":{"type":"cancel","cancels":[]},"user":"0x00000000000000000000000000000000000000e1"}"#,
        );
        let req: ExchangeBody = serde_json::from_slice(&body).unwrap();
        let err = order_parts(&body, &req.action).unwrap_err();
        assert!(err.contains("`cancel`"), "{err}");
        // A builder fee is refused with its numbers.
        let body = Bytes::from(
            r#"{"action":{"type":"order","orders":[],"builder":{"b":"0x00000000000000000000000000000000000000e1","f":10}},"user":"0x00000000000000000000000000000000000000e1"}"#,
        );
        let req: ExchangeBody = serde_json::from_slice(&body).unwrap();
        let err = order_parts(&body, &req.action).unwrap_err();
        assert!(err.contains("builder fees"), "{err}");
        // A trigger order maps into its typed shape (armed, not walked).
        let action: OrderAction = serde_json::from_str(
            r#"{"a":0,"b":true,"s":"1","t":{"trigger":{"isMarket":true,"triggerPx":"100","tpsl":"sl"}}}"#,
        )
        .unwrap();
        let order = action.to_order(Some(&eth_meta())).unwrap();
        let trigger = order.trigger.expect("a trigger order carries its trigger");
        assert!(trigger.is_market);
        assert_eq!(trigger.trigger_px, "100".parse().unwrap());
        assert_eq!(trigger.tpsl, crate::sim::order::Tpsl::Sl);
        // The cloid parses as its own type; expiresAfter rides the body.
        let action: OrderAction = serde_json::from_str(
            r#"{"a":0,"b":true,"s":"1","c":"0x1234567890abcdef1234567890abcdef"}"#,
        )
        .unwrap();
        assert!(action.c.is_some(), "the cloid parsed as B128");
        let req: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"order","orders":[]},"user":"0x00000000000000000000000000000000000000e1","expiresAfter":1792750000000}"#,
        )
        .unwrap();
        assert_eq!(req.expires_after, Some(1_792_750_000_000));
    }

    /// `noop` parses without a `user` (the SDK's shape) and answers the
    /// venue's no-data success envelope; an order without one refuses by name.
    #[test]
    fn noop_answers_the_venues_default_envelope() {
        let body: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"noop"},"nonce":1705234567890,"signature":{"r":"0x01","s":"0x02","v":27}}"#,
        )
        .unwrap();
        assert!(matches!(body.action, ExchangeAction::Noop));
        assert!(body.user.is_none(), "the SDK's noop carries no user");
        let out = serde_json::to_value(ExchangeOk {
            status: "ok",
            response: DefaultBody { r#type: "default" },
        })
        .unwrap();
        assert_eq!(out, serde_json::json!({"status": "ok", "response": {"type": "default"}}));
        // An order without `user`: parsed (the field is optional) but refused
        // by name on the way in — the devnet cannot derive it.
        let order: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"order","orders":[]},"nonce":1705234567890}"#,
        )
        .unwrap();
        assert!(order.user.is_none());
    }

    /// Every other action tag lands in `Unmodeled` (the named refusal path),
    /// and the filled status serializes in the venue's own shape.
    #[test]
    fn unmodeled_actions_and_the_fill_shape() {
        // A real venue action the devnet does not model (twapOrder) lands in
        // the named refusal; cancel is modeled now (its own tests cover it).
        let body: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"twapOrder","twap":{"a":0}},"user":"0x00000000000000000000000000000000000000e1"}"#,
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
