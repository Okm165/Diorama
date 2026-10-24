//! Engine context: the profile's clients and config, plus the devnet plane
//! (registry, journal, cursor state, admin gauges and flags) as one
//! [`Plane`] — the same bundle the venue engine embeds.
//!
//! One Anvil (T) and one source (reth). The spine's seams are wired here,
//! chosen at boot: the source ([`RethSource`] — native BALs with the trace
//! rung), the reset detector (cursor hash), and the scope (whole accounts).
//! The composer itself is the spine's generic one (`mirror_devnet::bal`).
use crate::beacon::BeaconClient;
use crate::drift::Drift;
use crate::error::Result;
use crate::metrics::MainnetMetrics;
use mirror_anvil::Rpc;
use mirror_config::{ChainL1, EngineSection, FollowHead, Instance};
use mirror_devnet::admin::AdminSurface;
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::plane::Plane;
use mirror_devnet::source::Source;
use mirror_devnet::spine::Mirror;
use std::sync::Arc;
use std::time::Duration;

pub struct Ctx {
    /// The ethereum chain's section (the source, follow policy, fork offset).
    pub(crate) l1: ChainL1,
    /// The engine's section (the run path moved it out of the instance: the
    /// context carries exactly the sections it reads).
    pub(crate) engine: EngineSection,
    pub(crate) reth: Arc<Rpc>,
    pub(crate) anvil_t: Arc<Rpc>,
    /// The mirror bundle: reth-as-source, the cursor-hash reset detector, the
    /// whole-account scope, and the policies (see `profile.rs` — the
    /// divergence set, in the profile's own folder).
    pub(crate) mirror: Arc<dyn Mirror>,
    /// The composer's target side: T itself (shared with the anvil handle).
    pub(crate) target: Arc<dyn Source>,
    /// The beacon-API client for the top-up loop (EIP-8379): the local CL.
    pub(crate) beacon: BeaconClient,
    /// Engine API client (reth authrpc) for the top-up loop; None when disabled
    /// or unconfigured.
    pub(crate) engine_api: Option<crate::engine_api::EngineClient>,
    /// The devnet plane: the shadow registry, the journal, the cursor state,
    /// the shared admin gauges and flags — the same bundle the venue engine
    /// embeds (one home for the wiring that must not drift).
    pub(crate) plane: Plane,
    /// The profile's own set (reth/T heads, the applier's counters, the
    /// probes); rendered beside the shared admin gauges.
    pub(crate) eth_metrics: MainnetMetrics,
    /// The feed bridge's counters (shared with the bridge task; /metrics
    /// renders them whether or not the bridge is configured).
    pub(crate) feed_bridge_metrics: Arc<crate::feed_bridge::FeedBridgeMetrics>,
    pub(crate) drift: Drift,
}

impl Ctx {
    /// Build the context. Every boundary that must exist for the engine to be
    /// correct is fallible here (clients, state files, the drift log): a
    /// missing boundary is a boot error naming it, never a degraded start.
    pub fn new(mut cfg: Instance, reth_url: &str, anvil_t_url: &str) -> Result<Self> {
        let (l1, engine) = cfg.take_mainnet()?;
        let plane = Plane::from_engine(&engine)?;
        let drift = Drift::new(engine.state_dir.join("drift.jsonl"))?;
        let beacon = BeaconClient::new(engine.beacon.urls.clone())?;
        let (reth, anvil_t) = build_clients(&engine, reth_url, anvil_t_url)?;
        let wiring = crate::profile::build(
            &engine,
            plane.state.clone(),
            reth.clone(),
            anvil_t.clone(),
            cfg.watch_set(),
        );
        let engine_api = build_engine_api(&engine)?;
        Ok(Self {
            l1,
            engine,
            reth,
            anvil_t,
            mirror: wiring.mirror,
            target: wiring.target,
            beacon,
            engine_api,
            plane,
            eth_metrics: MainnetMetrics::default(),
            feed_bridge_metrics: Arc::new(crate::feed_bridge::FeedBridgeMetrics::default()),
            drift,
        })
    }

    /// The devnet admin surface, wired to this engine's chain and re-sync
    /// source (the semantics live in `mirror-devnet`; the wiring is the
    /// profile's: T's chain, reth-as-source, the configured chunk size, and a
    /// journal that holds the applier's parent before-values, not the engine's
    /// own writes).
    pub fn admin(&self) -> AdminSurface<'_> {
        self.plane.admin(
            &self.anvil_t,
            self.mirror.source().as_ref(),
            self.engine.loops.chunk_size,
            false,
        )
    }

    /// Record one shadow-scan walk in this profile's counters: the drivers'
    /// inline scans and the shared admin hand-back all report through here.
    pub(crate) fn record_scan(&self, walk: &mirror_devnet::scan::ScanWalk) {
        self.eth_metrics.record_scan(
            walk,
            &self.eth_metrics.shadow_scanned_blocks,
            &self.eth_metrics.shadow_skipped_empty_blocks,
        );
    }

    /// Re-pin T to the source's latest block (the `/admin/epoch` route's one
    /// profile-specific fact: which chain, which source URL).
    pub(crate) async fn epoch_repin(&self) -> Result<mirror_devnet::admin::EpochResponse> {
        Ok(mirror_devnet::admin::AdminSurface::epoch_repin(&self.anvil_t, &self.l1.source_url)
            .await?)
    }

    /// Run the configured probes now and report the run (the shared
    /// `/admin/probes` route; the per-block counters stay the loop's).
    pub(crate) async fn probes_now(&self) -> Result<mirror_devnet::probes::ProbeOutcome> {
        crate::driver::probes_now(self).await
    }

    /// reth follow head (safe/latest/finalized per config).
    pub async fn follow_head(&self) -> Result<Option<u64>> {
        let tag = self.l1.follow.tag();
        let block = self.reth.block_by_tag(tag, false).await?;
        Ok(block.map(|b| b.header.number))
    }

    pub fn follow_tag(&self) -> &'static str {
        match self.l1.follow {
            FollowHead::Safe => "safe",
            FollowHead::Latest => "latest",
            FollowHead::Finalized => "finalized",
        }
    }
}

/// One client per endpoint, shared (`Arc`): the context, the source objects
/// and the composer's target all reference the same handle.
fn build_clients(
    engine: &EngineSection,
    reth_url: &str,
    anvil_t_url: &str,
) -> Result<(Arc<Rpc>, Arc<Rpc>)> {
    let timeout = Duration::from_secs(engine.reth.request_timeout_secs);
    Ok((
        Arc::new(Rpc::new(reth_url, timeout)?),
        Arc::new(Rpc::new(anvil_t_url, timeout)?),
    ))
}

/// The top-up loop's authrpc client. A configured boundary that cannot be
/// built is a boot error naming itself: silently disabling it would stop the
/// top-up loop while every dashboard still says "enabled".
fn build_engine_api(engine: &EngineSection) -> Result<Option<crate::engine_api::EngineClient>> {
    match (&engine.source_sync.engine_url, &engine.source_sync.jwt_path) {
        (Some(url), Some(jwt)) if engine.source_sync.enabled => {
            Ok(Some(crate::engine_api::EngineClient::new(url, jwt)?))
        }
        _ => Ok(None),
    }
}
