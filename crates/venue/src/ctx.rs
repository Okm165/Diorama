//! The venue engine's context: V's client, the devnet plane (registry,
//! journal, cursor state, admin gauges and flags) as one [`Plane`] — the same
//! bundle the ethereum engine embeds — and the venue's own metrics.
//!
//! Narrow by construction: the context carries exactly the sections the venue
//! profile reads (the run path moves them out of the instance), so a venue
//! build never sees the ethereum sections and vice versa. The divergence set
//! itself lives in the mirror bundle (`profile.rs`): the context only assembles
//! it. One client handle per endpoint (`Arc`): the context, the source views,
//! the composer's target and the driver all reference the same objects.
use crate::error::Result;
use crate::metrics::VenueMetrics;
use mirror_config::{ChainVenue, EngineSection};
use mirror_devnet::admin::AdminSurface;
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::plane::Plane;
use mirror_devnet::source::Source;
use mirror_devnet::spine::Mirror;
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
    pub(crate) anvil_v: Arc<Rpc>,
    /// The fork source V derives from (the unshadow re-sync truth at the pin).
    pub(crate) source_rpc: Arc<Rpc>,
    /// V's fork pin (0 = not yet resolved): the anchor `VenueSource` reads at
    /// (V's base state *is* the source's state at the pin) and the driver's
    /// epoch key. Shared with the source, set by the driver's reconcile.
    pub(crate) pin: Arc<AtomicU64>,
    /// The mirror bundle: the HL public API as the truth (hand-back and follow
    /// views), the pin canary, the declared scope, and the policies (see
    /// `profile.rs` — the divergence set, in the profile's own folder).
    pub(crate) mirror: Arc<dyn Mirror>,
    /// The composer's target side: V itself (shared with the anvil handle).
    pub(crate) target: Arc<dyn Source>,
    /// The devnet plane: the shadow registry, the journal, the cursor state,
    /// the shared admin gauges and flags — the same bundle the ethereum engine
    /// embeds. The venue never freezes and has no clock offset; those fields
    /// keep the shared surface's shape (documented on the plane).
    pub(crate) plane: Plane,
    /// The venue's own gauges/counters.
    pub(crate) venue_metrics: Arc<VenueMetrics>,
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
        let plane = Plane::from_engine(&engine)?;
        let venue_metrics = Arc::new(VenueMetrics::default());
        let source_rpc = Arc::new(Rpc::new(source_url, timeout)?);
        let anvil_v = Arc::new(Rpc::new(v_url, timeout)?);
        let wiring = crate::profile::build(
            &engine,
            crate::profile::Deps {
                state: plane.state.clone(),
                source_rpc: source_rpc.clone(),
                anvil_v: anvil_v.clone(),
                watch: cfg.watch_set(),
            },
        );
        Ok(Self {
            chain,
            engine,
            anvil_v,
            source_rpc,
            pin: wiring.pin,
            mirror: wiring.mirror,
            target: wiring.target,
            plane,
            venue_metrics,
        })
    }

    /// The devnet admin surface, wired to this engine's chain and re-sync
    /// source (the semantics live in `mirror-devnet`; the wiring is the
    /// profile's: V's chain, the fork source, the configured chunk size, and a
    /// journal that is exactly the engine's own writes).
    pub fn admin(&self) -> AdminSurface<'_> {
        self.plane.admin(
            &self.anvil_v,
            self.mirror.source().as_ref(),
            self.engine.loops.chunk_size,
            true,
        )
    }

    /// Record one shadow-scan walk in this profile's counters: the driver's
    /// scans and the shared admin hand-back all report through here.
    pub(crate) fn record_scan(&self, walk: &mirror_devnet::scan::ScanWalk) {
        self.venue_metrics.record_scan(
            walk,
            &self.venue_metrics.scanned_blocks_total,
            &self.venue_metrics.skipped_empty_blocks_total,
        );
    }

    /// Re-pin V to the fork source's latest block (the `/admin/epoch` route's
    /// one profile-specific fact: which chain, which source URL).
    pub(crate) async fn epoch_repin(&self) -> Result<mirror_devnet::admin::EpochResponse> {
        Ok(mirror_devnet::admin::AdminSurface::epoch_repin(&self.anvil_v, &self.chain.source_url).await?)
    }

    /// Run the at-pin probes now and report the run (the shared
    /// `/admin/probes` route; the cumulative counters live in `/metrics`).
    pub(crate) async fn probes_now(&self) -> Result<mirror_devnet::probes::ProbeOutcome> {
        crate::driver::probes_now(self).await
    }

}
