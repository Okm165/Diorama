//! The feed bridge (plan §7.1, Phase 0): the parent chain's live feed answers
//! → `updateAnswer` on the devnet's mock aggregators.
//!
//! Why it exists: daloc's harness periodically re-publishes its own mock
//! answers; in mirror mode **the bridge is the single price writer** for the
//! mock aggregators (their refresher is off, §17.2). The value's source is the
//! *parent chain's* aggregator for the same feed (the mirror is a mainnet
//! mirror: live prices come from mainnet's own Chainlink contracts, read at
//! the follow head via reth).
//!
//! Scenario discipline: a scenario's crash writes the mock aggregator
//! directly. The bridge detects that (the target no longer holds what the
//! bridge last wrote) and **pauses the feed** instead of overwriting the
//! scenario's value; `resume` hands the feed back. That is the §17.2 rule:
//! "the bridge pauses the crashed feed (divergence detection) and hand-back
//! restores".
//!
//! Stamping is chain-native by construction: `updateAnswer` records the block
//! timestamp of the T block that mines the bridge's tx — feeds are stamped in
//! the devnet chain's own domain, never in wall time.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use alloy_primitives::{Address, I256};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{sol, SolCall};
use mirror_config::FeedSection;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

sol! {
    /// The Chainlink aggregator surface the bridge needs (their mocks speak
    /// the same interface as the real feeds, §7.1).
    interface AggregatorV3Interface {
        function latestRoundData() external view returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound);
        function decimals() external view returns (uint8);
        function updateAnswer(int256 _answer) external;
    }
}

/// One feed's value state: what the chain confirmed the bridge wrote, and
/// what a publish in flight should land as. Both are needed: between the
/// `updateAnswer` tx and its mining, the target still holds the *previous*
/// value — comparing only against "last written" would read that as a
/// scenario's write and pause the bridge on itself.
///
/// `pending_since_ms` is the publish's send time: an anvil pool is not
/// durable (and a same-nonce sender can displace ours — measured: daloc's
/// deploy sharing the writer account wedged a feed in `pending` forever), so
/// a publish that does not land within the confirmation window is re-sent.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct ValueState {
    confirmed: Option<i128>,
    pending: Option<i128>,
    pending_since_ms: Option<u64>,
}

/// One feed's runtime state (the section is the config; this is the truth the
/// pass compares against).
struct FeedRuntime {
    section: FeedSection,
    values: Mutex<ValueState>,
    /// The scenario owns this feed's value until resumed.
    paused: AtomicBool,
    /// The last pass fault, when one happened (the verifier caught a wiped
    /// target error-looping invisibly: /admin/feed showed a stale confirmed
    /// value and the counter never moved).
    last_error: Mutex<Option<String>>,
    /// The source's decimals (read at boot; the target must match).
    source_decimals: u8,
    /// The target's decimals, resolved once the target is readable. `None`
    /// while the target is not installed yet — a fact the bridge *waits* on
    /// (the devnet's fixture deploy installs the mock aggregators, so refusing
    /// to boot would couple the mirror's start to that deploy's order).
    decimals: Mutex<Option<u8>>,
}

impl FeedRuntime {
    /// The target's decimals, resolved the first time the target is readable
    /// and cached. `None` = not installed yet (the decode of an empty return
    /// is the *fact*; a transport fault still surfaces).
    async fn decimals(&self, anvil_t: &mirror_anvil::Rpc) -> Result<Option<u8>> {
        if let Some(d) = *self
            .decimals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Ok(Some(d));
        }
        let decoded = match anvil_t
            .call_decode(self.section.target, &AggregatorV3Interface::decimalsCall {})
            .await
        {
            Ok(decoded) => decoded,
            // An empty return does not decode: the *fact* is "no code yet".
            // A transport fault still surfaces.
            Err(mirror_anvil::Error::Abi { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if decoded != self.source_decimals {
            return Err(Error::FeedBridgeDecimals {
                feed: self.section.name.clone(),
                have: self.source_decimals,
                want: decoded,
            });
        }
        *self
            .decimals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(decoded);
        tracing::info!(
            feed = %self.section.name,
            decimals = decoded,
            "feed target appeared; the bridge starts publishing"
        );
        Ok(Some(decoded))
    }
}

/// The bridge's metrics.
#[derive(Default)]
pub(crate) struct FeedBridgeMetrics {
    updates: Mutex<Vec<(String, u64)>>,
    pauses: Mutex<Vec<(String, u64)>>,
    /// Pass faults, whatever the stage: a source read, a target read, a
    /// publish, a decimals mismatch. The feed's `last_error` names the cause
    /// (measured: an uninstalled target is *waiting*, not a fault - the
    /// counter only moves when a pass actually fails).
    pass_errors: std::sync::atomic::AtomicU64,
}

impl FeedBridgeMetrics {
    pub fn inc_pass_errors(&self) {
        self.pass_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn count(field: &Mutex<Vec<(String, u64)>>, name: &str) {
        let mut v = field.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match v.iter_mut().find(|(n, _)| n == name) {
            Some((_, c)) => *c += 1,
            None => v.push((name.to_string(), 1)),
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512);
        let series = |out: &mut String, name: &str, help: &str, kind: &str, values: &Mutex<Vec<(String, u64)>>| {
            let values = values.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for (feed, v) in values.iter() {
                out.push_str(&format!(
                    "# HELP {name} {help}\n# TYPE {name} {kind}\n{name}{{feed=\"{feed}\"}} {v}\n"
                ));
            }
        };
        series(&mut out, "feed_bridge_updates_total", "answers published to mock aggregators", "counter", &self.updates);
        series(&mut out, "feed_bridge_pauses_total", "feeds paused on a scenario's write", "counter", &self.pauses);
        mirror_devnet::metrics::push_metric(
            &mut out,
            "counter",
            "feed_bridge_pass_errors_total",
            "feed bridge passes that failed (the feed's last_error names the cause)",
            self.pass_errors.load(Ordering::Relaxed),
        );
        out
    }
}

// TRUST: the mock aggregators are the devnet's own price surface. The guards:
// both sides' decimals must match at boot (an unscaled answer would poison
// every consumer), and a value the bridge did not write pauses the feed — a
// scenario's crash is never overwritten.

pub(crate) struct FeedBridge {
    feeds: Vec<FeedRuntime>,
    /// The account that submits `updateAnswer` on T (auto-impersonate makes
    /// any address usable; the mock's own access control decides who may).
    writer: Address,
    poll: Duration,
    /// How long a publish may stay unconfirmed before it is re-sent (the
    /// outcome-unknown reconciliation; config `confirm_window_ms`).
    confirm_window_ms: u64,
    /// Shared with the Ctx so `/metrics` renders the bridge's counters.
    pub metrics: Arc<FeedBridgeMetrics>,
}

impl FeedBridge {
    /// Build the bridge: read both sides' decimals for every feed up front
    /// (a mismatch or an unreachable feed is a boot fault naming it — a
    /// silently mis-scaled price would poison every scenario).
    pub async fn new(ctx: &Ctx) -> Result<Self> {
        let section = ctx
            .engine
            .feed_bridge
            .as_ref()
            .ok_or(Error::FeedBridgeUnconfigured)?;
        let mut feeds = Vec::with_capacity(section.feeds.len());
        for feed in &section.feeds {
            feeds.push(Self::feed_runtime(ctx, feed).await?);
        }
        Ok(Self {
            feeds,
            writer: section.writer,
            poll: Duration::from_millis(section.poll_interval_ms),
            confirm_window_ms: section.confirm_window_ms,
            metrics: ctx.feed_bridge_metrics.clone(),
        })
    }

    /// One feed's runtime: read both sides' decimals up front — a mismatch or
    /// an unreachable *source* is a boot fault naming it (a silently
    /// mis-scaled price would poison every scenario).
    async fn feed_runtime(ctx: &Ctx, feed: &FeedSection) -> Result<FeedRuntime> {
        let source = read_decimals(&ctx.reth, feed.source).await.map_err(|e| {
            Error::FeedBridgeFeed {
                feed: feed.name.clone(),
                what: "source decimals",
                source: Box::new(e),
            }
        })?;
        let decimals = target_decimals(&ctx.anvil_t, feed, source).await?;
        Ok(FeedRuntime {
            section: feed.clone(),
            values: Mutex::new(ValueState::default()),
            paused: AtomicBool::new(false),
            last_error: Mutex::new(None),
            source_decimals: source,
            decimals: Mutex::new(decimals),
        })
    }

    /// Run forever: one pass per `poll_interval_ms`.
    pub async fn run(self: Arc<Self>, ctx: Arc<Ctx>) {
        loop {
            for feed in &self.feeds {
                if let Err(e) = self.pass_one(&ctx, feed).await {
                    self.metrics.inc_pass_errors();
                    *feed.last_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(e.to_string());
                    tracing::warn!(feed = %feed.section.name, error = %e, "feed bridge pass failed; will retry");
                } else {
                    *feed.last_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                }
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// One feed, one pass.
    async fn pass_one(&self, ctx: &Ctx, feed: &FeedRuntime) -> Result<()> {
        if feed.paused.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(decimals) = feed.decimals(&ctx.anvil_t).await? else {
            // The target is not installed yet; wait (the read retries each
            // pass and the feed starts publishing the moment it appears).
            return Ok(());
        };
        let source_now = read_answer(&ctx.reth, feed.section.source).await?;
        let target_now = match read_answer(&ctx.anvil_t, feed.section.target).await {
            Ok(value) => value,
            Err(e @ Error::FeedBridgeAbi { .. }) => {
                forget_target(feed, &e);
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let state = feed_state(feed);
        let now = mirror_config::boot::now_ms();
        match step(state, target_now, source_now, now, self.confirm_window_ms) {
            Step::Confirm => {
                confirm(feed);
                Ok(())
            }
            Step::WaitPending | Step::SkipUnchanged => Ok(()),
            Step::Publish(answer) => {
                publish(&ctx.anvil_t, feed.section.target, self.writer, answer).await?;
                record_publish(feed, &self.metrics, answer, now);
                tracing::info!(feed = %feed.section.name, answer, decimals, "feed bridge published");
                Ok(())
            }
            Step::Pause { found } => {
                pause_feed(feed, &self.metrics, state, found);
                Ok(())
            }
        }
    }

    /// The admin list: each feed's state.
    pub fn list(&self) -> Vec<FeedStatus> {
        self.feeds
            .iter()
            .map(|f| {
                let values = *f.values.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                FeedStatus {
                    name: f.section.name.clone(),
                    target: format!("{:?}", f.section.target).to_lowercase(),
                    source: format!("{:?}", f.section.source).to_lowercase(),
                    paused: f.paused.load(Ordering::Relaxed),
                    confirmed_answer: values.confirmed,
                    pending_answer: values.pending,
                    pending_since_ms: values.pending_since_ms,
                    last_error: f
                        .last_error
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone(),
                    decimals: *f
                        .decimals
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                }
            })
            .collect()
    }

    /// Pause a feed (the scenario takes the value).
    pub fn pause(&self, name: &str) -> Result<()> {
        let feed = self.find(name)?;
        feed.paused.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Hand a feed back to the bridge; the *confirmed* value is forgotten so
    /// the next pass re-establishes the source's value (the hand-back rule),
    /// while a value published by `override` is still awaited as pending.
    pub fn resume(&self, name: &str) -> Result<()> {
        let feed = self.find(name)?;
        feed.paused.store(false, Ordering::Relaxed);
        feed.values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .confirmed = None;
        Ok(())
    }

    /// Own a feed's value now: publish `answer` and pause (a scripted crash).
    /// Refused while the target is not installed: a "crash" written to an
    /// empty address would report success and change nothing.
    pub async fn override_answer(
        &self,
        anvil_t: &mirror_anvil::Rpc,
        name: &str,
        answer: i128,
    ) -> Result<()> {
        let feed = self.find(name)?;
        if feed.decimals(anvil_t).await?.is_none() {
            return Err(Error::FeedBridgeWaiting {
                feed: name.to_string(),
            });
        }
        publish(anvil_t, feed.section.target, self.writer, answer).await?;
        let mut state = feed
            .values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = Some(answer);
        state.pending_since_ms = Some(mirror_config::boot::now_ms());
        drop(state);
        feed.paused.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn find(&self, name: &str) -> Result<&FeedRuntime> {
        self.feeds
            .iter()
            .find(|f| f.section.name == name)
            .ok_or_else(|| Error::FeedBridgeUnknown {
                feed: name.to_string(),
            })
    }
}

/// The feed's value state (a poisoned lock is recovered: the state is a plain
/// value, not a registry).
fn feed_state(feed: &FeedRuntime) -> ValueState {
    *feed
        .values
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The confirmation window elapsed: the pending answer becomes confirmed.
fn confirm(feed: &FeedRuntime) {
    let mut state = feed
        .values
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.confirmed = state.pending;
    state.pending = None;
    state.pending_since_ms = None;
}

/// Record the published answer; a pending publish that did not land within the
/// window is re-published (warned).
fn record_publish(feed: &FeedRuntime, metrics: &FeedBridgeMetrics, answer: i128, now: u64) {
    let mut state = feed
        .values
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.pending.is_some() {
        tracing::warn!(
            feed = %feed.section.name,
            age_ms = now.saturating_sub(state.pending_since_ms.unwrap_or(now)),
            answer,
            "the pending publish did not land within the confirmation window; re-publishing"
        );
    }
    state.pending = Some(answer);
    state.pending_since_ms = Some(now);
    FeedBridgeMetrics::count(&metrics.updates, &feed.section.name);
}

/// Forget the resolved decimals *and* the value state: the target's code is
/// gone (a re-pin wiped the devnet's fixture deploy, or the operator
/// re-pointed the feed). The reappeared target is first contact again — the
/// epoch rule: the devnet's values are gone with the old base. A transport
/// fault stays a fault (the caller only routes ABI faults here).
fn forget_target(feed: &FeedRuntime, error: &Error) {
    *feed
        .decimals
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    *feed
        .values
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = ValueState::default();
    tracing::warn!(
        feed = %feed.section.name,
        target = %feed.section.target,
        error = %error,
        "feed target vanished; waiting for it to reappear"
    );
}

/// A scenario owns this feed's value now; the bridge pauses it (resume hands
/// back).
fn pause_feed(feed: &FeedRuntime, metrics: &FeedBridgeMetrics, state: ValueState, found: i128) {
    feed.paused.store(true, Ordering::Relaxed);
    FeedBridgeMetrics::count(&metrics.pauses, &feed.section.name);
    tracing::warn!(
        feed = %feed.section.name,
        confirmed = ?state.confirmed,
        pending = ?state.pending,
        found,
        "a scenario owns this feed's value now; the bridge paused it (resume to hand back)"
    );
}

/// The pass decision, pure so the pause rule is testable without a chain.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// The target now holds the pending publish: record it as confirmed.
    Confirm,
    /// A publish is in flight (or the value is current): nothing to do.
    WaitPending,
    SkipUnchanged,
    Publish(i128),
    Pause { found: i128 },
}

/// The rules, in order:
/// 1. the target holds the pending publish → confirm it;
/// 2. the target holds the confirmed value (or nothing was ever confirmed —
///    first contact): publish the source's value when it moved, wait while a
///    publish is in flight *within the confirmation window*, re-publish when
///    it did not land in time, skip when nothing moved;
/// 3. anything else on the target → a scenario wrote it: pause, never
///    overwrite a scenario's value.
fn step(state: ValueState, target_now: i128, source_now: i128, now_ms: u64, window_ms: u64) -> Step {
    if state.pending == Some(target_now) {
        return Step::Confirm;
    }
    let ours = state.confirmed.is_none() || state.confirmed == Some(target_now);
    if !ours {
        return Step::Pause { found: target_now };
    }
    if let Some(since) = state.pending_since_ms {
        if now_ms.saturating_sub(since) < window_ms {
            return Step::WaitPending;
        }
        // The pending publish never landed (dropped, displaced by a
        // same-nonce sender, or reverted): re-send the *current* source
        // value. The effect is idempotent — a new round with the same answer
        // is exactly what a retry should leave behind.
        return Step::Publish(source_now);
    }
    if state.confirmed == Some(source_now) {
        return Step::SkipUnchanged;
    }
    Step::Publish(source_now)
}

async fn read_decimals(rpc: &mirror_anvil::Rpc, aggregator: Address) -> Result<u8> {
    Ok(rpc
        .call_decode(aggregator, &AggregatorV3Interface::decimalsCall {})
        .await?)
}

/// The target's decimals, when it exists and matches the source. `None` while
/// the fixture deploy has not run yet — *absence* is a fact the bridge waits
/// on; a present-but-mismatched target or an unexpected fault is a boot error
/// naming itself (a mis-scaled answer poisons consumers).
async fn target_decimals(anvil_t: &mirror_anvil::Rpc, feed: &FeedSection, source: u8) -> Result<Option<u8>> {
    match read_decimals(anvil_t, feed.target).await {
        Ok(d) if d == source => Ok(Some(d)),
        Ok(d) => Err(Error::FeedBridgeDecimals {
            feed: feed.name.clone(),
            have: source,
            want: d,
        }),
        Err(Error::FeedBridgeAbi { .. }) => {
            tracing::warn!(
                feed = %feed.name,
                target = %feed.target,
                "feed target has no code yet; the bridge waits for the devnet's fixture deploy"
            );
            Ok(None)
        }
        Err(e) => Err(Error::FeedBridgeFeed {
            feed: feed.name.clone(),
            what: "target decimals",
            source: Box::new(e),
        }),
    }
}

async fn read_answer(rpc: &mirror_anvil::Rpc, aggregator: Address) -> Result<i128> {
    let decoded = rpc
        .call_decode(aggregator, &AggregatorV3Interface::latestRoundDataCall {})
        .await?;
    decoded
        .answer
        .try_into()
        .map_err(|_| Error::FeedBridgeAbi {
            what: "latestRoundData() answer",
            detail: "does not fit i128".into(),
        })
}

async fn publish(rpc: &mirror_anvil::Rpc, aggregator: Address, writer: Address, answer: i128) -> Result<()> {
    let call = AggregatorV3Interface::updateAnswerCall {
        _answer: I256::try_from(answer).map_err(|_| Error::FeedBridgeAbi {
            what: "updateAnswer(int256)",
            detail: format!("answer {answer} does not fit int256"),
        })?,
    };
    let tx = TransactionRequest::default()
        .from(writer)
        .to(aggregator)
        .input(call.abi_encode().into());
    rpc.send_transaction(tx).await?;
    Ok(())
}

/// The `/admin/feed` list row.
#[derive(Debug, serde::Serialize)]
pub(crate) struct FeedStatus {
    pub name: String,
    pub target: String,
    pub source: String,
    pub paused: bool,
    /// The last value the chain confirmed the bridge wrote.
    pub confirmed_answer: Option<i128>,
    /// A publish awaiting its block, when one is in flight.
    pub pending_answer: Option<i128>,
    /// When that publish was sent (ms since the epoch): a pending older than
    /// `confirm_window_ms` means it is about to be re-sent (the outcome-unknown
    /// reconciliation) — the operator sees the wait, not a silent stall.
    pub pending_since_ms: Option<u64>,
    /// The last pass fault (a wiped target, a source outage), when one is
    /// active: /admin/feed must show a stuck feed, not a stale success.
    pub last_error: Option<String>,
    /// The target's decimals; `None` while the target is not installed yet.
    pub decimals: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(confirmed: Option<i128>, pending: Option<i128>) -> ValueState {
        // A pending state's send time defaults to 0 (fresh unless the test
        // says otherwise by advancing `now`).
        ValueState {
            confirmed,
            pending,
            pending_since_ms: pending.map(|_| 0),
        }
    }

    const WINDOW: u64 = 60_000;

    #[test]
    fn the_pause_rule_never_overwrites_a_scenario() {
        let now = 1_000;
        // The bridge confirmed 2000; the target now holds 1500 (a crash): pause.
        assert_eq!(step(state(Some(2000), None), 1500, 2000, now, WINDOW), Step::Pause { found: 1500 });
        // The target still holds the bridge's value; the source moved: publish.
        assert_eq!(step(state(Some(2000), None), 2000, 2100, now, WINDOW), Step::Publish(2100));
        // Nothing moved: no tx.
        assert_eq!(step(state(Some(2000), None), 2000, 2000, now, WINDOW), Step::SkipUnchanged);
        // First pass (never wrote): publish the source value, no pause.
        assert_eq!(step(ValueState::default(), 999, 2000, now, WINDOW), Step::Publish(2000));
    }

    /// Between the publish and its mining the target holds the *previous*
    /// value: that must read as "in flight", never as a scenario's write.
    #[test]
    fn a_publish_in_flight_is_not_a_scenario_write() {
        let now = 1_000;
        assert_eq!(step(state(Some(2000), Some(2100)), 2000, 2100, now, WINDOW), Step::WaitPending);
        // Mined: confirmed.
        assert_eq!(step(state(Some(2000), Some(2100)), 2100, 2100, now, WINDOW), Step::Confirm);
        // After Confirm the next pass sees the source unchanged: skip.
        assert_eq!(step(state(Some(2100), None), 2100, 2100, now, WINDOW), Step::SkipUnchanged);
        // A crash during the flight is still caught (neither value).
        assert_eq!(step(state(Some(2000), Some(2100)), 1500, 2100, now, WINDOW), Step::Pause { found: 1500 });
    }

    /// A publish that never lands is re-sent once the confirmation window
    /// passes — an anvil pool is not durable, and a same-nonce sender can
    /// displace the tx (measured: daloc's deploy sharing the writer account
    /// wedged a feed in `pending` forever).
    #[test]
    fn a_stale_pending_publish_is_re_sent() {
        let in_flight = state(Some(2000), Some(2100));
        // Within the window: wait.
        assert_eq!(step(in_flight, 2000, 2100, 1_000, WINDOW), Step::WaitPending);
        // Past the window, target still not holding it: re-publish the
        // *current* source value.
        assert_eq!(step(in_flight, 2000, 2200, 1_000 + WINDOW, WINDOW), Step::Publish(2200));
        // Past the window but the target does hold it: confirm wins.
        assert_eq!(step(in_flight, 2100, 2200, 1_000 + WINDOW, WINDOW), Step::Confirm);
    }
}
