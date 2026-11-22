//! The `/exchange` surface: the venue's action handlers (orders, cancels,
//! modifies, leverage and isolated-margin updates, scheduleCancel, noop), the
//! dispatch, and the boundary refusals. The transfer and account-setup
//! actions are `user_actions.rs`; the reads live in `info.rs`; the wire shapes
//! in `wire.rs`.
//!
//! The wire errors are the venue's own (measured, `fixtures/real_exchange.json`):
//! a body that is not JSON is a 400 and one that is not an action (a missing
//! field, a tag the venue does not define) a 422, both plain text; everything
//! else is HTTP 200 with `{"status":"err","response":"..."}`.
use super::user_actions::{self, SendAsset};
use super::ApiState;
use crate::error::{Error, Result};
use crate::sim::order::BuilderFee;
use crate::wire::{
    ActionTag, CancelAction, CancelByCloidAction, CancelData, CancelResponseBody, CancelStatus,
    DefaultBody, ErrorStatus, ExchangeAction, ExchangeBody, ExchangeOk, ExchangeResponse,
    FilledInfo, Grouping, ModifyItem, OrderAction, OrderData, OrderResponseBody, OrderStatus,
    RestingInfo, Signature, SuccessStatus,
};
use alloy_primitives::Address;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
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
/// their client parses), as are the TP/SL groupings the sim does not model. An
/// order action's `builder` is accepted when the account approved that builder
/// for at least `f` (CoreWriter `approveBuilderFee`) and charged on every fill
/// (see `order_builder`). The acting account is the venue's: the signer is
/// recovered from the signature (`signing`) and acts for the account that
/// registered it as an API wallet, or for its own — unless the body names
/// `user`, the keyless harness shape (the anvil-impersonation analogue), whose
/// signature is not checked. `vaultAddress` routes the action to that vault or
/// subaccount (see `acting_owner`). The transfer actions (`usdClassTransfer`,
/// `sendAsset`, `spotSend`, `usdSend`) and the account setup (`approveAgent`,
/// `approveBuilderFee`, `userSetAbstraction`, `agentSetAbstraction`) are
/// `user_actions.rs`; the user-signed ones must be signed by the account's own
/// key. An order reads its coin's book first when none has landed yet
/// (`ensure_books`), and every asset id of a builder dex resolves to that
/// dex's coins.
pub(crate) async fn exchange(State(state): State<Arc<ApiState>>, body: Bytes) -> Result<Response> {
    let req = match parse(&body) {
        Ok(req) => req,
        Err(response) => return Ok(*response),
    };
    // Every action, the noop (docs: no side effects) included, goes through
    // the signature: the venue answers a zero signature on a noop with its
    // "Unable to recover signer.", and a noop consumes its nonce.
    let now = state.sim.clock(crate::now_ms())?;
    let owner = match acting_user(&state, &req, &body, now) {
        Ok((_, owner)) => owner,
        Err(response) => return Ok(*response),
    };
    ensure_books(&state, &req.action).await;
    Ok(dispatch(&state, owner, &body, &req))
}

/// The venue's 400 for a body that is not JSON (measured).
const NOT_JSON: &str = "Failed to parse the request body as JSON";

/// The venue's 422 for JSON that is not an exchange request (measured).
const NOT_AN_ACTION: &str = "Failed to deserialize the JSON body into the target type";

/// A plain-text error response, as the venue's server sends its own.
fn plain(status: StatusCode, text: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response()
}

/// The body as an exchange request, or the venue's own plain-text error: a
/// body that is not JSON is a 400, one that is JSON but not an action (a
/// missing field, a type the venue does not define) a 422. The venue's body
/// carries `nonce` and `signature` always; only the keyless harness shape
/// (`user` named) may leave them out.
fn parse(body: &[u8]) -> std::result::Result<ExchangeBody, Box<Response>> {
    let req: ExchangeBody = serde_json::from_slice(body).map_err(|e| {
        Box::new(match e.classify() {
            serde_json::error::Category::Data => {
                plain(StatusCode::UNPROCESSABLE_ENTITY, NOT_AN_ACTION)
            }
            _ => plain(StatusCode::BAD_REQUEST, NOT_JSON),
        })
    })?;
    let shaped = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("action")
                .map(|a| super::shape::conforms(a, req.user.is_none()))
        })
        .unwrap_or(true);
    let unsigned = req.user.is_none() && (req.signature.is_none() || req.nonce.is_none());
    let unknown_tag = matches!(req.action, ExchangeAction::Unmodeled)
        && !serde_json::from_slice::<ActionTag>(body)
            .is_ok_and(|t| crate::wire::KNOWN_ACTION_TAGS.contains(&t.action.r#type.as_str()));
    if unsigned || unknown_tag || !shaped {
        return Err(Box::new(plain(
            StatusCode::UNPROCESSABLE_ENTITY,
            NOT_AN_ACTION,
        )));
    }
    Ok(req)
}

/// The most books one request reads on demand (a batch spans few coins).
const MAX_ENSURED_BOOKS: usize = 8;

/// Orders walk the coin's book: read it once when none has landed yet before
/// the action runs.
async fn ensure_books(state: &ApiState, action: &ExchangeAction) {
    let assets: Vec<u32> = match action {
        ExchangeAction::Order { orders, .. } => orders.iter().map(|o| o.a).collect(),
        ExchangeAction::Modify { order, .. } => vec![order.a],
        ExchangeAction::BatchModify { modifies, .. } => {
            modifies.iter().map(|m| m.order.a).collect()
        }
        _ => return,
    };
    let mut coins: Vec<String> = assets
        .into_iter()
        .filter_map(|a| state.feed.coin_of_asset(a))
        .collect();
    coins.sort();
    coins.dedup();
    for coin in coins.iter().take(MAX_ENSURED_BOOKS) {
        let _ = state.feed.ensure_book(coin).await;
    }
}

/// The docs' `expiresAfter`: `Err(reason)` refuses the whole payload when the
/// timestamp has passed ("a timestamp in milliseconds *after which* the
/// action will be rejected" — strict, so the timestamp itself is still valid).
fn expired(req: &ExchangeBody, now_ms: u64) -> Result<(), String> {
    let Some(expires) = req.expires_after else {
        return Ok(());
    };
    if expires < now_ms {
        return Err("Action already expired".to_string());
    }
    Ok(())
}

/// The docs' pre-validation pass: `Err(reason)` refuses the whole payload
/// with one error (the venue duplicates it n times for their clients).
fn prevalidate(
    state: &Arc<ApiState>,
    user: Address,
    orders: &[OrderAction],
) -> Result<(), String> {
    let account = state.sim.account(user).ok().flatten();
    if orders.is_empty() {
        return Err("venue-sim: an empty order batch is rejected (docs: pre-validation)".into());
    }
    for action in orders {
        let Ok(order) = action.to_order(|a| state.feed.coin_of_asset(a)) else {
            continue; // a per-order mapping error keeps its own status
        };
        if order.trigger.is_some() && !order.reduce_only {
            return Err(
                "venue-sim: TP/SL orders must be reduce-only (docs: pre-validation)".into(),
            );
        }
        let meta = state.feed.meta_for_coin(&order.coin);
        let position_szi = account
            .as_ref()
            .and_then(|a| a.positions.get(&order.coin).map(|p| p.szi));
        if let Err(e) =
            crate::sim::validate::validate_order(&state.feed, meta.as_ref(), &order, position_szi)
        {
            let reason = e.to_string();
            if reason.contains("too far from oracle") {
                return Err(reason);
            }
        }
    }
    Ok(())
}

/// The asset-index actions (`updateLeverage`, `updateIsolatedMargin`): resolve
/// the coin, run the sim call, answer in the venue's envelope.
fn asset_action<F>(state: &Arc<ApiState>, asset: u32, run: F) -> Response
where
    F: FnOnce(&str) -> std::result::Result<(), crate::error::Error>,
{
    match state.feed.coin_of_asset(asset) {
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
     scheduleCancel, noop, usdClassTransfer, sendAsset, spotSend, usdSend, approveAgent, \
     approveBuilderFee, userSetAbstraction and agentSetAbstraction. See docs/INTEGRATION.md.";

/// The wire's optional `fast` flag, converted once for the cancel actions.
fn cancel_speed(fast: Option<bool>) -> crate::sim::order::CancelSpeed {
    crate::sim::order::CancelSpeed::of(fast.unwrap_or(false))
}

/// The wire's optional `alwaysPlace` flag, converted once for the modify
/// actions.
fn always_mode(always_place: Option<bool>) -> crate::sim::order::AlwaysPlace {
    crate::sim::order::AlwaysPlace::of(always_place.unwrap_or(false))
}

/// The action dispatch (its own fn: the handler is the parse + the preamble).
fn dispatch(state: &Arc<ApiState>, owner: Address, body: &Bytes, req: &ExchangeBody) -> Response {
    match &req.action {
        ExchangeAction::Order { .. } => order_action(state, owner, body, req),
        ExchangeAction::Cancel { cancels, fast } => flagged("f", *fast, || {
            cancel_action(state, owner, cancels, cancel_speed(*fast))
        }),
        ExchangeAction::CancelByCloid { cancels, fast } => flagged("f", *fast, || {
            cancel_by_cloid_action(state, owner, cancels, cancel_speed(*fast))
        }),
        ExchangeAction::Modify {
            oid,
            order,
            always_place,
        } => modify_action(state, owner, oid, order, always_mode(*always_place)),
        ExchangeAction::BatchModify {
            modifies,
            always_place,
        } => batch_modify(state, owner, modifies, always_mode(*always_place)),
        ExchangeAction::UpdateIsolatedMargin { asset, ntli, .. } => {
            asset_action(state, *asset, |coin| {
                state
                    .sim
                    .update_isolated_margin(owner, coin, *ntli, &state.feed)
            })
        }
        ExchangeAction::UpdateLeverage {
            asset,
            is_cross,
            leverage,
        } => leverage_action(
            state,
            owner,
            *asset,
            crate::wire::LeverageType::of(*is_cross),
            *leverage,
        ),
        ExchangeAction::ScheduleCancel { time } => match state
            .sim
            .schedule_cancel(owner, crate::sim::order::ScheduleCancel::of(*time))
        {
            Ok(_) => default_ok(),
            Err(e) => err_envelope(e.to_string()),
        },
        ExchangeAction::Noop => noop_response(req),
        ExchangeAction::Unmodeled => err_envelope(unmodeled_reason(body)),
        account_action => user_action(state, owner, account_action),
    }
}

/// A transfer's `destination` string as an address: the venue reads it as a
/// free string and refuses a malformed one only after the signature held.
fn destination_of(text: &str) -> std::result::Result<Address, String> {
    text.parse::<Address>().map_err(|_| {
        format!("venue-sim: the destination `{text}` is not an address (0x and 40 hex digits)")
    })
}

/// The transfer and account-setup actions (`user_actions.rs`).
#[allow(clippy::too_many_lines)] // one dispatch arm per user-signed action
fn user_action(state: &Arc<ApiState>, owner: Address, action: &ExchangeAction) -> Response {
    match action {
        ExchangeAction::UsdClassTransfer { amount, to_perp } => {
            user_actions::usd_class_transfer(state, owner, amount, *to_perp)
        }
        ExchangeAction::SendAsset {
            destination,
            source_dex,
            destination_dex,
            token,
            amount,
            from_sub_account,
        } => match destination_of(destination) {
            Ok(destination) => user_actions::send_asset(
                state,
                owner,
                &SendAsset {
                    destination,
                    source_dex,
                    destination_dex,
                    token,
                    amount,
                    from_sub_account,
                },
            ),
            Err(text) => err_envelope(text),
        },
        ExchangeAction::SpotSend {
            destination,
            token,
            amount,
        } => match destination_of(destination) {
            Ok(destination) => user_actions::spot_send(state, owner, destination, (token, amount)),
            Err(text) => err_envelope(text),
        },
        ExchangeAction::UsdSend {
            destination,
            amount,
        } => match destination_of(destination) {
            Ok(destination) => user_actions::usd_send(state, owner, destination, amount),
            Err(text) => err_envelope(text),
        },
        ExchangeAction::Withdraw3 => user_actions::withdraw3(),
        ExchangeAction::ApproveAgent {
            agent_address,
            agent_name,
        } => user_actions::approve_agent(state, owner, *agent_address, agent_name.as_deref()),
        ExchangeAction::ApproveBuilderFee {
            max_fee_rate,
            builder,
        } => user_actions::approve_builder_fee(state, owner, *builder, max_fee_rate),
        ExchangeAction::UserSetAbstraction { user, abstraction } => {
            user_actions::user_set_abstraction(state, owner, *user, abstraction)
        }
        ExchangeAction::AgentSetAbstraction { abstraction } => {
            user_actions::agent_set_abstraction(state, owner, abstraction)
        }
        _ => err_envelope("venue-sim: not an account action".to_string()),
    }
}

/// The acting account's owner (L1 actions): a request that names a
/// `vaultAddress` acts on that vault or subaccount instead of the signer's
/// account (docs: "Subaccounts and vaults do not have private keys. To
/// perform actions on behalf of a subaccount or vault signing should be done
/// by the master account and the vaultAddress field should be set to the
/// address of the subaccount or vault."). Pure, so the routing is testable
/// without a state.
fn acting_owner(req: &ExchangeBody, account: Address) -> Address {
    req.vault_address.unwrap_or(account)
}

/// The signer and the owner, after the venue's nonce rule (the 100 highest
/// per signer, window (T − 2 d, T + 1 d), no reuse). Nonces are consumed per
/// signer (the docs' per-address nonce set); the state the action touches is
/// the owner's. `Err` carries the ready err-envelope response.
fn acting_user(
    state: &Arc<ApiState>,
    req: &ExchangeBody,
    body: &[u8],
    now_ms: u64,
) -> std::result::Result<(Address, Address), Box<Response>> {
    let refuse = |reason: String| Box::new(err_envelope(reason));
    // The venue checks, before it recovers the signer, that a user-signed
    // action's own `nonce`/`time` is the request's nonce, and its network.
    if req.user.is_none() {
        if let Some(reason) = crate::api::shape::pre_signature_refusal(body) {
            return Err(refuse(reason.to_string()));
        }
    }
    let (signer, account) = match req.user {
        Some(user) => (user, user),
        None => signed_account(state, body).map_err(refuse)?,
    };
    // The docs' `expiresAfter` (the exchange page): a millisecond timestamp
    // after which the action is rejected: the venue's own string is "Action
    // already expired" (the FAQ's 15 s protection is the client-side default).
    // It comes after the signature, as on the venue (a zero signature on an
    // expired action still reads "Unable to recover signer."). The sim's
    // clock is the scenario's, so a scripted time jump expires actions
    // exactly as the venue's would.
    expired(req, now_ms).map_err(refuse)?;
    if let Some(nonce) = req.nonce {
        if let Err(e) = state.sim.use_nonce(signer, nonce, crate::now_ms()) {
            return Err(refuse(e.to_string()));
        }
    }
    // A user-signed action is the account's own (it has no vault routing:
    // `sendAsset` names its source in `fromSubAccount`).
    let owner = if req.action.is_user_signed() {
        account
    } else {
        acting_owner(req, account)
    };
    Ok((signer, owner))
}

/// The venue's own reading of a body without `user`: the signer recovered
/// from the signature, and the account it acts for (the one that registered
/// it as an API wallet, or its own). A signer that is neither gets the
/// venue's string: a tampered body recovers to such an address. A
/// user-signed action must be signed by the account itself: an API wallet
/// signing one is refused.
fn signed_account(
    state: &ApiState,
    body: &[u8],
) -> std::result::Result<(Address, Address), String> {
    let recovered = crate::api::signing::recover_signer(body)?;
    let signer = recovered.address;
    if recovered.user_signed {
        return match state.sim.account(signer) {
            Ok(Some(_)) => Ok((signer, signer)),
            Ok(None) => match state.sim.signer_account(signer) {
                Ok(Some(owner)) => Err(format!(
                    "venue-sim: this action is signed by the account's own key; {signer:#x} is \
                     an API wallet of {owner:#x}"
                )),
                Ok(None) => Err(format!("User or API Wallet {signer:#x} does not exist.")),
                Err(e) => Err(e.to_string()),
            },
            Err(e) => Err(e.to_string()),
        };
    }
    match state.sim.signer_account(signer) {
        Ok(Some(account)) => Ok((signer, account)),
        Ok(None) => Err(format!("User or API Wallet {signer:#x} does not exist.")),
        Err(e) => Err(e.to_string()),
    }
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
    mode: crate::sim::order::AlwaysPlace,
) -> Response {
    for m in modifies {
        if let Err(e) = modify_one(state, owner, &m.oid, &m.order, mode) {
            return err_envelope(e.to_string());
        }
    }
    default_ok()
}

/// The exchange lane's ok envelope: every successful answer carries
/// `{"status":"ok","response":…}` — built once here.
fn ok_envelope<T: serde::Serialize>(response: T) -> Response {
    Json(ExchangeOk {
        status: "ok",
        response,
    })
    .into_response()
}

/// The venue's no-data success, built once.
pub(super) fn default_ok() -> Response {
    ok_envelope(DefaultBody { r#type: "default" })
}

/// The `updateLeverage` action (its own fn so the dispatcher stays small):
/// the sim's setting carries the docs' constraints.
fn leverage_action(
    state: &Arc<ApiState>,
    owner: Address,
    asset: u32,
    mode: crate::wire::LeverageType,
    leverage: u32,
) -> Response {
    asset_action(state, asset, |coin| {
        let meta = state.feed.meta_for_lookup(coin);
        state.sim.set_leverage(
            owner,
            coin,
            crate::sim::account::LeverageSetting { mode, leverage },
            meta.as_ref(),
        )
    })
}

/// The `order` action (its own fn so the dispatcher stays small).
fn order_action(
    state: &Arc<ApiState>,
    owner: Address,
    body: &Bytes,
    req: &ExchangeBody,
) -> Response {
    let (orders, grouping) = match order_parts(body, &req.action) {
        Ok(parts) => parts,
        Err(reason) => return err_envelope(reason),
    };
    let builder = match order_builder(state, owner, &req.action) {
        Ok(builder) => builder,
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
        builder = ?builder,
        "exchange order action accepted"
    );
    // The docs' pre-validation class: one error for the whole payload
    // (empty batches, non-reduce-only TP/SL, an order too far from the
    // reference price) — every status carries it, as their clients expect.
    if let Err(reason) = prevalidate(state, owner, orders) {
        let statuses = orders
            .iter()
            .map(|_| OrderStatus::Error(reason.clone()))
            .collect();
        return order_response(statuses);
    }
    let statuses = orders
        .iter()
        .map(|action| fill_status(state, (owner, builder), action))
        .collect();
    order_response(statuses)
}

/// The order action's `{"status":"ok","response":{"type":"order","data":…}}`
/// envelope (the shape their client parses).
fn order_response(statuses: Vec<OrderStatus>) -> Response {
    ok_envelope(OrderResponseBody {
        r#type: "order",
        data: OrderData { statuses },
    })
}

/// The unmodeled tag's refusal, naming the tag from the raw body.
fn unmodeled_reason(body: &Bytes) -> String {
    let tag = serde_json::from_slice::<ActionTag>(body)
        .map(|t| t.action.r#type)
        .unwrap_or_else(|_| "unknown".into());
    format!("venue-sim: the `{tag}` action is not modeled; {MODELED_ACTIONS}")
}

/// `cancel`: per-entry statuses in the venue's own shape.
fn cancel_action(
    state: &ApiState,
    owner: Address,
    cancels: &[CancelAction],
    speed: crate::sim::order::CancelSpeed,
) -> Response {
    if cancels.is_empty() {
        // The venue's batched actions answer an error for an empty payload
        // (the order action's pre-validation carries the same rule).
        return err_envelope(
            "venue-sim: an empty cancel batch is rejected (docs: pre-validation)".to_string(),
        );
    }
    let statuses = cancels
        .iter()
        .map(|c| match state.feed.coin_of_asset(c.a) {
            Some(coin) => match state.sim.cancel(owner, &coin, c.o, speed) {
                Ok(()) => CancelStatus::Success(SuccessStatus),
                Err(e) => CancelStatus::Error(Box::new(ErrorStatus {
                    error: e.to_string(),
                })),
            },
            None => CancelStatus::Error(Box::new(ErrorStatus {
                error: format!("unknown asset index {}", c.a),
            })),
        })
        .collect();
    ok_envelope(CancelResponseBody {
        r#type: "cancel",
        data: CancelData { statuses },
    })
}

/// `cancelByCloid`: the same, addressed by client id.
fn cancel_by_cloid_action(
    state: &ApiState,
    owner: Address,
    cancels: &[CancelByCloidAction],
    speed: crate::sim::order::CancelSpeed,
) -> Response {
    if cancels.is_empty() {
        return err_envelope(
            "venue-sim: an empty cancel batch is rejected (docs: pre-validation)".to_string(),
        );
    }
    let statuses = cancels
        .iter()
        .map(|c| match state.feed.coin_of_asset(c.asset) {
            Some(coin) => match state.sim.cancel_by_cloid(owner, &coin, c.cloid, speed) {
                Ok(()) => CancelStatus::Success(SuccessStatus),
                Err(e) => CancelStatus::Error(Box::new(ErrorStatus {
                    error: e.to_string(),
                })),
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
    mode: crate::sim::order::AlwaysPlace,
) -> Response {
    match modify_one(state, owner, oid, order, mode) {
        Ok(()) => default_ok(),
        Err(e) => err_envelope(e.to_string()),
    }
}

/// One modify: the cancel of the old order, then the placement. With
/// [`AlwaysPlace::RequiresCancel`] the cancel must succeed, the new order must
/// be non-trigger, and an executable Gtc is overridden to Alo (docs); with
/// [`AlwaysPlace::Regardless`] the placement happens regardless.
fn modify_one(
    state: &ApiState,
    owner: Address,
    oid: &crate::wire::OidOrCloid,
    order: &OrderAction,
    mode: crate::sim::order::AlwaysPlace,
) -> Result<()> {
    let canceled =
        match oid {
            crate::wire::OidOrCloid::Oid(o) => {
                // The oid's coin is the order's own asset (the docs' `a`).
                let coin = state.feed.coin_of_asset(order.a).ok_or_else(|| {
                    Error::bad_request(format!("unknown asset index {}", order.a))
                })?;
                state
                    .sim
                    .cancel(owner, &coin, *o, crate::sim::order::CancelSpeed::Normal)
            }
            crate::wire::OidOrCloid::Cloid(c) => {
                let coin = state.feed.coin_of_asset(order.a).ok_or_else(|| {
                    Error::bad_request(format!("unknown asset index {}", order.a))
                })?;
                state
                    .sim
                    .cancel_by_cloid(owner, &coin, *c, crate::sim::order::CancelSpeed::Normal)
            }
        };
    if matches!(mode, crate::sim::order::AlwaysPlace::RequiresCancel) {
        canceled?;
    }
    let mut mapped = order.to_order(|a| state.feed.coin_of_asset(a))?;
    if matches!(mode, crate::sim::order::AlwaysPlace::RequiresCancel) {
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
        "exchange noop accepted"
    );
    ok_envelope(DefaultBody { r#type: "default" })
}

/// The order action's builder fee, gated: the account must have approved the
/// builder (CoreWriter `approveBuilderFee`, read back by `maxBuilderFee`) for at
/// least `f` tenths of a basis point, and `f` must be within the venue's perp
/// cap. `Err` is the refusal the err envelope carries (the whole action is
/// refused, as the venue's is); `Ok(None)` is an order without a builder.
fn order_builder(
    state: &ApiState,
    owner: Address,
    action: &ExchangeAction,
) -> Result<Option<BuilderFee>, String> {
    let ExchangeAction::Order {
        builder: Some(builder),
        ..
    } = action
    else {
        return Ok(None);
    };
    state
        .sim
        .authorize_builder(owner, builder.b, builder.f)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// The action's orders after the boundary checks, or the refusal reason the
/// err envelope carries: the unmodeled tag (read alone so the reason *names*
/// it) and the TP/SL groupings. The builder fee is gated separately
/// (`order_builder`: it needs the account's approvals).
fn order_parts<'a>(
    body: &Bytes,
    action: &'a ExchangeAction,
) -> Result<(&'a [OrderAction], Option<Grouping>), String> {
    let ExchangeAction::Order {
        orders, grouping, ..
    } = action
    else {
        let tag = serde_json::from_slice::<ActionTag>(body)
            .map(|t| t.action.r#type)
            .unwrap_or_else(|_| "unknown".into());
        return Err(format!(
            "venue-sim: `{tag}` is not an order action; {MODELED_ACTIONS}"
        ));
    };
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
pub(super) fn err_envelope(response: String) -> Response {
    Json(ExchangeResponse {
        status: "err",
        response,
    })
    .into_response()
}

/// One order's status: the fill, or its refusal, in the venue's own shapes.
/// `acting` is the acting account and the builder fee the action carries
/// (every order of the action charges it on its fills).
fn fill_status(
    state: &ApiState,
    acting: (Address, Option<BuilderFee>),
    action: &OrderAction,
) -> OrderStatus {
    let (user, builder) = acting;
    if let Some(cloid) = action.c {
        tracing::info!(
            cloid = %cloid,
            asset = action.a,
            "order carries a client id (echoed; the resting lane keys cancels by it)"
        );
    }
    match action
        .to_order(|a| state.feed.coin_of_asset(a))
        .and_then(|o| state.sim.place_with_builder(&state.feed, user, &o, builder))
    {
        Ok(placed) => match (placed.fill, placed.resting) {
            (Some(fill), _) => OrderStatus::Filled(FilledInfo {
                total_sz: crate::wire::decimal_nano::text(&fill.total_sz),
                avg_px: crate::wire::decimal_nano::text(&fill.avg_px),
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
    use crate::sim::{Deal, DealClass};
    use crate::wire::{test_ctx, L2Book, Level, PerpDex};
    use rust_decimal::Decimal;

    /// The asset-id resolver over a one-asset default dex (ETH at index 0).
    fn eth_resolver() -> impl Fn(u32) -> Option<String> {
        let registry = crate::feed::dex::DexRegistry::of_default(eth_meta());
        move |asset| registry.coin_of_asset(asset)
    }

    /// A one-asset meta (ETH at index 0, szDecimals 4).
    fn eth_meta() -> crate::wire::Meta {
        let mut meta: crate::wire::Meta = serde_json::from_str(r#"{"universe": []}"#).unwrap();
        meta.universe.push(crate::wire::AssetInfo {
            name: "ETH".into(),
            sz_decimals: 4,
            max_leverage: 20,
            margin_table_id: None,
            margin_mode: None,
            only_isolated: None,
            is_delisted: None,
            ..Default::default()
        });
        meta
    }

    /// One `perpDexs` entry (only the resolver reads `name`): the list's
    /// position is what the builder asset-id namespace encodes.
    fn perp_dex(name: &str) -> PerpDex {
        PerpDex {
            name: name.into(),
            full_name: format!("{name} dex"),
            deployer: Address::repeat_byte(0x01),
            oracle_updater: None,
            fee_recipient: None,
            asset_to_streaming_oi_cap: Vec::new(),
            asset_to_funding_clamp: Vec::new(),
            asset_to_funding_interest_rate: Vec::new(),
            asset_to_funding_multiplier: Vec::new(),
            sub_deployers: Vec::new(),
        }
    }

    /// The docs' subaccount/vault routing: "Subaccounts and vaults do not
    /// have private keys. To perform actions on behalf of a subaccount or
    /// vault signing should be done by the master account and the
    /// vaultAddress field should be set to the address of the subaccount or
    /// vault." — the owner is the vault when present, the signer's account
    /// otherwise.
    #[test]
    fn acting_owner_routes_vault_and_subaccount_actions() {
        let master: Address = "0x00000000000000000000000000000000000000e1"
            .parse()
            .unwrap();
        let vault: Address = "0x00000000000000000000000000000000000000e2"
            .parse()
            .unwrap();
        let plain: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"order","orders":[]},"user":"0x00000000000000000000000000000000000000e1"}"#,
        )
        .unwrap();
        assert_eq!(
            acting_owner(&plain, master),
            master,
            "no vault: the signer acts"
        );
        let routed: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"order","orders":[]},"user":"0x00000000000000000000000000000000000000e1","vaultAddress":"0x00000000000000000000000000000000000000e2"}"#,
        )
        .unwrap();
        assert_eq!(
            acting_owner(&routed, master),
            vault,
            "the vault's state is touched"
        );
    }

    /// The compact order maps onto the lane's order: the asset index resolves
    /// through the live meta, the strings parse, and the tif mapping is
    /// exactly IOC/resting.
    #[test]
    fn compact_orders_map_onto_the_lane() {
        let resolve = eth_resolver();
        let action: OrderAction = serde_json::from_str(
            r#"{"a":0,"b":true,"p":"101.5","s":"2","t":{"limit":{"tif":"Ioc"}}}"#,
        )
        .unwrap();
        let order = action.to_order(&resolve).unwrap();
        assert_eq!(order.coin, "ETH");
        assert!(order.is_buy);
        assert_eq!(order.size, "2".parse().unwrap());
        assert_eq!(order.limit_px, Some("101.5".parse().unwrap()));
        assert_eq!(order.tif, crate::sim::order::Tif::Ioc);
        // An absent `t` (the market shape) is an IOC; a Gtc/Alo order maps to
        // the resting shape the lane refuses by name; an unknown tif is
        // refused here.
        let market: OrderAction = serde_json::from_str(r#"{"a":0,"b":false,"s":"1"}"#).unwrap();
        assert_eq!(
            market.to_order(&resolve).unwrap().tif,
            crate::sim::order::Tif::Ioc
        );
        let gtc: OrderAction =
            serde_json::from_str(r#"{"a":0,"b":false,"s":"1","t":{"limit":{"tif":"Gtc"}}}"#)
                .unwrap();
        assert_eq!(
            gtc.to_order(&resolve).unwrap().tif,
            crate::sim::order::Tif::Gtc
        );
        let bad: OrderAction =
            serde_json::from_str(r#"{"a":0,"b":false,"s":"1","t":{"limit":{"tif":"Fok"}}}"#)
                .unwrap();
        assert!(format!("{}", bad.to_order(&resolve).unwrap_err()).contains("unknown tif"));
        // An index past the meta is a named refusal.
        let oob: OrderAction = serde_json::from_str(r#"{"a":7,"b":false,"s":"1"}"#).unwrap();
        assert!(format!("{}", oob.to_order(&resolve).unwrap_err()).contains("unknown asset index"));
    }

    /// The docs' `expiresAfter` (the exchange page + the FAQ's "Action already
    /// expired"): a passed timestamp refuses the whole payload; the timestamp
    /// itself is still valid ("after which"), and an absent field never
    /// expires.
    #[test]
    fn a_stale_expires_after_is_refused() {
        let body = Bytes::from(
            r#"{"action":{"type":"noop"},"user":"0x00000000000000000000000000000000000000e1","expiresAfter":1000}"#,
        );
        let req: ExchangeBody = serde_json::from_slice(&body).unwrap();
        assert!(expired(&req, 1000).is_ok(), "the timestamp itself is valid");
        assert_eq!(expired(&req, 1001).unwrap_err(), "Action already expired");
        let body = Bytes::from(
            r#"{"action":{"type":"noop"},"user":"0x00000000000000000000000000000000000000e1"}"#,
        );
        let req: ExchangeBody = serde_json::from_slice(&body).unwrap();
        assert!(expired(&req, u64::MAX).is_ok());
    }

    /// The complete wire shapes parse (cloid as alloy's `B128`, the trigger
    /// order, `expiresAfter`, the builder) and the boundary refusals name
    /// what they refused.
    #[test]
    fn the_wire_shapes_parse_and_the_refusals_are_named() {
        // The unmodeled tag is read alone and named in the reason.
        let body = Bytes::from(
            r#"{"action":{"type":"cancel","cancels":[]},"user":"0x00000000000000000000000000000000000000e1"}"#,
        );
        let req: ExchangeBody = serde_json::from_slice(&body).unwrap();
        let err = order_parts(&body, &req.action).unwrap_err();
        assert!(err.contains("`cancel`"), "{err}");
        // A builder fee parses and passes the boundary: its approval gate
        // needs the account (`order_builder`), tested end to end below.
        let body = Bytes::from(
            r#"{"action":{"type":"order","orders":[],"builder":{"b":"0x00000000000000000000000000000000000000e1","f":10}},"user":"0x00000000000000000000000000000000000000e1"}"#,
        );
        let req: ExchangeBody = serde_json::from_slice(&body).unwrap();
        assert!(order_parts(&body, &req.action).is_ok());
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
        assert!(
            order_parts(&body, &req.action).is_ok(),
            "\"na\" is the plain path"
        );
        // A trigger order maps into its typed shape (armed, not walked).
        let action: OrderAction = serde_json::from_str(
            r#"{"a":0,"b":true,"s":"1","t":{"trigger":{"isMarket":true,"triggerPx":"100","tpsl":"sl"}}}"#,
        )
        .unwrap();
        let order = action.to_order(eth_resolver()).unwrap();
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
        assert_eq!(
            out,
            serde_json::json!({"status": "ok", "response": {"type": "default"}})
        );
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
    fn unmodeled_actions_land_in_the_named_refusal_and_fills_take_the_venue_shape() {
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
        assert_eq!(
            ok["response"]["data"]["statuses"][0]["filled"]["totalSz"],
            "1.5"
        );
        assert_eq!(ok["response"]["data"]["statuses"][0]["filled"]["oid"], 3);
        // The err envelope keeps the venue's shape (HTTP 200 in the handler).
        let err = serde_json::to_value(ExchangeResponse {
            status: "err",
            response: "venue-sim: only the order action is modeled".into(),
        })
        .unwrap();
        assert_eq!(err["status"], "err");
    }

    const OWNER: &str = "0x00000000000000000000000000000000000000aa";
    const BUILDER: &str = "0x00000000000000000000000000000000000000bb";

    /// A state serving one ETH book (bids 100, asks 101) over an in-memory
    /// sim with `OWNER` funded and `BUILDER` a simulated account.
    fn builder_state() -> Arc<ApiState> {
        let feed = crate::feed::MarketFeed::memory(vec!["ETH".into()]);
        let now = crate::now_ms();
        feed.put_meta(crate::sim::meta_with("ETH", 10), now);
        feed.put_ctx("ETH".into(), crate::wire::test_ctx("101", "101"), now);
        let level = |px: &str| Level {
            px: px.parse().unwrap(),
            sz: "9".parse().unwrap(),
            n: 1,
        };
        feed.put_book(
            L2Book {
                coin: "ETH".into(),
                time: 0,
                levels: [vec![level("100")], vec![level("101")]],
                spread: None,
            },
            now,
        );
        feed.put_oi_caps(Vec::new());
        let sim = Arc::new(crate::sim::Sim::memory());
        for (who, amount) in [(OWNER, "1000"), (BUILDER, "0")] {
            sim.deal(
                &feed,
                &Deal {
                    address: who.parse().unwrap(),
                    class: DealClass::Perp,
                    coin: None,
                    dex: None,
                    amount: amount.parse().unwrap(),
                },
            )
            .unwrap();
        }
        let api = crate::feed::rest::RestClient::new(&["http://127.0.0.1:1".to_string()]).unwrap();
        Arc::new(
            ApiState::new(
                Arc::new(feed),
                sim,
                api,
                None,
                None,
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(1),
            )
            .unwrap(),
        )
    }

    /// POST an order action carrying `builder` (a JSON object, or `null`) and
    /// return the envelope.
    async fn post_order(state: &Arc<ApiState>, builder: &str) -> serde_json::Value {
        let body = format!(
            r#"{{"action":{{"type":"order","orders":[{{"a":0,"b":true,"p":"105","s":"2","t":{{"limit":{{"tif":"Ioc"}}}}}}],"grouping":"na","builder":{builder}}},"user":"{OWNER}"}}"#
        );
        post(state, body).await
    }

    /// POST `body` to `/exchange` and return the envelope.
    async fn post(state: &Arc<ApiState>, body: String) -> serde_json::Value {
        let response = exchange(State(state.clone()), Bytes::from(body))
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// A buy of 2 ETH at `price` (IOC) as a venue client sends it: no `user`,
    /// signed by `key` over the venue's hash.
    fn signed_order(key: &k256::ecdsa::SigningKey, price: &str, nonce: u64) -> String {
        let action = format!(
            r#"{{"type":"order","orders":[{{"a":0,"b":true,"p":"{price}","s":"2","r":false,"t":{{"limit":{{"tif":"Ioc"}}}}}}],"grouping":"na"}}"#
        );
        let digest = crate::api::signing::signing_digest(&action, nonce, None, None).unwrap();
        let (signature, recovery) = key.sign_prehash_recoverable(digest.as_slice()).unwrap();
        let bytes = signature.to_bytes();
        let (r, s) = (
            alloy_primitives::hex::encode(&bytes[..32]),
            alloy_primitives::hex::encode(&bytes[32..]),
        );
        let v = 27 + recovery.to_byte();
        format!(
            r#"{{"action":{action},"nonce":{nonce},"signature":{{"r":"0x{r}","s":"0x{s}","v":{v}}},"vaultAddress":null,"expiresAfter":null}}"#
        )
    }

    fn key(byte: u8) -> (k256::ecdsa::SigningKey, Address) {
        let key = k256::ecdsa::SigningKey::from_slice(&[byte; 32]).unwrap();
        let address = Address::from_public_key(key.verifying_key());
        (key, address)
    }

    /// The venue client's shape end to end: an order signed by the API
    /// wallet an account registered, sent without `user`, fills on that
    /// account (and its nonce is the wallet's); the harness shape, `user`
    /// named and nothing signed, still acts as named.
    #[tokio::test]
    async fn a_signed_order_acts_for_the_account_that_registered_its_key() {
        let state = builder_state();
        let owner: Address = OWNER.parse().unwrap();
        let (agent, agent_address) = key(0x11);
        state.sim.note_agent(owner, agent_address, "agent").unwrap();
        let nonce = crate::now_ms();

        let out = post(&state, signed_order(&agent, "105", nonce)).await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(
            out["response"]["data"]["statuses"][0]["filled"]["totalSz"],
            "2.0"
        );
        assert!(
            balance(&state, OWNER) < "1000".parse().unwrap(),
            "the owner paid"
        );
        assert_eq!(state.sim.user_fills(owner).unwrap().len(), 1);

        let replay = post(&state, signed_order(&agent, "105", nonce)).await;
        assert_eq!(replay["status"], "err", "the wallet's nonce is spent");
        assert_eq!(replay["response"], "Nonce already used.");

        let out = post_order(&state, "null").await;
        assert_eq!(out["status"], "ok", "the harness shape: {out}");
        assert_eq!(state.sim.user_fills(owner).unwrap().len(), 2);
    }

    /// A key no account registered, a body changed after it was signed (it
    /// recovers to nobody's address), and a body with neither `user` nor a
    /// signature are refused, and nothing fills.
    #[tokio::test]
    async fn an_unknown_or_tampered_signature_is_refused_with_the_venues_string() {
        let state = builder_state();
        let owner: Address = OWNER.parse().unwrap();
        let (agent, agent_address) = key(0x11);
        let nonce = crate::now_ms();

        let out = post(&state, signed_order(&agent, "105", nonce)).await;
        assert_eq!(out["status"], "err");
        assert_eq!(
            out["response"],
            format!("User or API Wallet {agent_address:#x} does not exist.")
        );

        state.sim.note_agent(owner, agent_address, "agent").unwrap();
        let tampered =
            signed_order(&agent, "105", nonce + 1).replacen(r#""p":"105""#, r#""p":"106""#, 1);
        let out = post(&state, tampered).await;
        assert_eq!(out["status"], "err");
        let reason = out["response"].as_str().unwrap();
        assert!(reason.starts_with("User or API Wallet 0x"), "{reason}");

        // Neither `user` nor a signature: the venue's 422, not an envelope.
        let unsigned = r#"{"action":{"type":"order","orders":[],"grouping":"na"}}"#;
        let (status, _, text) = post_raw(&state, unsigned).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(text, NOT_AN_ACTION);
        assert!(state.sim.user_fills(owner).unwrap().is_empty());
    }

    /// The signer's account: the one that registered it as an API wallet,
    /// else its own; nobody's is `None`; a wallet two accounts registered is
    /// refused rather than guessed.
    #[test]
    fn a_signer_acts_for_its_registering_account_or_its_own() {
        let state = builder_state();
        let (owner, builder): (Address, Address) =
            (OWNER.parse().unwrap(), BUILDER.parse().unwrap());
        let (_, agent) = key(0x11);
        let (_, stranger) = key(0x22);
        assert_eq!(state.sim.signer_account(owner).unwrap(), Some(owner));
        assert_eq!(state.sim.signer_account(agent).unwrap(), None);
        assert_eq!(state.sim.signer_account(stranger).unwrap(), None);

        state.sim.note_agent(owner, agent, "agent").unwrap();
        assert_eq!(state.sim.signer_account(agent).unwrap(), Some(owner));

        state.sim.note_agent(builder, agent, "agent").unwrap();
        let refusal = state.sim.signer_account(agent).unwrap_err().to_string();
        assert!(refusal.contains("more than one account"), "{refusal}");
    }

    fn balance(state: &ApiState, who: &str) -> Decimal {
        state
            .sim
            .account(who.parse().unwrap())
            .unwrap()
            .unwrap()
            .perp_balance("")
    }

    /// The frontend flow end to end: an order carrying a builder the account
    /// never approved is refused with the venue's string; once approved it is
    /// accepted, fills, charges the account and credits the builder; a fee
    /// above the approval, or above the perp cap, is refused by name; an
    /// order without a builder is unchanged.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one request/response script: splitting it would hide the order of the refusals
    async fn a_builder_order_is_refused_until_approved_then_charged() {
        let state = builder_state();
        let owner: Address = OWNER.parse().unwrap();
        let builder: Address = BUILDER.parse().unwrap();
        let carried = format!(r#"{{"b":"{BUILDER}","f":50}}"#);

        // Nothing approved yet.
        let out = post_order(&state, &carried).await;
        assert_eq!(out["status"], "err");
        assert_eq!(out["response"], "Builder fee has not been approved.");
        assert_eq!(balance(&state, OWNER), "1000".parse().unwrap(), "no fill");

        // Approved up to 40: 50 is above it.
        state.sim.approve_builder_fee(owner, builder, 40).unwrap();
        let out = post_order(&state, &carried).await;
        assert_eq!(out["status"], "err");
        assert!(
            out["response"]
                .as_str()
                .unwrap()
                .contains("exceeds the 40 tenths of a basis point"),
            "{out}"
        );

        // Approved up to 1000 (the agent's cap): 101 is above the perp cap.
        state
            .sim
            .approve_builder_fee(owner, builder, 1_000)
            .unwrap();
        let out = post_order(&state, &format!(r#"{{"b":"{BUILDER}","f":101}}"#)).await;
        assert_eq!(out["status"], "err");
        assert!(
            out["response"]
                .as_str()
                .unwrap()
                .contains("perp builder fee cap of 100"),
            "{out}"
        );

        // 50 is within both: 2 at 101 = 202; builder fee 0.101, taker 0.0909.
        let out = post_order(&state, &carried).await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(
            out["response"]["data"]["statuses"][0]["filled"]["totalSz"],
            "2.0"
        );
        let taker = "202".parse::<Decimal>().unwrap() * crate::sim::order::taker_fee();
        let fee: Decimal = "0.101".parse().unwrap();
        assert_eq!(
            balance(&state, OWNER),
            "1000".parse::<Decimal>().unwrap() - taker - fee
        );
        assert_eq!(balance(&state, BUILDER), fee);
        let fill = state.sim.user_fills(owner).unwrap().remove(0);
        assert_eq!(fill.builder_fee, Some(fee));

        // Without a builder (`null` is the absent value): no builder fee.
        let out = post_order(&state, "null").await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(
            balance(&state, BUILDER),
            fee,
            "the builder is not paid again"
        );
        let fills = state.sim.user_fills(owner).unwrap();
        assert_eq!(fills[1].builder_fee, None);
    }

    // ---- the wire errors, the transfers and the builder dexes ---------------

    /// POST `body` and return the status, content type and text as the venue's
    /// client sees them.
    async fn post_raw(state: &Arc<ApiState>, body: &str) -> (StatusCode, String, String) {
        let response = exchange(State(state.clone()), Bytes::from(body.to_string()))
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            content_type,
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }

    /// The venue's wire errors, as measured (`fixtures/real_exchange.json`): a
    /// body that is not JSON is a 400 and a body that is JSON but not an
    /// action (an unknown tag, a missing signature) a 422, both plain text; a
    /// zero signature is `err` "Unable to recover signer." on every action,
    /// the noop included; a real action the sim does not model is refused by
    /// name once its signature holds.
    #[tokio::test]
    async fn the_wire_errors_are_the_venues() {
        let state = builder_state();
        let real: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/real_exchange.json")).unwrap();
        for probe in real["probes"].as_array().unwrap() {
            let name = probe["name"].as_str().unwrap();
            let (status, content_type, text) =
                post_raw(&state, probe["request"].as_str().unwrap()).await;
            assert_eq!(status.as_u16(), probe["status"], "{name}");
            assert_eq!(content_type, probe["contentType"], "{name}");
            assert_eq!(text, probe["body"], "{name}");
        }
    }

    /// An action the venue defines but the sim does not model names itself
    /// (after the signature holds); one the venue does not define is a 422.
    #[tokio::test]
    async fn an_unmodeled_action_is_named_and_an_unknown_one_is_a_422() {
        let state = builder_state();
        let named = post(
            &state,
            format!(r#"{{"action":{{"type":"twapOrder","twap":{{}}}},"user":"{OWNER}"}}"#),
        )
        .await;
        assert_eq!(named["status"], "err");
        assert!(
            named["response"].as_str().unwrap().contains("`twapOrder`"),
            "{named}"
        );
        let (status, _, text) = post_raw(
            &state,
            &format!(r#"{{"action":{{"type":"nopeNotAnAction"}},"user":"{OWNER}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(text, NOT_AN_ACTION);
        let (status, _, text) = post_raw(&state, "{").await;
        assert_eq!((status, text.as_str()), (StatusCode::BAD_REQUEST, NOT_JSON));
        let (status, _, _) = post_raw(
            &state,
            &format!(r#"{{"action":{{"type":"cancel","cancels":"x"}},"user":"{OWNER}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "a mistyped field");
    }

    /// A signature that holds is the only way through: a signed noop from a
    /// registered key answers the no-data success and spends its nonce; the
    /// keyless harness shape does the same on its named user.
    #[tokio::test]
    async fn the_noop_goes_through_the_signature_and_spends_its_nonce() {
        let state = builder_state();
        let owner: Address = OWNER.parse().unwrap();
        let (agent, agent_address) = key(0x11);
        let nonce = crate::now_ms();
        let noop = || r#"{"type":"noop"}"#.to_string();

        let out = post(&state, signed(&agent, &noop(), nonce)).await;
        assert_eq!(out["status"], "err");
        assert!(out["response"].as_str().unwrap().contains("does not exist"));

        state.sim.note_agent(owner, agent_address, "agent").unwrap();
        let out = post(&state, signed(&agent, &noop(), nonce)).await;
        assert_eq!(
            out,
            serde_json::json!({"status":"ok","response":{"type":"default"}})
        );
        let replay = post(&state, signed(&agent, &noop(), nonce)).await;
        assert_eq!(replay["response"], "Nonce already used.");

        let harness = format!(
            r#"{{"action":{{"type":"noop"}},"user":"{OWNER}","nonce":{}}}"#,
            nonce + 5
        );
        assert_eq!(post(&state, harness.clone()).await["status"], "ok");
        assert_eq!(
            post(&state, harness).await["response"],
            "Nonce already used."
        );
    }

    /// `isCross` and the camelCase fields of the L1 actions parse as the wire
    /// spells them (`isCross: false` once read as the default `true`).
    #[test]
    fn the_leverage_action_reads_its_camel_case_fields() {
        let body: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"updateLeverage","asset":3,"isCross":false,"leverage":5},"user":"0x00000000000000000000000000000000000000e1"}"#,
        )
        .unwrap();
        let ExchangeAction::UpdateLeverage {
            asset,
            is_cross,
            leverage,
        } = body.action
        else {
            panic!("not an updateLeverage");
        };
        assert_eq!((asset, is_cross, leverage), (3, false, 5));
        let body: ExchangeBody = serde_json::from_str(
            r#"{"action":{"type":"updateLeverage","asset":3,"leverage":5},"user":"0x00000000000000000000000000000000000000e1"}"#,
        )
        .unwrap();
        assert!(matches!(
            body.action,
            ExchangeAction::UpdateLeverage { is_cross: true, .. }
        ));
    }

    /// Sign `action` (its raw JSON, L1 or user-signed) with `key`, as the
    /// venue's clients do.
    fn signed(key: &k256::ecdsa::SigningKey, action: &str, nonce: u64) -> String {
        let digest = crate::api::signing::signing_digest(action, nonce, None, None).unwrap();
        let (signature, recovery) = key.sign_prehash_recoverable(digest.as_slice()).unwrap();
        let bytes = signature.to_bytes();
        let (r, s) = (
            alloy_primitives::hex::encode(&bytes[..32]),
            alloy_primitives::hex::encode(&bytes[32..]),
        );
        let v = 27 + recovery.to_byte();
        format!(
            r#"{{"action":{action},"nonce":{nonce},"signature":{{"r":"0x{r}","s":"0x{s}","v":{v}}},"vaultAddress":null,"expiresAfter":null}}"#
        )
    }

    /// A user-signed action: its fields after the chain, in the order the
    /// client writes them.
    fn user_signed(tag: &str, fields: &str) -> String {
        format!(
            r#"{{"type":"{tag}","hyperliquidChain":"Mainnet","signatureChainId":"0x66eee",{fields}}}"#
        )
    }

    const TOKEN: &str = "USDC:0x6d1e7cde53ba9467b783cb7c530ce054";

    /// The spot registry with USDC alone (token 0).
    fn usdc_registry() -> crate::wire::SpotMeta {
        use crate::wire::{SpotMeta, SpotToken};
        SpotMeta {
            tokens: vec![SpotToken {
                name: "USDC".into(),
                sz_decimals: 8,
                wei_decimals: 8,
                index: 0,
                is_canonical: true,
                evm_contract: None,
                token_id: None,
                full_name: None,
                deployer_trading_fee_share: Decimal::ZERO,
            }],
            universe: vec![],
        }
    }

    /// The default dex (`ETH`, a market) and the builder dex `xyz` (`xyz:TSLA`,
    /// asset id 110000), the spot registry (`USDC`), and a mark and a
    /// one-level book on both.
    fn dex_state() -> Arc<ApiState> {
        let feed = crate::feed::MarketFeed::memory(vec!["ETH".into(), "xyz:TSLA".into()]);
        let now = crate::now_ms();
        feed.put_meta(crate::sim::meta_with("ETH", 10), now);
        feed.put_perp_dexs(vec![None, Some(perp_dex("xyz"))]);
        feed.put_dex_meta(
            "xyz".to_string(),
            crate::sim::meta_of(&[("xyz:TSLA", 10)]),
            now,
        );
        let level = |px: &str| Level {
            px: px.parse().unwrap(),
            sz: "50".parse().unwrap(),
            n: 1,
        };
        for (coin, bid, ask) in [("ETH", "100", "101"), ("xyz:TSLA", "200", "201")] {
            feed.put_ctx(coin.into(), test_ctx(ask, ask), now);
            feed.put_book(
                L2Book {
                    coin: coin.into(),
                    time: 0,
                    levels: [vec![level(bid)], vec![level(ask)]],
                    spread: None,
                },
                now,
            );
        }
        feed.put_oi_caps(Vec::new());
        feed.put_oi_caps_of("xyz", Vec::new());
        feed.put_spot_meta(usdc_registry());
        let sim = Arc::new(crate::sim::Sim::memory());
        let api = crate::feed::rest::RestClient::new(&["http://127.0.0.1:1".to_string()]).unwrap();
        Arc::new(
            ApiState::new(
                Arc::new(feed),
                sim,
                api,
                None,
                None,
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(1),
            )
            .unwrap(),
        )
    }

    /// Fund `who` with `amount` in a wallet: spot USDC, or a perp dex's ledger.
    fn fund(state: &ApiState, who: Address, class: DealClass, dex: Option<&str>, amount: &str) {
        state
            .sim
            .deal(
                &state.feed,
                &Deal {
                    address: who,
                    class,
                    coin: None,
                    dex: dex.map(str::to_string),
                    amount: amount.parse().unwrap(),
                },
            )
            .unwrap();
    }

    fn ledger(state: &ApiState, who: Address, dex: &str) -> Decimal {
        state
            .sim
            .account(who)
            .unwrap()
            .map_or(Decimal::ZERO, |a| a.perp_balance(dex))
    }

    fn spot_usdc(state: &ApiState, who: Address) -> Decimal {
        state
            .sim
            .account(who)
            .unwrap()
            .and_then(|a| a.spot.get(&0).map(|e| e.total))
            .unwrap_or_default()
    }

    fn dec(text: &str) -> Decimal {
        text.parse().unwrap()
    }

    /// The signer's own transfers, signed as a TypeScript client signs them:
    /// the spot wallet to the default dex and back, to another account
    /// (`spotSend` on spot, `usdSend` on the default dex), and an
    /// `sendAsset` back to spot. Each answers the no-data success and moves
    /// exactly its two ledgers (the per-dex legs are the transfer matrix's
    /// cases).
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one request/response script: splitting it would hide the order of the moves
    async fn user_signed_transfers_move_the_signers_ledgers() {
        use crate::sim::DealClass::Spot;
        let state = dex_state();
        let (me_key, me) = key(0x11);
        let (_, other) = key(0x22);
        fund(&state, me, Spot, None, "1000");
        let nonce = crate::now_ms();
        let ok = |out: &serde_json::Value| assert_eq!(out["status"], "ok", "{out}");

        // usdClassTransfer: spot to the default dex, and part of it back.
        let action = user_signed(
            "usdClassTransfer",
            &format!(r#""amount":"100","toPerp":true,"nonce":{nonce}"#),
        );
        ok(&post(&state, signed(&me_key, &action, nonce)).await);
        assert_eq!(
            (ledger(&state, me, ""), spot_usdc(&state, me)),
            (dec("100"), dec("900"))
        );
        let n = nonce + 1;
        let action = user_signed(
            "usdClassTransfer",
            &format!(r#""amount":"30","toPerp":false,"nonce":{n}"#),
        );
        ok(&post(&state, signed(&me_key, &action, n)).await);
        assert_eq!(
            (ledger(&state, me, ""), spot_usdc(&state, me)),
            (dec("70"), dec("930"))
        );
        // More than the dex holds is refused, and nothing moves.
        let n = nonce + 2;
        let action = user_signed(
            "usdClassTransfer",
            &format!(r#""amount":"500","toPerp":false,"nonce":{n}"#),
        );
        let out = post(&state, signed(&me_key, &action, n)).await;
        assert_eq!(out["status"], "err", "{out}");
        assert_eq!(ledger(&state, me, ""), dec("70"));

        // sendAsset: the default dex back to the spot wallet.
        let n = nonce + 3;
        let action = user_signed(
            "sendAsset",
            &format!(
                r#""destination":"{me:#x}","sourceDex":"","destinationDex":"spot","token":"USDC","amount":"20","fromSubAccount":"","nonce":{n}"#
            ),
        );
        ok(&post(&state, signed(&me_key, &action, n)).await);
        assert_eq!(
            (ledger(&state, me, ""), spot_usdc(&state, me)),
            (dec("50"), dec("950"))
        );
        // An unknown destination dex is refused by name.
        let n = nonce + 4;
        let action = user_signed(
            "sendAsset",
            &format!(
                r#""destination":"{me:#x}","sourceDex":"","destinationDex":"nope","token":"USDC","amount":"1","fromSubAccount":"","nonce":{n}"#
            ),
        );
        let out = post(&state, signed(&me_key, &action, n)).await;
        assert!(
            out["response"].as_str().unwrap().contains("`nope`"),
            "{out}"
        );

        // spotSend: spot to another account; usdSend: default dex to another.
        let n = nonce + 5;
        let action = user_signed(
            "spotSend",
            &format!(r#""destination":"{other:#x}","token":"{TOKEN}","amount":"25","time":{n}"#),
        );
        ok(&post(&state, signed(&me_key, &action, n)).await);
        // `other` is new: credited in full, and the sender (funded by a
        // helper) owes no activation.
        assert_eq!(
            (spot_usdc(&state, me), spot_usdc(&state, other)),
            (dec("925"), dec("25"))
        );
        let n = nonce + 6;
        let action = user_signed(
            "usdSend",
            &format!(r#""destination":"{other:#x}","amount":"20","time":{n}"#),
        );
        ok(&post(&state, signed(&me_key, &action, n)).await);
        assert_eq!(
            (ledger(&state, me, ""), ledger(&state, other, "")),
            (dec("30"), dec("20"))
        );
        // An unknown token is refused by name.
        let n = nonce + 7;
        let action = user_signed(
            "spotSend",
            &format!(r#""destination":"{other:#x}","token":"NOPE","amount":"1","time":{n}"#),
        );
        let out = post(&state, signed(&me_key, &action, n)).await;
        assert!(
            out["response"].as_str().unwrap().contains("unknown token"),
            "{out}"
        );
    }

    /// Over `/exchange`, an account created by an inbound transfer is credited
    /// in full and pays the 1 USDC activation on its first outbound transfer:
    /// refused by name when the balance cannot cover amount plus fee (nothing
    /// moves), applied once it can, free the second time. A replayed nonce, a
    /// sub-account source and a token's system address as destination are
    /// refused.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one request/response script: splitting it would hide the order of the sends
    async fn an_accounts_first_outbound_transfer_pays_the_activation_fee_once() {
        use crate::sim::DealClass::Spot;
        let state = dex_state();
        let (me_key, me) = key(0x11);
        let (other_key, other) = key(0x22);
        fund(&state, me, Spot, None, "30");
        let nonce = crate::now_ms();
        let spot_send = |to: Address, amount: &str, n: u64| {
            user_signed(
                "spotSend",
                &format!(
                    r#""destination":"{to:#x}","token":"{TOKEN}","amount":"{amount}","time":{n}"#
                ),
            )
        };

        // `me` is funded by a helper: sending 10 to a new account costs it nothing extra.
        let action = spot_send(other, "10", nonce);
        let out = post(&state, signed(&me_key, &action, nonce)).await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(
            (spot_usdc(&state, me), spot_usdc(&state, other)),
            (dec("20"), dec("10"))
        );

        // `other` owes the activation: 10 of its 10 is refused, 9 goes.
        let n = nonce + 1;
        let action = spot_send(me, "10", n);
        let out = post(&state, signed(&other_key, &action, n)).await;
        assert_eq!(out["status"], "err", "{out}");
        assert!(
            out["response"].as_str().unwrap().contains("activation fee"),
            "{out}"
        );
        assert_eq!(spot_usdc(&state, other), dec("10"));
        let n = nonce + 2;
        let action = spot_send(me, "9", n);
        let body = signed(&other_key, &action, n);
        assert_eq!(post(&state, body.clone()).await["status"], "ok");
        assert_eq!(
            (spot_usdc(&state, other), spot_usdc(&state, me)),
            (dec("0"), dec("29"))
        );
        // A replay of the signed body is refused.
        assert_eq!(post(&state, body).await["status"], "err", "a replay");
        assert_eq!(spot_usdc(&state, me), dec("29"));

        // The Core to EVM leg is not an /exchange destination in the sim, for
        // a token's system address or HYPE's.
        for (i, system) in [
            "0x2000000000000000000000000000000000000000",
            "0x2222222222222222222222222222222222222222",
        ]
        .into_iter()
        .enumerate()
        {
            let n = nonce + 10 + i as u64;
            let action = user_signed(
                "spotSend",
                &format!(r#""destination":"{system}","token":"{TOKEN}","amount":"1","time":{n}"#),
            );
            let out = post(&state, signed(&me_key, &action, n)).await;
            assert!(
                out["response"].as_str().unwrap().contains("system address"),
                "{out}"
            );
        }
        // A sub-account source is refused by name.
        let n = nonce + 4;
        let action = user_signed(
            "sendAsset",
            &format!(
                r#""destination":"{other:#x}","sourceDex":"spot","destinationDex":"spot","token":"USDC","amount":"1","fromSubAccount":"{me:#x}","nonce":{n}"#
            ),
        );
        let out = post(&state, signed(&me_key, &action, n)).await;
        assert!(
            out["response"].as_str().unwrap().contains("fromSubAccount"),
            "{out}"
        );
    }

    /// The user-signed actions are the account's own: an API wallet signing
    /// one is refused (and moves nothing), a key with no account is the venue's
    /// "does not exist", and `withdraw3` is refused in the venue's envelope.
    #[tokio::test]
    async fn a_user_signed_action_needs_the_accounts_own_key() {
        use crate::sim::DealClass::Spot;
        let state = dex_state();
        let (_, me) = key(0x11);
        let (agent_key, agent) = key(0x33);
        let (stranger_key, stranger) = key(0x44);
        fund(&state, me, Spot, None, "100");
        state.sim.note_agent(me, agent, "bot").unwrap();
        let nonce = crate::now_ms();
        let action = user_signed(
            "usdClassTransfer",
            &format!(r#""amount":"10","toPerp":true,"nonce":{nonce}"#),
        );

        let out = post(&state, signed(&agent_key, &action, nonce)).await;
        assert_eq!(out["status"], "err");
        assert!(
            out["response"]
                .as_str()
                .unwrap()
                .contains("account's own key"),
            "{out}"
        );
        assert_eq!(spot_usdc(&state, me), dec("100"));

        let out = post(&state, signed(&stranger_key, &action, nonce)).await;
        assert_eq!(
            out["response"],
            format!("User or API Wallet {stranger:#x} does not exist.")
        );

        let withdraw = user_signed(
            "withdraw3",
            &format!(r#""destination":"{me:#x}","amount":"1","time":{nonce}"#),
        );
        let (own_key, _) = key(0x11);
        let out = post(&state, signed(&own_key, &withdraw, nonce)).await;
        assert_eq!(out["status"], "err");
        assert!(
            out["response"].as_str().unwrap().contains("withdraw3"),
            "{out}"
        );
    }

    /// The account setup: `approveAgent` registers the wallet (which then
    /// signs L1 actions for the account), `approveBuilderFee` sets the
    /// builder's maximum in tenths of a basis point (and 0% revokes it),
    /// `userSetAbstraction` and `agentSetAbstraction` set the mode and refuse
    /// portfolio margin.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one request/response script: splitting it would hide the order of the setup
    async fn the_account_setup_actions_register_what_they_name() {
        use crate::sim::DealClass::Spot;
        use crate::wire::UserAbstraction;
        let state = dex_state();
        let (me_key, me) = key(0x11);
        let (agent_key, agent) = key(0x33);
        let builder: Address = BUILDER.parse().unwrap();
        fund(&state, me, Spot, None, "100");
        let next = std::cell::Cell::new(crate::now_ms());
        // The next body: its nonce is the action's and the envelope's.
        let send = |key: &k256::ecdsa::SigningKey, build: &dyn Fn(u64) -> String| {
            let n = next.get() + 1;
            next.set(n);
            signed(key, &build(n), n)
        };

        // approveAgent: the wallet then signs for the account.
        let body = send(&me_key, &|n| {
            user_signed(
                "approveAgent",
                &format!(r#""agentAddress":"{agent:#x}","agentName":"bot","nonce":{n}"#),
            )
        });
        let out = post(&state, body).await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(state.sim.signer_account(agent).unwrap(), Some(me));
        let out = post(
            &state,
            send(&agent_key, &|_| r#"{"type":"noop"}"#.to_string()),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");

        // approveBuilderFee.
        let fee = |rate: &'static str| {
            move |n: u64| {
                user_signed(
                    "approveBuilderFee",
                    &format!(r#""maxFeeRate":"{rate}","builder":"{builder:#x}","nonce":{n}"#),
                )
            }
        };
        let out = post(&state, send(&me_key, &fee("0.01%"))).await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(state.sim.max_builder_fee(me, builder).unwrap(), Some(10));
        let out = post(&state, send(&me_key, &fee("0.0005%"))).await;
        assert!(
            out["response"].as_str().unwrap().contains("0.001%"),
            "{out}"
        );
        let out = post(&state, send(&me_key, &fee("2%"))).await;
        assert!(
            out["response"].as_str().unwrap().contains("1% cap"),
            "{out}"
        );
        let out = post(&state, send(&me_key, &fee("0%"))).await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(state.sim.max_builder_fee(me, builder).unwrap(), Some(0));

        // userSetAbstraction.
        let set = |user: Address, mode: &'static str| {
            move |n: u64| {
                user_signed(
                    "userSetAbstraction",
                    &format!(r#""user":"{user:#x}","abstraction":"{mode}","nonce":{n}"#),
                )
            }
        };
        let out = post(&state, send(&me_key, &set(me, "unifiedAccount"))).await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(
            state.sim.account(me).unwrap().unwrap().mode,
            UserAbstraction::UnifiedAccount
        );
        let out = post(&state, send(&me_key, &set(me, "portfolioMargin"))).await;
        assert!(
            out["response"]
                .as_str()
                .unwrap()
                .contains("portfolio margin is not modeled"),
            "{out}"
        );
        let out = post(&state, send(&me_key, &set(builder, "disabled"))).await;
        assert!(
            out["response"].as_str().unwrap().contains("signed it"),
            "{out}"
        );
        // an abstraction the venue does not define is a 422, before the signature
        let (status, _, _) = post_raw(&state, &send(&me_key, &set(me, "bogus"))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        // agentSetAbstraction (an L1 action: the agent signs it).
        let agent_sets = |code: &'static str| {
            move |_: u64| format!(r#"{{"type":"agentSetAbstraction","abstraction":"{code}"}}"#)
        };
        let out = post(&state, send(&agent_key, &agent_sets("i"))).await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(
            state.sim.account(me).unwrap().unwrap().mode,
            UserAbstraction::Disabled
        );
        let out = post(&state, send(&agent_key, &agent_sets("p"))).await;
        assert!(
            out["response"]
                .as_str()
                .unwrap()
                .contains("portfolio margin is not modeled"),
            "{out}"
        );
        let out = post(&state, send(&agent_key, &agent_sets("u"))).await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(
            state.sim.account(me).unwrap().unwrap().mode,
            UserAbstraction::UnifiedAccount
        );
    }

    /// The keyless harness shape stays what the first port left: `user` named,
    /// no signature, and the new actions act as that user (the transfer
    /// included).
    #[tokio::test]
    async fn the_keyless_harness_shape_acts_as_the_named_user() {
        use crate::sim::DealClass::Spot;
        let state = dex_state();
        let who: Address = OWNER.parse().unwrap();
        fund(&state, who, Spot, None, "100");
        let body = format!(
            r#"{{"action":{{"type":"usdClassTransfer","amount":"40","toPerp":true}},"user":"{OWNER}"}}"#
        );
        let out = post(&state, body).await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(ledger(&state, who, ""), dec("40"));
    }

    /// The venue disables `usdClassTransfer` on a unified account (one USDC
    /// pool); the signed API answers its own message, in both directions, and
    /// moves nothing.
    #[tokio::test]
    async fn a_unified_account_gets_the_venues_class_transfer_refusal() {
        use crate::sim::DealClass::Spot;
        let state = dex_state();
        let who: Address = OWNER.parse().unwrap();
        fund(&state, who, Spot, None, "100");
        state
            .sim
            .set_mode(
                who,
                crate::wire::UserAbstraction::UnifiedAccount,
                &state.feed,
            )
            .unwrap();
        for to_perp in ["true", "false"] {
            let out = act(
                &state,
                &format!(r#"{{"type":"usdClassTransfer","amount":"40","toPerp":{to_perp}}}"#),
            )
            .await;
            assert_eq!(out["status"], "err", "{out}");
            assert!(
                out["response"]
                    .as_str()
                    .unwrap()
                    .contains("Action disabled when unified account is active"),
                "{out}"
            );
        }
        assert_eq!(ledger(&state, who, ""), dec("0"), "nothing moved");
    }

    /// The venue disables the API-signed `spotSend` on a unified account (any
    /// token, any destination, a token's system address included) but accepts
    /// `sendAsset`. The refusal comes first and moves nothing.
    #[tokio::test]
    async fn a_unified_account_cannot_spot_send_but_can_send_asset() {
        use crate::sim::DealClass::Spot;
        let state = dex_state();
        let who: Address = OWNER.parse().unwrap();
        let (_, other) = key(0x22);
        fund(&state, who, Spot, None, "100");
        state
            .sim
            .set_mode(
                who,
                crate::wire::UserAbstraction::UnifiedAccount,
                &state.feed,
            )
            .unwrap();
        for destination in [
            format!("{other:#x}"),
            "0x2000000000000000000000000000000000000000".to_string(),
        ] {
            let out = act(
                &state,
                &format!(
                    r#"{{"type":"spotSend","destination":"{destination}","token":"{TOKEN}","amount":"10"}}"#
                ),
            )
            .await;
            assert_eq!(out["status"], "err", "{out}");
            assert_eq!(
                out["response"].as_str().unwrap(),
                "Action disabled when unified account is active",
                "{out}"
            );
        }
        assert_eq!(spot_usdc(&state, who), dec("100"), "nothing moved");

        let out = act(
            &state,
            &format!(
                r#"{{"type":"sendAsset","destination":"{other:#x}","sourceDex":"spot","destinationDex":"spot","token":"{TOKEN}","amount":"10"}}"#
            ),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(
            (spot_usdc(&state, who), spot_usdc(&state, other)),
            (dec("90"), dec("10"))
        );
    }

    const TSLA: u32 = 110_000;

    fn order_json(asset: u32, buy: bool, px: &str, tif: &str, cloid: Option<&str>) -> String {
        let c = cloid.map_or(String::new(), |c| format!(r#","c":"{c}""#));
        format!(
            r#"{{"a":{asset},"b":{buy},"p":"{px}","s":"1","r":false,"t":{{"limit":{{"tif":"{tif}"}}}}{c}}}"#
        )
    }

    /// A harness-shape action by `OWNER`.
    async fn act(state: &Arc<ApiState>, action: &str) -> serde_json::Value {
        post(state, format!(r#"{{"action":{action},"user":"{OWNER}"}}"#)).await
    }

    /// Asset ids of a builder dex work on every action that takes one, against
    /// the account's one USDC ledger: an order fills there, a resting one is
    /// modified, batch-modified and cancelled by oid and by cloid, leverage
    /// and isolated margin apply to the coin, and the dead-man's switch is
    /// accepted.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one request/response script: splitting it would hide the order of the actions
    async fn builder_dex_asset_ids_work_on_every_asset_action() {
        use crate::sim::DealClass::Perp;
        let state = dex_state();
        let who: Address = OWNER.parse().unwrap();
        fund(&state, who, Perp, Some("xyz"), "1000");
        let orders =
            |envelope: &serde_json::Value| envelope["response"]["data"]["statuses"][0].clone();

        // A market-style IOC fills against xyz:TSLA's book.
        let out = act(
            &state,
            &format!(
                r#"{{"type":"order","orders":[{}],"grouping":"na"}}"#,
                order_json(TSLA, true, "205", "Ioc", None)
            ),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(orders(&out)["filled"]["totalSz"], "1.0", "{out}");
        assert!(ledger(&state, who, "xyz") < dec("1000"), "the fee left the ledger");
        assert!(state
            .sim
            .account(who)
            .unwrap()
            .unwrap()
            .positions
            .contains_key("xyz:TSLA"));

        // Leverage and isolated margin on the coin.
        let out = act(
            &state,
            &format!(r#"{{"type":"updateLeverage","asset":{TSLA},"isCross":false,"leverage":5}}"#),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        let setting = state.sim.account(who).unwrap().unwrap().leverage_settings["xyz:TSLA"];
        assert_eq!(
            (setting.mode, setting.leverage),
            (crate::wire::LeverageType::Isolated, 5)
        );
        let margin = |state: &ApiState| {
            state.sim.account(who).unwrap().unwrap().positions["xyz:TSLA"].isolated_margin
        };
        let (before, ledger_before) = (margin(&state), ledger(&state, who, "xyz"));
        let out = act(
            &state,
            &format!(
                r#"{{"type":"updateIsolatedMargin","asset":{TSLA},"isBuy":true,"ntli":5000000}}"#
            ),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(
            margin(&state),
            before + dec("5"),
            "the position holds 5 more"
        );
        assert_eq!(
            ledger(&state, who, "xyz"),
            ledger_before - dec("5"),
            "taken from the ledger"
        );

        // Rest, modify, batch-modify, cancel by oid and by cloid.
        let cloid = "0x00000000000000000000000000000abc";
        let out = act(
            &state,
            &format!(
                r#"{{"type":"order","orders":[{},{}],"grouping":"na"}}"#,
                order_json(TSLA, true, "150", "Gtc", None),
                order_json(TSLA, true, "149", "Gtc", Some(cloid))
            ),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        let resting = state.sim.resting_orders(Some(who)).unwrap();
        assert_eq!(resting.len(), 2);
        assert!(resting.iter().all(|r| r.coin == "xyz:TSLA"));
        let oid = out["response"]["data"]["statuses"][0]["resting"]["oid"]
            .as_u64()
            .unwrap();
        let out = act(
            &state,
            &format!(
                r#"{{"type":"modify","oid":{oid},"order":{}}}"#,
                order_json(TSLA, true, "151", "Gtc", None)
            ),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        let resting = state.sim.resting_orders(Some(who)).unwrap();
        assert!(
            resting.iter().any(|r| r.px == dec("151")) && resting.len() == 2,
            "{resting:?}"
        );
        let moved = resting.iter().find(|r| r.px == dec("151")).unwrap().oid;
        let out = act(
            &state,
            &format!(
                r#"{{"type":"batchModify","modifies":[{{"oid":{moved},"order":{}}}]}}"#,
                order_json(TSLA, true, "152", "Gtc", None)
            ),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        let resting = state.sim.resting_orders(Some(who)).unwrap();
        let moved = resting
            .iter()
            .find(|r| r.px == dec("152"))
            .expect("batchModify moved it")
            .oid;
        let out = act(
            &state,
            &format!(r#"{{"type":"cancel","cancels":[{{"a":{TSLA},"o":{moved}}}]}}"#),
        )
        .await;
        assert_eq!(out["response"]["data"]["statuses"][0], "success", "{out}");
        let out = act(
            &state,
            &format!(
                r#"{{"type":"cancelByCloid","cancels":[{{"asset":{TSLA},"cloid":"{cloid}"}}]}}"#
            ),
        )
        .await;
        assert_eq!(out["response"]["data"]["statuses"][0], "success", "{out}");
        assert!(state.sim.resting_orders(Some(who)).unwrap().is_empty());

        // The dead-man's switch is accepted against the account's resting
        // orders on the dex too: it closes them when its time comes.
        let out = act(
            &state,
            &order_action_json(&order_json(TSLA, true, "150", "Gtc", None)),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        let when = crate::now_ms() + 30_000;
        let out = act(
            &state,
            &format!(r#"{{"type":"scheduleCancel","time":{when}}}"#),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(state.sim.resting_orders(Some(who)).unwrap().len(), 1);
    }

    fn order_action_json(order: &str) -> String {
        format!(r#"{{"type":"order","orders":[{order}],"grouping":"na"}}"#)
    }

    /// A venue stand-in answering `l2Book` for any coin, counting the reads.
    async fn l2_server(reads: Arc<std::sync::atomic::AtomicUsize>) -> String {
        use axum::extract::State;
        use axum::routing::post;
        async fn info(
            State(reads): State<Arc<std::sync::atomic::AtomicUsize>>,
            axum::Json(req): axum::Json<serde_json::Value>,
        ) -> axum::Json<serde_json::Value> {
            reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let coin = req["coin"].as_str().unwrap_or_default().to_string();
            axum::Json(serde_json::json!({
                "coin": coin, "time": 1,
                "levels": [[{"px": "199", "sz": "5", "n": 1}], [{"px": "200", "sz": "5", "n": 1}]]
            }))
        }
        let app = axum::Router::new()
            .route("/info", post(info))
            .with_state(reads);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// An order on a coin whose book has not landed reads it on demand
    /// before it fills: the mock venue serves `xyz:TSLA`'s `l2Book`, and the
    /// order fills against that book.
    #[tokio::test]
    async fn an_order_on_an_unconfigured_coin_reads_its_book_first() {
        use crate::sim::DealClass::Perp;
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let url = l2_server(hits.clone()).await;
        let feed = crate::feed::MarketFeed::memory(vec!["ETH".into()]);
        let now = crate::now_ms();
        feed.put_meta(crate::sim::meta_with("ETH", 10), now);
        feed.put_perp_dexs(vec![None, Some(perp_dex("xyz"))]);
        feed.put_dex_meta(
            "xyz".to_string(),
            crate::sim::meta_of(&[("xyz:TSLA", 10)]),
            now,
        );
        feed.put_ctx("xyz:TSLA".into(), test_ctx("200", "200"), now);
        feed.put_oi_caps(Vec::new());
        feed.put_oi_caps_of("xyz", Vec::new());
        let api = crate::feed::rest::RestClient::new(&[url]).unwrap();
        feed.attach_rest(api.clone());
        let state = Arc::new(
            ApiState::new(
                Arc::new(feed),
                Arc::new(crate::sim::Sim::memory()),
                api,
                None,
                None,
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(1),
            )
            .unwrap(),
        );
        let who: Address = OWNER.parse().unwrap();
        fund(&state, who, Perp, Some("xyz"), "1000");
        assert!(
            state.feed.book("xyz:TSLA", crate::now_ms()).is_none(),
            "no book yet"
        );
        let out = act(
            &state,
            &order_action_json(&order_json(TSLA, true, "305", "Ioc", None)),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(
            out["response"]["data"]["statuses"][0]["filled"]["avgPx"], "200.0",
            "{out}"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one l2Book read"
        );
    }
}
