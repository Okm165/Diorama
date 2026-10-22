//! The venue engine's context: V's client, the devnet plane's registries, the
//! venue's own metrics.
//!
//! Narrow by construction: the context carries exactly the sections the venue
//! profile reads (the run path moves them out of the instance), so a venue
//! build never sees the ethereum sections and vice versa.
use crate::error::Result;
use mirror_config::{ChainVenue, VenueSection};
use mirror_devnet::admin::AdminSurface;
use mirror_devnet::journal::Journal;
use mirror_devnet::metrics::{push_metric, Metrics};
use mirror_devnet::shadow::Shadow;
use mirror_devnet::source::Source;
use mirror_devnet::state::{ChainCursor, StateFile};
use mirror_devnet::Shared;
use mirror_anvil::Rpc;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The venue's own metric set (plan §7.8): the substrate gauges the shared
/// `Metrics` owns are rendered beside these (`render_admin`).
#[derive(Default)]
pub struct VenueMetrics {
    /// V's head.
    pub head: AtomicU64,
    /// The last scanned V block.
    pub cursor: AtomicU64,
    /// V's fork base (the pin).
    pub fork_pin: AtomicU64,
    /// Blocks since the first scanned block in this epoch.
    pub pin_age_blocks: AtomicU64,
    /// Times V was found re-forked under the driver (a re-pin, an OOM): the
    /// epoch rule ran (clear or restore).
    pub epochs_total: AtomicU64,
    /// Writes re-applied by a same-pin restore.
    pub restore_writes_total: AtomicU64,
    pub probe_ok: AtomicU64,
    pub probe_failed: AtomicU64,
    pub probe_skipped: AtomicU64,
    /// Probes that diverged in the last at-pin run.
    pub probe_last_failed: AtomicU64,
}

impl VenueMetrics {
    pub fn set(&self, field: &AtomicU64, v: u64) {
        field.store(v, Ordering::Relaxed);
    }

    pub fn add(&self, field: &AtomicU64, v: u64) {
        field.fetch_add(v, Ordering::Relaxed);
    }

    pub fn inc(&self, field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }

    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        let g = |out: &mut String, name: &str, help: &str, v: u64| push_metric(out, "gauge", name, help, v);
        let c = |out: &mut String, name: &str, help: &str, v: u64| push_metric(out, "counter", name, help, v);
        g(&mut out, "venue_head", "anvil-v head", self.head.load(Ordering::Relaxed));
        g(&mut out, "venue_cursor", "last scanned V block", self.cursor.load(Ordering::Relaxed));
        g(&mut out, "venue_fork_pin", "V's fork base (the pin)", self.fork_pin.load(Ordering::Relaxed));
        g(&mut out, "venue_pin_age_blocks", "blocks since the first scanned block in this epoch", self.pin_age_blocks.load(Ordering::Relaxed));
        c(&mut out, "venue_epochs_total", "times V was found re-forked under the driver (epoch rule ran)", self.epochs_total.load(Ordering::Relaxed));
        c(&mut out, "venue_restore_writes_total", "writes re-applied by same-pin restores", self.restore_writes_total.load(Ordering::Relaxed));
        c(&mut out, "venue_probe_ok_total", "at-pin probes matching the fork source", self.probe_ok.load(Ordering::Relaxed));
        c(&mut out, "venue_probe_failures_total", "at-pin probes diverging from the fork source (devnet-owned skipped)", self.probe_failed.load(Ordering::Relaxed));
        c(&mut out, "venue_probe_skipped_total", "probes skipped because their target is devnet-owned", self.probe_skipped.load(Ordering::Relaxed));
        g(&mut out, "venue_probe_last_failed", "probes that diverged in the last at-pin run", self.probe_last_failed.load(Ordering::Relaxed));
        out
    }
}

pub struct Ctx {
    /// The venue chain's section (the fork source).
    pub chain: ChainVenue,
    /// The venue service's section (cadence, chunk size, probes, http).
    pub venue: VenueSection,
    /// Anvil V (the devnet chain; chain id 999).
    pub v: Rpc,
    /// The fork source V derives from (the unshadow re-sync truth at the pin).
    pub source_rpc: Rpc,
    /// V's fork pin (0 = not yet resolved): the anchor `VenueSource` reads at
    /// (V's base state *is* the source's state at the pin) and the driver's
    /// epoch key. Shared with the source, set by the driver's reconcile.
    pub pin: Arc<AtomicU64>,
    /// The unshadow re-sync source: `VenueSource` reads the fork source at the
    /// pin (one implementation per profile, chosen at boot — rule 15).
    pub source: Arc<dyn Source>,
    pub shadow: Shared<Shadow>,
    pub shadow_path: PathBuf,
    pub journal: Journal,
    pub state: Shared<StateFile>,
    /// The shared admin-surface metrics (shadowed_slots/halted/frozen).
    pub metrics: Metrics,
    /// The venue's own gauges/counters.
    pub venue_metrics: VenueMetrics,
    /// Highest V block whose devnet writes have been scanned into the registry.
    pub shadow_scanned_upto: AtomicU64,
    /// Set by `/admin/pause`: the driver holds where it is.
    pub halted: AtomicBool,
    /// The venue has no persistent clock offset (nothing re-aligns the clock);
    /// the field keeps the shared admin surface's exact shape.
    pub time_offset: AtomicU64,
    /// The venue never freezes on divergence (there is no parent stream to
    /// diverge from); the field keeps the shared surface's shape.
    pub frozen: AtomicBool,
    /// Set by `/admin/rewind`: V blocks whose writes were rolled back but whose
    /// chain head is already past them (the driver re-scans them so the marks
    /// are re-established).
    pub reapply_from: Shared<Option<(u64, u64)>>,
}

impl Ctx {
    /// Build the context. Every boundary that must exist for the driver to be
    /// correct is fallible here (clients, state files): a missing boundary is
    /// a boot error naming it, never a degraded start.
    pub fn new(mut cfg: mirror_config::Instance, source_url: &str, v_url: &str) -> Result<Self> {
        let (chain, venue) = cfg.take_venue()?;
        let timeout = Duration::from_secs(30);
        let shadow_path = venue.state_dir.join("shadow.json");
        let shadow = Shadow::load(&shadow_path)?;
        let journal = Journal::new(venue.state_dir.join("journal"), venue.journal_blocks)?;
        let state = StateFile::open(venue.state_dir.join("cursor.json"))?;
        let source_rpc = Rpc::new(source_url, timeout)?;
        let pin = Arc::new(AtomicU64::new(0));
        Ok(Self {
            chain,
            venue,
            v: Rpc::new(v_url, timeout)?,
            source: Arc::new(crate::venue_source::VenueSource::new(
                source_rpc.clone(),
                pin.clone(),
            )),
            source_rpc,
            pin,
            shadow: Shared::new("shadow registry", shadow),
            shadow_path,
            journal,
            state: Shared::new("cursor state", state),
            metrics: Metrics::default(),
            venue_metrics: VenueMetrics::default(),
            shadow_scanned_upto: AtomicU64::new(0),
            halted: AtomicBool::new(false),
            time_offset: AtomicU64::new(0),
            frozen: AtomicBool::new(false),
            reapply_from: Shared::new("reapply range", None),
        })
    }

    /// The admin surface, wired per request from the context (references only).
    pub fn admin(&self) -> AdminSurface<'_> {
        AdminSurface {
            target: &self.v,
            source: self.source.as_ref(),
            shadow: &self.shadow,
            shadow_path: &self.shadow_path,
            journal: &self.journal,
            state: &self.state,
            metrics: &self.metrics,
            time_offset: &self.time_offset,
            halted: &self.halted,
            frozen: &self.frozen,
            reapply_from: &self.reapply_from,
            chunk_size: self.venue.chunk_size,
        }
    }

    pub fn cursor_v(&self) -> Result<Option<ChainCursor>> {
        Ok(self.state.lock()?.data.t.clone())
    }

    pub fn set_cursor_v(&self, cursor: ChainCursor) -> Result<()> {
        self.state.lock()?.data.t = Some(cursor);
        Ok(())
    }

    pub fn save_state(&self) -> Result<()> {
        self.state.lock()?.save()?;
        Ok(())
    }

    pub fn save_shadow(&self) -> Result<()> {
        mirror_devnet::shadow::save_registry(&self.shadow, &self.shadow_path, &self.metrics)?;
        Ok(())
    }

    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }
}
