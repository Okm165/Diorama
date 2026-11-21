//! The WS lane of the market plane: `allMids`, `activeAssetCtx` and `l2`
//! pushes (the primary freshness source; the REST poller backfills a coin the
//! socket has not updated within one poll interval).
//!
//! One upstream socket carries every stream (see [`crate::feed::relay`]):
//! `allMids`, an `activeAssetCtx` for every live coin of every perp dex, and
//! an `l2` book feed for each coin someone needs. The venue throttles the plain
//! `l2Book` channel to one 20-level frame every 5 to 12 s, while `l2` is a
//! snapshot plus a diff about every half second (the channel its own app
//! uses), rebuilt into full books by [`crate::feed::l2`]. The set changes while the socket
//! is up: a once-a-second sync diffs the desired streams against what the
//! socket carries and sends the `subscribe`/`unsubscribe` frames (a fresh
//! socket carries nothing, so a reconnect resubscribes the live set).
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
use crate::feed::l2;
use crate::feed::relay::{self, Op, SendBudget, Stream};
use crate::feed::MarketFeed;
use crate::wire::{AssetCtx, L2Book};
use diorama_anvil::{now_ms, Backoff, EndpointPool, Progress};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
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
    /// A full book (the plain `l2Book` channel; the lane subscribes `l2`, but
    /// a book frame is folded whichever channel carries it).
    #[serde(rename = "l2Book")]
    L2Book { data: L2Book },
    /// The `l2` feed: a snapshot, then diffs (see [`crate::feed::l2`]).
    #[serde(rename = "l2")]
    L2 { data: serde_json::Value },
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
    /// An `l2Book` push for this coin (data, and the coin's book stream is
    /// now known to be delivering).
    Book(String),
    /// An `l2` diff that could not apply to this coin's book: the stream is
    /// resubscribed for a fresh snapshot.
    Resync(String),
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
    /// The error names a coin the socket carries: drop that subscription's
    /// data, keep the session and the other coins alive.
    Coin(String),
    /// A session-wide refusal (allMids, rate limit, unknown protocol error).
    Session(String),
}

/// One subscription request.
#[derive(Debug, Serialize)]
struct Subscribe<'a> {
    /// `subscribe` or `unsubscribe`.
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
    /// The venue's `l2` feed (`c` is the coin): the book as a snapshot and
    /// diffs, not the throttled `l2Book`.
    #[serde(rename = "l2")]
    L2 { c: &'a str },
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
    /// The sent-message meter, shared by every session (a reconnect storm
    /// must not exceed the venue's per-minute message limit).
    budget: Mutex<SendBudget>,
}

/// What one socket carries and has seen: the live stream set (empty on a
/// fresh socket, which is what makes a reconnect resubscribe everything) and
/// the books whose first frame arrived.
#[derive(Default)]
struct SessionState {
    live: BTreeSet<Stream>,
    book_seen: BTreeSet<String>,
    /// The `l2` snapshots each book diff applies to.
    books: l2::Rebuilder,
    /// Books whose diffs stopped applying: the next sync unsubscribes them so
    /// the plan subscribes again and the venue sends a fresh snapshot.
    resync: BTreeSet<String>,
    /// Whether the socket has delivered a data frame (it is then a healthy
    /// endpoint whatever happens next).
    delivered: bool,
}

/// The sync cadence: the desired stream set is diffed against the socket's
/// this often, and the delivering books are confirmed current.
const SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// A book is confirmed current only while the socket delivered a frame this
/// recently (with the context streams up that is hundreds a second).
const CONFIRM_WINDOW: Duration = Duration::from_secs(10);

/// One session's outcome: `Served` means the endpoint delivered data (a later
/// drop is not its fault); `Failed` means it never became live, went idle, or
/// refused the subscription.
enum SessionOutcome {
    Served,
    Failed(WsError),
}

/// What one decoded frame asks the session's select loop to do.
enum FrameAction {
    Continue,
    Failed(WsError),
}

/// A closed socket: served when it delivered data, a failed attempt otherwise.
fn closed_outcome(delivered: bool) -> SessionOutcome {
    if delivered {
        SessionOutcome::Served
    } else {
        SessionOutcome::Failed(WsError::ClosedBeforeData)
    }
}

/// A read fault: after data the session was served (the drop is the
/// network's); before data it is a failed attempt.
fn read_outcome(
    url: &str,
    delivered: bool,
    error: tokio_tungstenite::tungstenite::Error,
) -> SessionOutcome {
    if delivered {
        tracing::warn!(url = %url, error = %error, "market websocket read failed; reconnecting");
        SessionOutcome::Served
    } else {
        SessionOutcome::Failed(WsError::Read(error))
    }
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
        Self {
            feed,
            pool,
            budget: Mutex::new(SendBudget::default()),
        }
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
    /// until the socket drops or the data-idle deadline fires, syncing the
    /// stream set once a second. A handshake or subscription failure, a
    /// session that never delivers data, or an idle session is `Failed`
    /// (rotate and cool); a session that delivered data and then closed is
    /// `Served` (the endpoint is healthy).
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
        let mut state = SessionState::default();
        let all_mids = match Self::encode("subscribe", Subscription::AllMids) {
            Ok(sub) => sub,
            Err(error) => return SessionOutcome::Failed(error),
        };
        self.budget_take(1);
        if let Err(error) = sink.send(Message::Text(all_mids.into())).await {
            return SessionOutcome::Failed(WsError::Send(error));
        }
        let mut last_data = tokio::time::Instant::now();
        let mut deadline = tokio::time::Instant::now() + FIRST_DATA_TIMEOUT;
        let mut ping = tokio::time::interval(Duration::from_secs(30));
        // The first tick fires immediately; consume it so the first ping is
        // one interval after connect.
        ping.tick().await;
        let mut sync = tokio::time::interval(SYNC_INTERVAL);
        sync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    return SessionOutcome::Failed(if state.delivered {
                        WsError::Idle
                    } else {
                        WsError::NoData
                    });
                }
                _ = ping.tick() => {
                    let ping_msg = match serde_json::to_string(&Ping { method: "ping" }) {
                        Ok(ping) => ping,
                        Err(error) => return SessionOutcome::Failed(WsError::Encode(error)),
                    };
                    self.budget_take(1);
                    if let Err(error) = sink.send(Message::Text(ping_msg.into())).await {
                        return SessionOutcome::Failed(WsError::Send(error));
                    }
                }
                _ = sync.tick() => {
                    // Nothing to wait on yet (no registry, so no stream): the
                    // first-data clock starts with the first stream.
                    if !state.delivered && state.live.is_empty() {
                        deadline = tokio::time::Instant::now() + FIRST_DATA_TIMEOUT;
                    }
                    let now = crate::now_ms();
                    let frames = match self.sync_frames(&mut state, now) {
                        Ok(frames) => frames,
                        Err(error) => return SessionOutcome::Failed(error),
                    };
                    for frame in frames {
                        if let Err(error) = sink.send(Message::Text(frame.into())).await {
                            return SessionOutcome::Failed(WsError::Send(error));
                        }
                    }
                    if last_data.elapsed() <= CONFIRM_WINDOW {
                        self.confirm_books(&state, now);
                    }
                }
                msg = source.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        deadline = tokio::time::Instant::now() + DATA_IDLE_TIMEOUT;
                        let frame = self.handle_text(&text, &mut state);
                        if matches!(frame, Frame::Data | Frame::Book(_)) {
                            last_data = tokio::time::Instant::now();
                        }
                        match self.handle_frame(index, url, frame, &mut state) {
                            FrameAction::Continue => {}
                            FrameAction::Failed(error) => return SessionOutcome::Failed(error),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return closed_outcome(state.delivered),
                    Some(Ok(_)) => {} // binary/ping/pong frames: nothing to fold
                    Some(Err(error)) => return read_outcome(url, state.delivered, error),
                },
            }
        }
    }

    /// Take `want` sends from the message budget; the count actually allowed.
    fn budget_take(&self, want: usize) -> usize {
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(crate::now_ms(), want)
    }

    /// The subscribe/unsubscribe frames that bring this socket's live set to
    /// the desired one, bounded by the send budget (the rest waits for the
    /// next sync). Updates the live set as if the frames were sent.
    fn sync_frames(&self, state: &mut SessionState, now_ms: u64) -> Result<Vec<String>, WsError> {
        let desired = self.feed.desired_streams(now_ms);
        let mut frames = Vec::new();
        // A book whose diffs stopped applying is dropped from the live set
        // (with an unsubscribe, so the venue forgets it) and the plan below
        // subscribes it again.
        for coin in std::mem::take(&mut state.resync) {
            let stream = Stream::Book(coin.clone());
            if state.live.contains(&stream) && self.budget_take(1) == 1 {
                frames.push(Self::encode("unsubscribe", Subscription::L2 { c: &coin })?);
                state.live.remove(&stream);
                state.book_seen.remove(&coin);
                state.books.forget(&coin);
            }
        }
        let ops = relay::plan(&desired, &state.live);
        if ops.is_empty() {
            return Ok(frames);
        }
        let allowed = self.budget_take(ops.len());
        frames.reserve(allowed);
        for op in ops.into_iter().take(allowed) {
            let (method, stream) = match &op {
                Op::Subscribe(stream) => ("subscribe", stream),
                Op::Unsubscribe(stream) => ("unsubscribe", stream),
            };
            let subscription = match stream {
                Stream::Book(coin) => Subscription::L2 { c: coin },
                Stream::Ctx(coin) => Subscription::ActiveAssetCtx { coin },
            };
            frames.push(Self::encode(method, subscription)?);
            match op {
                Op::Subscribe(stream) => {
                    state.live.insert(stream);
                }
                Op::Unsubscribe(stream) => {
                    if let Stream::Book(coin) = &stream {
                        state.book_seen.remove(coin);
                        state.books.forget(coin);
                    }
                    state.live.remove(&stream);
                }
            }
        }
        let books = state
            .live
            .iter()
            .filter(|s| matches!(s, Stream::Book(_)))
            .count() as u64;
        self.feed
            .metrics
            .set_relay_streams(books, state.live.len() as u64 - books);
        Ok(frames)
    }

    /// The venue pushes a book on change only, so a book that has delivered
    /// its first frame is current for as long as its stream is healthy.
    fn confirm_books(&self, state: &SessionState, now_ms: u64) {
        self.feed.confirm_books(
            state.live.iter().filter_map(|s| match s {
                Stream::Book(coin) if state.book_seen.contains(coin) => Some(coin.as_str()),
                _ => None,
            }),
            now_ms,
        );
    }

    /// Fold one decoded text frame into the cache and the lane's health.
    fn handle_frame(
        &self,
        index: usize,
        url: &str,
        frame: Frame,
        state: &mut SessionState,
    ) -> FrameAction {
        match frame {
            Frame::Book(coin) => {
                state.book_seen.insert(coin);
                return self.handle_frame(index, url, Frame::Data, state);
            }
            Frame::Data => {
                if !state.delivered {
                    state.delivered = true;
                    self.feed.metrics.mark_ws_up();
                    self.pool.record_success(index, now_ms());
                    tracing::info!(url = %url, "market websocket live");
                }
            }
            Frame::Control => {}
            Frame::Resync(coin) => {
                tracing::debug!(coin = %coin, "l2 diff did not apply; resubscribing the book");
                state.resync.insert(coin);
            }
            Frame::Error(SubscriptionFailure::Coin(coin)) => {
                // One refused coin is not the lane's failure: the allMids and
                // every other stream stay live. The REST poller backfills the
                // missing coin behind the same staleness gate, and the relay
                // does not ask for it again for a while.
                tracing::warn!(
                    url = %url,
                    coin = %coin,
                    "venue refused one coin subscription; keeping the session"
                );
                let now = crate::now_ms();
                for stream in [Stream::Book(coin.clone()), Stream::Ctx(coin)] {
                    state.live.remove(&stream);
                    self.feed.relay.refuse(stream, now);
                }
            }
            Frame::Error(SubscriptionFailure::Session(error)) => {
                return FrameAction::Failed(WsError::Subscription(error));
            }
        }
        FrameAction::Continue
    }

    /// One subscription frame, encoded. An encode fault is a session fault
    /// (the reconnect retries), never an empty message: a silent `""` would
    /// leave the socket "connected" with no data flowing.
    fn encode(method: &'static str, sub: Subscription<'_>) -> Result<String, WsError> {
        serde_json::to_string(&Subscribe {
            method,
            subscription: sub,
        })
        .map_err(WsError::Encode)
    }

    fn handle_text(&self, text: &str, state: &mut SessionState) -> Frame {
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
                let coin = data.coin.clone();
                self.feed.put_book(data, now);
                Frame::Book(coin)
            }
            WsMessage::L2 { data } => match state.books.apply(&data) {
                Some(book) => {
                    let coin = book.coin.clone();
                    self.feed.put_book(book, now);
                    Frame::Book(coin)
                }
                None => match l2::frame_coin(&data) {
                    // A diff that cannot apply (no snapshot, a gap) leaves
                    // the coin's book silently stale: ask for a fresh one.
                    Some(coin) if state.live.contains(&Stream::Book(coin.clone())) => {
                        Frame::Resync(coin)
                    }
                    _ => Frame::Control,
                },
            },
            WsMessage::Error { data } => Frame::Error(classify_subscription_error(
                state.live.iter().map(Stream::coin),
                &data,
            )),
            WsMessage::Pong | WsMessage::Other => Frame::Control,
        }
    }
}

/// Classify the venue's error frame. A message that names a coin the socket
/// carries (and is not an allMids refusal) is that coin's subscription
/// failing; anything else is session-level. Matching is by whole token, so a
/// coin like `ETH` does not match inside `METHOD`. A token keeps its `:`
/// (`xyz:tsla` is one token, never `xyz` and `tsla`), so a builder coin
/// matches itself and `xyz:BTC` does not match the plain coin `BTC`; a
/// builder coin also matches as a whole substring, for a name whose
/// punctuation would split a token.
fn classify_subscription_error<'a>(
    coins: impl Iterator<Item = &'a str>,
    data: &str,
) -> SubscriptionFailure {
    let lower = data.to_ascii_lowercase();
    if lower.contains("allmids") {
        return SubscriptionFailure::Session(data.to_string());
    }
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != ':')
        .map(|token| token.trim_matches(':'))
        .filter(|token| !token.is_empty())
        .collect();
    for coin in coins {
        let coin_lower = coin.to_ascii_lowercase();
        if tokens.iter().any(|token| *token == coin_lower)
            || (coin_lower.contains(':') && lower.contains(&coin_lower))
        {
            return SubscriptionFailure::Coin(coin.to_string());
        }
    }
    SubscriptionFailure::Session(data.to_string())
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

    fn pool() -> EndpointPool {
        EndpointPool::new(
            &["wss://example.invalid/ws".to_string()],
            diorama_anvil::Scheme::Ws,
            diorama_anvil::PoolPolicy::default(),
        )
        .unwrap()
    }

    /// A client over a feed that lists BTC and a delisted OLD on the default
    /// dex and `xyz:TSLA` on a builder dex.
    fn client() -> WsClient {
        let feed = Arc::new(MarketFeed::memory(Vec::new()));
        let mut default = crate::sim::meta_of(&[("BTC", 40), ("OLD", 10)]);
        default.universe[1].is_delisted = Some(true);
        feed.put_meta(default, 1);
        feed.put_dex_meta("xyz".to_string(), crate::sim::meta_of(&[("xyz:TSLA", 10)]), 1);
        WsClient::new(feed, pool())
    }

    #[test]
    fn subscriptions_serialize_to_the_documented_shape() {
        let encode = |method, sub| WsClient::encode(method, sub).unwrap();
        assert_eq!(
            encode("subscribe", Subscription::AllMids),
            r#"{"method":"subscribe","subscription":{"type":"allMids"}}"#
        );
        assert_eq!(
            encode("subscribe", Subscription::ActiveAssetCtx { coin: "BTC" }),
            r#"{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"BTC"}}"#
        );
        assert_eq!(
            encode("unsubscribe", Subscription::L2 { c: "xyz:TSLA" }),
            r#"{"method":"unsubscribe","subscription":{"type":"l2","c":"xyz:TSLA"}}"#,
            "a builder coin keeps its colon"
        );
    }

    fn frames(client: &WsClient, state: &mut SessionState, now: u64) -> Vec<String> {
        client.sync_frames(state, now).unwrap()
    }

    fn sub(coin: &str) -> String {
        format!(r#"{{"method":"subscribe","subscription":{{"type":"l2","c":"{coin}"}}}}"#)
    }

    fn ctx_sub(coin: &str) -> String {
        format!(
            r#"{{"method":"subscribe","subscription":{{"type":"activeAssetCtx","coin":"{coin}"}}}}"#
        )
    }

    /// The first sync subscribes a context for every live coin of every dex
    /// (the delisted one waits until it is wanted); a wanted coin adds its
    /// book; a second sync with nothing changed sends nothing.
    #[test]
    fn the_first_sync_subscribes_every_live_context_and_a_wanted_book() {
        let client = client();
        let mut state = SessionState::default();
        let now = crate::now_ms();
        let mut first = frames(&client, &mut state, now);
        first.sort();
        assert_eq!(first, [ctx_sub("BTC"), ctx_sub("xyz:TSLA")]);
        assert!(frames(&client, &mut state, now).is_empty(), "nothing new");

        assert!(client.feed.want_book("xyz:TSLA"));
        assert_eq!(frames(&client, &mut state, now), [sub("xyz:TSLA")]);
        assert_eq!(client.feed.metrics.relay_streams(), (1, 2));
    }

    /// The grace: a coin nobody needs any more is unsubscribed only after the
    /// grace window; a downstream lease holds it for as long as it lives.
    #[test]
    fn a_book_is_unsubscribed_after_the_last_user_left_plus_the_grace() {
        let client = client();
        let mut state = SessionState::default();
        let now = crate::now_ms();
        let _ = frames(&client, &mut state, now);
        let lease = client.feed.lease_coin("BTC").unwrap();
        assert_eq!(frames(&client, &mut state, now), [sub("BTC")]);
        assert!(
            frames(&client, &mut state, now + 10 * relay::GRACE_MS).is_empty(),
            "a lease holds the stream"
        );
        drop(lease);
        let released = crate::now_ms();
        assert!(
            frames(&client, &mut state, released + relay::GRACE_MS - 2_000).is_empty(),
            "still inside the grace"
        );
        assert_eq!(
            frames(&client, &mut state, released + relay::GRACE_MS + 2_000),
            [r#"{"method":"unsubscribe","subscription":{"type":"l2","c":"BTC"}}"#],
            "the stream goes after the grace"
        );
    }

    /// A reconnect is a fresh session state: the whole live set is
    /// resubscribed, books included.
    #[test]
    fn a_reconnect_resubscribes_the_live_set() {
        let client = client();
        let now = crate::now_ms();
        assert!(client.feed.want_book("BTC"));
        let mut first = SessionState::default();
        let mut before = frames(&client, &mut first, now);
        before.sort();
        let mut second = SessionState::default();
        let mut after = frames(&client, &mut second, now);
        after.sort();
        assert_eq!(before, after);
        assert_eq!(after.len(), 3, "two contexts and the wanted book");
    }

    /// The send budget bounds a resubscribe burst: the rest waits for the next
    /// sync instead of exceeding the venue's per-minute message limit.
    #[test]
    fn a_burst_of_subscribes_is_bounded_by_the_send_budget() {
        let client = client();
        let now = crate::now_ms();
        client.budget_take(1_199);
        let mut state = SessionState::default();
        assert_eq!(frames(&client, &mut state, now).len(), 1, "one left");
        assert_eq!(state.live.len(), 1);
        assert!(frames(&client, &mut state, now).is_empty(), "budget spent");
    }

    /// A per-coin refusal is isolated; an allMids/global refusal rotates. This
    /// is the classification the session's error arm branches on.
    #[test]
    fn subscription_errors_classify_per_coin_vs_session() {
        let coins = || ["BTC", "ETH"].into_iter();
        assert_eq!(
            classify_subscription_error(coins(), "activeAssetCtx BTC: unknown coin"),
            SubscriptionFailure::Coin("BTC".into())
        );
        assert_eq!(
            classify_subscription_error(coins(), "ETH subscription refused: invalid coin"),
            SubscriptionFailure::Coin("ETH".into())
        );
        assert_eq!(
            classify_subscription_error(coins(), "allMids subscription refused"),
            SubscriptionFailure::Session("allMids subscription refused".into())
        );
        assert_eq!(
            classify_subscription_error(coins(), "rate limited; retry later"),
            SubscriptionFailure::Session("rate limited; retry later".into())
        );
        // Whole-token matching: a coin name inside another word is not it.
        assert!(matches!(
            classify_subscription_error(coins(), "METHOD unsupported"),
            SubscriptionFailure::Session(_)
        ));
    }

    /// A builder coin's refusal names `dex:COIN`: it must isolate that coin
    /// (tokenising on `:` split it into `xyz` and `tsla` and never matched, so
    /// one refused builder coin rotated the whole socket), and must not be
    /// mistaken for the plain coin of the same ticker.
    #[test]
    fn a_builder_coin_refusal_is_isolated_to_that_coin() {
        let coins = || ["BTC", "xyz:TSLA", "xyz:BTC"].into_iter();
        assert_eq!(
            classify_subscription_error(coins(), "Invalid subscription: activeAssetCtx xyz:TSLA"),
            SubscriptionFailure::Coin("xyz:TSLA".into())
        );
        assert_eq!(
            classify_subscription_error(coins(), "xyz:tsla: unknown coin"),
            SubscriptionFailure::Coin("xyz:TSLA".into())
        );
        assert_eq!(
            classify_subscription_error(coins(), "l2Book xyz:BTC refused"),
            SubscriptionFailure::Coin("xyz:BTC".into())
        );
        // The plain ticker is still its own coin.
        assert_eq!(
            classify_subscription_error(coins(), "l2Book BTC refused"),
            SubscriptionFailure::Coin("BTC".into())
        );
        // A dex alone is not a coin.
        assert!(matches!(
            classify_subscription_error(coins(), "unknown dex xyz"),
            SubscriptionFailure::Session(_)
        ));
    }

    /// A refused coin leaves the live set and the relay does not ask for it
    /// again inside the cooldown.
    #[test]
    fn a_refused_coin_is_dropped_and_not_asked_for_again() {
        let client = client();
        let mut state = SessionState::default();
        let now = crate::now_ms();
        let _ = frames(&client, &mut state, now);
        state.delivered = true;
        let action = client.handle_frame(
            0,
            "ws://x",
            Frame::Error(SubscriptionFailure::Coin("xyz:TSLA".into())),
            &mut state,
        );
        assert!(matches!(action, FrameAction::Continue));
        assert!(!state.live.contains(&Stream::Ctx("xyz:TSLA".into())));
        assert!(
            frames(&client, &mut state, now).is_empty(),
            "the refused coin is not resubscribed in the cooldown"
        );
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
        let client = client();
        let feed = Arc::clone(&client.feed);
        let mut state = SessionState::default();
        let frame = r#"{"channel":"l2Book","data":{"coin":"BTC","time":1700000000000,"levels":[[{"px":"100.5","sz":"1.0","n":2}],[{"px":"101.0","sz":"2.0","n":1}]],"spread":"0.5"}}"#;
        assert!(matches!(
            client.handle_text(frame, &mut state),
            Frame::Book(coin) if coin == "BTC"
        ));
        let book = feed
            .book("BTC", crate::now_ms())
            .expect("the pushed book is served");
        assert_eq!(book.levels[0][0].px, Decimal::new(1005, 1));
        assert_eq!(book.levels[0][0].n, 2);
        assert_eq!(book.levels[1][0].px, Decimal::new(1010, 1));
    }

    /// The `l2` feed: a snapshot starts the book, each diff lands in the same
    /// cache `/info` serves (so the book moves at the venue's own cadence), and
    /// a diff that cannot apply resubscribes the stream for a fresh snapshot.
    #[test]
    fn the_l2_feed_rebuilds_the_book_and_a_broken_diff_resubscribes() {
        let client = client();
        let now = crate::now_ms();
        assert!(client.feed.want_book("BTC"));
        let mut state = SessionState::default();
        let _ = frames(&client, &mut state, now);
        let feed = Arc::clone(&client.feed);
        let feed_frame = |state: &mut SessionState, text: &str| {
            let frame = client.handle_text(text, state);
            client.handle_frame(0, "ws://x", frame, state);
        };
        feed_frame(
            &mut state,
            r#"{"channel":"l2","data":{"s":{"coin":"BTC","time":5,"levels":[[{"px":"100","sz":"1","n":2},{"px":"99","sz":"1","n":1}],[{"px":"101","sz":"1","n":1}]]}}}"#,
        );
        assert!(state.book_seen.contains("BTC"));
        feed_frame(
            &mut state,
            r#"{"channel":"l2","data":{"u":{"c":"BTC","t":6,"l":[[{"p":"100","s":"7"}],[{"p":"100.5","s":"3"}]],"r":[[1],[]]}}}"#,
        );
        let book = feed.book("BTC", crate::now_ms()).expect("served");
        assert_eq!(book.levels[0].len(), 1);
        assert_eq!(book.levels[0][0].sz, Decimal::from(7));
        assert_eq!(book.levels[1][0].px, Decimal::new(1005, 1));
        assert_eq!(book.levels[1].len(), 2);

        // A diff for a coin the lane has no snapshot of: resubscribe it.
        state.books.forget("BTC");
        feed_frame(
            &mut state,
            r#"{"channel":"l2","data":{"u":{"c":"BTC","t":7,"l":[[],[]],"r":[[],[]]}}}"#,
        );
        assert!(state.resync.contains("BTC"));
        assert_eq!(
            frames(&client, &mut state, now),
            [
                r#"{"method":"unsubscribe","subscription":{"type":"l2","c":"BTC"}}"#.to_string(),
                sub("BTC")
            ]
        );
        assert!(state.resync.is_empty());
    }

    /// A book whose first frame arrived is confirmed current while the socket
    /// delivers; one that never delivered is not conjured.
    #[test]
    fn a_delivering_book_stream_is_confirmed_a_silent_one_is_not() {
        let client = client();
        let now = crate::now_ms();
        assert!(client.feed.want_book("BTC"));
        let mut state = SessionState::default();
        let _ = frames(&client, &mut state, now);
        let frame = r#"{"channel":"l2Book","data":{"coin":"BTC","time":1,"levels":[[{"px":"100","sz":"1","n":1}],[{"px":"101","sz":"1","n":1}]]}}"#;
        let seen = client.handle_text(frame, &mut state);
        client.handle_frame(0, "ws://x", seen, &mut state);
        assert!(state.book_seen.contains("BTC"));
        // 40 s on, with no new push, the confirmation keeps the quiet book fresh.
        let later = now + 40_000;
        assert!(matches!(
            client.feed.book_age("BTC", later, MarketFeed::MAX_STALE_MS),
            crate::feed::BookAge::Stale(_)
        ));
        client.confirm_books(&state, later);
        assert_eq!(
            client.feed.book_age("BTC", later, MarketFeed::MAX_STALE_MS),
            crate::feed::BookAge::Fresh
        );
    }

    /// A fake upstream venue: accepts one websocket at a time, records every
    /// text frame it receives, answers each `activeAssetCtx`/`l2Book`
    /// subscribe with one push, and drops the first connection after a push
    /// to exercise the reconnect. The real lane (`WsClient::run`) runs
    /// against it end to end.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one scripted upstream: the sequence is the test
    async fn the_lane_relays_streams_and_resubscribes_after_a_reconnect() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received: Arc<Mutex<Vec<(usize, String)>>> = Arc::default();
        let connections = Arc::new(AtomicUsize::new(0));
        let (seen, conns) = (received.clone(), connections.clone());
        let server = tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let id = conns.fetch_add(1, Ordering::Relaxed);
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = socket.next().await {
                        let text = text.to_string();
                        seen.lock().unwrap().push((id, text.clone()));
                        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                        let sub = &value["subscription"];
                        let reply = match (value["method"].as_str(), sub["type"].as_str()) {
                            (Some("subscribe"), Some("activeAssetCtx")) => Some(format!(
                                r#"{{"channel":"activeAssetCtx","data":{{"coin":"{}","ctx":{{"funding":"0.0000125","openInterest":"1","prevDayPx":"1","dayNtlVlm":"1","premium":null,"oraclePx":"{}","markPx":"{}","midPx":"{}","impactPxs":null,"dayBaseVlm":"1"}}}}}}"#,
                                sub["coin"].as_str().unwrap(),
                                100 + id,
                                100 + id,
                                100 + id,
                            )),
                            (Some("subscribe"), Some("l2")) => Some(format!(
                                r#"{{"channel":"l2","data":{{"s":{{"coin":"{}","time":1,"levels":[[{{"px":"99","sz":"1","n":1}}],[{{"px":"101","sz":"1","n":1}}]]}}}}}}"#,
                                sub["c"].as_str().unwrap()
                            )),
                            _ => None,
                        };
                        if let Some(reply) = reply {
                            if socket.send(Message::Text(reply.into())).await.is_err() {
                                return;
                            }
                        }
                        // The first connection is dropped once it has served
                        // a book: the lane must come back and resubscribe.
                        if id == 0 && text.contains(r#""type":"l2""#) {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            return;
                        }
                    }
                });
            }
        });

        let feed = Arc::new(MarketFeed::memory(Vec::new()));
        feed.put_meta(crate::sim::meta_of(&[("BTC", 40)]), 1);
        feed.put_dex_meta("xyz".to_string(), crate::sim::meta_of(&[("xyz:TSLA", 10)]), 1);
        let pool = EndpointPool::new(
            &[format!("ws://{addr}")],
            diorama_anvil::Scheme::Ws,
            diorama_anvil::PoolPolicy::default(),
        )
        .unwrap();
        let task = tokio::spawn(WsClient::new(feed.clone(), pool).run());

        // Contexts for every coin of every dex arrive with nobody asking.
        let wait = |feed: Arc<MarketFeed>, coin: &'static str, mark: i64| async move {
            for _ in 0..150 {
                if feed.mark(coin) == Some(Decimal::from(mark)) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        };
        assert!(wait(feed.clone(), "BTC", 100).await, "BTC ctx relayed");
        assert!(wait(feed.clone(), "xyz:TSLA", 100).await, "builder ctx too");
        // A wanted builder coin gets its book streamed.
        assert!(feed.want_book("xyz:TSLA"));
        let mut book = None;
        for _ in 0..150 {
            book = feed.book("xyz:TSLA", crate::now_ms());
            if book.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(book.is_some(), "the builder book is relayed live");
        // The first connection drops; the second resubscribes the live set
        // (contexts and the wanted book) and its pushes land.
        assert!(
            wait(feed.clone(), "BTC", 101).await,
            "BTC ctx after reconnect"
        );
        let log = received.lock().unwrap().clone();
        let on = |id: usize, needle: &str| {
            log.iter()
                .any(|(conn, text)| *conn == id && text.contains(needle))
        };
        for id in [0, 1] {
            assert!(on(id, r#""type":"allMids""#), "allMids on connection {id}");
            assert!(on(id, r#""coin":"BTC""#) && on(id, r#""coin":"xyz:TSLA""#));
        }
        assert!(
            log.iter()
                .any(|(conn, text)| *conn == 1 && text.contains(r#""type":"l2""#)),
            "the book stream was resubscribed after the reconnect"
        );
        task.abort();
        server.abort();
    }

    /// Read-only against the real venue (`cargo test -- --ignored live_relay`):
    /// the registry from `perpDexs` + `allPerpMetas`, then the real lane over
    /// the real socket. Every live coin of every dex must get a context, and a
    /// wanted builder coin its book, within the budget. Nothing is signed or
    /// placed: only info reads and subscriptions.
    #[tokio::test]
    #[ignore = "network: reads the real venue"]
    #[allow(clippy::too_many_lines)] // one scripted live run
    async fn live_relay_against_the_real_venue() {
        let rest = crate::feed::rest::RestClient::new(&["https://api.hyperliquid.xyz".to_string()])
            .unwrap();
        let listed = rest
            .info::<Vec<serde_json::Value>>(&crate::wire::InfoRequest::PerpDexs)
            .await
            .unwrap();
        let metas = rest
            .info::<Vec<crate::wire::Meta>>(&crate::wire::InfoRequest::AllPerpMetas)
            .await
            .unwrap();
        let names: Vec<String> = listed
            .iter()
            .map(|entry| {
                entry
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        let feed = Arc::new(MarketFeed::memory(Vec::new()));
        feed.put_perp_dexs(
            names
                .iter()
                .map(|name| {
                    if name.is_empty() {
                        None
                    } else {
                        Some(crate::wire::PerpDex {
                            name: name.clone(),
                            full_name: String::new(),
                            deployer: alloy_primitives::Address::ZERO,
                            oracle_updater: None,
                            fee_recipient: None,
                            asset_to_streaming_oi_cap: Vec::new(),
                            asset_to_funding_clamp: Vec::new(),
                            asset_to_funding_interest_rate: Vec::new(),
                            asset_to_funding_multiplier: Vec::new(),
                            sub_deployers: Vec::new(),
                        })
                    }
                })
                .collect(),
        );
        for (name, meta) in names.iter().zip(metas.into_iter()) {
            feed.put_dex_meta(name.clone(), meta, crate::now_ms());
        }
        let live = feed.permanent_ctx_coins();
        let pool = EndpointPool::new(
            &["wss://api.hyperliquid.xyz/ws".to_string()],
            diorama_anvil::Scheme::Ws,
            diorama_anvil::PoolPolicy::default(),
        )
        .unwrap();
        let task = tokio::spawn(WsClient::new(feed.clone(), pool).run());
        assert!(feed.want_book("xyz:TSLA"));
        assert!(feed.want_book("BTC"));
        tokio::time::sleep(Duration::from_secs(25)).await;
        let now = crate::now_ms();
        let missing: Vec<&String> = live
            .iter()
            .filter(|coin| feed.stale_ms(coin, now).is_none_or(|age| age > 10_000))
            .collect();
        println!(
            "live coins {}, without a fresh ctx {}: {:?}",
            live.len(),
            missing.len(),
            &missing[..missing.len().min(10)]
        );
        println!("streams (books, ctxs) {:?}", feed.metrics.relay_streams());
        for coin in ["xyz:TSLA", "BTC"] {
            let book = feed.book(coin, now).expect("book relayed");
            println!(
                "{coin} book levels {}/{}",
                book.levels[0].len(),
                book.levels[1].len()
            );
            assert_eq!(
                feed.book_age(coin, now, MarketFeed::MAX_STALE_MS),
                crate::feed::BookAge::Fresh
            );
        }
        println!(
            "xyz:TSLA mid {:?}",
            feed.all_mids_of("xyz").0.get("xyz:TSLA")
        );
        assert!(missing.len() * 20 < live.len(), "nearly every coin is live");
        // The `l2` feed moves a busy book about twice a second (the plain
        // `l2Book` channel would show one change per 5 to 12 s).
        let mut stamps = std::collections::BTreeSet::new();
        for _ in 0..100 {
            if let Some(book) = feed.book("BTC", crate::now_ms()) {
                stamps.insert(book.time);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!("BTC book updates in 10 s: {}", stamps.len());
        assert!(stamps.len() >= 6, "the book follows the l2 feed's cadence");
        task.abort();
    }
}
