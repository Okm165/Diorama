//! The venue's websocket lane (plan §7.5: the sim serves "info + WS" on its
//! info port): the two market channels the venue pushes — `allMids` and
//! `activeAssetCtx` — fed from the same composed cache the REST reads serve,
//! so a subscriber never sees a different mark than the account plane used.
//!
//! Protocol (`feed::ws`'s vocabulary, the client's own): the client subscribes
//! with `{"method":"subscribe","subscription":{"type":…}}`, the server acks
//! with a `subscriptionResponse` frame and pushes `{"channel":…,"data":…}` on
//! each 1 Hz tick while the subscription is live; `{"method":"ping"}` answers
//! `{"channel":"pong"}`. An unsupported channel (or an unknown coin) is
//! answered with the venue's `error` frame — never silence.
use super::ApiState;
use crate::feed::MarketFeed;
use crate::wire::{AssetCtx, AllMids};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// One subscription the sim serves. `l2Book` is REST-only: the book is
/// scripted/polled, and a push lane would need a change detector the market
/// plan does not have yet — subscribing answers the error frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Subscription {
    #[serde(rename = "allMids")]
    AllMids,
    #[serde(rename = "activeAssetCtx")]
    ActiveAssetCtx { coin: String },
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

#[derive(Debug, Serialize)]
pub(crate) struct SubscriptionAck {
    method: &'static str,
    subscription: Subscription,
}

/// `GET /ws` (upgrade): one session per subscriber.
pub async fn ws(State(state): State<Arc<ApiState>>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| session(state, socket))
}

/// One connection: read requests, push the subscribed channels on the 1 Hz
/// tick (the automaton's own cadence).
async fn session(state: Arc<ApiState>, mut socket: WebSocket) {
    let mut subs: Vec<Subscription> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                for sub in &subs {
                    let Some(frame) = push_frame(sub, &state.feed) else { continue };
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
                let reply = request_reply(&text, &mut subs, &state.feed);
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
fn request_reply(text: &str, subs: &mut Vec<Subscription>, feed: &MarketFeed) -> WsPush {
    let request = match serde_json::from_str::<WsRequest>(text) {
        Ok(request) => request,
        Err(e) => return WsPush::Error { data: format!("unparsable request: {e}") },
    };
    match request {
        WsRequest::Ping => WsPush::Pong,
        WsRequest::Subscribe { subscription } => {
            if let Err(reason) = unsupported(&subscription, feed) {
                return WsPush::Error { data: reason };
            }
            if !subs.contains(&subscription) {
                subs.push(subscription.clone());
            }
            WsPush::SubscriptionResponse { data: SubscriptionAck { method: "subscribe", subscription } }
        }
        WsRequest::Unsubscribe { subscription } => {
            subs.retain(|s| s != &subscription);
            WsPush::SubscriptionResponse { data: SubscriptionAck { method: "unsubscribe", subscription } }
        }
    }
}

/// Why the sim cannot serve a subscription; `Err` = the refusal.
fn unsupported(sub: &Subscription, feed: &MarketFeed) -> Result<(), String> {
    match sub {
        Subscription::AllMids => Ok(()),
        Subscription::ActiveAssetCtx { coin } if feed.mark(coin).is_some() => Ok(()),
        Subscription::ActiveAssetCtx { coin } => Err(format!(
            "coin `{coin}` is not in the venue's tracked markets"
        )),
    }
}

/// The frame one subscription pushes, from the composed cache. `None` = the
/// coin has no ctx yet (the feed is warming; the tick retries) — a
/// serialization fault on these plain shapes is a bug, named here.
fn push_frame(sub: &Subscription, feed: &MarketFeed) -> Option<String> {
    let push = match sub {
        Subscription::AllMids => WsPush::AllMids { data: AllMidsData { mids: feed.all_mids() } },
        Subscription::ActiveAssetCtx { coin } => WsPush::ActiveAssetCtx {
            data: CtxData { coin: coin.clone(), ctx: crate::sim::account::ctx_of(feed, coin)? },
        },
    };
    match serde_json::to_string(&push) {
        Ok(frame) => Some(frame),
        Err(e) => {
            tracing::warn!(error = %e, "ws push serialization failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::ws::WsMessage;
    use crate::wire::test_ctx;

    /// Every frame the server emits parses through the *client's own* parser:
    /// the two sides of the venue's websocket vocabulary in one test — a shape
    /// change on either side fails here, not on a live subscriber.
    #[test]
    fn pushes_round_trip_through_the_client_parser() {
        let feed = MarketFeed::memory(vec!["BTC".into()]);
        feed.put_ctx("BTC".into(), test_ctx("100", "101"), 1);
        let ctx = crate::sim::account::ctx_of(&feed, "BTC").unwrap();
        let frames = vec![
            WsPush::AllMids { data: AllMidsData { mids: feed.all_mids() } },
            WsPush::ActiveAssetCtx { data: CtxData { coin: "BTC".into(), ctx } },
            WsPush::Pong,
            WsPush::Error { data: "x".into() },
        ];
        for frame in frames {
            let text = serde_json::to_string(&frame).unwrap();
            let parsed: WsMessage = serde_json::from_str(&text).unwrap();
            match (&frame, parsed) {
                (WsPush::AllMids { .. }, WsMessage::AllMids { .. })
                | (WsPush::ActiveAssetCtx { .. }, WsMessage::ActiveAssetCtx { .. })
                | (WsPush::Pong, WsMessage::Pong)
                | (WsPush::Error { .. }, WsMessage::Error { .. }) => {}
                other => panic!("frame did not round-trip: {other:?}"),
            }
        }
    }

    /// The subscription lifecycle: ack, at-most-once, unsubscribe, and the
    /// error frame for what the sim does not serve (never silence).
    #[test]
    fn the_ws_acks_unsubscribes_and_answers_unserved_frames_with_an_error() {
        let feed = MarketFeed::memory(vec!["BTC".into()]);
        let mut subs = Vec::new();
        let ack = request_reply(
            r#"{"method":"subscribe","subscription":{"type":"allMids"}}"#,
            &mut subs,
            &feed,
        );
        assert!(matches!(ack, WsPush::SubscriptionResponse { .. }));
        request_reply(
            r#"{"method":"subscribe","subscription":{"type":"allMids"}}"#,
            &mut subs,
            &feed,
        );
        assert_eq!(subs.len(), 1, "a duplicate subscribe is not queued twice");

        let error = request_reply(
            r#"{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#,
            &mut subs,
            &feed,
        );
        assert!(matches!(error, WsPush::Error { .. }), "l2Book is REST-only");
        let error = request_reply(
            r#"{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"NOPE"}}"#,
            &mut subs,
            &feed,
        );
        assert!(matches!(error, WsPush::Error { .. }), "an unknown coin is refused");

        request_reply(
            r#"{"method":"unsubscribe","subscription":{"type":"allMids"}}"#,
            &mut subs,
            &feed,
        );
        assert!(subs.is_empty());
        assert!(matches!(request_reply(r#"{"method":"ping"}"#, &mut subs, &feed), WsPush::Pong));
        assert!(matches!(
            request_reply("not json", &mut subs, &feed),
            WsPush::Error { .. }
        ));
    }

    /// The whole lane end-to-end, in-process: the sim's info router on a
    /// random port and the crate's own `WsClient` subscribing — the live
    /// subscriber path, not just the frame shapes. The client's cache must
    /// fill from the server's pushes (mark from `activeAssetCtx`).
    #[tokio::test]
    async fn ws_client_fills_its_cache_from_the_server() {
        let server_feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        server_feed.put_ctx("BTC".into(), test_ctx("100", "101"), 1);
        let api = crate::feed::rest::RestClient::new("http://127.0.0.1:1").unwrap();
        let state = Arc::new(
            ApiState::new(server_feed, Arc::new(crate::sim::Sim::memory()), api, None, None)
                .unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, super::super::info_router(state))
                .await
                .unwrap();
        });

        let client_feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        let client = crate::feed::ws::WsClient::new(client_feed.clone(), format!("ws://{addr}/ws"));
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
        assert!(filled, "the client's cache must fill from the server's pushes");
        task.abort();
        server.abort();
    }
}
