//! The devnet plane's wiring, one home for both profiles: the COW registry
//! (marks + recorded values), the journal (before-values for rewind), the
//! cursor state, the shared admin gauges, and the flags the shared admin
//! surface drives (halt, freeze, clock offset, scan watermark, re-apply
//! marker).
//!
//! Each engine embeds a `Plane` in its context beside its own profile fields;
//! the operations that must not drift — persist the cursor state, persist the
//! registry, the halt check, building the admin surface — live here. The
//! profile supplies only what is genuinely its own: the admin surface's
//! `target` (its chain), its re-sync `source`, its `chunk_size`, and whether
//! its journal holds the engine's own writes.
use crate::admin::AdminSurface;
use crate::error::Result;
use crate::journal::Journal;
use crate::metrics::Metrics;
use crate::shadow::{self, Shadow};
use crate::shared::Shared;
use crate::source::Source;
use crate::state::StateFile;
use alloy_primitives::Address;
use diorama_anvil::Rpc;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct Plane {
    /// The COW registry: devnet-written storage slots and account fields.
    pub shadow: Shared<Shadow>,
    /// The registry's state file (`shadow.json` in the state directory).
    pub(crate) shadow_path: PathBuf,
    /// The engine's own writes (before-values), for `/admin/rewind`.
    pub journal: Journal,
    /// The cursor state (`cursor.json`): the per-chain cursors, `epoch_pin`,
    /// the devnet's clock offset, the learned blob form. One file, shared
    /// fields — each profile reads its own chain's cursor.
    pub state: Shared<StateFile>,
    /// The shared admin gauges (shadowed slots, halted).
    pub metrics: Metrics,
    /// The fact bus: what happened, as events (in-process broadcast; the
    /// engines' `/events` SSE endpoint serves the same notices).
    pub facts: crate::facts::FactBus,
    /// Set by `/admin/pause`: the profile's loops hold where they are.
    pub halted: AtomicBool,
    /// The gap guard's state: a persistent gap is a state, not a per-pass
    /// event — the counter, the drift row and the error fire on the edge in
    /// (measured: a 760-block gap re-logged every pass), the resume line on
    /// the edge out.
    pub gap_paused: AtomicBool,
    /// Seconds the devnet's clock runs ahead of the parent block
    /// (`/admin/time`). The mainnet profile's applier aligns to
    /// `parent + offset` and clamps for monotonicity, so a jump sticks while
    /// mainnet's data keeps flowing; the venue's follow never re-aligns V's
    /// clock, so the venue never sets it — the field keeps the shared
    /// surface's shape. Seeded from the
    /// state file: the offset is part of the devnet's state contract (T's
    /// blocks carry it), so a restart must not drop it back to 0 (measured:
    /// Aave's `lastUpdateTimestamp` lands in the future and every supply
    /// panics with an arithmetic underflow).
    pub time_offset: AtomicU64,
    /// Highest devnet-chain block whose devnet writes were scanned into the
    /// registry. Shared by the profile's scan loop and the applier's inline
    /// scan, so a block is traced once.
    pub shadow_scanned_upto: AtomicU64,
    /// Set by `/admin/rewind`: devnet-chain blocks whose writes were rolled
    /// back but whose chain head is already past them. The profile's normal
    /// late-completion skip would leave them stale, so it re-applies the range
    /// (idempotent writes; re-journaling records the rolled-back values as the
    /// new before-values). Cleared once the whole range is re-applied.
    pub reapply_from: Shared<Option<ReapplyRange>>,
}

/// The journal-rewind re-apply range: the devnet-chain blocks whose writes
/// were rolled back while their chain head is already past them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReapplyRange {
    /// The first block to re-apply.
    pub from: u64,
    /// The last block to re-apply (the marker clears once the head reaches
    /// it).
    pub upto: u64,
}

/// Whether a reset signal was witnessable (a steady signal publishes
/// nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Witnessed {
    Published,
    Steady,
}

impl Plane {
    /// Clear the journal-rewind marker once `block` is at or past the range's
    /// end; returns the cleared range so the caller logs its own wording.
    pub fn clear_reapply_from(
        &self,
        block: u64,
        reapply: Option<ReapplyRange>,
    ) -> Result<Option<ReapplyRange>> {
        if let Some(range) = reapply {
            if block >= range.upto {
                *self.reapply_from.lock()? = None;
                return Ok(Some(range));
            }
        }
        Ok(None)
    }

    /// Open the plane from a state directory: the registry (`shadow.json`),
    /// the journal, the cursor state (`cursor.json`), the admin gauges and the
    /// flags. The layout is one convention for both profiles; the fixtures are
    /// the profile's config (empty for the venue).
    pub fn open(
        state_dir: &Path,
        journal_dir: PathBuf,
        journal_blocks: u64,
        fixtures: &[Address],
    ) -> Result<Self> {
        let shadow_path = state_dir.join("shadow.json");
        let mut shadow = Shadow::load(&shadow_path)?;
        shadow.set_fixtures(fixtures.iter().copied().collect());
        let journal = Journal::new(journal_dir, journal_blocks)?;
        let state = StateFile::open(state_dir.join("cursor.json"))?;
        let offset = state.data.time_offset_seconds.unwrap_or(0);
        Ok(Self {
            shadow: Shared::new("shadow registry", shadow),
            shadow_path,
            journal,
            state: Shared::new(crate::CURSOR_ARTIFACT, state),
            metrics: Metrics::default(),
            facts: crate::facts::FactBus::default(),
            halted: AtomicBool::new(false),
            gap_paused: AtomicBool::new(false),
            time_offset: AtomicU64::new(offset),
            shadow_scanned_upto: AtomicU64::new(0),
            reapply_from: Shared::new("reapply range", None),
        })
    }

    pub fn save_state(&self) -> Result<()> {
        self.state.lock()?.save()?;
        Ok(())
    }

    pub fn save_shadow(&self) -> Result<()> {
        shadow::save_registry(&self.shadow, &self.shadow_path, &self.metrics)
    }

    /// Open the plane from the engine section's layout (state dir, journal
    /// dir/blocks, fixture addresses): the one convention both profiles use.
    pub fn from_engine(engine: &diorama_config::EngineSection) -> Result<Self> {
        Self::open(
            &engine.state_dir,
            engine.journal.dir.clone(),
            engine.journal.blocks,
            &engine.fixture_addresses,
        )
    }

    /// Publish the reset fact for a witnessable signal (a steady signal
    /// publishes nothing): the witness mapping is the spine's, the fact shape
    /// here.
    pub fn witness_reset(
        &self,
        chain: &'static str,
        signal: crate::spine::ResetSignal,
    ) -> Witnessed {
        let Some(witness) = crate::spine::witness_for(signal) else {
            return Witnessed::Steady;
        };
        self.facts
            .publish(crate::facts::Fact::ResetDetected { chain, witness });
        Witnessed::Published
    }

    /// Publish the epoch-change fact: the pin the rule moved to (`None` =
    /// unpinned).
    pub fn publish_epoch_changed(&self, chain: &'static str, pin: Option<u64>) {
        self.facts
            .publish(crate::facts::Fact::EpochChanged { chain, pin });
    }

    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }

    /// `true` only on the edge into the gap pause (the record fires once per
    /// pause event, never once per pass).
    pub fn gap_pause_started(&self) -> bool {
        !self.gap_paused.swap(true, Ordering::Relaxed)
    }

    /// `true` only on the edge out of the gap pause (the resume line).
    pub fn gap_pause_cleared(&self) -> bool {
        self.gap_paused.swap(false, Ordering::Relaxed)
    }

    /// The devnet admin surface, wired to the profile's chain and
    /// re-sync source: the semantics live in [`crate::admin`], the wiring here.
    pub fn admin<'a>(
        &'a self,
        target: &'a Rpc,
        source: &'a dyn Source,
        chunk_size: usize,
        journal_scope: crate::engine::JournalScope,
    ) -> AdminSurface<'a> {
        AdminSurface {
            plane: self,
            target,
            source,
            chunk_size,
            journal_scope,
        }
    }
}
