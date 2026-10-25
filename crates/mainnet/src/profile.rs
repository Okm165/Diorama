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
use mirror_devnet::cow::Scope;
use mirror_devnet::source::Source;
use crate::reset::CursorHash;
use mirror_devnet::engine::JournalScope;
use mirror_devnet::spine::{
    Bundle, ClockPolicy, MinePolicy, Mirror, Policies, ProbeSchedule, ResetDetector,
};
use std::sync::Arc;

/// The mainnet mirror's bundle: reth as the truth (one object serves both
/// the truth and the follow reads — reth answers at any block), the
/// cursor-hash detector, the whole-account scope, and the policies the
/// applier reads.
pub fn bundle(
    engine: &EngineSection,
    source: Arc<dyn Source>,
    follow: Arc<dyn Source>,
    reset: Arc<dyn ResetDetector>,
    scope: Scope,
) -> Arc<dyn Mirror> {
    Arc::new(Bundle {
        label: "t",
        source,
        follow_source: follow,
        reset,
        policies: policies(&engine.loops),
        // T's journal holds the applier's parent before-values, not the
        // engine's own writes: a rewind restores them and the applier
        // re-derives.
        journal_scope: JournalScope::ParentBeforeValues,
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

/// The wiring's inputs: the spine's one `ProfileDeps` (the venue profile's
/// `Deps` is the same struct — the roles, not the chains, name the fields).
pub use mirror_devnet::spine::ProfileDeps as Deps;

pub fn build(engine: &EngineSection, deps: Deps) -> Wiring {
    let Deps {
        state,
        source_rpc,
        target_rpc,
        watch,
    } = deps;
    let source: Arc<dyn Source> = crate::source::reth(source_rpc);
    let reset: Arc<dyn ResetDetector> = Arc::new(CursorHash::new(state));
    // The mainnet watches whole accounts: the BAL enumerates every changed
    // slot, so no closures are needed (the scope's slot set stays empty).
    let scope = Scope::from_config(watch, &engine.closures);
    let target: Arc<dyn Source> = mirror_devnet::source::target(target_rpc);
    Wiring {
        mirror: bundle(engine, Arc::clone(&source), source, reset, scope),
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

    /// The bundle's own divergences, pinned: the label, the journal scope
    /// (T's parent before-values — the rewind's semantics), and the watch
    /// scope. The engine section is the deployed instance's, so a config
    /// drift fails here too (the venue's test pins its column the same way).
    #[test]
    fn the_bundle_pins_the_mainnet_divergences() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/instances/mainnet-mirror/instance.toml"
        ));
        let (mut inst, _) = mirror_config::Instance::inspect(path).unwrap();
        let (_, engine) = inst.take_mainnet().unwrap();
        let rpc = Arc::new(
            mirror_anvil::Rpc::new("http://127.0.0.1:1", std::time::Duration::from_secs(1))
                .unwrap(),
        );
        let source: Arc<dyn Source> = mirror_devnet::source::target(rpc);
        let watched = alloy_primitives::Address::repeat_byte(0xaa);
        let scope =
            Scope::from_config(std::collections::HashSet::from([watched]), &engine.closures);
        let mirror = bundle(&engine, Arc::clone(&source), source, Arc::new(mirror_devnet::spine::NoReset), scope);
        assert_eq!(mirror.label(), "t");
        assert_eq!(
            mirror.journal_scope(),
            mirror_devnet::engine::JournalScope::ParentBeforeValues
        );
        assert!(mirror.scope().watch().accounts.contains(&watched));
    }
}
