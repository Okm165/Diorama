//! The CCTP watcher's WebSocket log lane: the primary event source.
//!
//! The chains serve JSON-RPC `eth_subscribe("logs", {address, topics})` over
//! WSS; this module is the small client for that contract. It connects with a
//! bound, subscribes to the pinned `MessageTransmitterV2` address and the
//! `MessageSent(bytes)` topic, decodes each `eth_subscription` notification
//! into the same [`alloy_rpc_types_eth::Log`] the HTTP lane uses, answers
//! ping/pong, and reconnects with bounded exponential backoff (1 s → 30 s).
//!
//! **Liveness is data, not the handshake.** A socket that accepts, subscribes
//! and then stops delivering is the failure this lane exists to catch: the
//! session is failed after a 75 s data-idle deadline (the chain's own bound is
//! 60 s). The client pings every 30 s, so a compliant server's pong also
//! counts as liveness; a half-open socket still trips the deadline.
//!
//! The lane never owns the watcher's reconciliation cursor. It reports its
//! own health (connected, reconnects, events, last event age, last error) and
//! feeds decoded logs to the watcher, which applies the two-watermark rule:
//! WSS advances only the health watermark, HTTP `eth_getLogs` is the only arm
//! that advances the durable cursor.
use crate::now_ms;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::Log;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// The handshake's budget. An unbounded hang would leave the lane down
/// forever: the backoff never runs because the session never returns.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The data-idle deadline. The chain's own bound is 60 s; 75 s leaves one
/// full 15 s margin (and one 30 s ping cadence) before a quiet or half-open
/// socket is failed and re-established.
const DATA_IDLE_TIMEOUT: Duration = Duration::from_secs(75);

/// The client ping cadence: a pong proves the socket is still alive even when
/// no matching log is emitted.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// The first reconnect delay (1 s), doubling per consecutive failure.
const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// The reconnect delay's hard cap.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The JSON-RPC id of the one subscription request this client sends.
const SUBSCRIBE_ID: u64 = 1;

/// The WebSocket lane's health, shared with the watcher's `/health` snapshot.
///
/// Every field is an observation, not a promise: `connected` is true only
/// between a successful subscription acknowledgement and the session's end;
/// `last_event_age_s` is `None` until the first log (never "0 s ago" for a
/// lane that has seen nothing).
#[derive(Debug, Default)]
pub(crate) struct WsHealth {
    connected: AtomicBool,
    reconnects: AtomicU64,
    events: AtomicU64,
    /// Wall-clock milliseconds of the last decoded log (0 = never).
    last_event_ms: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl WsHealth {
    pub(crate) fn snapshot_at(&self, now_ms: u64) -> WsHealthSnapshot {
        let last_event_ms = self.last_event_ms.load(Ordering::Relaxed);
        WsHealthSnapshot {
            connected: self.connected.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            events: self.events.load(Ordering::Relaxed),
            last_event_age_s: (last_event_ms != 0)
                .then(|| now_ms.saturating_sub(last_event_ms) / 1_000),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    pub(crate) fn mark_connected(&self) {
        self.connected.store(true, Ordering::Relaxed);
    }

    pub(crate) fn mark_disconnected(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    fn record_event(&self, now_ms: u64) {
        self.events.fetch_add(1, Ordering::Relaxed);
        self.last_event_ms.store(now_ms, Ordering::Relaxed);
    }

    fn record_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    fn record_error(&self, error: impl Into<String>) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.into());
    }
}

/// The health fields `/health` serves for one WSS lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WsHealthSnapshot {
    pub connected: bool,
    pub reconnects: u64,
    pub events: u64,
    /// Seconds since the last decoded log; `None` = no log yet.
    pub last_event_age_s: Option<u64>,
    pub last_error: Option<String>,
}

/// What one inbound WebSocket frame did.
enum Frame {
    /// The `eth_subscribe` result: the subscription id.
    Subscription(String),
    /// One `eth_subscription` notification, decoded into the watcher's log.
    Log(Box<Log>),
    /// A WebSocket ping control frame (the payload must be echoed).
    Ping(Vec<u8>),
    /// A WebSocket pong control frame: liveness, no data.
    Pong,
    /// A JSON-RPC error response.
    RpcError(String),
    /// A well-formed frame this lane does not consume.
    Ignored,
    /// Bytes that are not a valid JSON-RPC frame. Malformed input is ignored,
    /// never fatal: a permissionless RPC cannot be allowed to kill the lane.
    Malformed(String),
}

/// Decode one WebSocket frame. JSON-RPC rides text frames; binary frames are
/// decoded as UTF-8 JSON too (some proxies tunnel the same payload), and
/// control frames map to their liveness answers.
fn decode_frame(message: Message) -> Frame {
    match message {
        Message::Text(text) => decode_text(text.as_str()),
        Message::Binary(bytes) => match std::str::from_utf8(&bytes) {
            Ok(text) => decode_text(text),
            Err(error) => Frame::Malformed(format!("binary frame is not UTF-8: {error}")),
        },
        Message::Ping(payload) => Frame::Ping(payload.to_vec()),
        Message::Pong(_) => Frame::Pong,
        Message::Close(_) => Frame::Ignored,
        _ => Frame::Ignored,
    }
}

/// Decode one JSON-RPC text frame.
///
/// The three shapes the lane must know: the subscribe result, an
/// `eth_subscription` notification, and an error. Everything else is
/// [`Frame::Ignored`] — an unknown method is not a lane failure.
fn decode_text(text: &str) -> Frame {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => return Frame::Malformed(format!("invalid JSON-RPC frame: {error}")),
    };

    if value.get("method").and_then(Value::as_str) == Some("eth_subscription") {
        let params = value.get("params");
        let subscription = params
            .and_then(|params| params.get("subscription"))
            .and_then(Value::as_str);
        let result = params.and_then(|params| params.get("result"));
        return match (subscription, result) {
            (Some(subscription), Some(result)) => {
                match serde_json::from_value::<Log>(result.clone()) {
                    Ok(log) => {
                        tracing::trace!(subscription, "cctp websocket log notification");
                        Frame::Log(Box::new(log))
                    }
                    Err(error) => {
                        Frame::Malformed(format!("eth_subscription result is not a log: {error}"))
                    }
                }
            }
            _ => Frame::Malformed(
                "eth_subscription without params.subscription/params.result".into(),
            ),
        };
    }

    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("JSON-RPC error without a message");
        return Frame::RpcError(message.to_string());
    }

    if value.get("id").is_some() {
        if let Some(result) = value.get("result").and_then(Value::as_str) {
            return Frame::Subscription(result.to_string());
        }
    }

    Frame::Ignored
}

/// The `eth_subscribe("logs", …)` request. The filter is the same address and
/// topic0 the HTTP reconciliation scans, so the lanes can never disagree on
/// which events they surface.
#[derive(Serialize)]
struct SubscribeRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: (&'static str, LogFilter<'a>),
}

#[derive(Serialize)]
struct LogFilter<'a> {
    address: Address,
    topics: &'a [B256],
}

fn subscribe_request(address: Address, topics: &[B256]) -> Result<String, serde_json::Error> {
    serde_json::to_string(&SubscribeRequest {
        jsonrpc: "2.0",
        id: SUBSCRIBE_ID,
        method: "eth_subscribe",
        params: ("logs", LogFilter { address, topics }),
    })
}

/// The lane's subscription facts: the same address/topic0 the HTTP scan uses.
pub(crate) struct WsLogsConfig {
    pub url: String,
    pub address: Address,
    pub topics: Vec<B256>,
    pub chain: &'static str,
}

/// The WSS log lane: subscribe, feed decoded logs into the watcher's channel,
/// reconnect forever. The HTTP reconciliation continues regardless; this lane
/// is a latency source, never the only source.
pub(crate) struct WsLogs {
    url: String,
    address: Address,
    topics: Vec<B256>,
    tx: mpsc::Sender<Log>,
    health: Arc<WsHealth>,
    chain: &'static str,
}

impl WsLogs {
    pub(crate) fn new(config: WsLogsConfig, tx: mpsc::Sender<Log>, health: Arc<WsHealth>) -> Self {
        Self {
            url: config.url,
            address: config.address,
            topics: config.topics,
            tx,
            health,
            chain: config.chain,
        }
    }

    /// Run forever: one session, then the bounded backoff, then the next.
    pub(crate) async fn run(self) {
        let mut attempt = 0u32;
        loop {
            let outcome = self.session().await;
            self.health.mark_disconnected();
            let delay = match outcome {
                SessionOutcome::ConsumerGone => return,
                SessionOutcome::Served => {
                    attempt = 0;
                    BACKOFF_BASE
                }
                SessionOutcome::Failed(error) => {
                    self.health.record_error(error.to_string());
                    let delay = backoff_delay(attempt);
                    attempt = attempt.saturating_add(1);
                    delay
                }
            };
            self.health.record_reconnect();
            tracing::warn!(
                chain = self.chain,
                url = %self.url,
                attempt,
                backoff_ms = delay.as_millis() as u64,
                "cctp websocket log lane disconnected; reconnecting"
            );
            tokio::time::sleep(delay).await;
        }
    }

    /// One connection: subscribe, then read until the socket ends or goes
    /// data-idle. A session that delivered at least one log is `Served`; a
    /// later drop is the network's, not the subscription's.
    #[allow(clippy::too_many_lines)] // one select loop: the protocol's states
    async fn session(&self) -> SessionOutcome {
        let request = match subscribe_request(self.address, &self.topics) {
            Ok(request) => request,
            Err(error) => return SessionOutcome::Failed(WsError::Encode(error)),
        };
        let (stream, _) = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(self.url.as_str()),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => return SessionOutcome::Failed(WsError::Connect(error)),
            Err(_) => return SessionOutcome::Failed(WsError::ConnectTimeout),
        };
        let (mut sink, mut source) = stream.split();
        if let Err(error) = sink.send(Message::Text(request.into())).await {
            return SessionOutcome::Failed(WsError::Send(error));
        }

        let mut delivered = false;
        let mut deadline = tokio::time::Instant::now() + DATA_IDLE_TIMEOUT;
        let mut ping = tokio::time::interval(PING_INTERVAL);
        // The first tick fires immediately; consume it so the first ping is
        // one interval after connect.
        ping.tick().await;

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    return SessionOutcome::Failed(WsError::Idle);
                }
                _ = ping.tick() => {
                    if let Err(error) = sink.send(Message::Ping(Vec::new().into())).await {
                        return SessionOutcome::Failed(WsError::Send(error));
                    }
                }
                message = source.next() => match message {
                    Some(Ok(message)) => {
                        deadline = tokio::time::Instant::now() + DATA_IDLE_TIMEOUT;
                        match self.handle_frame(decode_frame(message), &mut delivered).await {
                            FrameAction::Continue => {}
                            FrameAction::ReplyPong(payload) => {
                                if let Err(error) = sink.send(Message::Pong(payload.into())).await {
                                    return SessionOutcome::Failed(WsError::Send(error));
                                }
                            }
                            FrameAction::Failed(error) => return SessionOutcome::Failed(error),
                            FrameAction::ConsumerGone => return SessionOutcome::ConsumerGone,
                        }
                    }
                    Some(Err(error)) => return self.read_failed(delivered, error),
                    None => return closed_outcome(delivered),
                }
            }
        }
    }

    /// Fold one decoded frame into the lane's health/consumer state. The
    /// pong reply is an action: the session's select loop owns the sink.
    async fn handle_frame(&self, frame: Frame, delivered: &mut bool) -> FrameAction {
        match frame {
            Frame::Subscription(subscription) => {
                self.health.mark_connected();
                tracing::info!(
                    chain = self.chain,
                    url = %self.url,
                    %subscription,
                    "cctp websocket log subscription live"
                );
            }
            Frame::Log(log) => {
                *delivered = true;
                self.health.mark_connected();
                self.health.record_event(now_ms());
                if self.tx.send(*log).await.is_err() {
                    return FrameAction::ConsumerGone;
                }
            }
            Frame::Ping(payload) => return FrameAction::ReplyPong(payload),
            Frame::Pong | Frame::Ignored => {}
            Frame::RpcError(error) => return FrameAction::Failed(WsError::Subscription(error)),
            Frame::Malformed(error) => {
                tracing::debug!(
                    chain = self.chain,
                    url = %self.url,
                    %error,
                    "unparsable cctp websocket frame ignored"
                );
            }
        }
        FrameAction::Continue
    }

    /// A read fault: after data the session was served (the drop is the
    /// network's); before data it is a failed attempt.
    fn read_failed(
        &self,
        delivered: bool,
        error: tokio_tungstenite::tungstenite::Error,
    ) -> SessionOutcome {
        if delivered {
            tracing::warn!(
                chain = self.chain,
                url = %self.url,
                %error,
                "cctp websocket read failed; reconnecting"
            );
            SessionOutcome::Served
        } else {
            SessionOutcome::Failed(WsError::Read(error))
        }
    }
}

/// What one decoded frame asks the session's select loop to do.
enum FrameAction {
    Continue,
    ReplyPong(Vec<u8>),
    Failed(WsError),
    ConsumerGone,
}

/// A closed socket: served when it delivered data, a failed attempt otherwise.
fn closed_outcome(delivered: bool) -> SessionOutcome {
    if delivered {
        SessionOutcome::Served
    } else {
        SessionOutcome::Failed(WsError::ClosedBeforeData)
    }
}

/// One session's outcome. `Served` means the socket delivered at least one
/// log (a later close is not the endpoint's fault); `Failed` escalates the
/// reconnect backoff; `ConsumerGone` is permanent.
enum SessionOutcome {
    Served,
    Failed(WsError),
    ConsumerGone,
}

/// The lane's faults (logged by the reconnect loop, never propagated).
#[derive(Debug, thiserror::Error)]
enum WsError {
    #[error("connecting the cctp websocket: no answer in {CONNECT_TIMEOUT:?}")]
    ConnectTimeout,
    #[error("connecting the cctp websocket")]
    Connect(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("sending on the cctp websocket")]
    Send(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("reading the cctp websocket")]
    Read(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("encoding the cctp websocket subscription")]
    Encode(#[source] serde_json::Error),
    #[error("the cctp websocket refused the log subscription: {0}")]
    Subscription(String),
    #[error("the cctp websocket went data-idle for {DATA_IDLE_TIMEOUT:?}")]
    Idle,
    #[error("the cctp websocket closed before its first log")]
    ClosedBeforeData,
}

/// The reconnect delay: 1 s, 2 s, 4 s, … capped at 30 s. The attempt is the
/// number of consecutive failures; a session that served data resets it.
fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.min(5);
    Duration::from_secs(1u64 << shift).min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TX: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BLOCK_HASH: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TOPIC: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn notification(log_index: &str) -> String {
        format!(
            r#"{{
                "jsonrpc":"2.0",
                "method":"eth_subscription",
                "params":{{
                    "subscription":"0xsub",
                    "result":{{
                        "address":"0x81d40f21f12a8f0e3252bccb954d722d4c464b64",
                        "topics":["{TOPIC}"],
                        "data":"0x",
                        "blockNumber":"0x7",
                        "blockHash":"{BLOCK_HASH}",
                        "transactionHash":"{TX}",
                        "transactionIndex":"0x0",
                        "logIndex":"{log_index}",
                        "removed":false
                    }}
                }}
            }}"#
        )
    }

    /// The three JSON-RPC shapes: the subscribe result, a log notification,
    /// and an error. The notification decodes into the same `Log` type the
    /// HTTP lane returns.
    #[test]
    fn decodes_subscription_result_and_log_notification() {
        let ack = decode_text(r#"{"jsonrpc":"2.0","id":1,"result":"0xsub"}"#);
        match ack {
            Frame::Subscription(subscription) => assert_eq!(subscription, "0xsub"),
            _ => panic!("subscription result expected"),
        }

        let log = decode_text(&notification("0x2"));
        match log {
            Frame::Log(log) => {
                assert_eq!(log.log_index, Some(2));
                assert_eq!(log.block_number, Some(7));
                assert_eq!(log.transaction_hash, Some(TX.parse().unwrap()));
                assert_eq!(log.topic0(), Some(&TOPIC.parse().unwrap()));
                assert!(!log.removed);
            }
            _ => panic!("log notification expected"),
        }

        let error = decode_text(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#,
        );
        match error {
            Frame::RpcError(message) => assert_eq!(message, "method not found"),
            _ => panic!("rpc error expected"),
        }
    }

    /// Ping/pong are control frames, answered by the session; they must be
    /// distinguishable from JSON traffic so a ping is never decoded as a
    /// malformed log.
    #[test]
    fn decodes_ping_and_pong_frames() {
        match decode_frame(Message::Ping(vec![1, 2, 3].into())) {
            Frame::Ping(payload) => assert_eq!(payload, vec![1, 2, 3]),
            _ => panic!("ping expected"),
        }
        assert!(matches!(
            decode_frame(Message::Pong(Vec::new().into())),
            Frame::Pong
        ));
    }

    /// Malformed JSON and a malformed notification are ignored frames, not
    /// lane failures: a permissionless RPC can send anything.
    #[test]
    fn malformed_frames_are_reported_not_fatal() {
        assert!(matches!(decode_text("{not json"), Frame::Malformed(_)));
        let bad_result = r#"{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xsub","result":"0xdead"}}"#;
        assert!(matches!(decode_text(bad_result), Frame::Malformed(_)));
        let missing_params = r#"{"jsonrpc":"2.0","method":"eth_subscription","params":{}}"#;
        assert!(matches!(decode_text(missing_params), Frame::Malformed(_)));
        assert!(matches!(
            decode_text(r#"{"jsonrpc":"2.0","id":9,"result":42}"#),
            Frame::Ignored
        ));
    }

    /// The request is exactly the JSON-RPC `eth_subscribe("logs", filter)`
    /// shape, with the same address and topic0 the HTTP scan uses.
    #[test]
    fn subscribe_request_is_the_documented_shape() {
        let address = Address::repeat_byte(0x11);
        let topic = B256::repeat_byte(0x22);
        let request = subscribe_request(address, &[topic]).unwrap();
        let value: Value = serde_json::from_str(&request).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 1);
        assert_eq!(value["method"], "eth_subscribe");
        assert_eq!(value["params"][0], "logs");
        assert_eq!(
            value["params"][1]["address"].as_str().unwrap(),
            alloy_primitives::hex::encode_prefixed(address)
        );
        assert_eq!(
            value["params"][1]["topics"][0].as_str().unwrap(),
            alloy_primitives::hex::encode_prefixed(topic)
        );
    }

    /// The backoff is bounded: 1 s at the first failure, doubling to the 30 s
    /// cap and staying there.
    #[test]
    fn reconnect_backoff_is_bounded() {
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(4), Duration::from_secs(16));
        assert_eq!(backoff_delay(5), Duration::from_secs(30));
        assert_eq!(backoff_delay(100), Duration::from_secs(30));
    }

    /// The health snapshot carries every field `/health` needs, including a
    /// `None` age before the first event (never a false "0 s ago").
    #[test]
    fn health_snapshot_reports_the_lane_state() {
        let health = WsHealth::default();
        assert_eq!(
            health.snapshot_at(1_000),
            WsHealthSnapshot {
                connected: false,
                reconnects: 0,
                events: 0,
                last_event_age_s: None,
                last_error: None,
            }
        );
        health.mark_connected();
        health.record_event(2_000);
        health.record_reconnect();
        health.record_error("connection reset");
        let snapshot = health.snapshot_at(3_500);
        assert!(snapshot.connected);
        assert_eq!(snapshot.reconnects, 1);
        assert_eq!(snapshot.events, 1);
        assert_eq!(snapshot.last_event_age_s, Some(1));
        assert_eq!(snapshot.last_error.as_deref(), Some("connection reset"));
    }
}
