//! The `candle` channel's source: the venue's own candle stream, relayed.
//!
//! Candles are market data, and the sim has no trade tape of its own to build
//! them from, so a `candle` subscription is served by the real venue the marks
//! come from. One upstream subscription exists per `(coin, interval)` while at
//! least one session listens (the first listener opens it, the last one to
//! leave closes it); every session gets the latest candle on subscribe and
//! each update after it. The websocket's `candle` channel subscribes this
//! hub.
#![allow(dead_code)] // `inflate`-side helpers are test-only; the relay is live
use diorama_anvil::{now_ms, EndpointPool};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

/// How long a failed upstream attempt waits before the next one.
const RETRY: Duration = Duration::from_secs(2);

/// The upstream keep-alive cadence (the venue closes a silent socket after 60 s).
const PING: Duration = Duration::from_secs(30);

type Key = (String, String);

struct Relay {
    tx: broadcast::Sender<String>,
    last: Mutex<Option<String>>,
}

/// The shared candle relays, one per `(coin, interval)`.
#[derive(Default)]
pub(crate) struct CandleHub {
    pool: Mutex<Option<EndpointPool>>,
    relays: Mutex<HashMap<Key, Arc<Relay>>>,
}

/// One session's view of a relay.
pub(crate) struct CandleSub {
    rx: broadcast::Receiver<String>,
    first: Option<String>,
}

impl CandleSub {
    /// The candle the relay held when this session subscribed, once.
    pub(crate) fn take_first(&mut self) -> Option<String> {
        self.first.take()
    }

    /// The newest candle pushed since the last call (older ones are the same
    /// candle's earlier states; the chart only wants the latest).
    pub(crate) fn latest(&mut self) -> Option<String> {
        let mut newest = None;
        loop {
            match self.rx.try_recv() {
                Ok(frame) => newest = Some(frame),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => return newest,
            }
        }
    }
}

impl CandleHub {
    /// Give the hub the venue websocket endpoints (the market lane's pool).
    pub(crate) fn set_pool(&self, pool: EndpointPool) {
        *self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pool);
    }

    /// Listen to one candle stream, opening the upstream if nobody else has.
    /// `None` when the hub has no venue endpoint (nothing can serve candles).
    pub(crate) fn subscribe(self: &Arc<Self>, coin: &str, interval: &str) -> Option<CandleSub> {
        let pool = self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        let key: Key = (coin.to_string(), interval.to_string());
        let mut relays = self
            .relays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(relay) = relays.get(&key) {
            let first = relay
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            return Some(CandleSub {
                rx: relay.tx.subscribe(),
                first,
            });
        }
        let (tx, rx) = broadcast::channel(64);
        let relay = Arc::new(Relay {
            tx,
            last: Mutex::new(None),
        });
        relays.insert(key.clone(), Arc::clone(&relay));
        drop(relays);
        tokio::spawn(run(Arc::clone(self), key, relay, pool));
        Some(CandleSub { rx, first: None })
    }

    /// Drop a relay nobody listens to. Decided under the map's lock, the same
    /// lock `subscribe` takes, so a listener can never join a closing relay.
    fn retire(&self, key: &Key, relay: &Arc<Relay>) -> bool {
        let mut relays = self
            .relays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if relay.tx.receiver_count() == 0 {
            if relays.get(key).is_some_and(|r| Arc::ptr_eq(r, relay)) {
                relays.remove(key);
            }
            return true;
        }
        false
    }
}

/// The upstream task: connect, subscribe, relay candle frames, reconnect until
/// nobody listens.
#[allow(clippy::too_many_lines)] // one upstream session: connect, subscribe, relay and retire in one select loop
async fn run(hub: Arc<CandleHub>, key: Key, relay: Arc<Relay>, pool: EndpointPool) {
    let request = serde_json::json!({
        "method": "subscribe",
        "subscription": {"type": "candle", "coin": key.0, "interval": key.1},
    })
    .to_string();
    loop {
        if hub.retire(&key, &relay) {
            return;
        }
        let admission = pool.admit(now_ms(), &[]);
        let Some(index) = admission.indices.first().copied() else {
            tokio::time::sleep(RETRY).await;
            continue;
        };
        let url = pool.url(index).to_string();
        let Ok((stream, _)) = tokio_tungstenite::connect_async(url.as_str()).await else {
            pool.record_failure(index, now_ms());
            tokio::time::sleep(RETRY).await;
            continue;
        };
        let (mut sink, mut source) = stream.split();
        if sink
            .send(Message::Text(request.clone().into()))
            .await
            .is_err()
        {
            pool.record_failure(index, now_ms());
            tokio::time::sleep(RETRY).await;
            continue;
        }
        let mut ping = tokio::time::interval(PING);
        ping.tick().await;
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    if hub.retire(&key, &relay) {
                        return;
                    }
                    if sink.send(Message::Text(r#"{"method":"ping"}"#.into())).await.is_err() {
                        break;
                    }
                }
                msg = source.next() => match msg {
                    Some(Ok(Message::Text(text))) => forward(&relay, &text),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                },
            }
        }
        pool.record_success(index, now_ms());
        tokio::time::sleep(RETRY).await;
    }
}

/// Keep the venue's candle frames, drop its acks and pongs.
fn forward(relay: &Relay, text: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    if value["channel"] != "candle" {
        return;
    }
    *relay
        .last
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(text.to_string());
    // No listener is not an error: the task retires at its next check.
    let _ = relay.tx.send(text.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_keeps_candle_frames_only() {
        let (tx, _rx) = broadcast::channel(4);
        let relay = Relay {
            tx,
            last: Mutex::new(None),
        };
        forward(&relay, r#"{"channel":"subscriptionResponse","data":{}}"#);
        forward(&relay, r#"{"channel":"pong"}"#);
        assert!(relay.last.lock().unwrap().is_none());
        let candle = r#"{"channel":"candle","data":{"t":1}}"#;
        forward(&relay, candle);
        assert_eq!(relay.last.lock().unwrap().as_deref(), Some(candle));
    }

    #[test]
    fn a_session_gets_the_latest_candle_not_the_backlog() {
        let (tx, rx) = broadcast::channel(4);
        let mut sub = CandleSub { rx, first: None };
        tx.send("a".into()).unwrap();
        tx.send("b".into()).unwrap();
        assert_eq!(sub.latest().as_deref(), Some("b"));
        assert_eq!(sub.latest(), None);
    }

    #[test]
    fn the_hub_cannot_serve_candles_without_a_venue_endpoint() {
        let hub = Arc::new(CandleHub::default());
        assert!(hub.subscribe("BTC", "1m").is_none());
    }
}
