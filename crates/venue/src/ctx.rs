//! The venue engine's context: V's client, the devnet plane (registry,
//! journal, cursor state, admin gauges and flags) as one [`Plane`] — the same
//! bundle the mainnet engine embeds — and the venue's own metrics.
//!
//! Narrow by construction: the context carries exactly the sections the venue
//! profile reads (the run path moves them out of the instance), so a venue
//! build never sees the mainnet sections and vice versa. The divergence set
//! itself lives in the mirror bundle (`profile.rs`): the context only assembles
//! it. One client handle per endpoint (`Arc`): the context, the source views,
//! the composer's target and the driver all reference the same objects.
use crate::error::{Error, Result};
use crate::metrics::VenueMetrics;
use futures_util::future::BoxFuture;
use mirror_anvil::Rpc;
use mirror_config::{ChainVenue, EngineSection};
use mirror_devnet::drift::Drift;
use mirror_devnet::engine::Engine;
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::plane::Plane;
use mirror_devnet::probes::ProbeOutcome;
use mirror_devnet::scan::ScanWalk;
use mirror_devnet::source::Source;
use mirror_devnet::spine::Mirror;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

/// The pin handle's "not resolved yet" sentinel. A fork base is a real block
/// number and 0 is a legitimate genesis fork, so the sentinel must not be 0
/// (the canary's zero-word problem, same class — see `reset.rs`).
pub(crate) const PIN_UNRESOLVED: u64 = u64::MAX;

/// The pin handle's resolved value: `None` while [`PIN_UNRESOLVED`]. One
/// accessor, so no reader compares against the raw sentinel.
pub(crate) fn resolved_pin(pin: &AtomicU64) -> Option<u64> {
    match pin.load(std::sync::atomic::Ordering::Relaxed) {
        PIN_UNRESOLVED => None,
        p => Some(p),
    }
}

pub struct Ctx {
    /// The venue chain's section (the fork source).
    pub(crate) chain: ChainVenue,
    /// The engine's section (cadence, chunk size, probes, http) — the same
    /// `[services.engine]` section the mainnet engine carries, read for this
    /// profile's fields.
    pub(crate) engine: EngineSection,
    /// Anvil V (the devnet chain; chain id 999).
    pub(crate) anvil_v: Arc<Rpc>,
    /// The fork source V derives from (the unshadow re-sync truth at the pin).
    pub(crate) source_rpc: Arc<Rpc>,
    /// V's fork pin ([`PIN_UNRESOLVED`] until the driver's reconcile resolves
    /// it): the anchor `VenueSource` reads at (V's base state *is* the
    /// source's state at the pin) and the driver's epoch key. Shared with the
    /// source, set by the driver's reconcile.
    pub(crate) pin: Arc<AtomicU64>,
    /// The mirror bundle: the HL public API as the truth (hand-back and follow
    /// views), the pin canary, the declared scope, and the policies (see
    /// `profile.rs` — the divergence set, in the profile's own folder).
    pub(crate) mirror: Arc<dyn Mirror>,
    /// The composer's target side: V itself (shared with the anvil handle).
    pub(crate) target: Arc<dyn Source>,
    /// The devnet plane: the shadow registry, the journal, the cursor state,
    /// the shared admin gauges and flags — the same bundle the mainnet engine
    /// embeds. The venue never freezes and has no clock offset; those fields
    /// keep the shared surface's shape (documented on the plane).
    pub(crate) plane: Plane,
    /// The venue's own gauges/counters.
    pub(crate) venue_metrics: Arc<VenueMetrics>,
    /// The shared forensic log (`drift.jsonl`, one home for both mirrors):
    /// the follow's anomalies — a gap-guard pause, a source reorg, uncovered
    /// log emitters — the mainnet applier's counterpart.
    pub(crate) drift: Drift,
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
        let drift = Drift::new(engine.state_dir.join("drift.jsonl"))?;
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
            drift,
        })
    }

}

/// The shell's view of this context (`mirror_devnet::engine::Engine`): the
/// facts that differ, in one place — the chain (V), the source URL, the
/// journal semantics (the engine's own writes), the probes, the counters,
/// the status document, and the metric exposition. The admin surface, the
/// epoch re-pin, the boot asserts, the HTTP surface and the service tail are
/// the shell's.
impl Engine for Ctx {
    type Error = Error;
    type Status = crate::http::StatusResponse;

    fn plane(&self) -> &Plane {
        &self.plane
    }

    fn mirror(&self) -> &Arc<dyn Mirror> {
        &self.mirror
    }

    fn target(&self) -> &Arc<Rpc> {
        &self.anvil_v
    }

    fn source_url(&self) -> &str {
        &self.chain.source_url
    }

    fn journal_writes(&self) -> bool {
        // V's journal is exactly the engine's own writes (the deal faucet's
        // silent storage writes); the follow's source windows are never
        // journaled.
        true
    }

    fn engine(&self) -> &EngineSection {
        &self.engine
    }

    fn probes_now(&self) -> BoxFuture<'_, Result<ProbeOutcome>> {
        Box::pin(crate::driver::probes_now(self))
    }

    fn record_scan(&self, walk: &ScanWalk) {
        self.venue_metrics.record_scan(
            walk,
            &self.venue_metrics.scanned_blocks_total,
            &self.venue_metrics.skipped_empty_blocks_total,
        );
    }

    fn profile_metrics(&self) -> String {
        self.venue_metrics.render()
    }

    fn status(&self) -> BoxFuture<'_, Result<Self::Status>> {
        Box::pin(crate::http::status_doc(self))
    }
}
