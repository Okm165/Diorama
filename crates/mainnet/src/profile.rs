//! The mainnet mirror's [`Bundle`] — its divergence choices, filled and
//! documented in its own folder.
//!
//! The spine ([`mirror_devnet::spine`]) declares the [`Mirror`] interface and
//! never names a mirror; this file is where the mainnet profile's choices are
//! made and documented, next to the venue's `profile.rs`:
//!
//! | decision | mainnet (here) | venue (`crates/venue/src/profile.rs`) |
//! |---|---|---|
//! | label | `"t"` | `"v"` |
//! | source | reth (local full node) | HL public API (latest reads) |
//! | BAL rung | native (`eth_getBlockAccessList`), trace fallback | synthesized (envelopes → closures → reads) |
//! | reset | cursor hash / below base | chain-instance canary |
//! | clock | align to the parent block + live offset | native |
//! | mining | one T block per applied parent block | pool drain |
//! | probes | per applied block | live parity against the composed BAL |
//! | prepend | 3 rounds (a steady stream must not starve the parent) | 1 |
//!
//! The composer itself is generic (`mirror_devnet::bal::compose`): it picks the
//! native rung from `Capabilities::native_bal` and synthesizes otherwise, so
//! the two mirrors differ in *data* (source, scope), not in algorithm.
use mirror_config::{EngineSection, LoopsSection};
use mirror_devnet::cow::{Scope, WatchSet};
use mirror_devnet::shared::Shared;
use mirror_devnet::source::{RawSource, Source};
use crate::reset::CursorHash;
use mirror_devnet::spine::{
    Bundle, ClockPolicy, MinePolicy, Mirror, Policies, ProbeSchedule, ResetDetector,
};
use mirror_devnet::state::StateFile;
use std::collections::HashSet;
use std::sync::Arc;

/// The mainnet mirror's bundle: reth as the truth (one object serves both
/// the truth and the follow reads — reth answers at any block), the
/// cursor-hash detector, the whole-account scope, and the policies the
/// applier reads.
pub fn bundle(
    engine: &EngineSection,
    source: Arc<dyn Source>,
    reset: Arc<dyn ResetDetector>,
    scope: Scope,
) -> Arc<dyn Mirror> {
    Arc::new(Bundle {
        label: "t",
        follow_source: Arc::clone(&source),
        source,
        reset,
        policies: policies(&engine.loops),
        scope,
    })
}

/// The profile's decisions, assembled from the loops section: the gap guard
/// is the config's `max_gap_blocks` (a rebased cursor gets the plan's 10,064
/// replay bound instead — the caller's branch).
pub fn policies(loops: &LoopsSection) -> Policies {
    Policies {
        clock: ClockPolicy::AlignToSource,
        mine: MinePolicy::PerAppliedBlock,
        probes: ProbeSchedule::PerBlock,
        // A tx arriving mid-apply would be mined against a state its gas was
        // never estimated on (measured: ~5% short, reverted), so the pool is
        // drained first — bounded so a steady stream cannot starve the parent
        // block.
        prepend_rounds: Some(3),
        gap: Some(loops.max_gap_blocks),
    }
}

/// The engine's build step (the profile's wiring, kept beside the bundle so
/// `ctx.rs` reads as assembly).
pub struct Wiring {
    pub mirror: Arc<dyn Mirror>,
    /// The composer's target side: T itself (the synthesis rung diffs the
    /// source's values against it; the native rung does not need it).
    pub target: Arc<dyn Source>,
}

/// The wiring's inputs — the venue profile's `Deps`' twin: one struct, not
/// five positional arguments (the plumbing reading's finding; the two
/// profiles then read the same way).
pub struct Deps {
    pub state: Shared<StateFile>,
    pub reth: Arc<mirror_anvil::Rpc>,
    pub anvil_t: Arc<mirror_anvil::Rpc>,
    pub watch: HashSet<alloy_primitives::Address>,
}

pub fn build(engine: &EngineSection, deps: Deps) -> Wiring {
    let Deps {
        state,
        reth,
        anvil_t,
        watch,
    } = deps;
    let source: Arc<dyn Source> = Arc::new(crate::source::RethSource::new(reth));
    let reset: Arc<dyn ResetDetector> = Arc::new(CursorHash::new(state));
    // The mainnet watches whole accounts: the BAL enumerates every changed
    // slot, so no closures are needed (the scope's slot set stays empty).
    let scope = Scope::new(WatchSet::new(watch), Vec::new());
    let target: Arc<dyn Source> = Arc::new(RawSource::target(anvil_t));
    Wiring {
        mirror: bundle(engine, source, reset, scope),
        target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The divergence table above as a test: the mainnet's decisions pinned,
    /// so a doc row and the code cannot drift apart (the venue's mirror test
    /// pins its own column).
    #[test]
    fn the_policies_pin_the_divergence_table() {
        let loops = LoopsSection {
            poll_interval_ms: 1000,
            max_gap_blocks: 32,
            chunk_size: 100,
        };
        let p = policies(&loops);
        assert_eq!(p.clock, ClockPolicy::AlignToSource);
        assert_eq!(p.mine, MinePolicy::PerAppliedBlock);
        assert_eq!(p.probes, ProbeSchedule::PerBlock);
        assert_eq!(p.prepend_rounds, Some(3));
        assert_eq!(p.gap, Some(32));
    }
}
