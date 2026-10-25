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
use mirror_config::{EngineSection, LoopsSection};
use mirror_devnet::cow::Scope;
use mirror_devnet::source::{RawSource, Source};
use mirror_devnet::engine::JournalScope;
use mirror_devnet::spine::{
    Bundle, ClockPolicy, MinePolicy, Mirror, Policies, ProbeSchedule, ResetDetector,
};
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
        // V's journal holds the engine's *own* writes (the deal faucet's
        // silent storage writes), keyed by the scan cursor.
        journal_scope: JournalScope::OwnWrites,
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

/// The engine's build step (the profile's wiring, kept beside the bundle so
/// `ctx.rs` reads as assembly).
pub struct Wiring {
    pub mirror: Arc<dyn Mirror>,
    /// V's fork pin, shared with the spine's [`PinnedSource`].
    pub pin: Arc<AtomicU64>,
    /// The composer's target side: V itself (shared with the anvil handle).
    pub target: Arc<dyn Source>,
}

/// What the wiring needs: the spine's one `ProfileDeps` (the mainnet
/// profile's `Deps` is the same struct — the roles, not the chains, name the
/// fields).
pub use mirror_devnet::spine::ProfileDeps as Deps;

pub fn build(engine: &EngineSection, deps: Deps) -> Wiring {
    let Deps {
        state,
        source_rpc,
        target_rpc,
        watch,
    } = deps;
    let pin = Arc::new(AtomicU64::new(mirror_devnet::pin::PIN_UNRESOLVED));
    let source: Arc<dyn Source> = crate::source::pinned(source_rpc.clone(), pin.clone());
    let follow: Arc<dyn Source> = crate::source::follow(source_rpc);
    let target: Arc<dyn Source> = Arc::new(RawSource::target(target_rpc));
    let reset: Arc<dyn ResetDetector> = Arc::new(PinCanary::new(state));
    let scope = Scope::from_config(watch, &engine.closures);
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
