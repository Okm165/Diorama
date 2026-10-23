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
use mirror_anvil::Rpc;
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
    /// The shared admin gauges (shadowed slots, halted, frozen).
    pub metrics: Metrics,
    /// Set by `/admin/pause`: the profile's loops hold where they are.
    pub halted: AtomicBool,
    /// The divergence policy's switch (the ethereum profile's M chain). The
    /// venue never freezes — there is no parent stream to diverge from — so
    /// the field stays false there and `mirror_frozen` renders 0; it keeps the
    /// shared surface's exact shape.
    pub frozen: AtomicBool,
    /// Seconds the devnet's clock runs ahead of the parent block
    /// (`/admin/time`). The ethereum profile's applier aligns to
    /// `parent + offset` and clamps for monotonicity, so a jump sticks while
    /// mainnet's data keeps flowing; the venue has no parent stream and never
    /// sets it — the field keeps the shared surface's shape. Seeded from the
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
    pub reapply_from: Shared<Option<(u64, u64)>>,
}

impl Plane {
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
            state: Shared::new("cursor state", state),
            metrics: Metrics::default(),
            halted: AtomicBool::new(false),
            frozen: AtomicBool::new(false),
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

    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }

    /// The devnet admin surface (plan §11.3), wired to the profile's chain and
    /// re-sync source: the semantics live in [`crate::admin`], the wiring here.
    pub fn admin<'a>(
        &'a self,
        target: &'a Rpc,
        source: &'a dyn Source,
        chunk_size: usize,
        journal_writes: bool,
    ) -> AdminSurface<'a> {
        AdminSurface {
            plane: self,
            target,
            source,
            chunk_size,
            journal_writes,
        }
    }
}
