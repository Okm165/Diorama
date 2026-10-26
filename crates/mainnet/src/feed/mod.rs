//! The feed bridge (plan §7.1): the parent chain's live feed answers
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
pub(crate) mod chainlink;
pub(crate) mod http;
pub(crate) mod metrics;
pub(crate) mod policy;

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use alloy_primitives::Address;
use mirror_config::FeedSection;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) use http::admin_routes;

use crate::feed::chainlink::{
    publish, read_answer, read_decimals, read_target_decimals, target_decimals,
};
use crate::feed::policy::{step, Step, ValueState};
pub(crate) use crate::feed::metrics::FeedBridgeMetrics;

/// Who owns a feed's value: the bridge writes it from the source; a scenario
/// owns it (the pause) until the hand-back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FeedOwner {
    Bridge,
    Scenario,
}

impl FeedOwner {
    /// The byte the atomic carries.
    fn byte(self) -> u8 {
        match self {
            FeedOwner::Bridge => 0,
            FeedOwner::Scenario => 1,
        }
    }

    /// The byte back (an unknown value reads as the bridge's own: the safe
    /// default).
    fn of_byte(byte: u8) -> Self {
        match byte {
            0 => FeedOwner::Bridge,
            _ => FeedOwner::Scenario,
        }
    }
}

/// An atomic [`FeedOwner`] (Rust has no atomic enums): the pass's read path
/// never takes the values lock.
struct AtomicOwner(AtomicU8);

impl AtomicOwner {
    fn new(owner: FeedOwner) -> Self {
        Self(AtomicU8::new(owner.byte()))
    }

    fn load(&self) -> FeedOwner {
        FeedOwner::of_byte(self.0.load(Ordering::Relaxed))
    }

    fn store(&self, owner: FeedOwner) {
        self.0.store(owner.byte(), Ordering::Relaxed);
    }
}

/// One feed's runtime state (the section is the config; this is the truth the
/// pass compares against).
struct FeedRuntime {
    section: FeedSection,
    values: Mutex<ValueState>,
    /// Who owns this feed's value: the bridge from the source, or a scenario
    /// (the pause) until the hand-back.
    owner: AtomicOwner,
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
        let Some(decoded) = read_target_decimals(anvil_t, self.section.target).await? else {
            // An empty return does not decode: the *fact* is "no code yet".
            return Ok(None);
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
            owner: AtomicOwner::new(FeedOwner::Bridge),
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
        if feed.owner.load() == FeedOwner::Scenario {
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
        let now = crate::now_ms();
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
                    owner: f.owner.load(),
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
        feed.owner.store(FeedOwner::Scenario);
        Ok(())
    }

    /// Hand a feed back to the bridge; the *confirmed* value is forgotten so
    /// the next pass re-establishes the source's value (the hand-back rule),
    /// while a value published by `override` is still awaited as pending.
    pub fn resume(&self, name: &str) -> Result<()> {
        let feed = self.find(name)?;
        feed.owner.store(FeedOwner::Bridge);
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
        state.pending_since_ms = Some(crate::now_ms());
        drop(state);
        feed.owner.store(FeedOwner::Scenario);
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
    metrics.record_update(&feed.section.name);
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
    feed.owner.store(FeedOwner::Scenario);
    metrics.record_pause(&feed.section.name);
    tracing::warn!(
        feed = %feed.section.name,
        confirmed = ?state.confirmed,
        pending = ?state.pending,
        found,
        "a scenario owns this feed's value now; the bridge paused it (resume to hand back)"
    );
}

/// The `/admin/feed` list row.
#[derive(Debug, serde::Serialize)]
pub(crate) struct FeedStatus {
    pub name: String,
    pub target: String,
    pub source: String,
    /// Who owns the value now (the pause's own name).
    pub owner: FeedOwner,
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

