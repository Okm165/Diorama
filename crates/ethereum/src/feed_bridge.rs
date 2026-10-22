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
use alloy_primitives::{Address, Bytes, I256};
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
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct ValueState {
    confirmed: Option<i128>,
    pending: Option<i128>,
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
    /// Source/target decimals, read once at boot (a mismatch is a boot fault).
    decimals: u8,
}

/// The bridge's metrics.
#[derive(Default)]
pub struct FeedBridgeMetrics {
    updates: Mutex<Vec<(String, u64)>>,
    pauses: Mutex<Vec<(String, u64)>>,
    source_errors: std::sync::atomic::AtomicU64,
}

impl FeedBridgeMetrics {
    pub fn inc_source_errors(&self) {
        self.source_errors.fetch_add(1, Ordering::Relaxed);
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
            "feed_bridge_source_errors_total",
            "source aggregator reads that failed",
            self.source_errors.load(Ordering::Relaxed),
        );
        out
    }
}

pub struct FeedBridge {
    feeds: Vec<FeedRuntime>,
    /// The account that submits `updateAnswer` on T (auto-impersonate makes
    /// any address usable; the mock's own access control decides who may).
    writer: Address,
    poll: Duration,
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
            let source = read_decimals(&ctx.reth, feed.source).await.map_err(|e| {
                Error::FeedBridgeFeed {
                    feed: feed.name.clone(),
                    what: "source decimals",
                    source: Box::new(e),
                }
            })?;
            let target = read_decimals(&ctx.anvil_t, feed.target).await.map_err(|e| {
                Error::FeedBridgeFeed {
                    feed: feed.name.clone(),
                    what: "target decimals",
                    source: Box::new(e),
                }
            })?;
            if source != target {
                return Err(Error::FeedBridgeDecimals {
                    feed: feed.name.clone(),
                    have: source,
                    want: target,
                });
            }
            feeds.push(FeedRuntime {
                section: feed.clone(),
                values: Mutex::new(ValueState::default()),
                paused: AtomicBool::new(false),
                last_error: Mutex::new(None),
                decimals: source,
            });
        }
        Ok(Self {
            feeds,
            writer: section.writer,
            poll: Duration::from_millis(section.poll_interval_ms),
            metrics: ctx.feed_bridge_metrics.clone(),
        })
    }

    /// Run forever: one pass per `poll_interval_ms`.
    pub async fn run(self: Arc<Self>, ctx: Arc<Ctx>) {
        loop {
            for feed in &self.feeds {
                if let Err(e) = self.pass_one(&ctx, feed).await {
                    self.metrics.inc_source_errors();
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
        let source_now = read_answer(&ctx.reth, feed.section.source).await?;
        let target_now = read_answer(&ctx.anvil_t, feed.section.target).await?;
        let state = *feed.values.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match step(state, target_now, source_now) {
            Step::Confirm => {
                let mut state = feed.values.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                state.confirmed = state.pending;
                state.pending = None;
                Ok(())
            }
            Step::WaitPending | Step::SkipUnchanged => Ok(()),
            Step::Publish(answer) => {
                publish(&ctx.anvil_t, feed.section.target, self.writer, answer).await?;
                feed.values
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending = Some(answer);
                FeedBridgeMetrics::count(&self.metrics.updates, &feed.section.name);
                tracing::info!(feed = %feed.section.name, answer, decimals = feed.decimals, "feed bridge published");
                Ok(())
            }
            Step::Pause { found } => {
                feed.paused.store(true, Ordering::Relaxed);
                FeedBridgeMetrics::count(&self.metrics.pauses, &feed.section.name);
                tracing::warn!(
                    feed = %feed.section.name,
                    confirmed = ?state.confirmed,
                    pending = ?state.pending,
                    found,
                    "a scenario owns this feed's value now; the bridge paused it (resume to hand back)"
                );
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
                    last_error: f
                        .last_error
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone(),
                    decimals: f.decimals,
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
    pub async fn override_answer(&self, ctx: &Ctx, name: &str, answer: i128) -> Result<()> {
        let feed = self.find(name)?;
        publish(&ctx.anvil_t, feed.section.target, self.writer, answer).await?;
        feed.values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending = Some(answer);
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
///    publish is in flight, skip when nothing moved;
/// 3. anything else on the target → a scenario wrote it: pause, never
///    overwrite a scenario's value.
fn step(state: ValueState, target_now: i128, source_now: i128) -> Step {
    if state.pending == Some(target_now) {
        return Step::Confirm;
    }
    let ours = state.confirmed.is_none() || state.confirmed == Some(target_now);
    if !ours {
        return Step::Pause { found: target_now };
    }
    if state.pending.is_some() {
        return Step::WaitPending;
    }
    if state.confirmed == Some(source_now) {
        return Step::SkipUnchanged;
    }
    Step::Publish(source_now)
}

async fn read_decimals(rpc: &mirror_anvil::Rpc, aggregator: Address) -> Result<u8> {
    let data = Bytes::from(AggregatorV3Interface::decimalsCall {}.abi_encode());
    let out = rpc.call_latest(aggregator, &data).await?;
    let decoded = AggregatorV3Interface::decimalsCall::abi_decode_returns(&out)
        .map_err(|e| Error::FeedBridgeAbi {
            what: "decimals()",
            detail: e.to_string(),
        })?;
    Ok(decoded)
}

async fn read_answer(rpc: &mirror_anvil::Rpc, aggregator: Address) -> Result<i128> {
    let data = Bytes::from(AggregatorV3Interface::latestRoundDataCall {}.abi_encode());
    let out = rpc.call_latest(aggregator, &data).await?;
    let decoded = AggregatorV3Interface::latestRoundDataCall::abi_decode_returns(&out)
        .map_err(|e| Error::FeedBridgeAbi {
            what: "latestRoundData()",
            detail: e.to_string(),
        })?;
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
pub struct FeedStatus {
    pub name: String,
    pub target: String,
    pub source: String,
    pub paused: bool,
    /// The last value the chain confirmed the bridge wrote.
    pub confirmed_answer: Option<i128>,
    /// A publish awaiting its block, when one is in flight.
    pub pending_answer: Option<i128>,
    /// The last pass fault (a wiped target, a source outage), when one is
    /// active: /admin/feed must show a stuck feed, not a stale success.
    pub last_error: Option<String>,
    pub decimals: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(confirmed: Option<i128>, pending: Option<i128>) -> ValueState {
        ValueState { confirmed, pending }
    }

    #[test]
    fn the_pause_rule_never_overwrites_a_scenario() {
        // The bridge confirmed 2000; the target now holds 1500 (a crash): pause.
        assert_eq!(step(state(Some(2000), None), 1500, 2000), Step::Pause { found: 1500 });
        // The target still holds the bridge's value; the source moved: publish.
        assert_eq!(step(state(Some(2000), None), 2000, 2100), Step::Publish(2100));
        // Nothing moved: no tx.
        assert_eq!(step(state(Some(2000), None), 2000, 2000), Step::SkipUnchanged);
        // First pass (never wrote): publish the source value, no pause.
        assert_eq!(step(ValueState::default(), 999, 2000), Step::Publish(2000));
    }

    /// Between the publish and its mining the target holds the *previous*
    /// value: that must read as "in flight", never as a scenario's write.
    #[test]
    fn a_publish_in_flight_is_not_a_scenario_write() {
        assert_eq!(step(state(Some(2000), Some(2100)), 2000, 2100), Step::WaitPending);
        // Mined: confirmed.
        assert_eq!(step(state(Some(2000), Some(2100)), 2100, 2100), Step::Confirm);
        // After Confirm the next pass sees the source unchanged: skip.
        assert_eq!(step(state(Some(2100), None), 2100, 2100), Step::SkipUnchanged);
        // A crash during the flight is still caught (neither value).
        assert_eq!(step(state(Some(2000), Some(2100)), 1500, 2100), Step::Pause { found: 1500 });
    }
}
