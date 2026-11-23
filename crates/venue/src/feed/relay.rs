//! The upstream subscription relay: which `l2` book and `activeAssetCtx`
//! streams the single upstream socket carries.
//!
//! The venue sim mirrors the real venue, so every coin of every perp dex is a
//! first-class market. Two policies decide what the socket carries:
//!
//! - **Contexts (mark/oracle/funding/OI/mid) are permanent**: every live
//!   (non-delisted) coin of the dex registry has an `activeAssetCtx` stream.
//!   Measured on the real venue: about one push per coin per second, ~280 B
//!   each, so the whole set is a few hundred messages per second and ~130
//!   KiB/s: modest, and it makes every coin's mark, funding and OI live
//!   without the REST sweep.
//! - **Books are demand-driven**: the `l2` feed is subscribed for a coin only while
//!   someone needs it. A *lease* (a downstream websocket subscription holds
//!   one until it unsubscribes or disconnects) keeps a coin wanted; a *touch*
//!   (an order placed, a held position or resting order, an `/info` `l2Book`
//!   read) keeps it wanted for the grace window. After the last user left plus
//!   [`GRACE_MS`] the stream is unsubscribed.
//!
//! The venue allows 1000 subscriptions and 2000 sent messages per minute per
//! IP (docs); both are budgeted well below the limit here ([`MAX_STREAMS`],
//! [`SendBudget`]). The relay is pure bookkeeping: it computes the desired
//! stream set and the diff against what a socket carries, and the websocket
//! lane (`feed::ws`) performs the sends. A socket starts with an empty live
//! set, so a reconnect resubscribes the whole desired set with no extra code.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

/// How long a coin stays wanted after its last user left.
pub(crate) const GRACE_MS: u64 = 30_000;

/// The ceiling on the socket's streams (the venue allows 1000 per IP; the
/// allMids stream and other sockets of the same IP share that limit).
pub(crate) const MAX_STREAMS: usize = 850;

/// The most book streams at once (a lease or touch beyond it waits: REST
/// serves its book).
pub(crate) const MAX_BOOKS: usize = 300;

/// A refused coin is not retried for this long (a stream the venue refused
/// would otherwise be re-sent every plan).
#[allow(dead_code, reason = "the ws sync loop uses the budgets (next port step)")]
pub(crate) const REFUSAL_COOLDOWN_MS: u64 = 5 * 60_000;

/// The sent-message budget window and its ceiling (the venue allows 2000
/// messages per minute per IP across every socket; pings and the allMids
/// subscribe are counted in too).
#[allow(dead_code, reason = "the ws sync loop uses the budgets (next port step)")]
const SEND_WINDOW_MS: u64 = 60_000;
#[allow(dead_code, reason = "the ws sync loop uses the budgets (next port step)")]
const SEND_BUDGET: usize = 1_200;

/// One upstream stream. The coin is the venue's name (`BTC`, `xyz:TSLA`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Stream {
    Book(String),
    Ctx(String),
    /// The public trade tape: new trades only, as the venue serves them. Wanted
    /// exactly while a downstream client subscribes to the coin's `trades`, so
    /// the upstream cost follows the demand (the lease's coin is the same one).
    Trades(String),
}

impl Stream {
#[allow(dead_code, reason = "the ws sync loop diffs with these (next port step)")]
    pub(crate) fn coin(&self) -> &str {
        match self {
            Self::Book(coin) | Self::Ctx(coin) | Self::Trades(coin) => coin,
        }
    }
}

/// One change of the socket's live set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code, reason = "the ws sync loop diffs with these (next port step)")]
pub(crate) enum Op {
    Subscribe(Stream),
    Unsubscribe(Stream),
}

#[derive(Debug, Default)]
struct Interest {
    /// Live leases (downstream subscriptions) on the coin.
    leases: usize,
    /// The last lease release or touch (ms).
    touched_at: u64,
}

#[derive(Debug, Default)]
struct State {
    interest: BTreeMap<String, Interest>,
    refused: BTreeMap<Stream, u64>,
}

/// The coins someone needs, with the subscription manager's rules.
#[derive(Debug, Default)]
pub(crate) struct Relay {
    state: Mutex<State>,
}

/// A held interest in a coin (a downstream websocket subscription). Dropping
/// it releases the interest and starts the grace window.
#[derive(Debug)]
pub(crate) struct Lease {
    relay: Arc<Relay>,
    coin: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.relay.release(&self.coin, crate::now_ms());
    }
}

impl Relay {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Keep `coin` wanted for the grace window from `now_ms`.
    pub(crate) fn touch(&self, coin: &str, now_ms: u64) {
        let mut state = self.state();
        let interest = state.interest.entry(coin.to_string()).or_default();
        interest.touched_at = interest.touched_at.max(now_ms);
    }

    /// Hold `coin` wanted until the lease is dropped.
    pub(crate) fn lease(self: &Arc<Self>, coin: &str, now_ms: u64) -> Lease {
        let mut state = self.state();
        let interest = state.interest.entry(coin.to_string()).or_default();
        interest.leases += 1;
        interest.touched_at = interest.touched_at.max(now_ms);
        drop(state);
        Lease {
            relay: Arc::clone(self),
            coin: coin.to_string(),
        }
    }

    fn release(&self, coin: &str, now_ms: u64) {
        let mut state = self.state();
        if let Some(interest) = state.interest.get_mut(coin) {
            interest.leases = interest.leases.saturating_sub(1);
            interest.touched_at = interest.touched_at.max(now_ms);
        }
    }

    /// Whether `coin` is wanted at `now_ms` (a lease, or a touch inside the
    /// grace window).
    #[cfg(test)]
    pub(crate) fn is_wanted(&self, coin: &str, now_ms: u64) -> bool {
        self.state()
            .interest
            .get(coin)
            .is_some_and(|interest| Self::wanted_at(interest, now_ms))
    }

    fn wanted_at(interest: &Interest, now_ms: u64) -> bool {
        interest.leases > 0 || now_ms.saturating_sub(interest.touched_at) <= GRACE_MS
    }

    /// The wanted coins at `now_ms`, most important first (a leased coin
    /// before a touched one, then the most recently touched), plus the coins
    /// whose grace just ran out (dropped from the table: their cached book
    /// goes with them).
    pub(crate) fn sweep(&self, now_ms: u64) -> (Vec<String>, Vec<String>) {
        let mut state = self.state();
        let mut expired = Vec::new();
        state.interest.retain(|coin, interest| {
            let keep = Self::wanted_at(interest, now_ms);
            if !keep {
                expired.push(coin.clone());
            }
            keep
        });
        let mut wanted: Vec<(&String, &Interest)> = state.interest.iter().collect();
        wanted.sort_by(|(ca, a), (cb, b)| {
            (b.leases > 0)
                .cmp(&(a.leases > 0))
                .then(b.touched_at.cmp(&a.touched_at))
                .then(ca.cmp(cb))
        });
        let wanted = wanted.into_iter().map(|(coin, _)| coin.clone()).collect();
        (wanted, expired)
    }

    /// The venue refused a stream: keep it out of the desired set for the
    /// cooldown.
    #[allow(dead_code, reason = "the ws sync loop spends the budget and records refusals (next port step)")]
    pub(crate) fn refuse(&self, stream: Stream, now_ms: u64) {
        self.state()
            .refused
            .insert(stream, now_ms.saturating_add(REFUSAL_COOLDOWN_MS));
    }

    /// The streams the socket should carry: a context for every permanent
    /// coin and for every wanted coin, a book for the wanted coins in
    /// priority order, all inside [`MAX_STREAMS`] and [`MAX_BOOKS`]. Contexts
    /// win the cap (a book waits for REST before a mark does).
    pub(crate) fn desired(
        &self,
        permanent_ctx: &[String],
        wanted: &[String],
        now_ms: u64,
    ) -> BTreeSet<Stream> {
        let mut state = self.state();
        state.refused.retain(|_, until| *until > now_ms);
        let refused = &state.refused;
        let mut out: BTreeSet<Stream> = BTreeSet::new();
        for coin in permanent_ctx.iter().chain(wanted) {
            let stream = Stream::Ctx(coin.clone());
            if out.len() < MAX_STREAMS && !refused.contains_key(&stream) {
                out.insert(stream);
            }
        }
        let mut books = 0usize;
        for coin in wanted {
            let stream = Stream::Book(coin.clone());
            if books >= MAX_BOOKS || out.len() >= MAX_STREAMS {
                break;
            }
            if !refused.contains_key(&stream) {
                out.insert(stream);
                books += 1;
            }
        }
        // The tape follows the same demand and the same cap: a coin someone
        // watches is a coin whose trades a strategy may read.
        let mut tapes = 0usize;
        for coin in wanted {
            let stream = Stream::Trades(coin.clone());
            if tapes >= MAX_BOOKS || out.len() >= MAX_STREAMS {
                break;
            }
            if !refused.contains_key(&stream) {
                out.insert(stream);
                tapes += 1;
            }
        }
        out
    }
}

/// The diff from a socket's live set to the desired one: unsubscribes first
/// (they free slots), then subscribes (books before contexts only by the
/// stream order; the set order is stable).
#[allow(dead_code, reason = "the ws sync loop diffs with these (next port step)")]
pub(crate) fn plan(desired: &BTreeSet<Stream>, live: &BTreeSet<Stream>) -> Vec<Op> {
    let mut ops: Vec<Op> = live
        .difference(desired)
        .cloned()
        .map(Op::Unsubscribe)
        .collect();
    ops.extend(desired.difference(live).cloned().map(Op::Subscribe));
    ops
}

/// The sent-message meter: a rolling window of sends, so a reconnect storm
/// cannot exceed the venue's per-minute message limit.
#[derive(Debug, Default)]
#[allow(dead_code, reason = "the ws sync loop diffs with these (next port step)")]
pub(crate) struct SendBudget {
    sent: VecDeque<u64>,
}

impl SendBudget {
    /// How many of `want` messages may be sent at `now_ms` (and records them).
    #[allow(dead_code, reason = "the ws sync loop spends the budget and records refusals (next port step)")]
    pub(crate) fn take(&mut self, now_ms: u64, want: usize) -> usize {
        while self
            .sent
            .front()
            .is_some_and(|at| now_ms.saturating_sub(*at) >= SEND_WINDOW_MS)
        {
            self.sent.pop_front();
        }
        let allowed = want.min(SEND_BUDGET.saturating_sub(self.sent.len()));
        self.sent.extend(std::iter::repeat_n(now_ms, allowed));
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coins(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn a_touch_keeps_a_coin_wanted_for_the_grace_window_only() {
        let relay = Relay::default();
        relay.touch("xyz:TSLA", 1_000);
        assert!(relay.is_wanted("xyz:TSLA", 1_000 + GRACE_MS));
        assert!(!relay.is_wanted("xyz:TSLA", 1_001 + GRACE_MS));
        let (wanted, expired) = relay.sweep(1_001 + GRACE_MS);
        assert!(wanted.is_empty());
        assert_eq!(expired, ["xyz:TSLA"], "the cached book goes with it");
        assert!(relay.sweep(2_000 + GRACE_MS).1.is_empty(), "reaped once");
    }

    /// Two downstream users of one coin refcount: the coin stays wanted until
    /// the last lease is dropped, then for the grace window.
    #[test]
    fn leases_refcount_and_the_grace_starts_after_the_last_one() {
        let relay = Arc::new(Relay::default());
        let a = relay.lease("BTC", 1_000);
        let b = relay.lease("BTC", 2_000);
        let far = 1_000 + 10 * GRACE_MS;
        assert!(relay.is_wanted("BTC", far), "leased: never idles out");
        drop(a);
        assert!(relay.is_wanted("BTC", far), "one lease left");
        drop(b);
        // The drop stamped the real clock (`now_ms`); the grace is measured
        // from that release, not from the lease's start.
        let released = crate::now_ms();
        assert!(relay.is_wanted("BTC", released + GRACE_MS - 1_000));
        assert!(!relay.is_wanted("BTC", released + GRACE_MS + 1_000));
    }

    #[test]
    fn leased_coins_rank_before_touched_ones_then_by_recency() {
        let relay = Arc::new(Relay::default());
        relay.touch("OLD", 1_000);
        relay.touch("NEW", 5_000);
        let _held = relay.lease("HELD", 100);
        let (wanted, _) = relay.sweep(6_000);
        assert_eq!(wanted, ["HELD", "NEW", "OLD"]);
    }

    #[test]
    fn desired_is_ctx_for_everyone_and_books_for_the_wanted_inside_the_caps() {
        let relay = Relay::default();
        let desired = relay.desired(&coins(&["BTC", "ETH"]), &coins(&["xyz:TSLA", "BTC"]), 0);
        let expected: BTreeSet<Stream> = [
            Stream::Ctx("BTC".into()),
            Stream::Ctx("ETH".into()),
            Stream::Ctx("xyz:TSLA".into()),
            Stream::Book("xyz:TSLA".into()),
            Stream::Book("BTC".into()),
            Stream::Trades("xyz:TSLA".into()),
            Stream::Trades("BTC".into()),
        ]
        .into_iter()
        .collect();
        assert_eq!(desired, expected);

        // The book cap: the wanted list is in priority order, the first
        // MAX_BOOKS get a stream.
        let many: Vec<String> = (0..MAX_BOOKS + 20).map(|i| format!("C{i}")).collect();
        let desired = relay.desired(&[], &many, 0);
        let books: Vec<&Stream> = desired
            .iter()
            .filter(|s| matches!(s, Stream::Book(_)))
            .collect();
        assert_eq!(books.len(), MAX_BOOKS);
        assert!(desired.contains(&Stream::Book("C0".into())));
        assert!(!desired.contains(&Stream::Book(format!("C{}", MAX_BOOKS + 1))));
        // The tape follows the same priority order inside the overall ceiling
        // (every wanted coin also carries its context, so the tapes take what
        // the stream cap leaves).
        assert!(desired.len() <= MAX_STREAMS);
        assert!(desired.contains(&Stream::Trades("C0".into())));
        assert!(!desired.contains(&Stream::Trades(format!("C{}", MAX_BOOKS + 1))));
    }

    /// The subscription cap: contexts keep their slots, books take what is
    /// left, and nothing exceeds the ceiling the venue's 1000 limit leaves.
    #[test]
    fn the_total_never_exceeds_the_subscription_cap() {
        let relay = Relay::default();
        let permanent: Vec<String> = (0..MAX_STREAMS - 10).map(|i| format!("P{i}")).collect();
        let wanted: Vec<String> = (0..50).map(|i| format!("W{i}")).collect();
        let desired = relay.desired(&permanent, &wanted, 0);
        assert_eq!(desired.len(), MAX_STREAMS);
        let ctxs = desired
            .iter()
            .filter(|s| matches!(s, Stream::Ctx(_)))
            .count();
        assert!(ctxs > permanent.len(), "wanted coins keep their ctx too");
        const { assert!(MAX_STREAMS < 1000) }; // well below the venue's limit
    }

    #[test]
    fn a_refused_stream_is_left_out_until_its_cooldown_ends() {
        let relay = Relay::default();
        relay.refuse(Stream::Book("BAD".into()), 1_000);
        let wanted = coins(&["BAD"]);
        assert!(!relay
            .desired(&[], &wanted, 2_000)
            .contains(&Stream::Book("BAD".into())));
        assert!(relay
            .desired(&[], &wanted, 1_000 + REFUSAL_COOLDOWN_MS + 1)
            .contains(&Stream::Book("BAD".into())));
    }

    /// The diff: unsubscribes first, then subscribes; a fresh socket (empty
    /// live set) is a full resubscribe, which is the reconnect path.
    #[test]
    fn the_plan_diffs_desired_against_live_and_a_reconnect_resubscribes_all() {
        let desired: BTreeSet<Stream> = [Stream::Ctx("A".into()), Stream::Book("B".into())]
            .into_iter()
            .collect();
        let live: BTreeSet<Stream> = [Stream::Ctx("A".into()), Stream::Book("GONE".into())]
            .into_iter()
            .collect();
        assert_eq!(
            plan(&desired, &live),
            [
                Op::Unsubscribe(Stream::Book("GONE".into())),
                Op::Subscribe(Stream::Book("B".into())),
            ]
        );
        assert!(plan(&desired, &desired).is_empty());
        let reconnected = plan(&desired, &BTreeSet::new());
        assert_eq!(reconnected.len(), 2);
        assert!(reconnected.iter().all(|op| matches!(op, Op::Subscribe(_))));
    }

    #[test]
    fn the_send_budget_caps_a_minute_and_recovers_after_it() {
        let mut budget = SendBudget::default();
        assert_eq!(budget.take(0, 1_000), 1_000);
        assert_eq!(budget.take(10, 1_000), SEND_BUDGET - 1_000, "only the rest");
        assert_eq!(budget.take(20, 5), 0, "spent");
        assert_eq!(budget.take(SEND_WINDOW_MS + 15, 50), 50, "the window slid");
    }
}
