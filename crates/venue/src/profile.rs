//! The venue mirror's [`Bundle`] — its divergence choices, filled and
//! documented in its own folder.
//!
//! The spine ([`mirror_devnet::spine`]) declares the [`Mirror`] interface and
//! never names a mirror; this file is where the venue profile's choices are
//! made and documented, next to the mainnet profile's `crates/mainnet/src/profile.rs`:
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
use crate::source::public_capabilities;
use mirror_config::{ClosuresSection, EngineSection, LoopsSection};
use mirror_devnet::closure::{FixedSlots, KeyClosure, TopicMappedSlots};
use mirror_devnet::cow::{Scope, WatchSet};
use mirror_devnet::shared::Shared;
use mirror_devnet::source::{PinnedSource, RawSource, Source};
use mirror_devnet::spine::{
    Bundle, ClockPolicy, MinePolicy, Mirror, Policies, ProbeSchedule, ResetDetector,
};
use mirror_devnet::state::StateFile;
use mirror_anvil::Rpc;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// The venue mirror's bundle: the HL public API as the truth (two views —
/// the pin-exact hand-back source and the latest-reading follow source; the
/// pin guard would refuse the follow's reads, so the roles are two objects
/// over one handle), the pin canary, the declared scope, and the policies the
/// driver reads.
pub fn bundle(
    engine: &EngineSection,
    source: Arc<dyn Source>,
    follow: Arc<dyn Source>,
    reset: Arc<dyn ResetDetector>,
    scope: Scope,
) -> Arc<dyn Mirror> {
    Arc::new(Bundle {
        label: "v",
        source,
        follow_source: follow,
        reset,
        policies: policies(&engine.loops),
        scope,
    })
}

/// The profile's decisions: the clock is V's own, mining drains the pool (an
/// idle chain stays static), probes compare against the composed BAL, and the
/// gap guard pauses the poll when the source head runs too far ahead (a wide
/// window is a re-pin decision, never a silent partial poll).
pub fn policies(loops: &LoopsSection) -> Policies {
    Policies {
        clock: ClockPolicy::Native,
        mine: MinePolicy::PoolDrain,
        probes: ProbeSchedule::LiveParity,
        prepend_rounds: None,
        gap: Some(loops.max_gap_blocks),
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
    /// V's fork pin, shared with the spine's [`PinnedSource`].
    pub pin: Arc<AtomicU64>,
    /// The composer's target side: V itself (shared with the anvil handle).
    pub target: Arc<dyn Source>,
}

/// What the wiring needs (the context assembles it; `build` consumes it).
pub struct Deps {
    pub state: Shared<StateFile>,
    pub source_rpc: Arc<Rpc>,
    pub anvil_v: Arc<Rpc>,
    pub watch: std::collections::HashSet<alloy_primitives::Address>,
}

pub fn build(engine: &EngineSection, deps: Deps) -> Wiring {
    let Deps {
        state,
        source_rpc,
        anvil_v,
        watch,
    } = deps;
    let pin = Arc::new(AtomicU64::new(mirror_devnet::pin::PIN_UNRESOLVED));
    let source: Arc<dyn Source> = Arc::new(PinnedSource::new(
        source_rpc.clone(),
        pin.clone(),
        public_capabilities(),
    ));
    let follow: Arc<dyn Source> = Arc::new(RawSource::new(source_rpc, public_capabilities()));
    let target: Arc<dyn Source> = Arc::new(RawSource::target(anvil_v));
    let reset: Arc<dyn ResetDetector> = Arc::new(PinCanary::new(state));
    let scope = Scope::new(watch_set(watch, &engine.closures), closures(&engine.closures));
    let mirror = bundle(engine, source, follow, reset, scope);
    Wiring { mirror, pin, target }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The divergence table above as a test: the venue's decisions pinned, so
    /// a doc row and the code cannot drift apart (the mainnet's mirror test
    /// pins its own column).
    #[test]
    fn the_policies_pin_the_divergence_table() {
        let loops = LoopsSection {
            poll_interval_ms: 1000,
            max_gap_blocks: 32,
            chunk_size: 100,
        };
        let p = policies(&loops);
        assert_eq!(p.clock, ClockPolicy::Native);
        assert_eq!(p.mine, MinePolicy::PoolDrain);
        assert_eq!(p.probes, ProbeSchedule::LiveParity);
        assert_eq!(p.prepend_rounds, None);
        assert_eq!(p.gap, Some(32));
    }
}
