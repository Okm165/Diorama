//! The venue's websocket lane (the sim serves "info + WS" on its
//! info port): the channels the venue pushes (`allMids`, `activeAssetCtx`,
//! `l2Book`, the per-dex account channels, `webData3`, ...) fed from the same
//! composed cache the REST reads serve, so a subscriber never sees a different
//! mark/book/account than the account plane used.
//!
//! Protocol (`feed::ws`'s vocabulary, the client's own): the client subscribes
//! with `{"method":"subscribe","subscription":{"type":…}}`, the server acks
//! with a `subscriptionResponse` frame and pushes `{"channel":…,"data":…}` on
//! each tick while the subscription is live (the account automaton's tick,
//! the same cadence every account channel keeps); a whole-state market frame
//! is pushed on the first read and again only when its content changed, a
//! diffing channel (`fastAssetCtxs`, `pac`, `l2`) compares against its own
//! previous state and stays quiet when nothing moved. `{"method":"ping"}`
//! answers `{"channel":"pong"}`. An unsupported channel (or an unknown coin)
//! is answered with the venue's `error` frame — never silence.
//!
//! Books are on demand through the feed's one-shot read
//! ([`MarketFeed::ensure_book`]): a coin subscription warms its book before
//! admission.
//!
//! `l2Book` pushes the composed book exactly as `/info`'s `l2Book` serves it
//! (`{"coin","time","levels"}`, the live book stamped with its last update,
//! a scripted book with the scenario's present). Every perp dex is served:
//! `clearinghouseState` and `openOrders` take a `dex` (`openOrders` also
//! `ALL_DEXS`), `allDexsClearinghouseState` and `pac` list every dex (the
//! default dex first, then alphabetically, as the venue does), and
//! `fastAssetCtxs` is one map over every dex's coins. The venue's quirks are
//! mirrored: `allMids` for a named dex acks and pushes nothing, and
//! `clearinghouseState`/`openOrders` for an unlisted dex are not even acked.
//! A builder dex's clearinghouse carries that dex's own ledger.
use super::candles::CandleSub;
use super::ws_codec::{self, FastCtx};
use super::ApiState;
use crate::feed::dex::ALL_DEXS;
use crate::sim::Sim;
use crate::wire::{
    ActiveAssetData, AllMids, AssetCtx, ClearinghouseState, L2Book, MarginSummary,
    SpotClearinghouseState, UserAbstraction,
};
use alloy_primitives::Address;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// One subscription the sim serves, in the real venue's vocabulary. The ack
/// echoes the subscription with the venue's defaults filled in (`nSigFigs:
/// null`, `fast: false`, `dex: ""`, ...), exactly as the real venue does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Subscription {
    /// `dex` is echoed in the ack only when it names a dex (the venue acks
    /// `{"type":"allMids"}` for an empty one).
    #[serde(rename = "allMids")]
    AllMids {
        #[serde(
            default,
            deserialize_with = "strict_dex",
            skip_serializing_if = "dex_omitted"
        )]
        dex: Option<String>,
    },
    #[serde(rename = "activeAssetCtx")]
    ActiveAssetCtx { coin: String },
    #[serde(rename = "l2Book")]
    L2Book {
        coin: String,
        #[serde(rename = "nSigFigs", default)]
        n_sig_figs: Option<u8>,
        #[serde(default)]
        mantissa: Option<u8>,
        #[serde(default)]
        fast: bool,
    },
    /// The venue's account page channel: one entry per dex.
    #[serde(rename = "webData3")]
    WebData3 { user: Address },
    /// The real venue's per-dex account state channel (`{dex, user,
    /// clearinghouseState}`).
    #[serde(rename = "clearinghouseState")]
    ClearinghouseState {
        #[serde(default)]
        dex: String,
        user: Address,
    },
    /// The real venue's open-orders channel (`{dex, user, orders}`).
    #[serde(rename = "openOrders")]
    OpenOrders {
        #[serde(default)]
        dex: String,
        user: Address,
    },
    /// The real venue's spot-state channel (`{user, spotState}`).
    #[serde(rename = "spotState")]
    SpotState {
        user: Address,
        #[serde(rename = "ignorePortfolioMargin", default)]
        ignore_portfolio_margin: bool,
    },
    /// Every perp's mark and mid, deflate-compressed, changed fields after the
    /// first frame.
    #[serde(rename = "fastAssetCtxs")]
    FastAssetCtxs,
    /// Every perp's full context, per dex, deflate-compressed, changed fields
    /// after the first frame.
    #[serde(rename = "pac")]
    Pac,
    /// The order book: a full book first, compressed diffs after it.
    #[serde(rename = "l2")]
    L2 {
        c: String,
        #[serde(default)]
        s: Option<u8>,
        #[serde(default)]
        m: Option<u8>,
    },
    #[serde(rename = "candle")]
    Candle { coin: String, interval: String },
    /// The account's state on every perp dex (`[[dex, state], ...]`).
    #[serde(rename = "allDexsClearinghouseState")]
    AllDexsClearinghouseState { user: Address },
    #[serde(rename = "userFills")]
    UserFills {
        user: Address,
        #[serde(rename = "aggregateByTime", default)]
        aggregate_by_time: bool,
    },
    #[serde(rename = "userFundings")]
    UserFundings { user: Address },
    #[serde(rename = "activeAssetData")]
    ActiveAssetData { user: Address, coin: String },
    /// The best bid and ask of a coin's book, pushed when either changes.
    #[serde(rename = "bbo")]
    Bbo { coin: String },
    /// The account's order status changes (`open`, `filled`, `canceled`, ...).
    #[serde(rename = "orderUpdates")]
    OrderUpdates { user: Address },
    /// The account's fills and funding payments, one event object per frame
    /// (pushed on channel `user`).
    #[serde(rename = "userEvents")]
    UserEvents { user: Address },
}

/// A `dex` field: absent is `None`, a string is `Some`, and an explicit
/// `null` is a request the venue cannot parse.
fn strict_dex<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    <String as Deserialize>::deserialize(deserializer).map(Some)
}

/// An absent or empty `dex` stays out of the ack's echo.
fn dex_omitted(dex: &Option<String>) -> bool {
    dex.as_deref().is_none_or(str::is_empty)
}

/// The candle intervals the venue accepts.
const CANDLE_INTERVALS: [&str; 14] = [
    "1m", "3m", "5m", "15m", "30m", "1h", "2h", "4h", "8h", "12h", "1d", "3d", "1w", "1M",
];

/// One client request (the client's `Subscribe`/`Ping` shapes, plus
/// `unsubscribe`: the venue's own method set).
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
    #[serde(rename = "webData3")]
    WebData3 { data: WebData3Data },
    #[serde(rename = "clearinghouseState")]
    ClearinghouseState { data: ClearinghouseStateData },
    #[serde(rename = "openOrders")]
    OpenOrders { data: OpenOrdersData },
    #[serde(rename = "spotState")]
    SpotState { data: SpotStateData },
    /// Base64 of a raw deflate stream (see `ws_codec`).
    #[serde(rename = "fastAssetCtxs")]
    FastAssetCtxs { data: String },
    #[serde(rename = "pac")]
    Pac { data: String },
    /// `{"s": book}` first, `{"c": compressed diff}` after.
    #[serde(rename = "l2")]
    L2 { data: serde_json::Value },
    #[serde(rename = "allDexsClearinghouseState")]
    AllDexsClearinghouseState { data: serde_json::Value },
    #[serde(rename = "userFills")]
    UserFills { data: serde_json::Value },
    #[serde(rename = "userFundings")]
    UserFundings { data: serde_json::Value },
    #[serde(rename = "activeAssetData")]
    ActiveAssetData { data: ActiveAssetData },
    #[serde(rename = "bbo")]
    Bbo { data: serde_json::Value },
    #[serde(rename = "orderUpdates")]
    OrderUpdates { data: serde_json::Value },
    /// The `userEvents` subscription's frames (the venue's channel is `user`).
    #[serde(rename = "user")]
    UserEvent { data: serde_json::Value },
    #[serde(rename = "subscriptionResponse")]
    SubscriptionResponse { data: SubscriptionAck },
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "error")]
    Error { data: String },
    /// No frame at all: the venue stays silent (not even an ack) on a
    /// subscription it cannot serve, such as `clearinghouseState` for a dex
    /// it does not list. The session never serializes it.
    #[serde(skip)]
    Silent,
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

/// The real venue's `webData3` payload (measured on the public socket): the
/// user's identity facts and one state per perp dex, in `perpDexs` order. The
/// venue sends each dex's `totalVaultEquity` and, only where some coin is at
/// its open-interest cap, `perpsAtOpenInterestCap`; the per-dex clearinghouse,
/// orders and contexts are not part of what the public socket sends, so they
/// are not served here either.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebData3Data {
    user_state: WebData3User,
    perp_dex_states: Vec<WebData3Dex>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebData3User {
    /// The account's API wallet; `null` when none is registered.
    agent_address: Option<Address>,
    /// The API wallet's expiry (ms); `null` when none is registered.
    agent_valid_until: Option<u64>,
    /// The account's cumulative ledger (net deposits). The sim keeps no
    /// deposit ledger, so it serves `"0.0"`.
    cum_ledger: String,
    server_time: u64,
    is_vault: bool,
    user: Address,
    abstraction: UserAbstraction,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebData3Dex {
    /// The sim has no vaults: always `"0.0"`.
    total_vault_equity: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    perps_at_open_interest_cap: Vec<String>,
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
    orders: Vec<super::info::FrontendOpenOrder>,
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

/// What a live subscription remembers between pushes: the previous state the
/// next diff is computed against.
enum Mem {
    /// A channel that pushes its whole state every tick (the account
    /// channels).
    Whole,
    /// A whole-state market channel: pushed on the first read and then only
    /// when its content (a book's levels, not its stamp) differs from the last
    /// frame sent.
    Dedup(Option<String>),
    /// Nothing pushed yet on a diffing channel.
    Fresh,
    Fast(BTreeMap<String, FastCtx>),
    Pac(ws_codec::PacSnapshot),
    L2(L2Book),
    Candle(CandleSub),
    /// The newest fill id (`tid`) already pushed.
    Fills(Option<u64>),
    /// The newest funding `(time, coin)` already pushed.
    Fundings(Option<(u64, String)>),
    /// The last best bid/ask pushed.
    Bbo(Option<serde_json::Value>),
    /// The orders already reported (`None` until primed).
    Orders(Option<OrderMem>),
    /// The newest fill id and funding `(time, coin)` already seen.
    Events(Option<(u64, (u64, String))>),
}

/// The order ids an `orderUpdates` subscription has already reported.
#[derive(Default)]
struct OrderMem {
    open: std::collections::BTreeSet<u64>,
    done: std::collections::BTreeSet<u64>,
}

/// One live subscription and what it remembers.
pub(crate) struct Live {
    sub: Subscription,
    mem: Mem,
    /// The upstream relay lease: the coin's book stays wanted while this
    /// subscription lives (dropped on unsubscribe or disconnect).
    _lease: Option<crate::feed::relay::Lease>,
}

/// The coin whose book a subscription reads, if any (the session warms it
/// through [`MarketFeed::ensure_book`] before admission).
fn coin_of(sub: &Subscription) -> Option<&str> {
    match sub {
        Subscription::ActiveAssetCtx { coin }
        | Subscription::L2Book { coin, .. }
        | Subscription::Bbo { coin }
        | Subscription::ActiveAssetData { coin, .. } => Some(coin),
        Subscription::L2 { c, .. } => Some(c),
        _ => None,
    }
}

/// Warm one subscribe request's coin, when it names one: the feed's one-shot
/// on-demand read, so a book that has not landed yet is fetched before the
/// subscription is admitted. The fork's relay keeps the stream
/// live after the first read (and per-dex books); this is the gap fill until
/// then.
async fn warm(text: &str, state: &ApiState) {
    if let Ok(WsRequest::Subscribe { subscription }) = serde_json::from_str(text) {
        if let Some(coin) = coin_of(&subscription) {
            let _ = state.feed.ensure_book(coin).await;
        }
    }
}

/// One connection: read requests, push a subscription's first frame right
/// after its ack (the real venue does), and push every live channel on the
/// account automaton's tick. A whole-state market frame is deduplicated, a
/// diffing channel compares with its own previous state, and an unchanged
/// tick sends nothing.
#[allow(clippy::too_many_lines)] // one select loop: the arms are the session
async fn session(state: Arc<ApiState>, mut socket: WebSocket) {
    let mut subs: Vec<Live> = Vec::new();
    let mut tick = tokio::time::interval(state.push_interval);
    let mut changes = state.feed.subscribe_changes();
    // When the pending market flush is due: set by the first change of a
    // burst, cleared by the flush that composes it (25 ms, the documented
    // coalescing window — the venue does not stream per-write).
    let mut flush_at: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if !push_channels(&mut socket, &mut subs, &state, false).await
                    || !push_channels(&mut socket, &mut subs, &state, true).await
                {
                    return;
                }
            }
            signal = changes.changed() => {
                if signal.is_err() {
                    return;
                }
                if flush_at.is_none() {
                    flush_at = Some(tokio::time::Instant::now() + MARKET_COALESCE);
                }
            }
            () = async {
                match flush_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                flush_at = None;
                if !push_channels(&mut socket, &mut subs, &state, true).await {
                    return;
                }
            }
            msg = socket.recv() => {
                let Some(Ok(Message::Text(text))) = msg else {
                    if msg.is_none() || matches!(msg, Some(Err(_))) { return; }
                    continue; // binary/ping/pong frames: nothing to answer
                };
                warm(&text, &state).await;
                let before = subs.len();
                let reply = request_reply(&text, &mut subs, &state);
                if matches!(reply, WsPush::Silent) {
                    continue;
                }
                let Ok(reply_text) = serde_json::to_string(&reply) else { continue };
                if socket.send(Message::Text(reply_text.into())).await.is_err() {
                    return;
                }
                if subs.len() > before {
                    if let Some(live) = subs.last_mut() {
                        for frame in pushes(live, &state, crate::now_ms()) {
                            if socket.send(Message::Text(frame.into())).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The market channels ride the feed's change signal (coalesced); every
/// account channel stays on the automaton's tick.
fn is_market(sub: &Subscription) -> bool {
    matches!(
        sub,
        Subscription::L2Book { .. }
            | Subscription::L2 { .. }
            | Subscription::Bbo { .. }
            | Subscription::ActiveAssetCtx { .. }
            | Subscription::ActiveAssetData { .. }
            | Subscription::Candle { .. }
    )
}

/// The coalescing window for a market change burst (the venue's own cadence).
const MARKET_COALESCE: std::time::Duration = std::time::Duration::from_millis(25);

/// Push one tick's frames for every live subscription. One clock read per
/// tick: every channel in it serves the same instant (a scripted book is
/// stamped with it). `false` when the socket is gone.
async fn push_channels(
    socket: &mut WebSocket,
    subs: &mut [Live],
    state: &ApiState,
    market: bool,
) -> bool {
    let now = crate::now_ms();
    for live in subs.iter_mut() {
        if is_market(&live.sub) != market {
            continue;
        }
        for frame in pushes(live, state, now) {
            if socket.send(Message::Text(frame.into())).await.is_err() {
                return false;
            }
        }
    }
    true
}

/// The venue's answer to a request it cannot parse: the request text echoed.
fn parse_error(text: &str) -> WsPush {
    WsPush::Error {
        data: format!("Error parsing JSON into valid websocket request: {text}"),
    }
}

/// One request's answer: the ack or the error, or the pong. A subscription is
/// added at most once and removed at most once: the venue answers a repeat of
/// either with an error frame naming the subscription, not an ack.
#[allow(clippy::too_many_lines)] // one dispatch over every subscription method
pub(super) fn request_reply(text: &str, subs: &mut Vec<Live>, state: &ApiState) -> WsPush {
    let Ok(request) = serde_json::from_str::<WsRequest>(text) else {
        return parse_error(text);
    };
    match request {
        WsRequest::Ping => WsPush::Pong,
        WsRequest::Subscribe { subscription } => {
            let named = subscription_json(&subscription);
            if subs
                .iter()
                .any(|live| subscription_json(&live.sub) == named)
            {
                return WsPush::Error {
                    data: format!("Already subscribed: {named}"),
                };
            }
            let mem = match admit(&subscription, state) {
                Ok(mem) => mem,
                Err(Refusal::Parse) => return parse_error(text),
                Err(Refusal::Invalid) => {
                    return WsPush::Error {
                        data: format!("Invalid subscription {named}"),
                    };
                }
                Err(Refusal::Named(reason)) => return WsPush::Error { data: reason },
                Err(Refusal::Silent) => return WsPush::Silent,
            };
            // The coin's book was warmed before admission (the session's
            // `warm`); the subscription now holds the relay lease, so the
            // upstream stream stays on it while this client wants it.
            let lease = coin_of(&subscription).and_then(|coin| state.feed.lease_coin(coin));
            subs.push(Live {
                sub: subscription.clone(),
                mem,
                _lease: lease,
            });
            WsPush::SubscriptionResponse {
                data: SubscriptionAck {
                    method: "subscribe",
                    subscription,
                },
            }
        }
        WsRequest::Unsubscribe { subscription } => {
            let before = subs.len();
            let named = subscription_json(&subscription);
            subs.retain(|live| subscription_json(&live.sub) != named);
            if subs.len() == before {
                return WsPush::Error {
                    data: format!("Already unsubscribed: {}", subscription_json(&subscription)),
                };
            }
            WsPush::SubscriptionResponse {
                data: SubscriptionAck {
                    method: "unsubscribe",
                    subscription,
                },
            }
        }
    }
}

/// A subscription as the venue's error frames print it.
fn subscription_json(sub: &Subscription) -> String {
    serde_json::to_string(sub).unwrap_or_default()
}

/// Why a subscription is refused.
enum Refusal {
    /// The request does not parse as a valid one (the venue's parse error).
    Parse,
    /// A well-formed subscription the venue does not serve (unknown coin).
    Invalid,
    /// A refusal that names its own reason (the sim's own lanes).
    Named(String),
    /// The venue answers with nothing at all (no ack, no error).
    Silent,
}

/// Admit a subscription: `Ok` with the memory its pushes diff against, or the
/// refusal. A bookless coin is refused at subscribe time, never left silent:
/// the composed book is the same one `/info`'s `l2Book` serves.
#[allow(clippy::too_many_lines)] // one match over the served subscriptions: splitting it would hide which channel refuses what
fn admit(sub: &Subscription, state: &ApiState) -> Result<Mem, Refusal> {
    let feed = &state.feed;
    // The account plane is addressable from the API state: a touched account
    // is computed from the sim, an untouched one is served the venue's empty
    // shape (the same contract `/info`'s passthrough holds). Only a state
    // read fault refuses the subscription.
    let account_ok =
        |user: &Address| {
            state.sim.account(*user).map(|_| ()).map_err(|e| {
                Refusal::Named(format!("account state for `{user}` is unavailable: {e}"))
            })
        };
    let has_book = |coin: &str| {
        // The session warms a coin subscription through the feed's one-shot
        // on-demand read before admission; a book still absent is refused
        // (a coin no dex lists included). The fork's relay keeps
        // the stream live; this is the gap fill until then.
        if feed.book(coin, crate::now_ms()).is_some() {
            Ok(())
        } else {
            Err(Refusal::Named(format!(
                "coin `{coin}` has no book in the venue's feed"
            )))
        }
    };
    match sub {
        Subscription::AllMids { .. } => Ok(Mem::Dedup(None)),
        // A listed coin whose context has not landed yet is served as soon as
        // it does (the push retries each tick); an unlisted coin is invalid.
        Subscription::ActiveAssetCtx { coin }
            if feed.mark(coin).is_some() || feed.meta_for_coin(coin).is_some() =>
        {
            Ok(Mem::Dedup(None))
        }
        Subscription::ActiveAssetCtx { .. } => Err(Refusal::Invalid),
        Subscription::L2Book {
            coin,
            n_sig_figs,
            mantissa,
            ..
        } => {
            super::info::book_params_ok(*n_sig_figs, *mantissa).map_err(|_| Refusal::Invalid)?;
            has_book(coin)?;
            Ok(Mem::Dedup(None))
        }
        Subscription::L2 { c, s, m } => {
            super::info::book_params_ok(*s, *m).map_err(|_| Refusal::Invalid)?;
            has_book(c)?;
            Ok(Mem::Fresh)
        }
        // A dex the venue does not list (and `ALL_DEXS`, which this channel
        // does not take) gets no frame at all, not even an ack.
        Subscription::ClearinghouseState { dex, user } => {
            if super::info::dex_scope(feed, dex, false).is_none() {
                return Err(Refusal::Silent);
            }
            account_ok(user)?;
            Ok(Mem::Whole)
        }
        Subscription::OpenOrders { dex, user } => {
            if super::info::dex_scope(feed, dex, true).is_none() {
                return Err(Refusal::Silent);
            }
            account_ok(user)?;
            Ok(Mem::Whole)
        }
        Subscription::WebData3 { user }
        | Subscription::SpotState { user, .. }
        | Subscription::AllDexsClearinghouseState { user } => {
            account_ok(user)?;
            Ok(Mem::Whole)
        }
        Subscription::ActiveAssetData { user, coin } => {
            account_ok(user)?;
            if feed.mark(coin).is_none() {
                return Err(Refusal::Invalid);
            }
            Ok(Mem::Whole)
        }
        Subscription::FastAssetCtxs | Subscription::Pac => Ok(Mem::Fresh),
        Subscription::Candle { coin, interval } => {
            if !CANDLE_INTERVALS.contains(&interval.as_str()) {
                return Err(Refusal::Parse);
            }
            state
                .candles
                .subscribe(coin, interval)
                .map(Mem::Candle)
                .ok_or_else(|| {
                    Refusal::Named("the venue's candle stream is unavailable".to_string())
                })
        }
        Subscription::UserFills { user, .. } => {
            account_ok(user)?;
            Ok(Mem::Fills(None))
        }
        Subscription::UserFundings { user } => {
            account_ok(user)?;
            Ok(Mem::Fundings(None))
        }
        Subscription::Bbo { coin } => {
            has_book(coin)?;
            Ok(Mem::Bbo(None))
        }
        Subscription::OrderUpdates { user } => {
            account_ok(user)?;
            Ok(Mem::Orders(None))
        }
        Subscription::UserEvents { user } => {
            account_ok(user)?;
            Ok(Mem::Events(None))
        }
    }
}

/// The frames one live subscription pushes on this tick: none while a diffing
/// channel has nothing new, one for the whole-state channels.
#[allow(clippy::too_many_lines)] // one match over the channels: the diffing state lives in the arms
pub(super) fn pushes(live: &mut Live, state: &ApiState, now_ms: u64) -> Vec<String> {
    let feed = &state.feed;
    match (&live.sub, &mut live.mem) {
        (Subscription::FastAssetCtxs, mem) => {
            let next = ws_codec::fast_snapshot(feed);
            if next.is_empty() {
                // The meta has not landed: nothing to push yet, retry.
                return Vec::new();
            }
            let prev = match mem {
                Mem::Fast(prev) => Some(&*prev),
                _ => None,
            };
            let Some(frame) = ws_codec::fast_frame(prev, &next) else {
                return Vec::new();
            };
            *mem = Mem::Fast(next);
            encode(&WsPush::FastAssetCtxs { data: frame })
        }
        (Subscription::Pac, mem) => {
            let next = ws_codec::pac_snapshot(feed);
            if next.is_empty() {
                // No dex has landed: nothing to push yet, retry.
                return Vec::new();
            }
            let prev = match mem {
                Mem::Pac(prev) => Some(&*prev),
                _ => None,
            };
            let Some(frame) = ws_codec::pac_frame(prev, &next) else {
                return Vec::new();
            };
            *mem = Mem::Pac(next);
            encode(&WsPush::Pac { data: frame })
        }
        (Subscription::L2 { c, s, m }, mem) => {
            let Some(book) = feed.book(c, now_ms) else {
                return encode(&WsPush::Error {
                    data: format!("coin `{c}` has no book in the venue's feed"),
                });
            };
            let next = match s {
                Some(_) => super::info::aggregate_book(book, *s, m.unwrap_or(1)),
                None => book,
            };
            let data = match mem {
                Mem::L2(prev) => {
                    let Some(diff) = ws_codec::l2_diff(prev, &next) else {
                        return Vec::new();
                    };
                    serde_json::json!({ "c": ws_codec::deflate_b64(&diff) })
                }
                _ => serde_json::json!({ "s": next }),
            };
            *mem = Mem::L2(next);
            encode(&WsPush::L2 { data })
        }
        (Subscription::Candle { .. }, Mem::Candle(candles)) => {
            let mut frames = Vec::new();
            if let Some(first) = candles.take_first() {
                frames.push(first);
            }
            if let Some(latest) = candles.latest() {
                frames.push(latest);
            }
            frames
        }
        (Subscription::UserFills { user, .. }, Mem::Fills(cursor)) => {
            user_fills_push(state, *user, cursor)
        }
        (Subscription::UserFundings { user }, Mem::Fundings(cursor)) => {
            user_fundings_push(state, *user, cursor)
        }
        (Subscription::Bbo { coin }, Mem::Bbo(last)) => bbo_push(state, coin, last, now_ms),
        (Subscription::OrderUpdates { user }, Mem::Orders(seen)) => {
            order_updates_push(state, *user, seen)
        }
        (Subscription::UserEvents { user }, Mem::Events(cursor)) => {
            user_events_push(state, *user, cursor)
        }
        (sub, Mem::Dedup(last)) => {
            let Some(frame) = push_frame(sub, state, now_ms) else {
                return Vec::new();
            };
            let key = dedup_key(sub, &frame);
            if last.as_deref() == Some(key.as_str()) {
                return Vec::new();
            }
            *last = Some(key);
            vec![frame]
        }
        (sub, _) => push_frame(sub, state, now_ms).into_iter().collect(),
    }
}

/// What makes a whole-state market frame "the same" as the last one sent: the
/// frame itself, except a book's stamp (a book whose levels did not change is
/// not an update, whatever its time says).
fn dedup_key(sub: &Subscription, frame: &str) -> String {
    if !matches!(sub, Subscription::L2Book { .. }) {
        return frame.to_string();
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(frame) else {
        return frame.to_string();
    };
    if let Some(data) = value.get_mut("data").and_then(|d| d.as_object_mut()) {
        data.remove("time");
    }
    value.to_string()
}

fn encode(push: &WsPush) -> Vec<String> {
    match serde_json::to_string(push) {
        Ok(frame) => vec![frame],
        Err(e) => {
            tracing::warn!(error = %e, "ws push serialization failed");
            Vec::new()
        }
    }
}

/// The levels per side a `fast: true` `l2Book` carries (measured on the venue).
const FAST_BOOK_DEPTH: usize = 5;

/// The venue's `fast: true` `l2Book` frame: the top [`FAST_BOOK_DEPTH`] levels
/// of each side and a `fast: true` key beside `coin`, `time` and `levels`.
fn fast_book_frame(mut book: L2Book) -> Option<String> {
    for side in &mut book.levels {
        side.truncate(FAST_BOOK_DEPTH);
    }
    let mut data = serde_json::to_value(&book).ok()?;
    data.as_object_mut()?
        .insert("fast".to_string(), serde_json::Value::Bool(true));
    let frame = serde_json::json!({"channel": "l2Book", "data": data});
    Some(frame.to_string())
}

/// The frame a whole-state subscription pushes, from the composed cache.
/// `None` = the coin has no ctx yet (the feed is warming; the tick retries) or
/// the channel is one of the diffing ones (`pushes` owns those): a
/// serialization fault on these plain shapes is a bug, named here.
#[allow(clippy::too_many_lines)] // one match over the market channels
fn push_frame(sub: &Subscription, state: &ApiState, now_ms: u64) -> Option<String> {
    let feed = &state.feed;
    let push = match sub {
        // A named dex's `allMids` is acked and then silent (the venue's own
        // quirk: only the default and `ALL_DEXS` selections carry data).
        Subscription::AllMids { dex } => match dex.as_deref() {
            None => WsPush::AllMids {
                data: AllMidsData {
                    mids: feed.all_mids(),
                },
            },
            Some(ALL_DEXS) => WsPush::AllMids {
                data: AllMidsData {
                    mids: super::info::mids_in(feed, &super::info::DexScope::All),
                },
            },
            Some(_) => return None,
        },
        Subscription::ActiveAssetCtx { coin } => WsPush::ActiveAssetCtx {
            data: CtxData {
                coin: coin.clone(),
                ctx: crate::sim::account::ctx_of(feed, coin)?,
            },
        },
        // The composed book: scripted override over the live remainder, stamped
        // exactly as the REST read stamps it (`feed.book` owns that rule),
        // aggregated per the subscription's parameters.
        Subscription::L2Book {
            coin,
            n_sig_figs,
            mantissa,
            fast,
        } => match feed.book(coin, now_ms) {
            Some(book) => {
                let book = match n_sig_figs {
                    Some(_) => {
                        super::info::aggregate_book(book, *n_sig_figs, mantissa.unwrap_or(1))
                    }
                    None => book,
                };
                if *fast {
                    // The venue's fast book: the top levels only, tagged.
                    return fast_book_frame(book);
                }
                WsPush::L2Book { data: book }
            }
            // A book that vanished after the ack is an error frame, not
            // silence (the subscriber must know its channel stopped serving).
            None => WsPush::Error {
                data: format!("coin `{coin}` has no book in the venue's feed"),
            },
        },
        // The account plane's channels share one body; the channel name only
        // selects the frame shape.
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
#[allow(clippy::too_many_lines)] // one match over the account channels
fn account_push(sub: &Subscription, state: &ApiState) -> Option<WsPush> {
    let push = match sub {
        Subscription::WebData3 { user } => match web_data3(state, *user) {
            Ok(data) => WsPush::WebData3 { data },
            Err(e) => WsPush::Error {
                data: format!("webData3 for `{user}` is unavailable: {e}"),
            },
        },
        Subscription::ClearinghouseState { dex, user } => match dex_state(state, *user, dex) {
            Ok(clearinghouse_state) => WsPush::ClearinghouseState {
                data: ClearinghouseStateData {
                    dex: dex.clone(),
                    user: *user,
                    clearinghouse_state,
                },
            },
            Err(e) => WsPush::Error {
                data: format!("clearinghouseState for `{user}` is unavailable: {e}"),
            },
        },
        Subscription::AllDexsClearinghouseState { user } => match all_dexs_states(state, *user) {
            Ok(states) => WsPush::AllDexsClearinghouseState {
                data: serde_json::json!({
                    "user": user,
                    "clearinghouseStates": states,
                }),
            },
            Err(e) => WsPush::Error {
                data: format!("allDexsClearinghouseState for `{user}` is unavailable: {e}"),
            },
        },
        Subscription::OpenOrders { dex, user } => open_orders_push(*user, dex, state),
        Subscription::SpotState { user, .. } => spot_state_push(*user, state),
        Subscription::ActiveAssetData { user, coin } => match state.sim.account(*user) {
            Ok(Some(account)) => {
                match super::info::active_asset_data(state, *user, &account, coin) {
                    Ok(data) => WsPush::ActiveAssetData { data },
                    Err(e) => WsPush::Error {
                        data: format!("activeAssetData for `{user}` is unavailable: {e}"),
                    },
                }
            }
            Ok(None) => WsPush::Error {
                data: format!(
                    "activeAssetData for `{user}` is unavailable: not a simulated account"
                ),
            },
            Err(e) => WsPush::Error {
                data: format!("activeAssetData for `{user}` is unavailable: {e}"),
            },
        },
        _ => return None,
    };
    Some(push)
}

/// The account's perp state on one dex: the sim's clearinghouse for a touched
/// account, the venue's empty shape for an untouched one (the WS lane cannot
/// pass through).
fn dex_state(
    state: &ApiState,
    user: Address,
    dex: &str,
) -> crate::error::Result<ClearinghouseState> {
    Ok(match super::info::dex_clearinghouse_of(state, user, dex)? {
        Some(served) => served,
        None => empty_clearinghouse(Sim::served_time(crate::now_ms())),
    })
}

/// Every dex's state as `[[dex, state], ...]`: the default dex first, then the
/// others alphabetically (the venue's order, measured), whether or not the
/// account trades there.
fn all_dexs_states(
    state: &ApiState,
    user: Address,
) -> crate::error::Result<Vec<(String, ClearinghouseState)>> {
    ws_codec::dex_order(&state.feed)
        .into_iter()
        .map(|dex| dex_state(state, user, &dex).map(|served| (dex, served)))
        .collect()
}

/// The `openOrders` channel's frame: the sim's resting rows in the venue's
/// `{dex, user, orders}` shape (the same rows `frontendOpenOrders` serves),
/// for one dex or `ALL_DEXS`.
fn open_orders_push(user: Address, dex: &str, state: &ApiState) -> WsPush {
    let Some(scope) = super::info::dex_scope(&state.feed, dex, true) else {
        return WsPush::Error {
            data: format!("openOrders for `{user}` names no dex the venue lists: `{dex}`"),
        };
    };
    match super::info::resting_in(state, user, &scope) {
        Ok(rows) => WsPush::OpenOrders {
            data: OpenOrdersData {
                dex: dex.to_string(),
                user,
                orders: rows.iter().map(super::info::frontend_row).collect(),
            },
        },
        Err(e) => WsPush::Error {
            data: format!("openOrders for `{user}` is unavailable: {e}"),
        },
    }
}

/// The most a `userFills` or `userFundings` snapshot carries.
const SNAPSHOT_PAGE: usize = 2_000;

/// The `userFills` frames: the account's recent fills as a snapshot on the
/// first push, each new fill after it. The ledger is the sim's own; an
/// address the sim never touched has none.
fn user_fills_push(state: &ApiState, user: Address, cursor: &mut Option<u64>) -> Vec<String> {
    let fills = state.sim.user_fills(user).unwrap_or_default();
    let (fresh, snapshot): (Vec<_>, bool) = match *cursor {
        None => {
            let skip = fills.len().saturating_sub(SNAPSHOT_PAGE);
            (fills.into_iter().skip(skip).collect(), true)
        }
        Some(last) => (fills.into_iter().filter(|f| f.tid > last).collect(), false),
    };
    if !snapshot && fresh.is_empty() {
        return Vec::new();
    }
    if let Some(newest) = fresh.last() {
        *cursor = Some(newest.tid);
    } else {
        // An empty snapshot still counts as pushed.
        *cursor = Some(0);
    }
    let mut data = serde_json::json!({ "user": user, "fills": fresh });
    if snapshot {
        data["isSnapshot"] = serde_json::Value::Bool(true);
    }
    encode(&WsPush::UserFills { data })
}

/// The `userFundings` frames: same contract as the fills, for funding payments
/// (served flat, without the info route's ledger wrapper).
fn user_fundings_push(
    state: &ApiState,
    user: Address,
    cursor: &mut Option<(u64, String)>,
) -> Vec<String> {
    let all = state.sim.user_fundings(user, 0).unwrap_or_default();
    let (fresh, snapshot): (Vec<_>, bool) = match cursor.as_ref() {
        None => {
            let skip = all.len().saturating_sub(SNAPSHOT_PAGE);
            (all.into_iter().skip(skip).collect(), true)
        }
        Some((time, coin)) => (
            all.into_iter()
                .filter(|f| (f.time, f.coin.as_str()) > (*time, coin.as_str()))
                .collect(),
            false,
        ),
    };
    if !snapshot && fresh.is_empty() {
        return Vec::new();
    }
    *cursor = Some(
        fresh
            .last()
            .map_or((0, String::new()), |f| (f.time, f.coin.clone())),
    );
    let mut data = serde_json::json!({ "user": user, "fundings": fresh });
    if snapshot {
        data["isSnapshot"] = serde_json::Value::Bool(true);
    }
    encode(&WsPush::UserFundings { data })
}

/// The `bbo` frame: the book's best bid and ask (`null` for an empty side),
/// pushed on the first read and whenever either changes.
fn bbo_push(
    state: &ApiState,
    coin: &str,
    last: &mut Option<serde_json::Value>,
    now_ms: u64,
) -> Vec<String> {
    let Some(book) = state.feed.book(coin, now_ms) else {
        return Vec::new();
    };
    let side = |i: usize| {
        book.levels[i]
            .first()
            .map_or(serde_json::Value::Null, |l| serde_json::json!(l))
    };
    let bbo = serde_json::json!([side(0), side(1)]);
    if last.as_ref() == Some(&bbo) {
        return Vec::new();
    }
    *last = Some(bbo.clone());
    encode(&WsPush::Bbo {
        data: serde_json::json!({ "coin": coin, "time": book.time, "bbo": bbo }),
    })
}

/// One order as `orderUpdates` carries it (the venue's `WsBasicOrder`).
fn ws_order(r: &crate::sim::order::RestingOrder, sz: &rust_decimal::Decimal) -> serde_json::Value {
    let row = super::info::frontend_row(r);
    let mut order = serde_json::json!({
        "coin": row.coin,
        "side": row.side,
        "limitPx": row.limit_px,
        "sz": super::info::dtext(sz),
        "oid": row.oid,
        "timestamp": row.timestamp,
        "origSz": row.orig_sz,
    });
    if let Some(cloid) = r.cloid {
        order["cloid"] = serde_json::Value::String(format!("{cloid:#x}"));
    }
    order
}

/// The `orderUpdates` frames: the account's orders whose status changed since
/// the last tick (`open` when one starts resting; `filled`, `canceled` or
/// `marginCanceled` when it ends). Nothing is pushed for what existed at
/// subscribe time: the channel carries changes only.
fn order_updates_push(state: &ApiState, user: Address, seen: &mut Option<OrderMem>) -> Vec<String> {
    let Ok((resting, terminal)) = state.sim.order_history(user) else {
        return Vec::new();
    };
    let prime = seen.is_none();
    let mem = seen.get_or_insert_with(OrderMem::default);
    let mut updates: Vec<(u64, serde_json::Value)> = Vec::new();
    for r in &resting {
        if mem.open.insert(r.oid) && !prime {
            updates.push((
                r.placed_ms,
                serde_json::json!({
                    "order": ws_order(r, &r.sz),
                    "status": "open",
                    "statusTimestamp": r.placed_ms,
                }),
            ));
        }
    }
    for t in &terminal {
        if mem.done.insert(t.order.oid) && !prime {
            let sz = if matches!(t.status, crate::sim::order::TerminalStatus::Filled) {
                rust_decimal::Decimal::ZERO
            } else {
                t.order.sz
            };
            updates.push((
                t.status_ms,
                serde_json::json!({
                    "order": ws_order(&t.order, &sz),
                    "status": super::info::terminal_status_label(t.status),
                    "statusTimestamp": t.status_ms,
                }),
            ));
        }
    }
    if updates.is_empty() {
        return Vec::new();
    }
    updates.sort_by_key(|(time, _)| *time);
    let rows: Vec<serde_json::Value> = updates.into_iter().map(|(_, row)| row).collect();
    encode(&WsPush::OrderUpdates {
        data: serde_json::Value::Array(rows),
    })
}

/// The `userEvents` frames: `{"fills": [...]}` for new fills and one
/// `{"funding": {...}}` per new funding payment. Nothing is pushed for what
/// existed at subscribe time.
fn user_events_push(
    state: &ApiState,
    user: Address,
    cursor: &mut Option<(u64, (u64, String))>,
) -> Vec<String> {
    let fills = state.sim.user_fills(user).unwrap_or_default();
    let fundings = state.sim.user_fundings(user, 0).unwrap_or_default();
    let newest_fill = fills.last().map_or(0, |f| f.tid);
    let newest_funding = fundings
        .last()
        .map_or((0, String::new()), |f| (f.time, f.coin.clone()));
    let Some((last_fill, last_funding)) = cursor.clone() else {
        *cursor = Some((newest_fill, newest_funding));
        return Vec::new();
    };
    let mut frames = Vec::new();
    let fresh: Vec<_> = fills.into_iter().filter(|f| f.tid > last_fill).collect();
    if !fresh.is_empty() {
        frames.extend(encode(&WsPush::UserEvent {
            data: serde_json::json!({ "fills": fresh }),
        }));
    }
    for f in fundings
        .into_iter()
        .filter(|f| (f.time, f.coin.as_str()) > (last_funding.0, last_funding.1.as_str()))
    {
        frames.extend(encode(&WsPush::UserEvent {
            data: serde_json::json!({ "funding": f }),
        }));
    }
    *cursor = Some((newest_fill.max(last_fill), newest_funding.max(last_funding)));
    frames
}

/// The `spotState` channel's frame: the same balances `/info` serves, in the
/// WS shape (which carries no `tokenToAvailableAfterMaintenance`).
fn spot_state_push(user: Address, state: &ApiState) -> WsPush {
    match spot_state_of(state, user) {
        Ok(spot_state) => WsPush::SpotState {
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

/// The API wallet expiry served for a simulated account's agent: the sim's
/// agents never expire, so it is the far future (2100-01-01 UTC, ms).
const AGENT_NEVER_EXPIRES_MS: u64 = 4_102_444_800_000;

/// The account's served `webData3` payload: the identity facts the sim has
/// and one state per dex in `perpDexs` order (see [`WebData3Data`]).
fn web_data3(state: &ApiState, user: Address) -> crate::error::Result<WebData3Data> {
    let account = state.sim.account(user)?;
    let (agent, abstraction) = match &account {
        Some(a) => (
            a.agents.first().map(|w| w.wallet),
            crate::sim::account::mode_of(a),
        ),
        None => (None, UserAbstraction::Default),
    };
    let mut perp_dex_states: Vec<WebData3Dex> = state
        .feed
        .dex_collateral_tokens()
        .into_iter()
        .map(|(dex, _)| WebData3Dex {
            total_vault_equity: "0.0".to_string(),
            perps_at_open_interest_cap: state.feed.open_interest_caps_of(&dex).unwrap_or_default(),
        })
        .collect();
    if perp_dex_states.is_empty() {
        perp_dex_states.push(WebData3Dex {
            total_vault_equity: "0.0".to_string(),
            perps_at_open_interest_cap: state.feed.open_interest_caps_of("").unwrap_or_default(),
        });
    }
    Ok(WebData3Data {
        user_state: WebData3User {
            agent_address: agent,
            agent_valid_until: agent.map(|_| AGENT_NEVER_EXPIRES_MS),
            cum_ledger: "0.0".to_string(),
            server_time: Sim::served_time(crate::now_ms()),
            is_vault: false,
            user,
            abstraction,
        },
        perp_dex_states,
    })
}

/// The account's served spot state. An untouched address is served the
/// venue's empty shape (the `/info` passthrough's documented contract); the WS
/// lane cannot pass through, so it serves that same shape rather than
/// refusing.
fn spot_state_of(state: &ApiState, user: Address) -> crate::error::Result<SpotClearinghouseState> {
    Ok(match state.sim.account(user)? {
        Some(account) => SpotClearinghouseState {
            balances: crate::sim::account::served_spot_balances(&account, &state.feed)?,
            token_to_available_after_maintenance: crate::sim::account::available_after_maintenance(
                &account,
                &state.feed,
            )?,
        },
        None => SpotClearinghouseState {
            balances: Vec::new(),
            token_to_available_after_maintenance: Vec::new(),
        },
    })
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

    fn book_sub(coin: &str) -> Subscription {
        Subscription::L2Book {
            coin: coin.into(),
            n_sig_figs: None,
            mantissa: None,
            fast: false,
        }
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
                    dex: None,
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
                    dex: None,
                    amount: Decimal::from(25),
                },
            )
            .unwrap();

        let frames = vec![
            push_frame(&Subscription::AllMids { dex: None }, &state, 2_000).unwrap(),
            push_frame(
                &Subscription::ActiveAssetCtx { coin: "BTC".into() },
                &state,
                2_000,
            )
            .unwrap(),
            push_frame(&book_sub("BTC"), &state, 2_000).unwrap(),
            push_frame(&Subscription::WebData3 { user }, &state, 2_000).unwrap(),
            push_frame(
                &Subscription::ClearinghouseState {
                    dex: String::new(),
                    user,
                },
                &state,
                2_000,
            )
            .unwrap(),
            push_frame(
                &Subscription::OpenOrders {
                    dex: String::new(),
                    user,
                },
                &state,
                2_000,
            )
            .unwrap(),
            push_frame(
                &Subscription::SpotState {
                    user,
                    ignore_portfolio_margin: false,
                },
                &state,
                2_000,
            )
            .unwrap(),
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
                "webData3" => {
                    assert!(
                        !matches!(parsed, WsMessage::Error { .. }),
                        "webData3 parsed as an error: {text}"
                    );
                    let data = &value["data"];
                    assert_eq!(data["userState"]["user"], serde_json::json!(user));
                    assert_eq!(data["userState"]["abstraction"], "default");
                    assert_eq!(data["perpDexStates"].as_array().unwrap().len(), 1);
                    assert_eq!(data["perpDexStates"][0]["totalVaultEquity"], "0.0");
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
        let mut subs: Vec<Live> = Vec::new();

        let ack = request_reply(
            r#"{"method":"subscribe","subscription":{"type":"allMids"}}"#,
            &mut subs,
            &state,
        );
        assert!(matches!(ack, WsPush::SubscriptionResponse { .. }));
        let repeat = request_reply(
            r#"{"method":"subscribe","subscription":{"type":"allMids"}}"#,
            &mut subs,
            &state,
        );
        assert_eq!(subs.len(), 1, "a duplicate subscribe is not queued twice");
        assert!(
            matches!(&repeat, WsPush::Error { data } if data == r#"Already subscribed: {"type":"allMids"}"#),
            "{repeat:?}"
        );

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
                r#"{{"method":"subscribe","subscription":{{"type":"webData3","user":"{user}"}}}}"#
            ),
            &mut subs,
            &state,
        );
        assert!(matches!(ack, WsPush::SubscriptionResponse { .. }));
        assert_eq!(subs.len(), 3, "webData3 is a served channel");
        let unknown = Address::repeat_byte(0xff);
        let ack = request_reply(
            &format!(
                r#"{{"method":"subscribe","subscription":{{"type":"webData3","user":"{unknown}"}}}}"#
            ),
            &mut subs,
            &state,
        );
        assert!(
            matches!(ack, WsPush::SubscriptionResponse { .. }),
            "an untouched account is served the venue's empty shape, not refused"
        );
        let frame = push_frame(&Subscription::WebData3 { user: unknown }, &state, 2_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["channel"], "webData3");
        assert_eq!(
            value["data"]["userState"]["agentAddress"],
            serde_json::Value::Null
        );
        assert_eq!(value["data"]["userState"]["abstraction"], "default");

        // `webData2` is gone: it reads like any unknown subscription.
        let legacy = format!(
            r#"{{"method":"subscribe","subscription":{{"type":"webData2","user":"{user}"}}}}"#
        );
        let error = request_reply(&legacy, &mut subs, &state);
        assert!(
            matches!(&error, WsPush::Error { data } if *data == format!("Error parsing JSON into valid websocket request: {legacy}")),
            "{error:?}"
        );
        assert_eq!(subs.len(), 4, "the refused channel is not queued");

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
        // Unsubscribing something that is not live is the venue's own error.
        let error = request_reply(
            r#"{"method":"unsubscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#,
            &mut subs,
            &state,
        );
        assert!(
            matches!(&error, WsPush::Error { data } if data.starts_with("Already unsubscribed: ")),
            "{error:?}"
        );
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
        let sub = book_sub("BTC");
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

        let missing = push_frame(&book_sub("NOPE"), &state, 5_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&missing).unwrap();
        assert_eq!(value["channel"], "error");
    }

    /// `l2Book {fast: true}` is the venue's fast book: five levels a side and
    /// a `fast` key; the plain subscription keeps the full book and no key.
    #[test]
    fn a_fast_l2_book_is_five_levels_tagged_fast() {
        let feed = Arc::new(MarketFeed::memory(vec!["BTC".into()]));
        let mut book = test_book("BTC", "100");
        for i in 1..8 {
            book.levels[0].push(Level {
                px: Decimal::from(100 - i),
                sz: Decimal::ONE,
                n: 1,
            });
            book.levels[1].push(Level {
                px: Decimal::from(101 + i),
                sz: Decimal::ONE,
                n: 1,
            });
        }
        feed.put_book(book, 1_000);
        let state = api_state(feed);
        let fast = Subscription::L2Book {
            coin: "BTC".into(),
            n_sig_figs: None,
            mantissa: None,
            fast: true,
        };
        let frame = push_frame(&fast, &state, 5_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["channel"], "l2Book");
        assert_eq!(value["data"]["fast"], true);
        assert_eq!(value["data"]["levels"][0].as_array().unwrap().len(), 5);
        assert_eq!(value["data"]["levels"][1].as_array().unwrap().len(), 5);
        assert_eq!(value["data"]["levels"][0][0]["px"], "100.0");

        let plain = push_frame(&book_sub("BTC"), &state, 5_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&plain).unwrap();
        assert!(value["data"].get("fast").is_none());
        assert_eq!(value["data"]["levels"][0].as_array().unwrap().len(), 8);
    }

    /// `webData3` carries the identity facts and one state per dex in
    /// `perpDexs` order; only a dex with a coin at its open-interest cap
    /// carries `perpsAtOpenInterestCap` (the venue's measured shape), and an
    /// account's API wallet and abstraction are the sim's.
    #[test]
    fn web_data3_serves_one_state_per_dex_with_only_what_the_venue_sends() {
        let (state, user) = dex_world();
        state.feed.put_oi_caps(vec!["BTC".to_string()]);
        let frame = push_frame(&Subscription::WebData3 { user }, &state, 2_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["channel"], "webData3");
        let data = value["data"].as_object().unwrap();
        assert_eq!(data.len(), 2, "{data:?}");
        let user_state = data["userState"].as_object().unwrap();
        let mut keys: Vec<&str> = user_state.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "abstraction",
                "agentAddress",
                "agentValidUntil",
                "cumLedger",
                "isVault",
                "serverTime",
                "user"
            ]
        );
        assert_eq!(user_state["user"], serde_json::json!(user));
        assert_eq!(user_state["isVault"], false);
        assert_eq!(user_state["agentAddress"], serde_json::Value::Null);
        assert_eq!(user_state["agentValidUntil"], serde_json::Value::Null);
        let dexs = data["perpDexStates"].as_array().unwrap();
        assert_eq!(dexs.len(), 3, "one per dex: default, xyz, flx");
        assert_eq!(
            dexs[0]["perpsAtOpenInterestCap"],
            serde_json::json!(["BTC"])
        );
        for dex in dexs {
            assert_eq!(dex["totalVaultEquity"], "0.0");
        }
        assert_eq!(dexs[1].as_object().unwrap().len(), 1, "no cap, no key");
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
        // The relay's permanent contexts come from the registry: the client
        // must know the coin's market row to stream its context.
        client_feed.put_meta(crate::sim::meta_of(&[("BTC", 40)]), 1);
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
    async fn the_session_pushes_l2_book_and_web_data3() {
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
                    r#"{{"method":"subscribe","subscription":{{"type":"webData3","user":"{user}"}}}}"#
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
                "webData3" => {
                    assert_eq!(value["data"]["userState"]["user"], serde_json::json!(user));
                    web_seen = true;
                }
                _ => {}
            }
        }
        assert!(book_seen, "the session must push l2Book");
        assert!(web_seen, "the session must push webData3");
        assert_eq!(acks, 2, "both subscriptions are acked");
        socket.close(None).await.ok();
        server.abort();
    }

    /// A venue with the default dex (`BTC`) and two builder dexes registered
    /// in the order xyz, flx (so the registry order is not the alphabetical
    /// one): `xyz` margins in USDC and lists `xyz:TSLA` and a coin with no
    /// context (`xyz:GONE`), `flx` is a USDH dex. One account holds a position
    /// and a resting order on xyz and a resting order on the default dex.
    #[allow(clippy::too_many_lines)] // one fixture world
    fn dex_world() -> (Arc<ApiState>, Address) {
        let feed = Arc::new(MarketFeed::memory(vec![]));
        let now = crate::now_ms();
        feed.put_meta(crate::sim::meta_with("BTC", 10), now);
        feed.put_dex_meta(
            "xyz".into(),
            crate::sim::meta_of(&[("xyz:TSLA", 10), ("xyz:GONE", 10)]),
            now,
        );
        let mut flx = crate::sim::meta_of(&[("flx:TSLA", 10)]);
        flx.collateral_token = 360;
        feed.put_dex_meta("flx".into(), flx, now);
        for (coin, px) in [("BTC", "100"), ("xyz:TSLA", "200"), ("flx:TSLA", "300")] {
            feed.put_ctx(coin.into(), test_ctx(px, px), now);
        }
        feed.put_book(test_book("BTC", "99"), now);
        feed.put_book(test_book("xyz:TSLA", "199"), now);
        feed.put_oi_caps(Vec::new());
        feed.put_oi_caps_of("xyz", Vec::new());
        feed.put_oi_caps_of("flx", Vec::new());
        feed.put_mids(
            [
                ("BTC", "100"),
                ("@107", "5"),
                ("xyz:TSLA", "200"),
                ("flx:TSLA", "300"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.parse().unwrap()))
            .collect(),
            now,
        );
        let state = api_state(feed.clone());
        let user = Address::repeat_byte(0xaa);
        let deal = |dex: Option<&str>| {
            state
                .sim
                .deal(
                    &feed,
                    &Deal {
                        address: user,
                        class: DealClass::Perp,
                        coin: None,
                        dex: dex.map(str::to_string),
                        amount: Decimal::from(1_000),
                    },
                )
                .unwrap();
        };
        deal(None);
        deal(Some("xyz"));
        let order = |coin: &str, px: Option<&str>, tif| crate::sim::order::Order {
            coin: coin.into(),
            is_buy: true,
            size: "0.5".parse().unwrap(),
            limit_px: px.map(|p| p.parse().unwrap()),
            tif,
            reduce_only: false,
            cloid: None,
            trigger: None,
        };
        use crate::sim::order::Tif;
        state
            .sim
            .order(&feed, user, &order("xyz:TSLA", None, Tif::Ioc))
            .unwrap();
        state
            .sim
            .place(&feed, user, &order("BTC", Some("50"), Tif::Gtc))
            .unwrap();
        state
            .sim
            .place(&feed, user, &order("xyz:TSLA", Some("150"), Tif::Gtc))
            .unwrap();
        (state, user)
    }

    /// Subscribe through the request path; `None` = the venue's silence.
    fn subscribe_to(
        state: &ApiState,
        subs: &mut Vec<Live>,
        subscription: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let request = serde_json::json!({"method": "subscribe", "subscription": subscription});
        let reply = request_reply(&request.to_string(), subs, state);
        (!matches!(reply, WsPush::Silent)).then(|| serde_json::to_value(&reply).unwrap())
    }

    fn frames_of(live: &mut Live, state: &ApiState) -> Vec<serde_json::Value> {
        pushes(live, state, crate::now_ms())
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect()
    }

    /// `allDexsClearinghouseState` lists every dex of the registry for any
    /// account: the default dex first, then alphabetically (not in `perpDexs`
    /// order). Every dex carries the account's own ledger, so a builder dex
    /// shows the positions it actually holds.
    #[test]
    fn all_dexs_states_list_every_dex_default_first_then_alphabetically() {
        let (state, user) = dex_world();
        let frame = push_frame(
            &Subscription::AllDexsClearinghouseState { user },
            &state,
            2_000,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["data"]["user"], serde_json::json!(user));
        let states = value["data"]["clearinghouseStates"].as_array().unwrap();
        let names: Vec<&str> = states.iter().map(|s| s[0].as_str().unwrap()).collect();
        assert_eq!(names, ["", "flx", "xyz"]);
        let positions = |i: usize| states[i][1]["assetPositions"].as_array().unwrap().len();
        assert_eq!((positions(0), positions(1), positions(2)), (0, 0, 1));
        assert_eq!(
            states[2][1]["assetPositions"][0]["position"]["coin"],
            "xyz:TSLA"
        );
        // An account the sim never saw is served the empty state of every dex.
        let stranger = Address::repeat_byte(0xfe);
        let frame = push_frame(
            &Subscription::AllDexsClearinghouseState { user: stranger },
            &state,
            2_000,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(
            value["data"]["clearinghouseStates"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    /// `clearinghouseState` and `openOrders` take a `dex`: the ack echoes it,
    /// the frames carry it, and a dex the venue does not list (or `ALL_DEXS`
    /// on `clearinghouseState`) gets no frame at all, not even an ack.
    #[test]
    #[allow(clippy::too_many_lines)] // one scenario over the per-dex channels
    fn per_dex_channels_follow_the_dex_and_stay_silent_for_unlisted_ones() {
        let (state, user) = dex_world();
        let mut subs: Vec<Live> = Vec::new();

        let ack = subscribe_to(
            &state,
            &mut subs,
            &serde_json::json!({"type": "clearinghouseState", "user": user, "dex": "xyz"}),
        )
        .unwrap();
        assert_eq!(
            ack["data"]["subscription"],
            serde_json::json!({"type": "clearinghouseState", "dex": "xyz", "user": user})
        );
        let frames = frames_of(subs.last_mut().unwrap(), &state);
        assert_eq!(frames[0]["data"]["dex"], "xyz");
        assert_eq!(
            frames[0]["data"]["clearinghouseState"]["assetPositions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // The default dex's subscription is acked with the empty dex string.
        let ack = subscribe_to(
            &state,
            &mut subs,
            &serde_json::json!({"type": "clearinghouseState", "user": user}),
        )
        .unwrap();
        assert_eq!(
            ack["data"]["subscription"]["dex"], "",
            "the dex key is echoed"
        );
        let frames = frames_of(subs.last_mut().unwrap(), &state);
        assert_eq!(frames[0]["data"]["dex"], "");
        assert!(frames[0]["data"]["clearinghouseState"]["assetPositions"]
            .as_array()
            .unwrap()
            .is_empty());

        let before = subs.len();
        for dex in ["nope", "ALL_DEXS"] {
            assert!(
                subscribe_to(
                    &state,
                    &mut subs,
                    &serde_json::json!({"type": "clearinghouseState", "user": user, "dex": dex}),
                )
                .is_none(),
                "clearinghouseState for `{dex}` is silent"
            );
        }
        assert!(subscribe_to(
            &state,
            &mut subs,
            &serde_json::json!({"type": "openOrders", "user": user, "dex": "nope"}),
        )
        .is_none());
        assert_eq!(subs.len(), before, "a silent refusal queues nothing");

        // openOrders: one dex, the default dex, or all of them (newest first).
        let orders = |dex: &str, subs: &mut Vec<Live>| {
            let ack = subscribe_to(
                &state,
                subs,
                &serde_json::json!({"type": "openOrders", "user": user, "dex": dex}),
            )
            .unwrap();
            assert_eq!(ack["data"]["subscription"]["dex"], dex);
            let frames = frames_of(subs.last_mut().unwrap(), &state);
            assert_eq!(frames[0]["data"]["dex"], dex);
            frames[0]["data"]["orders"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["coin"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(orders("xyz", &mut subs), ["xyz:TSLA"]);
        assert_eq!(orders("", &mut subs), ["BTC"]);
        assert_eq!(orders("ALL_DEXS", &mut subs), ["xyz:TSLA", "BTC"]);
    }

    /// The venue's `allMids` quirks: default and `ALL_DEXS` push a snapshot, a
    /// named dex (and an unlisted one) is acked and then silent, an empty dex
    /// is acked as the plain channel (and is the same subscription).
    #[test]
    #[allow(clippy::too_many_lines)] // one scenario over the allMids quirks
    fn all_mids_per_dex_mirrors_the_venues_quirks() {
        let (state, _) = dex_world();
        let mut subs: Vec<Live> = Vec::new();

        let ack = subscribe_to(&state, &mut subs, &serde_json::json!({"type": "allMids"})).unwrap();
        assert_eq!(
            ack["data"]["subscription"],
            serde_json::json!({"type": "allMids"})
        );
        let frames = frames_of(subs.last_mut().unwrap(), &state);
        let mids = frames[0]["data"]["mids"].as_object().unwrap();
        assert!(mids.contains_key("BTC") && mids.contains_key("@107"));
        assert!(!mids.contains_key("xyz:TSLA"), "no builder keys by default");

        let ack = subscribe_to(
            &state,
            &mut subs,
            &serde_json::json!({"type": "allMids", "dex": "ALL_DEXS"}),
        )
        .unwrap();
        assert_eq!(ack["data"]["subscription"]["dex"], "ALL_DEXS");
        let frames = frames_of(subs.last_mut().unwrap(), &state);
        let mids = frames[0]["data"]["mids"].as_object().unwrap();
        assert!(mids.contains_key("BTC") && mids.contains_key("xyz:TSLA"));
        assert!(mids.contains_key("flx:TSLA") && mids.contains_key("@107"));

        for dex in ["xyz", "nope"] {
            let ack = subscribe_to(
                &state,
                &mut subs,
                &serde_json::json!({"type": "allMids", "dex": dex}),
            )
            .unwrap();
            assert_eq!(ack["channel"], "subscriptionResponse");
            assert_eq!(ack["data"]["subscription"]["dex"], dex);
            assert!(
                frames_of(subs.last_mut().unwrap(), &state).is_empty(),
                "allMids for `{dex}` pushes no data"
            );
        }

        // An empty dex: acked as the plain channel, which it duplicates.
        let mut subs: Vec<Live> = Vec::new();
        let ack = subscribe_to(
            &state,
            &mut subs,
            &serde_json::json!({"type": "allMids", "dex": ""}),
        )
        .unwrap();
        assert_eq!(
            ack["data"]["subscription"],
            serde_json::json!({"type": "allMids"})
        );
        assert!(frames_of(subs.last_mut().unwrap(), &state).is_empty());
        let again =
            subscribe_to(&state, &mut subs, &serde_json::json!({"type": "allMids"})).unwrap();
        assert_eq!(again["data"], r#"Already subscribed: {"type":"allMids"}"#);
        // A `null` dex is a request the venue cannot parse.
        let bad = r#"{"method":"subscribe","subscription":{"type":"allMids","dex":null}}"#;
        let reply = serde_json::to_value(request_reply(bad, &mut subs, &state)).unwrap();
        assert_eq!(
            reply["data"],
            format!("Error parsing JSON into valid websocket request: {bad}")
        );
    }

    /// `pac` carries every dex in the venue's order, one slot per universe
    /// coin (`{}` for a coin with no context), the same dex list in every
    /// frame; `fastAssetCtxs` is one flat map over every dex's coins.
    #[test]
    #[allow(clippy::too_many_lines)] // one scenario over pac and fastAssetCtxs
    fn pac_and_fast_asset_ctxs_cover_every_dex() {
        let (state, _) = dex_world();
        let mut subs: Vec<Live> = Vec::new();
        subscribe_to(&state, &mut subs, &serde_json::json!({"type": "pac"})).unwrap();
        let live = subs.last_mut().unwrap();
        let frames = frames_of(live, &state);
        let decode = |frame: &serde_json::Value| -> serde_json::Value {
            serde_json::from_str(&ws_codec::inflate_b64(frame["data"].as_str().unwrap())).unwrap()
        };
        let first = decode(&frames[0]);
        let dexs = first.as_array().unwrap();
        let names: Vec<&str> = dexs.iter().map(|d| d[0].as_str().unwrap()).collect();
        assert_eq!(names, ["", "flx", "xyz"]);
        assert_eq!(
            dexs[2][1].as_array().unwrap().len(),
            2,
            "xyz lists two coins"
        );
        assert_eq!(dexs[2][1][0]["markPx"], "200.0");
        assert_eq!(
            dexs[2][1][1],
            serde_json::json!({}),
            "xyz:GONE has no context"
        );
        assert!(
            frames_of(live, &state).is_empty(),
            "nothing changed, no frame"
        );
        state
            .feed
            .put_ctx("xyz:TSLA".into(), test_ctx("201", "201"), crate::now_ms());
        let frames = frames_of(live, &state);
        let next = decode(&frames[0]);
        assert_eq!(next.as_array().unwrap().len(), 3, "every dex is listed");
        assert_eq!(next[0][1], serde_json::json!([{}]));
        assert_eq!(next[2][1][0]["markPx"], "201.0");

        subscribe_to(
            &state,
            &mut subs,
            &serde_json::json!({"type": "fastAssetCtxs"}),
        )
        .unwrap();
        let frames = frames_of(subs.last_mut().unwrap(), &state);
        let fast = decode(&frames[0]);
        let mut keys: Vec<&str> = fast
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["BTC", "flx:TSLA", "xyz:TSLA"],
            "no coin without a mark"
        );
        assert_eq!(fast["xyz:TSLA"]["markPx"], "201.0");
        assert_eq!(fast["flx:TSLA"]["midPx"], "300.0");
    }

    /// `activeAssetCtx` of a builder coin is that coin's own context, found by
    /// name: two coins with the same mark and oracle (the old lookup matched
    /// on those) keep their own open interest.
    #[test]
    fn active_asset_ctx_is_looked_up_by_coin() {
        let (state, _) = dex_world();
        let mut tsla = test_ctx("200", "200");
        tsla.open_interest = Decimal::from(7);
        let mut twin = test_ctx("200", "200");
        twin.open_interest = Decimal::from(9);
        let now = crate::now_ms();
        state.feed.put_ctx("BTC".into(), twin, now);
        state.feed.put_ctx("xyz:TSLA".into(), tsla, now);
        let frame = push_frame(
            &Subscription::ActiveAssetCtx {
                coin: "xyz:TSLA".into(),
            },
            &state,
            now,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["data"]["coin"], "xyz:TSLA");
        assert_eq!(value["data"]["ctx"]["openInterest"], "7.0");
    }

    /// The candle channel is wired to the relay hub: without a venue endpoint
    /// there is nothing to listen to (the venue's error, not silence), and an
    /// interval outside the venue's set is the venue's parse error.
    #[test]
    fn the_candle_channel_subscribes_the_relay_hub() {
        let state = api_state(Arc::new(MarketFeed::memory(vec!["BTC".into()])));
        let mut subs: Vec<Live> = Vec::new();
        let reply = request_reply(
            r#"{"method":"subscribe","subscription":{"type":"candle","coin":"BTC","interval":"1m"}}"#,
            &mut subs,
            &state,
        );
        assert!(
            matches!(&reply, WsPush::Error { data } if data == "the venue's candle stream is unavailable"),
            "{reply:?}"
        );
        let bad = r#"{"method":"subscribe","subscription":{"type":"candle","coin":"BTC","interval":"2m"}}"#;
        let reply = request_reply(bad, &mut subs, &state);
        assert!(
            matches!(&reply, WsPush::Error { data } if *data == format!("Error parsing JSON into valid websocket request: {bad}")),
            "{reply:?}"
        );
        assert!(subs.is_empty());
    }
}
