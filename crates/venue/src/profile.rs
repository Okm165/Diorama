//! `VenueMirror` — the venue mirror's divergence bundle, in its own folder.
//!
//! The spine ([`mirror_devnet::spine`]) declares the [`Mirror`] interface and
//! never names a mirror; this file is where the venue profile's choices are
//! made and documented, next to the ethereum profile's `crates/mainnet/src/profile.rs`:
//!
//! | decision | venue (here) | mainnet (`crates/mainnet/src/profile.rs`) |
//! |---|---|---|
//! | label | `"v"` | `"t"` |
//! | source | HL public API (latest reads) | reth (local full node) |
//! | BAL rung | synthesized (envelopes → closures → reads) | native (`eth_getBlockAccessList`), trace fallback |
//! | reset | chain-instance canary | cursor hash / below base |
//! | clock | native | align to the parent block + live offset |
//! | mining | pool drain | one T block per applied parent block |
//! | probes | live parity against the composed BAL | per applied block |
//! | prepend | none — mining drains the pool every pass | 3 rounds (a steady stream must not starve the parent) |
//!
//! Both mirrors therefore do the same things — follow their parent chain,
//! compose its changes as a BAL, COW it against the devnet's own writes, and
//! measure the result — while the decisions above are the only places the
//! implementations diverge. The composer itself is generic
//! (`mirror_devnet::bal::compose`).
use crate::reset::PinCanary;
use crate::source::{public_capabilities, VenueSource};
use mirror_config::{ClosuresSection, EngineSection};
use mirror_devnet::closure::{FixedSlots, KeyClosure, TopicMappedSlots};
use mirror_devnet::cow::{Scope, WatchSet};
use mirror_devnet::shared::Shared;
use mirror_devnet::source::{RawSource, Source};
use mirror_devnet::spine::{
    ClockPolicy, MinePolicy, Mirror, Policies, ProbeSchedule, ResetDetector,
};
use mirror_devnet::state::StateFile;
use mirror_anvil::Rpc;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// The venue mirror's bundle: the HL public API as the truth (two views — the
/// pin-exact hand-back source and the latest-reading follow source), the pin
/// canary, the declared scope, and the policies the driver reads.
pub struct VenueMirror {
    source: Arc<dyn Source>,
    follow: Arc<dyn Source>,
    reset: Arc<dyn ResetDetector>,
    policies: Policies,
    scope: Scope,
}

impl VenueMirror {
    /// The profile's decisions: the clock is V's own, mining drains the pool
    /// (an idle chain stays static), probes compare against the composed BAL,
    /// and the gap guard pauses the poll when the source head runs too far
    /// ahead (a wide window is a re-pin decision, never a silent partial poll).
    pub fn policies(engine: &EngineSection) -> Policies {
        Policies {
            clock: ClockPolicy::Native,
            mine: MinePolicy::PoolDrain,
            probes: ProbeSchedule::LiveParity,
            prepend_rounds: None,
            gap: Some(engine.loops.max_gap_blocks),
        }
    }
}

impl Mirror for VenueMirror {
    fn label(&self) -> &'static str {
        "v"
    }

    fn source(&self) -> &Arc<dyn Source> {
        &self.source
    }

    /// The follow reads the source at *latest* (the composer's values), while
    /// [`Self::source`] is pin-exact for hand-back: the pin guard would refuse
    /// the follow's reads, so the two roles are two objects over one handle.
    fn follow_source(&self) -> &Arc<dyn Source> {
        &self.follow
    }

    fn reset(&self) -> &Arc<dyn ResetDetector> {
        &self.reset
    }

    fn policies(&self) -> Policies {
        self.policies
    }

    fn scope(&self) -> &Scope {
        &self.scope
    }
}

/// The declared closures, from config: fixed slot sets and topic-mapped
/// mapping slots. Empty config = watch-only follow (the watchlist's account
/// fields; the composer's audit counts every uncovered emitter).
pub fn closures(section: &ClosuresSection) -> Vec<Box<dyn KeyClosure>> {
    let mut out: Vec<Box<dyn KeyClosure>> = Vec::new();
    let mut fixed = FixedSlots::new();
    for spec in &section.fixed {
        fixed.declare(spec.contract, spec.slots.iter().copied());
    }
    if !section.fixed.is_empty() {
        out.push(Box::new(fixed));
    }
    for spec in &section.mappings {
        out.push(Box::new(TopicMappedSlots {
            contract: spec.contract,
            base_slot: spec.base_slot,
            topic_index: spec.topic_index,
        }));
    }
    out
}

/// The venue's watch set: the watchlist accounts plus the declared closures —
/// fixed slots as slot declarations, mapping contracts as watched accounts
/// (the *plan* is the narrower set the closures derive; the watch set feeds
/// the COW filter and the audit).
pub fn watch_set(
    accounts: std::collections::HashSet<alloy_primitives::Address>,
    section: &ClosuresSection,
) -> WatchSet {
    let mut watch = WatchSet::new(accounts);
    for spec in &section.fixed {
        watch.declare_slots(spec.contract, spec.slots.iter().copied());
    }
    for spec in &section.mappings {
        watch.accounts.insert(spec.contract);
    }
    watch
}

/// The engine's build step (the profile's wiring, kept beside the bundle so
/// `ctx.rs` reads as assembly).
pub struct Wiring {
    pub mirror: Arc<dyn Mirror>,
    /// V's fork pin, shared with [`VenueSource`].
    pub pin: Arc<AtomicU64>,
    /// The composer's target side: V itself (shared with the anvil handle).
    pub target: Arc<dyn Source>,
}

/// What the wiring needs (the context assembles it; `build` consumes it).
pub struct Deps {
    pub state: Shared<StateFile>,
    pub source_rpc: Arc<Rpc>,
    pub v: Arc<Rpc>,
    pub watch: std::collections::HashSet<alloy_primitives::Address>,
}

pub fn build(engine: &EngineSection, deps: Deps) -> Wiring {
    let Deps {
        state,
        source_rpc,
        v,
        watch,
    } = deps;
    let pin = Arc::new(AtomicU64::new(0));
    let source: Arc<dyn Source> = Arc::new(VenueSource::new(source_rpc.clone(), pin.clone()));
    let follow: Arc<dyn Source> = Arc::new(RawSource::new(source_rpc, public_capabilities()));
    let target: Arc<dyn Source> = Arc::new(RawSource::target(v));
    let reset: Arc<dyn ResetDetector> = Arc::new(PinCanary::new(state));
    let scope = Scope::new(watch_set(watch, &engine.closures), closures(&engine.closures));
    let mirror: Arc<dyn Mirror> = Arc::new(VenueMirror {
        source,
        follow,
        reset,
        policies: VenueMirror::policies(engine),
        scope,
    });
    Wiring { mirror, pin, target }
}
