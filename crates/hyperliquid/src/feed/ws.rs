//! The WS lane of the market plane: `allMids` and `activeAssetCtx` pushes
//! (the primary freshness source; the REST poller backfills).
//!
//! Protocol facts (docs, §16.3): subscriptions are
//! `{"method":"subscribe","subscription":{"type":…}}`; the server must send a
//! message within 60 s, so the client pings every 30 s
//! (`{"method":"ping"}` → `{"channel":"pong"}`); a dropped connection is
//! re-established with exponential backoff (1 s → 30 s). Every message is a
//! typed [`WsMessage`] — an unknown channel is ignored, never fatal.
use crate::feed::MarketFeed;
use crate::wire::AssetCtx;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// One server push (or ack). `channel` tags the payload.
#[derive(Debug, Deserialize)]
#[serde(tag = "channel")]
pub(crate) enum WsMessage {
    #[serde(rename = "allMids")]
    AllMids { data: WsAllMids },
    #[serde(rename = "activeAssetCtx")]
    ActiveAssetCtx { data: WsAssetCtx },
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "error")]
    Error { data: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsAllMids {
    mids: BTreeMap<String, Decimal>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsAssetCtx {
    coin: String,
    ctx: AssetCtx,
}

/// One subscription request.
#[derive(Debug, Serialize)]
struct Subscribe<'a> {
    method: &'static str,
    subscription: Subscription<'a>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum Subscription<'a> {
    #[serde(rename = "allMids")]
    AllMids,
    #[serde(rename = "activeAssetCtx")]
    ActiveAssetCtx { coin: &'a str },
}

#[derive(Debug, Serialize)]
struct Ping {
    method: &'static str,
}

/// The WS client task: subscribe, fold pushes into the feed, reconnect.
pub struct WsClient {
    feed: Arc<MarketFeed>,
    ws_url: String,
}

impl WsClient {
    pub fn new(feed: Arc<MarketFeed>, ws_url: String) -> Self {
        Self { feed, ws_url }
    }

    /// Run forever with exponential backoff between sessions.
    pub async fn run(self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.session().await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(e) => {
                    tracing::warn!(error = %e, "market websocket session failed; reconnecting");
                }
            }
            self.feed.metrics.set_ws_connected(false);
            self.feed.metrics.inc_ws_reconnects();
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    /// One connection: subscribe, then read until the socket drops.
    async fn session(&self) -> Result<(), WsError> {
        let (stream, _) = tokio_tungstenite::connect_async(&self.ws_url)
            .await
            .map_err(WsError::Connect)?;
        let (mut sink, mut source) = stream.split();
        for sub in self.subscriptions()? {
            sink.send(Message::Text(sub.into())).await.map_err(WsError::Send)?;
        }
        self.feed.metrics.set_ws_connected(true);
        tracing::info!(url = %self.ws_url, markets = self.feed.markets.len(), "market websocket connected");

        let mut ping = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    let ping_msg = serde_json::to_string(&Ping { method: "ping" }).map_err(WsError::Encode)?;
                    sink.send(Message::Text(ping_msg.into())).await.map_err(WsError::Send)?;
                }
                msg = source.next() => match msg {
                    Some(Ok(Message::Text(text))) => self.handle_text(&text),
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => {} // binary/ping/pong frames: nothing to fold
                    Some(Err(e)) => return Err(WsError::Read(e)),
                },
            }
        }
    }

    /// The session's subscription messages. An encode fault is a session
    /// fault (the reconnect retries), never an empty message: a silent `""`
    /// would leave the socket "connected" with no data flowing.
    fn subscriptions(&self) -> Result<Vec<String>, WsError> {
        let mut subs = Vec::with_capacity(self.feed.markets.len() + 1);
        let encode = |sub: Subscription<'_>| {
            serde_json::to_string(&Subscribe {
                method: "subscribe",
                subscription: sub,
            })
            .map_err(WsError::Encode)
        };
        subs.push(encode(Subscription::AllMids)?);
        for coin in &self.feed.markets {
            subs.push(encode(Subscription::ActiveAssetCtx { coin })?);
        }
        Ok(subs)
    }

    fn handle_text(&self, text: &str) {
        let msg: WsMessage = match serde_json::from_str(text) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::debug!(error = %e, "unparsable websocket message ignored");
                return;
            }
        };
        let now = crate::now_ms();
        match msg {
            WsMessage::AllMids { data } => self.feed.put_mids(data.mids, now),
            WsMessage::ActiveAssetCtx { data } => self.feed.put_ctx(data.coin, data.ctx, now),
            WsMessage::Error { data } => tracing::warn!(error = data, "venue websocket error message"),
            WsMessage::Pong | WsMessage::Other => {}
        }
    }
}

/// The WS lane's faults (logged by the reconnect loop, never propagated).
#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("connecting the market websocket")]
    Connect(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("reading the market websocket")]
    Read(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("sending on the market websocket")]
    Send(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("encoding a websocket request")]
    Encode(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_parse_typed_and_unknown_ignored() {
        let mids = r#"{"channel":"allMids","data":{"mids":{"BTC":"81271.5","ETH":"3000.0"}}}"#;
        let m: WsMessage = serde_json::from_str(mids).unwrap();
        match m {
            WsMessage::AllMids { data } => {
                assert_eq!(data.mids.get("BTC").unwrap(), &Decimal::new(812_715, 1));
            }
            _ => panic!("allMids expected"),
        }
        let ctx = r#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"funding":"0.0000125","openInterest":"1","prevDayPx":"1","dayNtlVlm":"1","premium":null,"oraclePx":"81280.0","markPx":"81271.0","midPx":null,"impactPxs":null,"dayBaseVlm":"1"}}}"#;
        let m: WsMessage = serde_json::from_str(ctx).unwrap();
        assert!(matches!(m, WsMessage::ActiveAssetCtx { .. }));
        let pong: WsMessage = serde_json::from_str(r#"{"channel":"pong"}"#).unwrap();
        assert!(matches!(pong, WsMessage::Pong));
        let ack: WsMessage = serde_json::from_str(r#"{"channel":"subscriptionResponse","data":{"method":"subscribe"}}"#).unwrap();
        assert!(matches!(ack, WsMessage::Other));
    }

    #[test]
    fn subscriptions_serialize_to_the_documented_shape() {
        let client = WsClient::new(
            Arc::new(MarketFeed::memory(vec!["BTC".into()])),
            "wss://example.invalid/ws".into(),
        );
        let subs = client.subscriptions().unwrap();
        assert_eq!(subs[0], r#"{"method":"subscribe","subscription":{"type":"allMids"}}"#);
        assert_eq!(subs[1], r#"{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"BTC"}}"#);
    }
}
