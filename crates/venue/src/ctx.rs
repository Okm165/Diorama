//! The venue engine's context: V's client, the devnet plane (registry,
//! journal, cursor state, admin gauges and flags) as one [`Plane`] — the same
//! bundle the ethereum engine embeds — and the venue's own metrics.
//!
//! Narrow by construction: the context carries exactly the sections the venue
//! profile reads (the run path moves them out of the instance), so a venue
//! build never sees the ethereum sections and vice versa.
use crate::error::Result;
use crate::metrics::VenueMetrics;
use mirror_config::{ChainVenue, EngineSection};
use mirror_devnet::admin::AdminSurface;
use mirror_devnet::plane::Plane;
use mirror_devnet::source::Source;
use mirror_devnet::state::ChainCursor;
use mirror_anvil::Rpc;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

pub struct Ctx {
    /// The venue chain's section (the fork source).
    pub(crate) chain: ChainVenue,
    /// The engine's section (cadence, chunk size, probes, http) — the same
    /// `[services.engine]` section the ethereum engine carries, read for this
    /// profile's fields.
    pub(crate) engine: EngineSection,
    /// Anvil V (the devnet chain; chain id 999).
    pub(crate) v: Rpc,
    /// The fork source V derives from (the unshadow re-sync truth at the pin).
    pub(crate) source_rpc: Rpc,
    /// V's fork pin (0 = not yet resolved): the anchor `VenueSource` reads at
    /// (V's base state *is* the source's state at the pin) and the driver's
    /// epoch key. Shared with the source, set by the driver's reconcile.
    pub(crate) pin: Arc<AtomicU64>,
    /// The unshadow re-sync source: `VenueSource` reads the fork source at the
    /// pin (one implementation per profile, chosen at boot — rule 15).
    pub(crate) source: Arc<dyn Source>,
    /// The devnet plane: the shadow registry, the journal, the cursor state,
    /// the shared admin gauges and flags — the same bundle the ethereum engine
    /// embeds. The venue never freezes and has no clock offset; those fields
    /// keep the shared surface's shape (documented on the plane).
    pub(crate) plane: Plane,
    /// The venue's own gauges/counters.
    pub(crate) venue_metrics: VenueMetrics,
}

impl Ctx {
    /// Build the context. Every boundary that must exist for the driver to be
    /// correct is fallible here (clients, state files): a missing boundary is
    /// a boot error naming it, never a degraded start.
    pub fn new(mut cfg: mirror_config::Instance, source_url: &str, v_url: &str) -> Result<Self> {
        let (chain, engine) = cfg.take_venue()?;
        // A fixed, generous per-call budget: V's chain is a local anvil and the
        // source is one HTTP client. The mainnet profile's timeout is
        // configurable because its reth queries race the sync; no measured need
        // justifies a second knob here.
        let timeout = Duration::from_secs(30);
        let plane = Plane::open(
            &engine.state_dir,
            engine.journal.dir.clone(),
            engine.journal.blocks,
            &engine.fixture_addresses,
        )?;
        let source_rpc = Rpc::new(source_url, timeout)?;
        let pin = Arc::new(AtomicU64::new(0));
        Ok(Self {
            chain,
            engine,
            v: Rpc::new(v_url, timeout)?,
            source: Arc::new(crate::venue_source::VenueSource::new(
                source_rpc.clone(),
                pin.clone(),
            )),
            source_rpc,
            pin,
            plane,
            venue_metrics: VenueMetrics::default(),
        })
    }

    /// The devnet admin surface, wired to this engine's chain and re-sync
    /// source (the semantics live in `mirror-devnet`; the wiring is the
    /// profile's: V's chain, the fork source, the configured chunk size, and a
    /// journal that is exactly the engine's own writes).
    pub fn admin(&self) -> AdminSurface<'_> {
        self.plane
            .admin(&self.v, self.source.as_ref(), self.engine.loops.chunk_size, true)
    }

    pub fn cursor_v(&self) -> Result<Option<ChainCursor>> {
        Ok(self.plane.state.lock()?.data.t.clone())
    }

    pub fn set_cursor_v(&self, cursor: ChainCursor) -> Result<()> {
        self.plane.state.lock()?.data.t = Some(cursor);
        Ok(())
    }
}
