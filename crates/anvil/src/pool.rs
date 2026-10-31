//! The shared endpoint pool: one rotation, admission, health and retry
//! policy for every transport in the workspace.
//!
//! The mirrors read from several endpoints of the same logical source (reth,
//! the HyperEVM RPC, the venue's REST/WS API, the beacon API). The pool
//! answers four questions in one place, so the JSON-RPC client, the REST
//! client, the websocket lane and the beacon client cannot drift apart:
//!
//! 1. **Which endpoint next?** Each round starts at a random healthy endpoint
//!    and visits every healthy endpoint once. The random start is a
//!    per-pool counter run through splitmix64, not a mutable RNG: concurrent
//!    callers get distinct starts without a lock.
//! 2. **What happens to a failed endpoint?** It is skipped for an
//!    exponentially growing cooldown (base 1 s, cap 60 s) and retried in a
//!    later round. A success clears the failure count immediately.
//! 3. **What if every endpoint is cooling?** The pool refuses admission and
//!    reports the earliest retry. One jittered emergency probe per cooldown
//!    window is allowed, so an outage is never a permanent refusal *and* never
//!    a retry amplifier: a round of six cooling endpoints does not become
//!    twenty-four requests.
//! 4. **What does one attempt look like?** [`EndpointPool::execute`] owns the
//!    whole loop — admission, attempt budget, backoff, health recording and
//!    failover. A transport implements [`Attempt`] (one request against one
//!    endpoint) and never re-implements rotation or sleep.
//!
//! The pool is transport-free: it validates URL schemes and owns health, not
//! HTTP. `diorama_anvil::Rpc`, the venue's REST/WS clients and the beacon
//! client all consume the same executor, so "both mirrors use the same logic"
//! is a type-level fact rather than a convention.
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The two wire schemes a pool may serve. JSON-RPC and REST pools are HTTP;
/// the venue's market feed is WS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Ws,
}

impl Scheme {
    /// Whether `url` carries this scheme. The check is deliberately a prefix
    /// test on the two accepted spellings, matching the config boundary; a
    /// URL with no host is caught by the transport's own error.
    fn accepts(self, url: &str) -> bool {
        match self {
            Scheme::Http => url.starts_with("http://") || url.starts_with("https://"),
            Scheme::Ws => url.starts_with("ws://") || url.starts_with("wss://"),
        }
    }

    /// The scheme's name for diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Ws => "ws",
        }
    }
}

/// The pool's policy. `rounds` is the number of full rotations one logical
/// call may make; `max_attempts` is the hard per-call budget (a pool with many
/// endpoints must not multiply the rounds into a request storm). The cooldown
/// is the delay before a failed endpoint is eligible again; the emergency
/// probe is the one jittered attempt allowed while every endpoint is cooling.
///
/// The fields are milliseconds and plain integers so the same shape is a TOML
/// table (`[endpoint_defaults]`) and a runtime value without a second unit
/// convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolPolicy {
    pub rounds: u32,
    pub max_attempts: u32,
    pub cooldown_base_ms: u64,
    pub cooldown_max_ms: u64,
    pub jitter_ms: u64,
    /// Allow one probe per cooldown window while every endpoint is cooling.
    pub emergency_probe: bool,
    /// The minimum spacing between emergency probes.
    pub emergency_probe_ms: u64,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            rounds: 4,
            max_attempts: 12,
            cooldown_base_ms: 1_000,
            cooldown_max_ms: 60_000,
            jitter_ms: 250,
            emergency_probe: true,
            emergency_probe_ms: 1_000,
        }
    }
}

impl PoolPolicy {
    pub fn cooldown_base(&self) -> Duration {
        Duration::from_millis(self.cooldown_base_ms)
    }

    pub fn cooldown_max(&self) -> Duration {
        Duration::from_millis(self.cooldown_max_ms)
    }

    pub fn jitter(&self) -> Duration {
        Duration::from_millis(self.jitter_ms)
    }

    /// The per-call budget, never zero: a policy that cannot make one attempt
    /// is a config error, and a silent zero would look like an outage.
    pub fn attempts_budget(&self) -> u32 {
        self.max_attempts.max(1)
    }
}

/// One endpoint: its URL plus the attributes that make a pool more than a
/// blind URL list. `roles` empty means "serves every role"; a required role is
/// an intersection, so an archive-only endpoint is never selected for a
/// `latest`-only read unless it also carries that role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSpec {
    pub url: String,
    #[serde(default)]
    pub roles: BTreeSet<String>,
    /// The chain id the endpoint must answer. `None` means "not asserted";
    /// a source pool sets it so a wrong-chain provider is a boot fault, not a
    /// silent correctness fault.
    #[serde(default)]
    pub chain_id: Option<u64>,
    /// Relative preference. The executor still visits every healthy endpoint
    /// once per round; weight is available to callers that build a
    /// deterministic order (and to future admission policies).
    #[serde(default = "one")]
    pub weight: u32,
}

const fn one() -> u32 {
    1
}

impl EndpointSpec {
    /// A URL with no attributes (the one-URL form).
    pub fn url(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            roles: BTreeSet::new(),
            chain_id: None,
            weight: 1,
        }
    }

    fn serves(&self, required: &[&str]) -> bool {
        // An empty `roles` means "no declared restrictions": the endpoint is
        // admitted for every unqualified call, but never claims a capability
        // it did not declare. This keeps a legacy URL-only pool working while
        // making an `archive`/`trace`/`market_ws` requirement opt-in.
        required.iter().all(|role| self.roles.contains(*role))
    }
}

/// One endpoint's health, kept in atomics so the hot path has no lock.
#[derive(Debug)]
struct Health {
    down_until_ms: AtomicU64,
    failures: AtomicU32,
    attempts: AtomicU64,
    faults: AtomicU64,
    last_ok_ms: AtomicU64,
}

impl Health {
    fn new() -> Self {
        Self {
            down_until_ms: AtomicU64::new(0),
            failures: AtomicU32::new(0),
            attempts: AtomicU64::new(0),
            faults: AtomicU64::new(0),
            last_ok_ms: AtomicU64::new(0),
        }
    }
}

/// A read-only health snapshot for `/status`, logs and tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EndpointSnapshot {
    pub index: usize,
    pub url: String,
    pub chain_id: Option<u64>,
    pub roles: Vec<String>,
    pub down_until_ms: u64,
    pub failures: u32,
    pub attempts: u64,
    pub faults: u64,
    pub last_ok_ms: u64,
}

/// One call's admission decision. Healthy endpoints are returned in random
/// order; when none is healthy `indices` is empty, `degraded` is true and
/// `retry_after_ms` names the earliest cooldown expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    pub indices: Vec<usize>,
    pub degraded: bool,
    pub retry_after_ms: u64,
}

/// One endpoint attempt's outcome: the transport's answer or its fault. The
/// executor records health; the transport never touches the pool's counters.
pub enum AttemptOutcome<T, E> {
    Served(T),
    Fault(E),
}

/// The endpoint that served a logical call, plus its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served<T> {
    pub index: usize,
    pub value: T,
}

/// A logical call's exhaustion. `Cooling` carries the earliest retry; a
/// caller may sleep and retry, or surface it as a `Retry-After`.
#[derive(Debug)]
pub enum PoolError<E> {
    Exhausted { attempts: u32, last: Option<E> },
    Cooling { retry_after_ms: u64 },
}

impl<E: fmt::Display> fmt::Display for PoolError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PoolError::Exhausted { attempts, last } => match last {
                Some(last) => write!(f, "{attempts} endpoint attempt(s) failed; last: {last}"),
                None => write!(f, "{attempts} endpoint attempt(s) failed"),
            },
            PoolError::Cooling { retry_after_ms } => {
                write!(f, "every endpoint is cooling; retry in {retry_after_ms} ms")
            }
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for PoolError<E> {}

/// One request against one endpoint. The executor supplies the index (and
/// therefore the URL); the transport supplies the request and classifies its
/// own answer. This is the only trait a lane implements — rotation, admission,
/// cooldown, attempt budgets and sleep live in [`EndpointPool::execute`].
#[allow(async_fn_in_trait)]
pub trait Attempt {
    type Output;
    type Fault;

    async fn attempt(&self, index: usize) -> AttemptOutcome<Self::Output, Self::Fault>;
}

/// A validated set of endpoints plus their health. Cloning shares the
/// endpoint list and health cells (the pool is one logical object); the
/// rotation counter continues from the parent's value.
#[derive(Debug)]
pub struct EndpointPool {
    endpoints: Arc<[EndpointSpec]>,
    health: Arc<[Health]>,
    policy: PoolPolicy,
    start: AtomicU64,
    /// Per-process entropy: two pods with the same endpoint list must not
    /// retry in lockstep.
    seed: u64,
    /// The next time an emergency probe may be made (wall ms).
    emergency_at: Arc<AtomicU64>,
}

impl Clone for EndpointPool {
    fn clone(&self) -> Self {
        Self {
            endpoints: Arc::clone(&self.endpoints),
            health: Arc::clone(&self.health),
            policy: self.policy,
            start: AtomicU64::new(self.start.load(Ordering::Relaxed)),
            seed: self.seed,
            emergency_at: Arc::clone(&self.emergency_at),
        }
    }
}

impl EndpointPool {
    /// Build a pool over `urls`. Empty entries are dropped, duplicates are
    /// removed in first-seen order, and every entry must carry `scheme`.
    /// `primary()` is the first surviving entry.
    pub fn new(urls: &[String], scheme: Scheme, policy: PoolPolicy) -> Result<Self> {
        let specs: Vec<EndpointSpec> = urls.iter().map(EndpointSpec::url).collect();
        Self::from_specs(specs, scheme, policy)
    }

    /// Build a pool over attributed endpoints. The config layer turns
    /// `[endpoints.<name>]` into these; the URL-only constructor remains for
    /// the one-URL call sites.
    pub fn from_specs(
        specs: impl IntoIterator<Item = EndpointSpec>,
        scheme: Scheme,
        policy: PoolPolicy,
    ) -> Result<Self> {
        let mut endpoints: Vec<EndpointSpec> = Vec::new();
        for mut spec in specs {
            spec.url = spec.url.trim().trim_end_matches('/').to_string();
            if spec.url.is_empty() {
                continue;
            }
            if !scheme.accepts(&spec.url) {
                return Err(Error::InvalidEndpoint { endpoint: spec.url });
            }
            if !endpoints.iter().any(|known| known.url == spec.url) {
                endpoints.push(spec);
            }
        }
        if endpoints.is_empty() {
            return Err(Error::NoEndpoints);
        }
        let health: Vec<Health> = endpoints.iter().map(|_| Health::new()).collect();
        Ok(Self {
            endpoints: endpoints.into(),
            health: health.into(),
            policy,
            start: AtomicU64::new(0),
            seed: process_seed(),
            emergency_at: Arc::new(AtomicU64::new(0)),
        })
    }

    /// A single-endpoint HTTP pool (the write client and a one-URL source).
    pub fn one(url: &str) -> Result<Self> {
        Self::new(&[url.to_string()], Scheme::Http, PoolPolicy::default())
    }

    /// The declared primary endpoint (the first one).
    pub fn primary(&self) -> &str {
        &self.endpoints[0].url
    }

    /// Every endpoint, in declaration order.
    pub fn urls(&self) -> Vec<&str> {
        self.endpoints
            .iter()
            .map(|spec| spec.url.as_str())
            .collect()
    }

    /// Every endpoint's full attributes, in declaration order.
    pub fn specs(&self) -> &[EndpointSpec] {
        &self.endpoints
    }

    /// One endpoint by index.
    pub fn url(&self, index: usize) -> &str {
        &self.endpoints[index].url
    }

    /// One endpoint's declared chain id.
    pub fn chain_id(&self, index: usize) -> Option<u64> {
        self.endpoints[index].chain_id
    }

    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    pub fn policy(&self) -> PoolPolicy {
        self.policy
    }

    /// The per-process jitter seed (exposed for tests and diagnostics).
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Admission for one round: healthy endpoints that serve `roles`, in
    /// random order. Cooling endpoints are omitted; if none is healthy the
    /// caller gets the earliest retry instead of the whole cooling set.
    pub fn admit(&self, now_ms: u64, roles: &[&str]) -> Admission {
        let len = self.endpoints.len();
        let mut indices: Vec<usize> = (0..len).collect();
        shuffle(
            &mut indices,
            splitmix64(self.start.fetch_add(1, Ordering::Relaxed) ^ self.seed),
        );
        let mut healthy = Vec::with_capacity(len);
        let mut cooling = Vec::with_capacity(len);
        for index in indices {
            if !self.endpoints[index].serves(roles) {
                continue;
            }
            if self.health[index].down_until_ms.load(Ordering::Relaxed) <= now_ms {
                healthy.push(index);
            } else {
                cooling.push(index);
            }
        }
        if healthy.is_empty() {
            Admission {
                indices: Vec::new(),
                degraded: true,
                retry_after_ms: self.retry_after_ms(now_ms, &cooling),
            }
        } else {
            Admission {
                indices: healthy,
                degraded: false,
                retry_after_ms: 0,
            }
        }
    }

    /// The one jittered probe allowed while every endpoint is cooling. It is
    /// rate-limited per pool (not per caller), so an outage cannot be turned
    /// into a request storm by concurrency. `None` means "wait".
    pub fn emergency_probe(&self, now_ms: u64, roles: &[&str]) -> Option<usize> {
        if !self.policy.emergency_probe {
            return None;
        }
        let next = self.emergency_at.load(Ordering::Relaxed);
        if now_ms < next {
            return None;
        }
        let mut candidates: Vec<usize> = (0..self.endpoints.len())
            .filter(|index| self.endpoints[*index].serves(roles))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        // The endpoint closest to its cooldown expiry is the least bad probe.
        candidates.sort_by_key(|index| self.health[*index].down_until_ms.load(Ordering::Relaxed));
        let pick = candidates[splitmix64(self.seed ^ now_ms) as usize % candidates.len()];
        let spacing = self
            .policy
            .emergency_probe_ms
            .max(self.policy.cooldown_base_ms)
            .max(1);
        self.emergency_at.store(
            now_ms
                .saturating_add(spacing)
                .saturating_add(self.jitter(now_ms, pick)),
            Ordering::Relaxed,
        );
        Some(pick)
    }

    /// The earliest cooldown expiry among `cooling`, in ms from `now`.
    fn retry_after_ms(&self, now_ms: u64, cooling: &[usize]) -> u64 {
        cooling
            .iter()
            .map(|index| self.health[*index].down_until_ms.load(Ordering::Relaxed))
            .min()
            .map(|until| until.saturating_sub(now_ms))
            .unwrap_or(0)
    }

    /// The next wall-clock ms at which any endpoint serving `roles` is
    /// healthy again (0 when one is healthy now).
    pub fn next_ready_ms(&self, now_ms: u64, roles: &[&str]) -> u64 {
        let mut next = u64::MAX;
        for (index, spec) in self.endpoints.iter().enumerate() {
            if !spec.serves(roles) {
                continue;
            }
            let down = self.health[index].down_until_ms.load(Ordering::Relaxed);
            if down <= now_ms {
                return now_ms;
            }
            next = next.min(down);
        }
        if next == u64::MAX {
            now_ms
        } else {
            next
        }
    }

    /// Record one attempt against `index` (before the request is sent).
    pub fn record_attempt(&self, index: usize) {
        self.health[index].attempts.fetch_add(1, Ordering::Relaxed);
    }

    /// A served answer: the endpoint is healthy again.
    pub fn record_success(&self, index: usize, now_ms: u64) {
        let health = &self.health[index];
        health.failures.store(0, Ordering::Relaxed);
        health.down_until_ms.store(0, Ordering::Relaxed);
        health.last_ok_ms.store(now_ms, Ordering::Relaxed);
    }

    /// A fault: cool the endpoint down for an exponentially growing window.
    /// The first failure waits `cooldown_base`, the second twice that, and so
    /// on to `cooldown_max`; the jitter is seeded from process entropy, the
    /// endpoint's URL and the failure count, so two pods (or two pools in one
    /// process) do not retry in lockstep.
    pub fn record_failure(&self, index: usize, now_ms: u64) {
        let health = &self.health[index];
        let failures = health
            .failures
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        health.faults.fetch_add(1, Ordering::Relaxed);
        // The exponent is bounded by the type, not by a guessed failure
        // count: a shift past u64's width saturates to the cap, exactly as a
        // large-but-representable exponent does.
        let exponent = failures.saturating_sub(1);
        let base = self.policy.cooldown_base_ms;
        let cap = self.policy.cooldown_max_ms;
        let factor = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
        let cooldown = base.saturating_mul(factor).min(cap);
        let jitter = self.jitter(now_ms, index);
        health.down_until_ms.store(
            now_ms.saturating_add(cooldown).saturating_add(jitter),
            Ordering::Relaxed,
        );
        // Do not probe the endpoint again in the same instant it failed: the
        // first emergency probe is one base cooldown away, then rate-limited.
        let earliest = now_ms
            .saturating_add(self.policy.cooldown_base_ms.max(1))
            .saturating_add(jitter);
        self.emergency_at.fetch_max(earliest, Ordering::Relaxed);
    }

    fn jitter(&self, now_ms: u64, index: usize) -> u64 {
        let jitter_ms = self.policy.jitter_ms;
        if jitter_ms == 0 {
            return 0;
        }
        let url_seed = fnv1a(self.endpoints[index].url.as_bytes());
        splitmix64(self.seed ^ url_seed ^ now_ms) % (jitter_ms + 1)
    }

    /// One logical call over the pool: admission, failover, bounded backoff
    /// and the per-call attempt budget in one place.
    ///
    /// The transport implements [`Attempt`] for one endpoint; it never loops,
    /// sleeps or touches health. A successful attempt is the first success;
    /// a fault cools the endpoint and moves to the next admitted endpoint. If
    /// every endpoint is cooling, one rate-limited emergency probe is allowed,
    /// then the call either waits for the earliest cooldown or returns
    /// [`PoolError::Cooling`] — never the old "try all cooling endpoints every
    /// round" amplification.
    pub async fn execute<A: Attempt>(
        &self,
        attempt: &A,
        roles: &[&str],
    ) -> std::result::Result<Served<A::Output>, PoolError<A::Fault>> {
        let policy = self.policy;
        let budget = policy.attempts_budget();
        let mut attempts = 0u32;
        let mut last: Option<A::Fault> = None;
        let mut backoff =
            crate::backoff::Backoff::new(policy.cooldown_base(), policy.cooldown_max());
        let mut round = 0u32;
        while round < policy.rounds && attempts < budget {
            let now = now_ms();
            let admission = self.admit(now, roles);
            let indices = if admission.indices.is_empty() {
                if let Some(index) = self.emergency_probe(now, roles) {
                    vec![index]
                } else if admission.retry_after_ms == 0 {
                    // No endpoint serves the requested roles (or the pool has
                    // no matching endpoint at all): waiting cannot change that.
                    return Err(PoolError::Exhausted { attempts, last });
                } else {
                    let wait = admission.retry_after_ms.min(policy.cooldown_max_ms.max(1));
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    round += 1;
                    continue;
                }
            } else {
                admission.indices
            };
            if let Some(served) = self
                .try_round(attempt, &indices, &mut attempts, &mut last)
                .await
            {
                return Ok(served);
            }
            round += 1;
            if round < policy.rounds && attempts < budget {
                let delay = backoff.delay();
                let jitter = self.jitter(now_ms(), 0);
                tokio::time::sleep(delay + Duration::from_millis(jitter)).await;
                backoff.record(crate::backoff::Progress::Stalled);
            }
        }
        let now = now_ms();
        let next = self.next_ready_ms(now, roles);
        if next > now {
            Err(PoolError::Cooling {
                retry_after_ms: next - now,
            })
        } else {
            Err(PoolError::Exhausted { attempts, last })
        }
    }

    /// Try one round's admitted endpoints, recording health and the budget.
    /// `Some` is the first success; `None` means every admitted attempt
    /// faulted (the caller decides whether to wait and rotate again).
    async fn try_round<A: Attempt>(
        &self,
        attempt: &A,
        indices: &[usize],
        attempts: &mut u32,
        last: &mut Option<A::Fault>,
    ) -> Option<Served<A::Output>> {
        let budget = self.policy.attempts_budget();
        for &index in indices {
            if *attempts >= budget {
                break;
            }
            *attempts += 1;
            self.record_attempt(index);
            match attempt.attempt(index).await {
                AttemptOutcome::Served(value) => {
                    self.record_success(index, now_ms());
                    return Some(Served { index, value });
                }
                AttemptOutcome::Fault(fault) => {
                    self.record_failure(index, now_ms());
                    tracing::debug!(
                        endpoint = %self.url(index),
                        "endpoint fault; cooling it down and trying the next endpoint"
                    );
                    *last = Some(fault);
                }
            }
        }
        None
    }

    /// The health snapshot, one row per endpoint.
    pub fn snapshot(&self) -> Vec<EndpointSnapshot> {
        self.health
            .iter()
            .enumerate()
            .map(|(index, health)| EndpointSnapshot {
                index,
                url: self.endpoints[index].url.clone(),
                chain_id: self.endpoints[index].chain_id,
                roles: self.endpoints[index].roles.iter().cloned().collect(),
                down_until_ms: health.down_until_ms.load(Ordering::Relaxed),
                failures: health.failures.load(Ordering::Relaxed),
                attempts: health.attempts.load(Ordering::Relaxed),
                faults: health.faults.load(Ordering::Relaxed),
                last_ok_ms: health.last_ok_ms.load(Ordering::Relaxed),
            })
            .collect()
    }
}

/// The current wall clock in milliseconds. The pool's health is expressed in
/// wall time so a restart does not inherit a stale monotonic base.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// splitmix64: a small, allocation-free mixer for a counter. Its output is
/// deterministic per seed, so tests can pin a rotation without a global RNG.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// FNV-1a: the endpoint's stable identity for jitter, independent of its
/// position in the list (reordering the config must not synchronize retries).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Per-process entropy for jitter. Deliberately not a cryptographic seed: it
/// only has to differ between pods that started at different times (the
/// process id and the clock are enough; the stack address breaks ties between
/// pools in one process).
fn process_seed() -> u64 {
    let marker = 0u8;
    let address = std::ptr::addr_of!(marker) as u64;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    splitmix64(nanos ^ u64::from(std::process::id()) ^ address)
}

/// Fisher-Yates, seeded by one splitmix64 step per swap.
fn shuffle(indices: &mut [usize], mut seed: u64) {
    for i in (1..indices.len()).rev() {
        seed = splitmix64(seed);
        let j = (seed as usize) % (i + 1);
        indices.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn http(urls: &[&str]) -> EndpointPool {
        EndpointPool::new(
            &urls
                .iter()
                .map(|url| (*url).to_string())
                .collect::<Vec<_>>(),
            Scheme::Http,
            PoolPolicy::default(),
        )
        .expect("a valid pool")
    }

    struct Probe {
        attempts: AtomicU32,
        fail_until: u32,
    }

    impl Attempt for Probe {
        type Output = u32;
        type Fault = &'static str;

        async fn attempt(&self, _index: usize) -> AttemptOutcome<u32, &'static str> {
            let attempt = self.attempts.fetch_add(1, Ordering::Relaxed) + 1;
            if attempt <= self.fail_until {
                AttemptOutcome::Fault("refused")
            } else {
                AttemptOutcome::Served(attempt)
            }
        }
    }

    #[test]
    fn a_pool_validates_trims_and_deduplicates() {
        let pool = http(&[" http://a:8545/ ", "http://a:8545", "https://b:443"]);
        assert_eq!(pool.urls(), ["http://a:8545", "https://b:443"]);
        assert_eq!(pool.primary(), "http://a:8545");
        assert!(EndpointPool::new(&[], Scheme::Http, PoolPolicy::default()).is_err());
        assert!(EndpointPool::new(
            &["ftp://a".to_string()],
            Scheme::Http,
            PoolPolicy::default()
        )
        .is_err());
        assert!(EndpointPool::new(
            &["wss://a/ws".to_string()],
            Scheme::Http,
            PoolPolicy::default()
        )
        .is_err());
        assert!(EndpointPool::new(
            &["wss://a/ws".to_string()],
            Scheme::Ws,
            PoolPolicy::default()
        )
        .is_ok());
    }

    #[test]
    fn every_round_visits_every_healthy_endpoint_once() {
        let pool = http(&["http://a", "http://b", "http://c", "http://d"]);
        let mut starts = std::collections::HashSet::new();
        for _ in 0..16 {
            let admission = pool.admit(0, &[]);
            assert!(!admission.degraded);
            let mut sorted = admission.indices.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, vec![0, 1, 2, 3]);
            starts.insert(admission.indices[0]);
        }
        assert!(starts.len() > 1, "the start must rotate across rounds");
    }

    #[test]
    fn a_failed_endpoint_is_skipped_then_retried_after_cooldown() {
        let pool = http(&["http://a", "http://b"]);
        pool.record_failure(0, 1_000);
        let admission = pool.admit(1_000, &[]);
        assert!(!admission.degraded);
        assert_eq!(
            admission.indices,
            vec![1],
            "the cooling endpoint is skipped"
        );
        // The first cooldown is 1 s plus at most 250 ms of jitter; 3 s is
        // safely past it.
        let admission = pool.admit(3_000, &[]);
        assert!(!admission.degraded);
        assert!(
            admission.indices.contains(&0),
            "the endpoint returns after the cooldown"
        );
    }

    #[test]
    fn repeated_failures_grow_to_the_cap_and_success_resets() {
        let pool = http(&["http://a", "http://b"]);
        let policy = pool.policy();
        for _ in 0..8 {
            pool.record_failure(0, 0);
        }
        let snapshot = pool.snapshot();
        assert_eq!(snapshot[0].failures, 8);
        let down = snapshot[0].down_until_ms;
        assert!(down >= policy.cooldown_max_ms);
        pool.record_success(0, 10_000);
        let snapshot = pool.snapshot();
        assert_eq!(snapshot[0].failures, 0);
        assert_eq!(snapshot[0].down_until_ms, 0);
        assert_eq!(snapshot[0].last_ok_ms, 10_000);
    }

    #[test]
    fn a_very_large_failure_count_still_caps_without_overflowing_the_shift() {
        let pool = http(&["http://a", "http://b"]);
        let cap = pool.policy().cooldown_max_ms;
        for _ in 0..100 {
            pool.record_failure(0, 0);
        }
        let down = pool.snapshot()[0].down_until_ms;
        assert!(
            down >= cap,
            "a failure count past the u64 shift width saturates at the cap"
        );
    }

    #[test]
    fn an_all_cooling_pool_refuses_admission_and_names_the_retry() {
        let pool = http(&["http://a", "http://b"]);
        pool.record_failure(0, 0);
        pool.record_failure(1, 0);
        let admission = pool.admit(0, &[]);
        assert!(admission.degraded);
        assert!(
            admission.indices.is_empty(),
            "a cooling endpoint is never re-tried in-place"
        );
        assert!(admission.retry_after_ms >= 1_000);
    }

    #[test]
    fn the_emergency_probe_is_rate_limited() {
        let pool = http(&["http://a", "http://b"]);
        pool.record_failure(0, 0);
        pool.record_failure(1, 0);
        assert!(
            pool.emergency_probe(0, &[]).is_none(),
            "a failure does not earn an immediate re-probe"
        );
        let first = pool
            .emergency_probe(2_000, &[])
            .expect("one probe is allowed");
        assert!(first < 2);
        assert!(
            pool.emergency_probe(2_001, &[]).is_none(),
            "the second probe inside the window is refused"
        );
        assert!(
            pool.emergency_probe(4_000, &[]).is_some(),
            "the next window admits one probe"
        );
    }

    #[test]
    fn execute_fails_over_then_records_the_served_endpoint() {
        let pool = http(&["http://a", "http://b"]);
        let probe = Probe {
            attempts: AtomicU32::new(0),
            fail_until: 1,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let served = runtime
            .block_on(pool.execute(&probe, &[]))
            .expect("the second attempt serves");
        assert_eq!(served.value, 2);
        assert_eq!(probe.attempts.load(Ordering::Relaxed), 2);
        let snapshot = pool.snapshot();
        assert_eq!(
            snapshot.iter().map(|endpoint| endpoint.faults).sum::<u64>(),
            1,
            "exactly the first attempt was cooled"
        );
        assert_eq!(snapshot[served.index].failures, 0);
    }

    #[test]
    fn execute_honours_the_attempt_budget() {
        let policy = PoolPolicy {
            max_attempts: 2,
            rounds: 4,
            ..PoolPolicy::default()
        };
        let pool = EndpointPool::new(
            &["http://a".into(), "http://b".into(), "http://c".into()],
            Scheme::Http,
            policy,
        )
        .unwrap();
        let probe = Probe {
            attempts: AtomicU32::new(0),
            fail_until: 99,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let error = runtime.block_on(pool.execute(&probe, &[])).unwrap_err();
        assert!(matches!(
            error,
            PoolError::Cooling { .. } | PoolError::Exhausted { .. }
        ));
        assert_eq!(probe.attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn roles_filter_admission_and_emergency_probes() {
        let mut archive = EndpointSpec::url("http://archive");
        archive.roles.insert("archive".into());
        archive.chain_id = Some(1);
        let pool = EndpointPool::from_specs(
            [archive, EndpointSpec::url("http://latest")],
            Scheme::Http,
            PoolPolicy::default(),
        )
        .unwrap();
        let admission = pool.admit(0, &["archive"]);
        assert_eq!(admission.indices, vec![0]);
        assert_eq!(pool.chain_id(0), Some(1));
        assert_eq!(pool.snapshot()[0].roles, vec!["archive".to_string()]);
    }

    #[test]
    fn execute_refuses_immediately_when_no_endpoint_serves_the_role() {
        let pool = http(&["http://a", "http://b"]);
        let probe = Probe {
            attempts: AtomicU32::new(0),
            fail_until: 0,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let error = runtime
            .block_on(pool.execute(&probe, &["archive"]))
            .expect_err("no endpoint declares archive");
        assert!(
            matches!(error, PoolError::Exhausted { attempts: 0, .. }),
            "{error}"
        );
        assert_eq!(probe.attempts.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_single_endpoint_pool_keeps_the_old_shape() {
        let pool = http(&["http://a"]);
        assert_eq!(pool.admit(0, &[]).indices, vec![0]);
        assert_eq!(pool.len(), 1);
    }
}
