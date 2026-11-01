//! The venue's websocket lane (the sim serves "info + WS" on its
//! info port): the channels the venue pushes — `allMids`, `activeAssetCtx`,
//! `l2Book`, and `webData2` — fed from the same composed cache the REST reads
//! serve, so a subscriber never sees a different mark/book/account than the
//! account plane used.
//!
//! Protocol (`feed::ws`'s vocabulary, the client's own): the client subscribes
//! with `{"method":"subscribe","subscription":{"type":…}}`, the server acks
//! with a `subscriptionResponse` frame and pushes `{"channel":…,"data":…}` on
//! each configured account-tick while the subscription is live; `{"method":"ping"}` answers
//! `{"channel":"pong"}`. An unsupported channel (or an unknown coin) is
//! answered with the venue's `error` frame — never silence.
//!
//! `l2Book` pushes the composed book exactly as `/info`'s `l2Book` serves it
//! (`{"coin","time","levels"}`, the live book stamped with its last update,
//! a scripted book with the scenario's present). `webData2` pushes the sim
//! account's served state: `clearinghouseState`, `spotState`, `assetCtxs`
//! (omitted while the meta is warming — never an invented empty list), and
//! `user`.
use super::ApiState;
use crate::wire::{
    AllMids, AssetCtx, ClearinghouseState, L2Book, MarginSummary, SpotClearinghouseState,
};
use alloy_primitives::Address;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// One subscription the sim serves. `l2Book` pushes the composed book on the
/// session's tick; `webData2` pushes the account's served clearinghouse/spot
/// state (the real venue's own account channel).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Subscription {
    #[serde(rename = "allMids")]
    AllMids,
    #[serde(rename = "activeAssetCtx")]
    ActiveAssetCtx { coin: String },
    #[serde(rename = "l2Book")]
    L2Book { coin: String },
    #[serde(rename = "webData2")]
    WebData2 { user: Address },
    /// The real venue's per-dex account state channel (`{dex, user,
    /// clearinghouseState}`), the current vocabulary beside the legacy
    /// `webData2` alias.
    #[serde(rename = "clearinghouseState")]
    ClearinghouseState { user: Address },
    /// The real venue's open-orders channel (`{dex, user, orders}`).
    #[serde(rename = "openOrders")]
    OpenOrders { user: Address },
    /// The real venue's spot-state channel (`{user, spotState}`).
    #[serde(rename = "spotState")]
    SpotState { user: Address },
}

/// One client request (the client's `Subscribe`/`Ping` shapes, plus
/// `unsubscribe` — the venue's own method set).
#[derive(Debug, Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub(crate) enum WsRequest {
    Subscribe { subscription: Subscription },
    Unsubscribe { subscription: Subscription },
    Ping,
}

/// One server frame. The shapes mirror `feed::ws`'s parse types; the
/// round-trip test parses every variant back through the client's own parser.
#[derive(Debug, Serialize)]
#[serde(tag = "channel")]
pub(crate) enum WsPush {
    #[serde(rename = "allMids")]
    AllMids { data: AllMidsData },
    #[serde(rename = "activeAssetCtx")]
    ActiveAssetCtx { data: CtxData },
    #[serde(rename = "l2Book")]
    L2Book { data: L2Book },
    #[serde(rename = "webData2")]
    WebData2 { data: WebData2Data },
    #[serde(rename = "clearinghouseState")]
    ClearinghouseState { data: ClearinghouseStateData },
    #[serde(rename = "openOrders")]
    OpenOrders { data: OpenOrdersData },
    #[serde(rename = "spotState")]
    SpotState { data: SpotStateData },
    #[serde(rename = "subscriptionResponse")]
    SubscriptionResponse { data: SubscriptionAck },
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "error")]
    Error { data: String },
}

#[derive(Debug, Serialize)]
pub(crate) struct AllMidsData {
    mids: AllMids,
}

#[derive(Debug, Serialize)]
pub(crate) struct CtxData {
    coin: String,
    ctx: AssetCtx,
}

/// The real venue's `webData2` account payload: the served clearinghouse and
/// spot states, the market contexts, and the user. Optional fields the sim
/// cannot serve yet are omitted rather than fabricated (`assetCtxs` while the
/// meta is warming).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebData2Data {
    user: Address,
    clearinghouse_state: ClearinghouseState,
    spot_state: SpotClearinghouseState,
    #[serde(skip_serializing_if = "Option::is_none")]
    asset_ctxs: Option<Vec<AssetCtx>>,
}

/// The real venue's `clearinghouseState` channel payload.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClearinghouseStateData {
    /// The perp dex; the empty string is the first (default) dex.
    dex: String,
    user: Address,
    clearinghouse_state: ClearinghouseState,
}

/// The real venue's `openOrders` channel payload.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenOrdersData {
    dex: String,
    user: Address,
    orders: Vec<super::info::OpenOrder>,
}

/// The real venue's `spotState` channel payload.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpotStateData {
    user: Address,
    spot_state: SpotState,
}

/// The real venue's `spotState` body: balances only (the WS shape omits the
/// REST read's `tokenToAvailableAfterMaintenance`).
#[derive(Debug, Serialize)]
pub(crate) struct SpotState {
    balances: Vec<crate::wire::SpotBalance>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SubscriptionAck {
    method: &'static str,
    subscription: Subscription,
}

/// `GET /ws` (upgrade): one session per subscriber.
pub async fn ws(
    State(state): State<Arc<ApiState>>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| session(state, socket))
}

/// One connection: read requests, push the subscribed channels on the 1 Hz
/// tick (the automaton's own cadence).
async fn session(state: Arc<ApiState>, mut socket: WebSocket) {
    let mut subs: Vec<Subscription> = Vec::new();
    let mut tick = tokio::time::interval(state.push_interval);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                // One clock read per tick: every channel in this tick serves
                // the same instant (a scripted book is stamped with it).
                let now = crate::now_ms();
                for sub in &subs {
                    let Some(frame) = push_frame(sub, &state, now) else { continue };
                    if socket.send(Message::Text(frame.into())).await.is_err() {
                        return;
                    }
                }
            }
            msg = socket.recv() => {
                let Some(Ok(Message::Text(text))) = msg else {
                    if msg.is_none() || matches!(msg, Some(Err(_))) { return; }
                    continue; // binary/ping/pong frames: nothing to answer
                };
                let reply = request_reply(&text, &mut subs, &state);
                let Ok(text) = serde_json::to_string(&reply) else { continue };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// One request's answer: the ack, the error, or the pong. A subscription is
/// added at most once; unsubscribing a subscription that is not live is still
/// acked (idempotent, the venue's own contract).
fn request_reply(text: &str, subs: &mut Vec<Subscription>, state: &ApiState) -> WsPush {
    let request = match serde_json::from_str::<WsRequest>(text) {
        Ok(request) => request,
        Err(e) => {
            return WsPush::Error {
                data: format!("unparsable request: {e}"),
            };
        }
    };
    match request {
        WsRequest::Ping => WsPush::Pong,
        WsRequest::Subscribe { subscription } => {
            if let Err(reason) = unsupported(&subscription, state) {
                return WsPush::Error { data: reason };
            }
            if !subs.contains(&subscription) {
                subs.push(subscription.clone());
            }
            WsPush::SubscriptionResponse {
                data: SubscriptionAck {
                    method: "subscribe",
                    subscription,
                },
            }
        }
        WsRequest::Unsubscribe { subscription } => {
            subs.retain(|s| s != &subscription);
            WsPush::SubscriptionResponse {
                data: SubscriptionAck {
                    method: "unsubscribe",
                    subscription,
                },
            }
        }
    }
}

/// Why the sim cannot serve a subscription; `Err` = the refusal.
fn unsupported(sub: &Subscription, state: &ApiState) -> Result<(), String> {
    let feed = &state.feed;
    match sub {
        Subscription::AllMids => Ok(()),
        Subscription::ActiveAssetCtx { coin } if feed.mark(coin).is_some() => Ok(()),
        Subscription::ActiveAssetCtx { coin } => Err(format!(
            "coin `{coin}` is not in the venue's tracked markets"
        )),
        // A bookless coin is refused at subscribe time, never left silent:
        // the composed book is the same one `/info`'s `l2Book` serves.
        Subscription::L2Book { coin } if feed.book(coin, crate::now_ms()).is_some() => Ok(()),
        Subscription::L2Book { coin } => {
            Err(format!("coin `{coin}` has no book in the venue's feed"))
        }
        // The account plane is addressable from the API state: a touched account
        // is computed from the sim, an untouched one is served the venue's empty
        // shape (the same contract `/info`'s passthrough holds). Only a state
        // read fault refuses the subscription.
        Subscription::WebData2 { user }
        | Subscription::ClearinghouseState { user }
        | Subscription::OpenOrders { user }
        | Subscription::SpotState { user } => match state.sim.account(*user) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("account state for `{user}` is unavailable: {e}")),
        },
    }
}

/// The frame one subscription pushes, from the composed cache. `None` = the
/// coin has no ctx yet (the feed is warming; the tick retries) — a
/// serialization fault on these plain shapes is a bug, named here.
fn push_frame(sub: &Subscription, state: &ApiState, now_ms: u64) -> Option<String> {
    let feed = &state.feed;
    let push = match sub {
        Subscription::AllMids => WsPush::AllMids {
            data: AllMidsData {
                mids: feed.all_mids(),
            },
        },
        Subscription::ActiveAssetCtx { coin } => WsPush::ActiveAssetCtx {
            data: CtxData {
                coin: coin.clone(),
                ctx: crate::sim::account::ctx_of(feed, coin)?,
            },
        },
        // The composed book: scripted override over the live remainder, stamped
        // exactly as the REST read stamps it (`feed.book` owns that rule).
        Subscription::L2Book { coin } => match feed.book(coin, now_ms) {
            Some(book) => WsPush::L2Book { data: book },
            // A book that vanished after the ack is an error frame, not
            // silence (the subscriber must know its channel stopped serving).
            None => WsPush::Error {
                data: format!("coin `{coin}` has no book in the venue's feed"),
            },
        },
        // The account plane's four channels share one body; the channel name
        // only selects the frame shape.
        _ => account_push(sub, state)?,
    };
    match serde_json::to_string(&push) {
        Ok(frame) => Some(frame),
        Err(e) => {
            tracing::warn!(error = %e, "ws push serialization failed");
            None
        }
    }
}

/// The account-plane frame for one subscription. `None` only for a non-account
/// variant (the caller's match has already handled the market channels).
fn account_push(sub: &Subscription, state: &ApiState) -> Option<WsPush> {
    let push = match sub {
        Subscription::WebData2 { user } => match web_data2(state, *user) {
            Ok(data) => WsPush::WebData2 { data },
            Err(e) => WsPush::Error {
                data: format!("webData2 for `{user}` is unavailable: {e}"),
            },
        },
        Subscription::ClearinghouseState { user } => match account_states(state, *user) {
            Ok((clearinghouse_state, _)) => WsPush::ClearinghouseState {
                data: ClearinghouseStateData {
                    dex: String::new(),
                    user: *user,
                    clearinghouse_state,
                },
            },
            Err(e) => WsPush::Error {
                data: format!("clearinghouseState for `{user}` is unavailable: {e}"),
            },
        },
        Subscription::OpenOrders { user } => open_orders_push(*user, state),
        Subscription::SpotState { user } => spot_state_push(*user, state),
        Subscription::AllMids
        | Subscription::ActiveAssetCtx { .. }
        | Subscription::L2Book { .. } => return None,
    };
    Some(push)
}

/// The `openOrders` channel's frame: the sim's resting rows in the venue's
/// `{dex, user, orders}` shape.
fn open_orders_push(user: Address, state: &ApiState) -> WsPush {
    match state.sim.resting_orders(Some(user)) {
        Ok(rows) => WsPush::OpenOrders {
            data: OpenOrdersData {
                dex: String::new(),
                user,
                orders: rows
                    .into_iter()
                    .map(|r| super::info::OpenOrder {
                        coin: r.coin.clone(),
                        limit_px: r.px.to_string(),
                        oid: r.oid,
                        side: super::info::side_letter(r.side),
                        sz: r.sz.to_string(),
                        timestamp: r.placed_ms,
                    })
                    .collect(),
            },
        },
        Err(e) => WsPush::Error {
            data: format!("openOrders for `{user}` is unavailable: {e}"),
        },
    }
}

/// The `spotState` channel's frame: the same balances `/info` serves, in the
/// WS shape (which carries no `tokenToAvailableAfterMaintenance`).
fn spot_state_push(user: Address, state: &ApiState) -> WsPush {
    match account_states(state, user) {
        Ok((_, spot_state)) => WsPush::SpotState {
            data: SpotStateData {
                user,
                spot_state: SpotState {
                    balances: spot_state.balances,
                },
            },
        },
        Err(e) => WsPush::Error {
            data: format!("spotState for `{user}` is unavailable: {e}"),
        },
    }
}

/// The account's served `webData2` payload: the same clearinghouse read
/// `/info` serves (the observation-time rule included), the spot state, and
/// the market contexts when the meta has landed.
fn web_data2(state: &ApiState, user: Address) -> crate::error::Result<WebData2Data> {
    let (clearinghouse_state, spot_state) = account_states(state, user)?;
    let asset_ctxs = state
        .feed
        .meta_and_ctxs()
        .map(|crate::wire::MetaAndAssetCtxs(_, ctxs)| ctxs);
    Ok(WebData2Data {
        user,
        clearinghouse_state,
        spot_state,
        asset_ctxs,
    })
}

/// The account's served clearinghouse/spot states — the one body both the legacy
/// `webData2` payload and the current `clearinghouseState`/`spotState` channels
/// publish. An untouched address is served the venue's empty shape (the `/info`
/// passthrough's documented contract); the WS lane cannot pass through, so it
/// serves that same shape rather than refusing.
fn account_states(
    state: &ApiState,
    user: Address,
) -> crate::error::Result<(ClearinghouseState, SpotClearinghouseState)> {
    match state.sim.account(user)? {
        Some(account) => {
            let clearinghouse_state =
                super::info::clearinghouse_of(state, user)?.ok_or_else(|| {
                    crate::error::Error::NotFound {
                        what: format!("simulated account `{user}`"),
                    }
                })?;
            let meta = state.feed.meta();
            let spot_state = SpotClearinghouseState {
                balances: crate::sim::account::spot_balances(&account),
                token_to_available_after_maintenance:
                    crate::sim::account::available_after_maintenance(
                        &account,
                        &state.feed,
                        meta.as_ref(),
                    )?,
            };
            Ok((clearinghouse_state, spot_state))
        }
        None => Ok((
            empty_clearinghouse(state.sim.served_time(crate::now_ms())?),
            SpotClearinghouseState {
                balances: Vec::new(),
                token_to_available_after_maintenance: Vec::new(),
            },
        )),
    }
}

/// The venue's empty `clearinghouseState`: zero summaries, no positions, the
/// served clock. This is the shape the live venue returns for an untouched
/// address — not a fabricated position, and not a refusal.
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
        control_owned: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::ws::WsMessage;
    use crate::feed::MarketFeed;
    use crate::sim::{Deal, DealClass};
    use crate::wire::{test_ctx, Level, SpotMeta, SpotToken};
    use rust_decimal::Decimal;
    use std::time::Duration;

    fn api_state(feed: Arc<MarketFeed>) -> Arc<ApiState> {
        let api = crate::feed::rest::RestClient::new(&["http://127.0.0.1:1".to_string()]).unwrap();
        Arc::new(
            ApiState::new(
                feed,
                Arc::new(crate::sim::Sim::memory()),
                api,
                None,
                None,
                Duration::from_secs(45),
                Duration::from_secs(1),
            )
            .unwrap(),
        )
    }

    /// One two-sided book (the same shape `/info`'s `l2Book` serves).
    fn test_book(coin: &str, px: &str) -> L2Book {
        let bid: Decimal = px.parse().unwrap();
        L2Book {
            coin: coin.into(),
            time: 1,
            levels: [
                vec![Level {
                    px: bid,
                    sz: Decimal::ONE,
                    n: 1,
                }],
                vec![Level {
                    px: bid + Decimal::ONE,
                    sz: Decimal::ONE,
                    n: 1,
                }],
            ],
            spread: None,
        }
    }

    fn fund_perp(state: &ApiState, feed: &MarketFeed, user: Address) {
        state
            .sim
            .deal(
                feed,
                &Deal {
                    address: user,
                    class: DealClass::Perp,
                    coin: None,
                    amount: Decimal::from(1_000),
                },
            )
            .unwrap();
    }

    fn put_usdc(feed: &MarketFeed) {
        feed.put_spot_meta(SpotMeta {
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
        });
    }

    /// Every frame the server emits parses through the *client's own* parser:
    /// the two sides of the venue's websocket vocabulary in one test — a shape
    /// change on either side fails here, not on a live subscriber. The new
    /// channels' payloads are additionally deserialized through the same wire
    /// types `/info` serves.
    #[test]
    #[allow(clippy::too_many_lines)] // one frame-by-frame round-trip table: splitting it would hide the shape mapping
    fn pushes_round_trip_through_the_client_parser() {
        let feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        feed.put_meta(crate::sim::meta_with("BTC", 10), 1);
        feed.put_ctx("BTC".into(), test_ctx("100", "101"), 1);
        feed.put_book(test_book("BTC", "100"), 1_000);
        put_usdc(&feed);
        let state = api_state(feed.clone());
        let user = Address::repeat_byte(0xaa);
        fund_perp(&state, &feed, user);
        state
            .sim
            .deal(
                &feed,
                &Deal {
                    address: user,
                    class: DealClass::Spot,
                    coin: Some("USDC".into()),
                    amount: Decimal::from(25),
                },
            )
            .unwrap();

        let frames = vec![
            push_frame(&Subscription::AllMids, &state, 2_000).unwrap(),
            push_frame(
                &Subscription::ActiveAssetCtx { coin: "BTC".into() },
                &state,
                2_000,
            )
            .unwrap(),
            push_frame(&Subscription::L2Book { coin: "BTC".into() }, &state, 2_000).unwrap(),
            push_frame(&Subscription::WebData2 { user }, &state, 2_000).unwrap(),
            push_frame(&Subscription::ClearinghouseState { user }, &state, 2_000).unwrap(),
            push_frame(&Subscription::OpenOrders { user }, &state, 2_000).unwrap(),
            push_frame(&Subscription::SpotState { user }, &state, 2_000).unwrap(),
            serde_json::to_string(&WsPush::Pong).unwrap(),
            serde_json::to_string(&WsPush::Error { data: "x".into() }).unwrap(),
        ];
        for text in frames {
            let parsed: WsMessage = serde_json::from_str(&text).unwrap();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            match value["channel"].as_str().unwrap() {
                "allMids" => assert!(matches!(parsed, WsMessage::AllMids { .. })),
                "activeAssetCtx" => {
                    assert!(matches!(parsed, WsMessage::ActiveAssetCtx { .. }));
                }
                "l2Book" => {
                    assert!(
                        !matches!(parsed, WsMessage::Error { .. }),
                        "l2Book parsed as an error: {text}"
                    );
                    let data: L2Book = serde_json::from_value(value["data"].clone()).unwrap();
                    assert_eq!(data.coin, "BTC");
                    assert_eq!(data.time, 1_000, "the live book keeps its update stamp");
                }
                "clearinghouseState" => {
                    let data = &value["data"];
                    assert_eq!(data["user"], serde_json::json!(user));
                    let clearinghouse: ClearinghouseState =
                        serde_json::from_value(data["clearinghouseState"].clone()).unwrap();
                    assert_eq!(
                        clearinghouse.margin_summary.account_value,
                        Decimal::from(1_000)
                    );
                }
                "openOrders" => {
                    assert_eq!(value["data"]["orders"], serde_json::json!([]));
                }
                "spotState" => {
                    let spot: SpotClearinghouseState =
                        serde_json::from_value(value["data"]["spotState"].clone()).unwrap();
                    assert!(spot.balances.iter().any(|b| b.coin == "USDC"));
                }
                "webData2" => {
                    assert!(
                        !matches!(parsed, WsMessage::Error { .. }),
                        "webData2 parsed as an error: {text}"
                    );
                    let data = &value["data"];
                    assert_eq!(data["user"], serde_json::json!(user));
                    let clearinghouse: ClearinghouseState =
                        serde_json::from_value(data["clearinghouseState"].clone()).unwrap();
                    assert_eq!(
                        clearinghouse.margin_summary.account_value,
                        Decimal::from(1_000)
                    );
                    let spot: SpotClearinghouseState =
                        serde_json::from_value(data["spotState"].clone()).unwrap();
                    assert!(spot
                        .balances
                        .iter()
                        .any(|b| b.coin == "USDC" && b.total == Decimal::from(25)));
                    let ctxs: Vec<AssetCtx> =
                        serde_json::from_value(data["assetCtxs"].clone()).unwrap();
                    assert_eq!(ctxs.len(), 1);
                    assert_eq!(ctxs[0].mark_px, Decimal::from(100));
                }
                "pong" => assert!(matches!(parsed, WsMessage::Pong)),
                "error" => assert!(matches!(parsed, WsMessage::Error { .. })),
                other => panic!("unexpected channel {other}"),
            }
        }
    }

    /// The subscription lifecycle: ack, at-most-once, unsubscribe, and the
    /// error frame for what the sim does not serve (never silence).
    #[test]
    #[allow(clippy::too_many_lines)] // one subscription-lifecycle table: splitting it would hide the ack/error ordering
    fn the_ws_acks_unsubscribes_and_answers_unserved_frames_with_an_error() {
        let feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        feed.put_ctx("BTC".into(), test_ctx("100", "101"), 1);
        feed.put_book(test_book("BTC", "100"), 1_000);
        let state = api_state(feed.clone());
        let user = Address::repeat_byte(0xaa);
        fund_perp(&state, &feed, user);
        let mut subs = Vec::new();

        let ack = request_reply(
            r#"{"method":"subscribe","subscription":{"type":"allMids"}}"#,
            &mut subs,
            &state,
        );
        assert!(matches!(ack, WsPush::SubscriptionResponse { .. }));
        request_reply(
            r#"{"method":"subscribe","subscription":{"type":"allMids"}}"#,
            &mut subs,
            &state,
        );
        assert_eq!(subs.len(), 1, "a duplicate subscribe is not queued twice");

        let ack = request_reply(
            r#"{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#,
            &mut subs,
            &state,
        );
        assert!(matches!(ack, WsPush::SubscriptionResponse { .. }));
        assert_eq!(subs.len(), 2, "l2Book is a served channel");
        let error = request_reply(
            r#"{"method":"subscribe","subscription":{"type":"l2Book","coin":"NOPE"}}"#,
            &mut subs,
            &state,
        );
        assert!(
            matches!(error, WsPush::Error { .. }),
            "a bookless coin is refused, never silent"
        );

        let ack = request_reply(
            &format!(
                r#"{{"method":"subscribe","subscription":{{"type":"webData2","user":"{user}"}}}}"#
            ),
            &mut subs,
            &state,
        );
        assert!(matches!(ack, WsPush::SubscriptionResponse { .. }));
        assert_eq!(subs.len(), 3, "webData2 is a served channel");
        let unknown = Address::repeat_byte(0xff);
        let ack = request_reply(
            &format!(
                r#"{{"method":"subscribe","subscription":{{"type":"webData2","user":"{unknown}"}}}}"#
            ),
            &mut subs,
            &state,
        );
        assert!(
            matches!(ack, WsPush::SubscriptionResponse { .. }),
            "an untouched account is served the venue's empty shape, not refused"
        );
        let frame = push_frame(&Subscription::WebData2 { user: unknown }, &state, 2_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["channel"], "webData2");
        let clearinghouse: ClearinghouseState =
            serde_json::from_value(value["data"]["clearinghouseState"].clone()).unwrap();
        assert_eq!(clearinghouse.margin_summary.account_value, Decimal::ZERO);
        assert!(clearinghouse.asset_positions.is_empty());

        let error = request_reply(
            r#"{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"NOPE"}}"#,
            &mut subs,
            &state,
        );
        assert!(
            matches!(error, WsPush::Error { .. }),
            "an unknown coin is refused"
        );

        request_reply(
            r#"{"method":"unsubscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#,
            &mut subs,
            &state,
        );
        assert_eq!(subs.len(), 3);
        // Unsubscribing something that is not live is still acked.
        let ack = request_reply(
            r#"{"method":"unsubscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#,
            &mut subs,
            &state,
        );
        assert!(matches!(ack, WsPush::SubscriptionResponse { .. }));
        request_reply(
            r#"{"method":"unsubscribe","subscription":{"type":"allMids"}}"#,
            &mut subs,
            &state,
        );
        assert_eq!(subs.len(), 2);
        assert!(matches!(
            request_reply(r#"{"method":"ping"}"#, &mut subs, &state),
            WsPush::Pong
        ));
        assert!(matches!(
            request_reply("not json", &mut subs, &state),
            WsPush::Error { .. }
        ));
    }

    /// `l2Book` pushes the composed book: a live book keeps its update stamp,
    /// a scripted book is the scenario's present, and the order lane's
    /// consumption shows in the served size (the same read `/info` serves).
    #[test]
    fn l2_book_push_is_the_composed_book() {
        let feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        feed.put_book(test_book("BTC", "100"), 1_000);
        let state = api_state(feed.clone());
        let sub = Subscription::L2Book { coin: "BTC".into() };
        let live = push_frame(&sub, &state, 5_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&live).unwrap();
        assert_eq!(value["channel"], "l2Book");
        let book: L2Book = serde_json::from_value(value["data"].clone()).unwrap();
        assert_eq!(book.time, 1_000, "the live book's stamp is its update");
        assert_eq!(book.levels[0][0].px, Decimal::from(100));

        feed.own_book(&test_book("BTC", "99")).unwrap();
        feed.consume(crate::feed::Consumption {
            coin: "BTC".into(),
            side: 0,
            price: Decimal::from(99),
            size: Decimal::new(4, 1),
        })
        .unwrap();
        let scripted = push_frame(&sub, &state, 5_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&scripted).unwrap();
        let book: L2Book = serde_json::from_value(value["data"].clone()).unwrap();
        assert_eq!(book.time, 5_000, "a scripted book is stamped now");
        assert_eq!(book.levels[0][0].px, Decimal::from(99));
        assert_eq!(
            book.levels[0][0].sz,
            Decimal::new(6, 1),
            "scripted − consumed"
        );

        let missing = push_frame(
            &Subscription::L2Book {
                coin: "NOPE".into(),
            },
            &state,
            5_000,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&missing).unwrap();
        assert_eq!(value["channel"], "error");
    }

    /// `webData2` carries the served account state, and omits `assetCtxs`
    /// while the meta is warming instead of inventing an empty market list.
    #[test]
    fn web_data2_omits_asset_ctxs_while_the_meta_is_warming() {
        let feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        let state = api_state(feed.clone());
        let user = Address::repeat_byte(0xaa);
        fund_perp(&state, &feed, user);

        let frame = push_frame(&Subscription::WebData2 { user }, &state, 2_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["channel"], "webData2");
        let data = value["data"].as_object().unwrap();
        assert_eq!(data.len(), 3, "only the served account fields are present");
        for key in ["user", "clearinghouseState", "spotState"] {
            assert!(data.contains_key(key), "missing `{key}`: {data:?}");
        }
        assert!(
            data.get("assetCtxs").is_none(),
            "no invented empty ctx list"
        );
        let clearinghouse: ClearinghouseState =
            serde_json::from_value(value["data"]["clearinghouseState"].clone()).unwrap();
        assert_eq!(
            clearinghouse.margin_summary.account_value,
            Decimal::from(1_000)
        );
        let spot: SpotClearinghouseState =
            serde_json::from_value(value["data"]["spotState"].clone()).unwrap();
        assert!(spot.balances.is_empty());
    }

    /// The whole lane end-to-end, in-process: the sim's info router on a
    /// random port and the crate's own `WsClient` subscribing — the live
    /// subscriber path, not just the frame shapes. The client's cache must
    /// fill from the server's pushes (mark from `activeAssetCtx`).
    #[tokio::test]
    async fn ws_client_fills_its_cache_from_the_server() {
        let server_feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        server_feed.put_ctx("BTC".into(), test_ctx("100", "101"), 1);
        let state = api_state(server_feed);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, super::super::info_router(state))
                .await
                .unwrap();
        });

        let client_feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        let ws_pool = diorama_anvil::EndpointPool::new(
            &[format!("ws://{addr}/ws")],
            diorama_anvil::Scheme::Ws,
            diorama_anvil::PoolPolicy::default(),
        )
        .unwrap();
        let client = crate::feed::ws::WsClient::new(client_feed.clone(), ws_pool);
        let task = tokio::spawn(client.run());
        // 20 s: the server pushes on a 1 Hz tick; the budget only has to
        // outlast a loaded parallel test run (the same flake class as the
        // bridge's deadlock guard).
        let filled = 'wait: {
            for _ in 0..200 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if client_feed.mark("BTC") == Some(rust_decimal::Decimal::from(100)) {
                    break 'wait true;
                }
            }
            false
        };
        assert!(
            filled,
            "the client's cache must fill from the server's pushes"
        );
        task.abort();
        server.abort();
    }

    /// The session lane itself (not just the frame builders): a raw subscriber
    /// acks both new channels and receives their pushes, each parsed through
    /// the client's own parser.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one raw-socket session: the subscribe/ack/push sequence is the test
    async fn the_session_pushes_l2_book_and_web_data2() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as ClientMessage;

        let feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        feed.put_meta(crate::sim::meta_with("BTC", 10), 1);
        feed.put_ctx("BTC".into(), test_ctx("100", "101"), 1);
        feed.put_book(test_book("BTC", "100"), 1_000);
        let state = api_state(feed.clone());
        let user = Address::repeat_byte(0xaa);
        fund_perp(&state, &feed, user);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, super::super::info_router(state))
                .await
                .unwrap();
        });
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        socket
            .send(ClientMessage::Text(
                r#"{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#.into(),
            ))
            .await
            .unwrap();
        socket
            .send(ClientMessage::Text(
                format!(
                    r#"{{"method":"subscribe","subscription":{{"type":"webData2","user":"{user}"}}}}"#
                )
                .into(),
            ))
            .await
            .unwrap();

        let mut acks = 0;
        let mut book_seen = false;
        let mut web_seen = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while (!book_seen || !web_seen || acks < 2) && tokio::time::Instant::now() < deadline {
            let Some(Ok(ClientMessage::Text(text))) =
                tokio::time::timeout(Duration::from_secs(5), socket.next())
                    .await
                    .ok()
                    .flatten()
            else {
                continue;
            };
            let parsed: WsMessage = serde_json::from_str(&text).unwrap();
            assert!(
                !matches!(parsed, WsMessage::Error { .. }),
                "unexpected error frame: {text}"
            );
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            match value["channel"].as_str().unwrap_or("") {
                "subscriptionResponse" => acks += 1,
                "l2Book" => {
                    let data: L2Book = serde_json::from_value(value["data"].clone()).unwrap();
                    assert_eq!(data.coin, "BTC");
                    book_seen = true;
                }
                "webData2" => {
                    assert_eq!(value["data"]["user"], serde_json::json!(user));
                    web_seen = true;
                }
                _ => {}
            }
        }
        assert!(book_seen, "the session must push l2Book");
        assert!(web_seen, "the session must push webData2");
        assert_eq!(acks, 2, "both subscriptions are acked");
        socket.close(None).await.ok();
        server.abort();
    }
}
