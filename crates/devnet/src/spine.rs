//! The spine's divergence seams: the decisions that may differ per mirror,
//! each an interface the shared pipeline consumes — so the pipeline itself has
//! none.
//!
//! 1. the truth followed ([`crate::source::Source`]: reth | the HL public API);
//! 2. the BAL rung ([`crate::bal::compose`]: native | synthesized — the
//!    generic composer selects it from the source's declared capabilities);
//! 3. the reset signal ([`ResetDetector`]);
//! 4. the clock rule ([`ClockPolicy`]);
//! 5. block production ([`MinePolicy`]);
//! 6. probe cadence ([`ProbeSchedule`]).
//!
//! [`Mirror`] bundles them: one object per mirror, implemented in that
//! mirror's own folder (`crates/mainnet`, `crates/venue`), holding the
//! profile's wiring and reading its label, sources, reset detector, policies
//! and scope. COW ([`crate::cow`]) and everything after are shared.
use crate::cow::Scope;
use crate::error::Result;
use crate::source::Source;
use futures_util::future::BoxFuture;
use mirror_anvil::Rpc;
use std::sync::Arc;

/// One mirror's divergence bundle — the single object the engines read
/// everything profile-specific through. The implementations live in the
/// mirror folders; this crate never names one.
pub trait Mirror: Send + Sync {
    /// The chain tag facts and drift entries carry ("t" | "v").
    fn label(&self) -> &'static str;
    /// The truth for state reads (re-sync/hand-back): reth at a block | the
    /// fork source at the pin.
    fn source(&self) -> &Arc<dyn Source>;
    /// The composer's read side: the same object as [`Self::source`] unless a
    /// mirror's follow reads differ from its truth reads — the venue's
    /// latest-reading source, whose pin guard would refuse the follow's reads
    /// (reth serves both roles on the mainnet, so the default is exact there).
    fn follow_source(&self) -> &Arc<dyn Source> {
        self.source()
    }
    /// The reset seam: how a chain reset is witnessed.
    fn reset(&self) -> &Arc<dyn ResetDetector>;
    /// The profile's decisions, as data.
    fn policies(&self) -> Policies;
    /// What the mirror keeps fresh: the watch set and declared closures the
    /// composer plans from and the COW filter keeps.
    fn scope(&self) -> &Scope;
}

/// The divergence bundle — the spine's own struct, one per mirror. The
/// profiles *fill* it (their choices are data, which is the whole point of
/// the seam list above), so [`Mirror`] has exactly one implementation and the
/// profiles carry no accessor bodies.
pub struct Bundle {
    pub label: &'static str,
    pub source: Arc<dyn Source>,
    /// The composer's read side: the same object as [`Self::source`] unless
    /// the mirror's follow reads differ (the venue's latest-reading source,
    /// whose pin guard would refuse the follow's reads). The profiles pass a
    /// clone when one object serves both roles.
    pub follow_source: Arc<dyn Source>,
    pub reset: Arc<dyn ResetDetector>,
    pub policies: Policies,
    pub scope: Scope,
}

impl Mirror for Bundle {
    fn label(&self) -> &'static str {
        self.label
    }

    fn source(&self) -> &Arc<dyn Source> {
        &self.source
    }

    fn follow_source(&self) -> &Arc<dyn Source> {
        &self.follow_source
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

/// What a reset check saw — the witness, not just a boolean: the evidence
/// decides what an operator reads and what the fact stream reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetSignal {
    /// The cursor still belongs to this chain instance.
    Steady,
    /// The cursor's position is stale: it sits below the chain's fork base
    /// (a re-pin moved the base past it).
    BaseMoved { base: Option<u64>, cursor: u64 },
    /// The cursor's block no longer carries its recorded hash.
    HashMismatch {
        block: u64,
        expected: alloy_primitives::B256,
        found: Option<alloy_primitives::B256>,
    },
    /// The recorded chain instance's canary word is gone or foreign (the
    /// venue's witness: heights below a fork's pin resolve to the source, so
    /// cursor hashes are unsound there — ADR 0002).
    PinMismatch {
        expected: Option<u64>,
        found: Option<u64>,
    },
}

/// How a chain reset is detected. The detector is self-sufficient: it holds
/// the state whose expectation it validates (the cursor or the recorded
/// instance), reads the chain, and answers with the witness. Each mirror's
/// detector lives in its own folder — `crates/mainnet/src/reset.rs` (the
/// cursor hash; a cursor below the fork base is stale by position) and
/// `crates/venue/src/reset.rs` (the pin canary: the recorded instance's
/// storage word must read back its pin).
pub trait ResetDetector: Send + Sync {
    fn detect<'a>(&'a self, target: &'a Rpc) -> BoxFuture<'a, Result<ResetSignal>>;
}

/// The clock rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockPolicy {
    /// The applied block's context is the source's: timestamp + the plane's
    /// live offset, clamped to last+1 so a deliberate jump can never go
    /// non-monotonic. (The offset is runtime state — `/admin/time` — so it is
    /// read from the plane, not declared here.)
    AlignToSource,
    /// The target's own clock (the venue; nothing re-aligns it).
    Native,
}

/// Who produces blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinePolicy {
    /// One target block per applied source block (T: the parent stream is the
    /// devnet's block cadence).
    PerAppliedBlock,
    /// Mine only when the devnet's own pool is non-empty (V: an idle chain
    /// stays static; the harness's mines are extra empty blocks).
    PoolDrain,
}

/// When probes run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeSchedule {
    /// After every apply (T: reth can serve the applied block).
    PerBlock,
    /// After every composed window, against the composed BAL (V live: the
    /// values the composition expected are the expected side).
    LiveParity,
}

/// The profile's decisions, as data — the only per-profile behavior.
#[derive(Debug, Clone, Copy)]
pub struct Policies {
    pub clock: ClockPolicy,
    pub mine: MinePolicy,
    pub probes: ProbeSchedule,
    /// Rounds of prepend (devnet txs mined before the source's writes land):
    /// `Some(3)` on T (a steady stream must not starve the parent block);
    /// `None` on V, which never prepends — mining drains the pool every pass
    /// (`MinePolicy::PoolDrain`).
    pub prepend_rounds: Option<u8>,
    /// Pause when the follow head runs this far ahead; `None` = no guard.
    pub gap: Option<u64>,
}


