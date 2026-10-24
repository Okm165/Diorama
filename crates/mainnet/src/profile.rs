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
//! | source | reth (local full node) | HL public API (poll) |
//! | composer | per-block BAL (trace fallback) | poll window over envelopes |
//! | reset | cursor hash / below base | chain-instance canary |
//! | clock | align to the parent block + live offset | native |
//! | mining | one T block per applied parent block | pool drain |
//! | probes | per applied block | live parity against the polled values |
//! | prepend | 3 rounds (a steady stream must not starve the parent) | 1 |
use crate::error::Result;
use mirror_config::EngineSection;
use mirror_devnet::diff::DiffComposer;
use mirror_devnet::source::Source;
use mirror_devnet::spine::{
    ClockPolicy, MinePolicy, Mirror, Policies, ProbeSchedule, ResetDetector,
};
use std::sync::Arc;

/// The ethereum mirror's bundle: reth as the truth, the BAL composer, the
/// cursor-hash detector, and the policies the applier reads.
pub struct MainnetMirror {
    source: Arc<dyn Source>,
    composer: Arc<dyn DiffComposer>,
    reset: Arc<dyn ResetDetector>,
    policies: Policies,
}

impl MainnetMirror {
    pub fn new(
        source: Arc<dyn Source>,
        composer: Arc<dyn DiffComposer>,
        reset: Arc<dyn ResetDetector>,
        policies: Policies,
    ) -> Self {
        Self {
            source,
            composer,
            reset,
            policies,
        }
    }

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

    fn composer(&self) -> &Arc<dyn DiffComposer> {
        &self.composer
    }

    fn reset(&self) -> &Arc<dyn ResetDetector> {
        &self.reset
    }

    fn policies(&self) -> Policies {
        self.policies
    }
}

/// The engine's build step (the profile's wiring, kept beside the bundle so
/// `ctx.rs` reads as assembly).
pub fn build(
    engine: &EngineSection,
    state: mirror_devnet::shared::Shared<mirror_devnet::state::StateFile>,
    reth: &crate::rpc::Rpc,
) -> Result<Arc<dyn Mirror>> {
    let source: Arc<dyn Source> = Arc::new(crate::source::RethSource::new(reth.clone()));
    let composer: Arc<dyn DiffComposer> =
        Arc::new(crate::compose::RethComposer::new(reth.clone()));
    let reset: Arc<dyn ResetDetector> = Arc::new(mirror_devnet::spine::CursorHash::new(state));
    Ok(Arc::new(MainnetMirror::new(
        source,
        composer,
        reset,
        MainnetMirror::policies(engine),
    )))
}
