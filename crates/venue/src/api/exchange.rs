//! The `/exchange` surface: the venue's action handlers (orders, cancels,
//! modifies, leverage and isolated-margin updates, scheduleCancel, noop), the
//! dispatch, and the boundary refusals. The reads live in `info.rs`; the wire
//! shapes in `wire.rs`.
use super::ApiState;
use crate::error::{Error, Result};
use crate::wire::{
    ActionTag, CancelAction, CancelByCloidAction, CancelData, CancelResponseBody, CancelStatus,
    DefaultBody, ErrorStatus, ExchangeAction, ExchangeBody, ExchangeOk, ExchangeResponse,
    FilledInfo, Grouping, ModifyItem, OrderAction, OrderData, OrderResponseBody, OrderStatus,
    RestingInfo, Signature, SuccessStatus,
};
use alloy_primitives::Address;
use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

/// `POST /exchange` — the venue's action surface: orders (market, resting,
/// post-only, triggers), cancels (by oid and cloid, fast flagged), modifies
/// (single and batch), leverage and isolated-margin updates, scheduleCancel,
/// and the noop. Orders fill against the **real book** — the live depth the
/// feed caches — with per-level consumption, the impact lane. The plan's
/// identity (I3) holds by construction: an order moves the *book*, never the
/// *mark*; the mark moves only by an explicit crash. An unmodeled action tag
/// is refused by name in the venue's own `err` envelope (HTTP 200 — the shape
/// their client parses), as are the TP/SL groupings and builder fees the sim
/// does not model. The signature is accepted, never verified (the devnet has
/// no keys — the anvil-impersonation analogue), and the acting account is
/// read from `user` — or from
/// `vaultAddress`, which routes the action to that vault or subaccount
/// (see `acting_owner`).
pub(crate) async fn exchange(State(state): State<Arc<ApiState>>, body: Bytes) -> Result<Response> {
    let req: ExchangeBody = serde_json::from_slice(&body).map_err(|source| Error::bad_request(format!("invalid exchange request: {source}"),))?;
    // The noop action (docs): no side effects, the venue's no-data success —
    // but it still consumes its nonce, as the venue's does.
    if matches!(req.action, ExchangeAction::Noop) {
        if let (Some(user), Some(nonce)) = (req.user, req.nonce) {
            if let Err(e) = state.sim.use_nonce(user, nonce, crate::now_ms()) {
                return Ok(err_envelope(e.to_string()));
            }
        }
        return Ok(noop_response(&req));
    }
    let owner = match acting_user(&state, &req) {
        Ok((_, owner)) => owner,
        Err(response) => return Ok(*response),
    };
    Ok(dispatch(&state, owner, &body, &req))
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
        if let Err(e) = crate::sim::validate::validate_order(&state.feed, meta.as_ref(), &order) {
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
            Err(e) => err_envelope(e.to_string()),
        },
        None => err_envelope(format!("unknown asset index {asset}")),
    }
}

/// The modeled exchange actions, named once: both refusal messages (the
/// dispatcher's and the order path's) carry the same list.
const MODELED_ACTIONS: &str = "the modeled exchange actions are order, cancel, \
     cancelByCloid, modify, batchModify, updateLeverage, updateIsolatedMargin, \
     scheduleCancel and noop. See docs/INTEGRATION.md.";

/// The action dispatch (its own fn: the handler is the parse + the preamble).
fn dispatch(state: &Arc<ApiState>, owner: Address, body: &Bytes, req: &ExchangeBody) -> Response {
    match &req.action {
        ExchangeAction::Order { .. } => order_action(state, owner, body, req),
        ExchangeAction::Cancel { cancels, fast } => {
            let fast = *fast;
            flagged("f", fast, || cancel_action(state, owner, cancels, fast.unwrap_or(false)))
        }
        ExchangeAction::CancelByCloid { cancels, fast } => {
            let fast = *fast;
            flagged("f", fast, || {
                cancel_by_cloid_action(state, owner, cancels, fast.unwrap_or(false))
            })
        }
        ExchangeAction::Modify {
            oid,
            order,
            always_place,
        } => modify_action(state, owner, oid, order, always_place.unwrap_or(false)),
        ExchangeAction::BatchModify {
            modifies,
            always_place,
        } => batch_modify(state, owner, modifies, always_place.unwrap_or(false)),
        ExchangeAction::UpdateIsolatedMargin { asset, ntli, .. } => {
            asset_action(state, *asset, |coin| {
                state.sim.update_isolated_margin(owner, coin, *ntli, &state.feed)
            })
        }
        ExchangeAction::UpdateLeverage {
            asset,
            is_cross,
            leverage,
        } => leverage_action(state, owner, *asset, *is_cross, *leverage),
        ExchangeAction::ScheduleCancel { time } => match state.sim.schedule_cancel(owner, *time) {
            Ok(_) => default_ok(),
            Err(e) => err_envelope(e.to_string()),
        },
        ExchangeAction::Noop => noop_response(req),
        ExchangeAction::Unmodeled => err_envelope(unmodeled_reason(body)),
    }
}

/// The acting account + the venue's nonce rule (plan §5: the 100 highest per
/// signer, window (T − 2 d, T + 1 d), no reuse). `Err` carries the ready
/// err-envelope response.
/// The acting account's owner: a request that names a `vaultAddress` acts on
/// that vault or subaccount instead of the signer (docs: "Subaccounts and
/// vaults do not have private keys. To perform actions on behalf of a
/// subaccount or vault signing should be done by the master account and the
/// vaultAddress field should be set to the address of the subaccount or
/// vault."). Pure, so the routing is testable without a state.
fn acting_owner(req: &ExchangeBody) -> Option<Address> {
    req.vault_address.or(req.user)
}

/// The signer and the owner. The request's `user` is the signer — nonces are
/// consumed per signer (the docs' per-address nonce set) — and the state the
/// action touches is the owner's. The signature is never verified, so the
/// master's authority is moot (plan §17).
fn acting_user(
    state: &Arc<ApiState>,
    req: &ExchangeBody,
) -> std::result::Result<(Address, Address), Box<Response>> {
    let Some(user) = req.user else {
        return Err(Box::new(
            err_envelope(
                "venue-sim: an action must name the acting account in `user` — the devnet has \
                 no keys to derive it from the signature (plan §17). See docs/INTEGRATION.md."
                    .to_string(),
            ),
        ));
    };
    if let Some(nonce) = req.nonce {
        if let Err(e) = state.sim.use_nonce(user, nonce, crate::now_ms()) {
            return Err(Box::new(err_envelope(e.to_string())));
        }
    }
    Ok((user, acting_owner(req).unwrap_or(user)))
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
        Err(reason) => err_envelope(reason),
    }
}

/// Modify every leg; the first refusal is the response (the venue answers a
/// batch all-or-nothing).
fn batch_modify(
    state: &Arc<ApiState>,
    owner: Address,
    modifies: &[ModifyItem],
    always: bool,
) -> Response {
    for m in modifies {
        if let Err(e) = modify_one(state, owner, &m.oid, &m.order, always) {
            return err_envelope(e.to_string());
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

/// The `updateLeverage` action (its own fn so the dispatcher stays small):
/// the sim's setting carries the docs' constraints.
fn leverage_action(
    state: &Arc<ApiState>,
    owner: Address,
    asset: u32,
    is_cross: bool,
    leverage: u32,
) -> Response {
    let meta = state.feed.meta();
    asset_action(state, asset, |coin| {
        state.sim.set_leverage(
            owner,
            coin,
            crate::sim::account::LeverageSetting { is_cross, leverage },
            meta.as_ref(),
        )
    })
}

/// The `order` action (its own fn so the dispatcher stays small).
fn order_action(state: &Arc<ApiState>, owner: Address, body: &Bytes, req: &ExchangeBody) -> Response {
    let (orders, grouping) = match order_parts(body, &req.action) {
        Ok(parts) => parts,
        Err(reason) => return err_envelope(reason),
    };
    tracing::info!(
        owner = %owner,
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
        .map(|action| fill_status(state, owner, meta.as_ref(), action))
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
    format!("venue-sim: the `{tag}` action is not modeled; {MODELED_ACTIONS}")
}

/// The asset index → coin map (one home; `None` = unknown index).
fn coin_of(meta: Option<&crate::wire::Meta>, asset: u32) -> Option<String> {
    meta?.universe.get(asset as usize).map(|a| a.name.clone())
}

/// `cancel`: per-entry statuses in the venue's own shape.
fn cancel_action(
    state: &ApiState,
    owner: Address,
    cancels: &[CancelAction],
    fast: bool,
) -> Response {
    if cancels.is_empty() {
        // The venue's batched actions answer an error for an empty payload
        // (the order action's pre-validation carries the same rule).
        return err_envelope("venue-sim: an empty cancel batch is rejected (docs: pre-validation)".to_string());
    }
    let meta = state.feed.meta();
    let statuses = cancels
        .iter()
        .map(|c| match coin_of(meta.as_ref(), c.a) {
            Some(coin) => match state.sim.cancel(owner, &coin, c.o, fast) {
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
    owner: Address,
    cancels: &[CancelByCloidAction],
    fast: bool,
) -> Response {
    if cancels.is_empty() {
        return err_envelope("venue-sim: an empty cancel batch is rejected (docs: pre-validation)".to_string());
    }
    let meta = state.feed.meta();
    let statuses = cancels
        .iter()
        .map(|c| match coin_of(meta.as_ref(), c.asset) {
            Some(coin) => match state.sim.cancel_by_cloid(owner, &coin, c.cloid, fast) {
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
    owner: Address,
    oid: &crate::wire::OidOrCloid,
    order: &OrderAction,
    always_place: bool,
) -> Response {
    match modify_one(state, owner, oid, order, always_place) {
        Ok(()) => default_ok(),
        Err(e) => err_envelope(e.to_string()),
    }
}

/// One modify: the cancel of the old order, then the placement. With
/// `always_place = false` the cancel must succeed, the new order must be
/// non-trigger, and an executable Gtc is overridden to Alo (docs); with
/// `always_place = true` the placement happens regardless.
fn modify_one(
    state: &ApiState,
    owner: Address,
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
            state.sim.cancel(owner, &coin, *o, false)
        }
        crate::wire::OidOrCloid::Cloid(c) => {
            let coin = coin_of(meta.as_ref(), order.a).ok_or_else(|| {
                Error::bad_request(format!("unknown asset index {}", order.a))
            })?;
            state.sim.cancel_by_cloid(owner, &coin, *c, false)
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
    state.sim.place(&state.feed, owner, &mapped)?;
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
        return Err(format!("venue-sim: `{tag}` is not an order action; {MODELED_ACTIONS}"));
    };
    if let Some(builder) = builder {
        return Err(format!(
            "venue-sim: builder fees are not modeled (fees are a research lane, plan §16.3) \
             — the order would fill without charging {} its {} tenths of a basis point. See \
             docs/INTEGRATION.md.",
            builder.b, builder.f
        ));
    }
    // The grouping's association semantics are not modeled: a `normalTpsl`
    // parent's children are placed only when it fully fills (or is
    // margin-canceled — the docs' exception), and `positionTpsl` orders
    // resize with the position. Placing them as plain orders would diverge
    // silently, so they are refused by name (the portfolio-margin
    // precedent); plain trigger orders cover the product's need.
    if let Some(Grouping::NormalTpsl | Grouping::PositionTpsl) = *grouping {
        return Err(format!(
            "venue-sim: the `{}` grouping is not modeled (the parent/child TP/SL \
             lifecycle); send the TP/SL as plain trigger orders (grouping \"na\"). See \
             docs/INTEGRATION.md.",
            if matches!(*grouping, Some(Grouping::NormalTpsl)) {
                "normalTpsl"
            } else {
                "positionTpsl"
            }
        ));
    }
    Ok((orders, *grouping))
}

/// The venue's `err` envelope (HTTP 200 — the shape their client parses).
fn err_envelope(response: String) -> Response {
    Json(ExchangeResponse {
        status: "err",
        response,
    })
    .into_response()
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
            "order carries a client id (echoed; the resting lane keys cancels by it)"
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

#[cfg(test)]
mod tests {
    use super::*;

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
            margin_mode: None,
        });
        meta
    }

    /// The docs' subaccount/vault routing: "Subaccounts and vaults do not
    /// have private keys. To perform actions on behalf of a subaccount or
    /// vault signing should be done by the master account and the
    /// vaultAddress field should be set to the address of the subaccount or
    /// vault." — the owner is the vault when present, the signer otherwise.
    #[test]
    fn acting_owner_routes_vault_and_subaccount_actions() {
        let master = "0x00000000000000000000000000000000000000e1".parse().unwrap();
        let vault = "0x00000000000000000000000000000000000000e2".parse().unwrap();
        let plain: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"order","orders":[]},"user":"0x00000000000000000000000000000000000000e1"}"#,
        )
        .unwrap();
        assert_eq!(acting_owner(&plain), Some(master), "no vault: the signer acts");
        let routed: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"order","orders":[]},"user":"0x00000000000000000000000000000000000000e1","vaultAddress":"0x00000000000000000000000000000000000000e2"}"#,
        )
        .unwrap();
        assert_eq!(acting_owner(&routed), Some(vault), "the vault's state is touched");
    }

    /// The compact order maps onto the lane's order: the asset index resolves
    /// through the live meta, the strings parse, and the tif mapping is
    /// exactly IOC/resting.
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
        // The TP/SL groupings are refused by name (the parent/child
        // lifecycle and the position-linked resizing are not modeled);
        // `"na"` is the plain path.
        let body = Bytes::from(
            r#"{"action":{"type":"order","orders":[],"grouping":"normalTpsl"},"user":"0x00000000000000000000000000000000000000e1"}"#,
        );
        let req: ExchangeBody = serde_json::from_slice(&body).unwrap();
        let err = order_parts(&body, &req.action).unwrap_err();
        assert!(err.contains("normalTpsl"), "{err}");
        let body = Bytes::from(
            r#"{"action":{"type":"order","orders":[],"grouping":"na"},"user":"0x00000000000000000000000000000000000000e1"}"#,
        );
        let req: ExchangeBody = serde_json::from_slice(&body).unwrap();
        assert!(order_parts(&body, &req.action).is_ok(), "\"na\" is the plain path");
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
