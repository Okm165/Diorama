//! `MainnetMirror` — the ethereum mirror's divergence bundle, in its own
//! folder.
//!
//! The spine ([`mirror_devnet::spine`]) declares the [`Mirror`] interface and
//! never names a mirror; this file is where the ethereum profile's choices are
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
use crate::error::Result;
use mirror_config::EngineSection;
use mirror_devnet::cow::{Scope, WatchSet};
use mirror_devnet::shared::Shared;
use mirror_devnet::source::{Capabilities, RawSource, Source};
use mirror_devnet::spine::{
    ClockPolicy, CursorHash, MinePolicy, Mirror, Policies, ProbeSchedule, ResetDetector,
};
use mirror_devnet::state::StateFile;
use std::collections::HashSet;
use std::sync::Arc;

/// The ethereum mirror's bundle: reth as the truth, the cursor-hash detector,
/// the whole-account scope, and the policies the applier reads.
pub struct MainnetMirror {
    source: Arc<dyn Source>,
    reset: Arc<dyn ResetDetector>,
    policies: Policies,
    scope: Scope,
}

impl MainnetMirror {
    /// The profile's decisions, assembled from the engine section: the gap
    /// guard is the config's `max_gap_blocks` (a rebased cursor gets the
    /// plan's 10,064 replay bound instead — the caller's branch).
    pub fn policies(engine: &EngineSection) -> Policies {
        Policies {
            clock: ClockPolicy::AlignToSource,
            mine: MinePolicy::PerAppliedBlock,
            probes: ProbeSchedule::PerBlock,
            // A tx arriving mid-apply would be mined against a state its gas
            // was never estimated on (measured: ~5% short, reverted), so the
            // pool is drained first — bounded so a steady stream cannot starve
            // the parent block.
            prepend_rounds: 3,
            gap: Some(engine.loops.max_gap_blocks),
        }
    }
}

impl Mirror for MainnetMirror {
    fn label(&self) -> &'static str {
        "t"
    }

    fn source(&self) -> &Arc<dyn Source> {
        &self.source
    }

    /// One reth serves both roles here: the state reads at a block and the
    /// composer's reads (historical reads are exact).
    fn follow_source(&self) -> &Arc<dyn Source> {
        &self.source
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

/// The engine's build step (the profile's wiring, kept beside the bundle so
/// `ctx.rs` reads as assembly).
pub struct Wiring {
    pub mirror: Arc<dyn Mirror>,
    /// The composer's target side: T itself (the synthesis rung diffs the
    /// source's values against it; the native rung does not need it).
    pub target: Arc<dyn Source>,
}

pub fn build(
    engine: &EngineSection,
    state: Shared<StateFile>,
    reth: Arc<crate::rpc::Rpc>,
    anvil_t: Arc<crate::rpc::Rpc>,
    watch: HashSet<alloy_primitives::Address>,
) -> Result<Wiring> {
    let source: Arc<dyn Source> = Arc::new(crate::source::RethSource::new(reth));
    let reset: Arc<dyn ResetDetector> = Arc::new(CursorHash::new(state));
    // The mainnet watches whole accounts: the BAL enumerates every changed
    // slot, so no closures are needed (the scope's slot set stays empty).
    let scope = Scope::new(WatchSet::new(watch), Vec::new());
    let target: Arc<dyn Source> = Arc::new(RawSource::new(anvil_t, target_capabilities()));
    Ok(Wiring {
        mirror: Arc::new(MainnetMirror {
            source,
            reset,
            policies: MainnetMirror::policies(engine),
            scope,
        }),
        target,
    })
}

/// What T (anvil) serves as the composer's target: traces yes, BAL not
/// exercised (the target is only read for the synthesis rung's diff).
fn target_capabilities() -> Capabilities {
    Capabilities {
        native_bal: false,
        traces: true,
        historical: true,
        heads_ws: false,
        system_txs: false,
        system_tx_receipts: false,
    }
}
