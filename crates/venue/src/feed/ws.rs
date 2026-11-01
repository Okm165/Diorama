//! The WS lane of the market plane: `allMids`, `activeAssetCtx` and `l2Book`
//! pushes (the primary freshness source; the REST poller backfills a coin the
//! socket has not updated within one poll interval).
//!
//! Protocol facts (docs, §16.3): subscriptions are
//! `{"method":"subscribe","subscription":{"type":…}}`; the server must send a
//! message within 60 s, so the client pings every 30 s
//! (`{"method":"ping"}` → `{"channel":"pong"}`); a dropped connection is
//! re-established with exponential backoff (1 s → 30 s). Every message is a
//! typed [`WsMessage`] — an unknown channel is ignored, never fatal.
//!
//! **Liveness is data, not the handshake.** A socket that accepts, subscribes
//! and then stops pushing is the failure this lane exists to catch: the
//! session is healthy only after the first data frame, and a data-idle
//! deadline (2× the documented 30 s push cadence) fails the endpoint, cools it
//! and rotates to the next one. A long-lived session that delivered data and
//! then dropped reconnects immediately; only a failed session sleeps.
use crate::feed::MarketFeed;
use crate::wire::{AssetCtx, L2Book};
use diorama_anvil::{now_ms, Backoff, EndpointPool, Progress};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// The websocket handshake's budget (measured: normally well under a second,
/// once 26 s — a slow venue-side accept). An unbounded hang would leave the
/// lane down forever: the backoff never runs because the session never
/// returns. 30 s keeps a slow-but-valid accept and retries a hung one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The data-idle deadline. The venue protocol requires the server to send a
/// message within **60 s** (docs §16.3; ADR 0006: "the server must send a
/// message within 60 s, so the client pings every 30 s"). The old 45 s
/// deadline was shorter than the protocol's own bound, so a compliant quiet
/// feed was falsely rotated; 75 s leaves one full 15 s margin beyond the
/// bound (and beyond the 30 s ping cadence).
const DATA_IDLE_TIMEOUT: Duration = Duration::from_secs(75);

/// The first-data budget after a successful handshake. A subscription that
/// never starts pushing is a failed session, not a healthy idle one. It is
/// deliberately below the 60 s server bound (a first push is expected at the
/// feed's ~1 Hz cadence), but no longer the old 15 s race.
const FIRST_DATA_TIMEOUT: Duration = Duration::from_secs(20);

/// The floor between reconnect attempts, even after a session that served
/// data and then dropped. Without it a server that accepts and immediately
/// closes becomes a hot reconnect loop (the endpoint's own cooldown only
/// applies to *failed* sessions).
const MIN_RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// One server push (or ack). `channel` tags the payload.
#[derive(Debug, Deserialize)]
#[serde(tag = "channel")]
pub(crate) enum WsMessage {
    #[serde(rename = "allMids")]
    AllMids { data: WsAllMids },
    #[serde(rename = "activeAssetCtx")]
    ActiveAssetCtx { data: WsAssetCtx },
    #[serde(rename = "l2Book")]
    L2Book { data: L2Book },
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

/// What one text frame did to the feed.
enum Frame {
    /// A data channel updated the cache (the session is live).
    Data,
    /// A pong or an unknown channel: alive, but not data.
    Control,
    /// The venue's subscription error. A per-coin refusal is isolated (the
    /// session keeps serving the other coins); a session-level refusal rotates.
    Error(SubscriptionFailure),
}

/// The two classes of venue subscription error. One unknown coin must not
/// take down the whole market lane (the old code failed the session on any
/// `Frame::Error`), while a refused `allMids` or an unrecognized global
/// refusal really does mean this socket cannot serve.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SubscriptionFailure {
    /// The error names a configured coin: drop that subscription's data,
    /// keep the session and the other coins alive.
    Coin(String),
    /// A session-wide refusal (allMids, rate limit, unknown protocol error).
    Session(String),
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
    #[serde(rename = "l2Book")]
    L2Book { coin: &'a str },
}

#[derive(Debug, Serialize)]
struct Ping {
    method: &'static str,
}

/// The WS client task: subscribe, fold pushes into the feed, reconnect. The
/// URL pool is the shared [`EndpointPool`]: each reconnect starts at a random
/// healthy endpoint, a failed handshake or an idle session moves to the next
/// one, and the failed endpoint is cooled down for a later session.
pub struct WsClient {
    feed: Arc<MarketFeed>,
    pool: EndpointPool,
}

/// One session's outcome: `Served` means the endpoint delivered data (a later
/// drop is not its fault); `Failed` means it never became live, went idle, or
/// refused the subscription.
enum SessionOutcome {
    Served,
    Failed(WsError),
}

/// The sleep before retrying an all-cooling pool: the pool's own earliest
/// cooldown, floored at 1 ms so a zero can never spin the loop.
fn cooling_wait_ms(retry_after_ms: u64) -> u64 {
    retry_after_ms.max(1)
}

/// The reconnect sleep: the endpoint policy's backoff, never below the floor.
fn reconnect_delay(backoff: Duration) -> Duration {
    backoff.max(MIN_RECONNECT_DELAY)
}

impl WsClient {
    pub fn new(feed: Arc<MarketFeed>, pool: EndpointPool) -> Self {
        Self { feed, pool }
    }

    /// Run forever. Admission is the pool's: when every endpoint is cooling,
    /// one rate-limited emergency probe is tried, then the loop waits for the
    /// earliest cooldown instead of hammering the whole pool.
    pub async fn run(self) {
        // The reconnect shape is the deployment's endpoint policy: the same
        // cooldown base/cap a failed HTTP endpoint gets, not a second copy.
        let policy = self.pool.policy();
        let mut backoff = Backoff::new(policy.cooldown_base(), policy.cooldown_max());
        loop {
            let now = now_ms();
            let admission = self.pool.admit(now, &[]);
            let indices = if admission.indices.is_empty() {
                if let Some(index) = self.pool.emergency_probe(now, &[]) {
                    vec![index]
                } else {
                    // The pool already names the earliest cooldown; only a
                    // zero wait is floored (it would spin). No ceiling: the
                    // configured `cooldown_max_ms` is the policy's own bound.
                    tokio::time::sleep(Duration::from_millis(cooling_wait_ms(
                        admission.retry_after_ms,
                    )))
                    .await;
                    continue;
                }
            } else {
                admission.indices
            };

            let mut served = false;
            for index in indices {
                let url = self.pool.url(index).to_string();
                match self.session(index, &url).await {
                    SessionOutcome::Served => {
                        self.pool.record_success(index, now_ms());
                        served = true;
                        break;
                    }
                    SessionOutcome::Failed(error) => {
                        self.pool.record_failure(index, now_ms());
                        tracing::warn!(
                            url = %url,
                            error = %error,
                            "market websocket endpoint failed; trying the next endpoint"
                        );
                    }
                }
            }
            self.feed.metrics.mark_ws_down();
            self.feed.metrics.inc_ws_reconnects();
            if served {
                // The endpoint was live; a later close is the network's, not
                // the endpoint's. Reset the failure backoff, but still wait
                // the floor: an accept-then-close server must not spin.
                backoff.record(Progress::Progressed);
                tokio::time::sleep(reconnect_delay(backoff.delay())).await;
                continue;
            }
            let delay = reconnect_delay(backoff.delay());
            backoff.record(Progress::Stalled);
            tokio::time::sleep(delay).await;
        }
    }

    /// One connection: subscribe, wait for the first data frame, then read
    /// until the socket drops or the data-idle deadline fires. A handshake or
    /// subscription failure, a session that never delivers data, or an idle
    /// session is `Failed` (rotate and cool); a session that delivered data
    /// and then closed is `Served` (the endpoint is healthy).
    #[allow(clippy::too_many_lines)] // one select loop: the protocol's states
    async fn session(&self, index: usize, url: &str) -> SessionOutcome {
        let (stream, _) = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(url),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => return SessionOutcome::Failed(WsError::Connect(error)),
            Err(_) => return SessionOutcome::Failed(WsError::ConnectTimeout),
        };
        let (mut sink, mut source) = stream.split();
        let subscriptions = match self.subscriptions() {
            Ok(subscriptions) => subscriptions,
            Err(error) => return SessionOutcome::Failed(error),
        };
        for sub in subscriptions {
            if let Err(error) = sink.send(Message::Text(sub.into())).await {
                return SessionOutcome::Failed(WsError::Send(error));
            }
        }

        let mut delivered = false;
        let mut deadline = tokio::time::Instant::now() + FIRST_DATA_TIMEOUT;
        let mut ping = tokio::time::interval(Duration::from_secs(30));
        // The first tick fires immediately; consume it so the first ping is
        // one interval after connect.
        ping.tick().await;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    return if delivered {
                        SessionOutcome::Failed(WsError::Idle)
                    } else {
                        SessionOutcome::Failed(WsError::NoData)
                    };
                }
                _ = ping.tick() => {
                    let ping_msg = match serde_json::to_string(&Ping { method: "ping" }) {
                        Ok(ping) => ping,
                        Err(error) => return SessionOutcome::Failed(WsError::Encode(error)),
                    };
                    if let Err(error) = sink.send(Message::Text(ping_msg.into())).await {
                        return SessionOutcome::Failed(WsError::Send(error));
                    }
                }
                msg = source.next() => match msg {
                    Some(Ok(Message::Text(text))) => match self.handle_text(&text) {
                        Frame::Data => {
                            if !delivered {
                                delivered = true;
                                self.feed.metrics.mark_ws_up();
                                self.pool.record_success(index, now_ms());
                                tracing::info!(
                                    url = %url,
                                    markets = self.feed.markets.len(),
                                    "market websocket live"
                                );
                            }
                            deadline = tokio::time::Instant::now() + DATA_IDLE_TIMEOUT;
                        }
                        Frame::Control => {}
                        Frame::Error(SubscriptionFailure::Coin(coin)) => {
                            // One refused coin is not the lane's failure: the
                            // allMids and every other coin subscription stay
                            // live. The REST poller backfills the missing coin
                            // behind the same staleness gate.
                            tracing::warn!(
                                url = %url,
                                coin = %coin,
                                "venue refused one coin subscription; keeping the session"
                            );
                        }
                        Frame::Error(SubscriptionFailure::Session(error)) => {
                            return SessionOutcome::Failed(WsError::Subscription(error));
                        }
                    },
                    Some(Ok(Message::Close(_))) | None => {
                        return if delivered {
                            SessionOutcome::Served
                        } else {
                            SessionOutcome::Failed(WsError::ClosedBeforeData)
                        };
                    }
                    Some(Ok(_)) => {} // binary/ping/pong frames: nothing to fold
                    Some(Err(error)) => {
                        return if delivered {
                            tracing::warn!(url = %url, error = %error, "market websocket read failed; reconnecting");
                            SessionOutcome::Served
                        } else {
                            SessionOutcome::Failed(WsError::Read(error))
                        };
                    }
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
        // The book is the same WSS-primary shape: the venue pushes `l2Book` at
        // its own cadence, and the REST poller backfills a coin the socket has
        // not updated within one poll interval.
        for coin in &self.feed.markets {
            subs.push(encode(Subscription::L2Book { coin })?);
        }
        Ok(subs)
    }

    fn handle_text(&self, text: &str) -> Frame {
        let msg: WsMessage = match serde_json::from_str(text) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::debug!(error = %e, "unparsable websocket message ignored");
                return Frame::Control;
            }
        };
        let now = crate::now_ms();
        match msg {
            WsMessage::AllMids { data } => {
                self.feed.put_mids(data.mids, now);
                Frame::Data
            }
            WsMessage::ActiveAssetCtx { data } => {
                self.feed.put_ctx(data.coin, data.ctx, now);
                Frame::Data
            }
            WsMessage::L2Book { data } => {
                self.feed.put_book(data, now);
                Frame::Data
            }
            WsMessage::Error { data } => Frame::Error(self.classify_subscription_error(&data)),
            WsMessage::Pong | WsMessage::Other => Frame::Control,
        }
    }

    /// Classify the venue's error frame. A message that names a configured
    /// coin (and is not an allMids refusal) is that coin's subscription
    /// failing; anything else is session-level. Matching is by whole token,
    /// so a coin like `ETH` does not match inside `METHOD`.
    fn classify_subscription_error(&self, data: &str) -> SubscriptionFailure {
        let lower = data.to_ascii_lowercase();
        if lower.contains("allmids") {
            return SubscriptionFailure::Session(data.to_string());
        }
        let tokens: Vec<&str> = lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .collect();
        for coin in &self.feed.markets {
            let coin_lower = coin.to_ascii_lowercase();
            if tokens.iter().any(|token| *token == coin_lower) {
                return SubscriptionFailure::Coin(coin.clone());
            }
        }
        SubscriptionFailure::Session(data.to_string())
    }
}

/// The WS lane's faults (logged by the reconnect loop, never propagated).
#[derive(Debug, thiserror::Error)]
pub(crate) enum WsError {
    /// The handshake did not answer inside [`CONNECT_TIMEOUT`] (a hung
    /// accept): a retryable failure, never an unbounded wait.
    #[error("connecting the market websocket: no answer in {CONNECT_TIMEOUT:?}")]
    ConnectTimeout,
    #[error("connecting the market websocket")]
    Connect(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("sending on the market websocket")]
    Send(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("reading the market websocket")]
    Read(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("encoding a websocket request")]
    Encode(#[source] serde_json::Error),
    /// The socket accepted and subscribed but never pushed data inside
    /// [`FIRST_DATA_TIMEOUT`].
    #[error("the market websocket accepted but pushed no data in {FIRST_DATA_TIMEOUT:?}")]
    NoData,
    /// The session was live, then stopped pushing for [`DATA_IDLE_TIMEOUT`]:
    /// a half-open socket, not a healthy quiet market.
    #[error("the market websocket went data-idle for {DATA_IDLE_TIMEOUT:?}")]
    Idle,
    #[error("the market websocket closed before its first data frame")]
    ClosedBeforeData,
    #[error("the venue refused a subscription: {0}")]
    Subscription(String),
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
        let ack: WsMessage = serde_json::from_str(
            r#"{"channel":"subscriptionResponse","data":{"method":"subscribe"}}"#,
        )
        .unwrap();
        assert!(matches!(ack, WsMessage::Other));
    }

    #[test]
    fn a_cooling_wait_is_the_pools_own_cooldown_not_a_shorter_ceiling() {
        assert_eq!(cooling_wait_ms(0), 1, "a zero wait must not spin");
        assert_eq!(
            cooling_wait_ms(60_000),
            60_000,
            "a configured minute cooldown is not truncated to 30 s"
        );
    }

    #[test]
    fn subscriptions_serialize_to_the_documented_shape() {
        let client = WsClient::new(
            Arc::new(MarketFeed::memory(vec!["BTC".into()])),
            EndpointPool::new(
                &["wss://example.invalid/ws".to_string()],
                diorama_anvil::Scheme::Ws,
                diorama_anvil::PoolPolicy::default(),
            )
            .unwrap(),
        );
        let subs = client.subscriptions().unwrap();
        assert_eq!(
            subs[0],
            r#"{"method":"subscribe","subscription":{"type":"allMids"}}"#
        );
        assert_eq!(
            subs[1],
            r#"{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"BTC"}}"#
        );
        assert_eq!(
            subs[2],
            r#"{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#
        );
    }

    fn client(markets: &[&str]) -> WsClient {
        WsClient::new(
            Arc::new(MarketFeed::memory(
                markets.iter().map(|coin| (*coin).to_string()).collect(),
            )),
            EndpointPool::new(
                &["wss://example.invalid/ws".to_string()],
                diorama_anvil::Scheme::Ws,
                diorama_anvil::PoolPolicy::default(),
            )
            .unwrap(),
        )
    }

    /// A per-coin refusal is isolated; an allMids/global refusal rotates. This
    /// is the classification the session's error arm branches on.
    #[test]
    fn subscription_errors_classify_per_coin_vs_session() {
        let client = client(&["BTC", "ETH"]);
        assert_eq!(
            client.classify_subscription_error("activeAssetCtx BTC: unknown coin"),
            SubscriptionFailure::Coin("BTC".into())
        );
        assert_eq!(
            client.classify_subscription_error("ETH subscription refused: invalid coin"),
            SubscriptionFailure::Coin("ETH".into())
        );
        assert_eq!(
            client.classify_subscription_error("allMids subscription refused"),
            SubscriptionFailure::Session("allMids subscription refused".into())
        );
        assert_eq!(
            client.classify_subscription_error("rate limited; retry later"),
            SubscriptionFailure::Session("rate limited; retry later".into())
        );
        // Whole-token matching: a coin name inside another word is not it.
        assert!(matches!(
            client.classify_subscription_error("METHOD unsupported"),
            SubscriptionFailure::Session(_)
        ));
    }

    #[test]
    fn reconnect_never_spins_below_the_floor() {
        assert_eq!(reconnect_delay(Duration::ZERO), MIN_RECONNECT_DELAY);
        assert_eq!(
            reconnect_delay(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        assert!(DATA_IDLE_TIMEOUT > Duration::from_secs(60));
        assert!(FIRST_DATA_TIMEOUT > Duration::from_secs(15));
    }

    /// The book is a WSS channel, not only a REST read: an `l2Book` push is
    /// folded into the same cache the `/info` `l2Book` handler serves, and it
    /// counts as live data (a session that only pushes books is healthy).
    #[test]
    fn an_l2_book_push_is_folded_as_a_live_book() {
        let feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        let client = WsClient::new(
            Arc::clone(&feed),
            EndpointPool::new(
                &["wss://example.invalid/ws".to_string()],
                diorama_anvil::Scheme::Ws,
                diorama_anvil::PoolPolicy::default(),
            )
            .unwrap(),
        );
        let frame = r#"{"channel":"l2Book","data":{"coin":"BTC","time":1700000000000,"levels":[[{"px":"100.5","sz":"1.0","n":2}],[{"px":"101.0","sz":"2.0","n":1}]],"spread":"0.5"}}"#;
        assert!(matches!(client.handle_text(frame), Frame::Data));
        let book = feed
            .book("BTC", crate::now_ms())
            .expect("the pushed book is served");
        assert_eq!(book.levels[0][0].px, Decimal::new(1005, 1));
        assert_eq!(book.levels[0][0].n, 2);
        assert_eq!(book.levels[1][0].px, Decimal::new(1010, 1));
    }
}
