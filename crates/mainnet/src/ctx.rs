//! The mainnet engine's context: reth's and T's clients, the devnet plane
//! (registry, journal, cursor state, admin gauges and flags) as one [`Plane`]
//! — the same bundle the venue engine embeds — and the mainnet's own metrics.
//!
//! One Anvil (T) and one source (reth). The spine's seams are wired here,
//! chosen at boot: the source ([`RethSource`] — native BALs with the trace
//! rung), the reset detector (cursor hash), and the scope (whole accounts).
//! The composer itself is the spine's generic one (`mirror_devnet::bal`).
use crate::source_sync::beacon::BeaconClient;
use crate::error::{Error, Result};
use crate::metrics::MainnetMetrics;
use futures_util::future::BoxFuture;
use mirror_anvil::Rpc;
use mirror_config::{ChainL1, EngineSection, Instance};
use mirror_devnet::drift::Drift;
use mirror_devnet::engine::{Engine, EngineCore};
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::plane::Plane;
use mirror_devnet::probes::ProbeOutcome;
use mirror_devnet::scan::ScanWalk;
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
    pub(crate) anvil_t: Arc<Rpc>,
    pub(crate) reth: Arc<Rpc>,
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
    pub(crate) engine_api: Option<crate::source_sync::engine_api::EngineClient>,
    /// The devnet plane: the shadow registry, the journal, the cursor state,
    /// the shared admin gauges and flags — the same bundle the venue engine
    /// embeds (one home for the wiring that must not drift).
    pub(crate) plane: Plane,
    /// The profile's own set (reth/T heads, the applier's counters, the
    /// probes); rendered beside the shared admin gauges.
    pub(crate) mainnet_metrics: MainnetMetrics,
    /// The feed bridge's counters (shared with the bridge task; /metrics
    /// renders them whether or not the bridge is configured).
    pub(crate) feed_bridge_metrics: Arc<crate::feed::FeedBridgeMetrics>,
    pub(crate) drift: Drift,
}

impl Ctx {
    /// Build the context. Every boundary that must exist for the engine to be
    /// correct is fallible here (clients, state files, the drift log): a
    /// missing boundary is a boot error naming it, never a degraded start.
    pub fn new(mut cfg: Instance, reth_url: &str, anvil_t_url: &str) -> Result<Self> {
        let (l1, engine) = cfg.take_mainnet()?;
        let plane = Plane::from_engine(&engine)?;
        let drift = Drift::from_engine(&engine)?;
        // The timeout is the config's: reth queries race the sync.
        let (reth, anvil_t) = mirror_devnet::engine::build_clients(
            reth_url,
            anvil_t_url,
            Duration::from_secs(engine.reth.request_timeout_secs),
        )?;
        let wiring = crate::profile::build(
            &engine,
            crate::profile::Deps {
                state: plane.state.clone(),
                source_rpc: reth.clone(),
                target_rpc: anvil_t.clone(),
                watch: cfg.watch_set(),
            },
        );
        let beacon = BeaconClient::new(engine.beacon.urls.clone())?;
        let engine_api = build_engine_api(&engine)?;
        Ok(Self {
            l1,
            engine,
            anvil_t,
            reth,
            mirror: wiring.mirror,
            target: wiring.target,
            beacon,
            engine_api,
            plane,
            mainnet_metrics: MainnetMetrics::default(),
            feed_bridge_metrics: Arc::new(crate::feed::FeedBridgeMetrics::default()),
            drift,
        })
    }

    /// reth follow head (safe/latest/finalized per config).
    pub async fn follow_head(&self) -> Result<Option<u64>> {
        let tag = self.l1.follow.tag();
        let block = self.reth.block_by_tag(tag, mirror_anvil::Txs::Hashes).await?;
        Ok(block.map(|b| b.header.number))
    }

    /// reth follow tag (safe/latest/finalized per config) — the config's own
    /// mapping, never a second copy.
    pub fn follow_tag(&self) -> &'static str {
        self.l1.follow.tag()
    }
}

/// The shell's view of this context (`mirror_devnet::engine::Engine`): the
/// facts that differ, in one place — the chain (T), the source URL, the
/// journal semantics (the applier's parent before-values), the probes, the
/// counters, the status document, and the metric exposition. The admin
/// surface, the epoch re-pin, the boot asserts, the HTTP surface and the
/// service tail are the shell's.
impl Engine for Ctx {
    type Error = Error;
    type Status = crate::http::StatusResponse;

    fn core(&self) -> EngineCore<'_> {
        EngineCore {
            plane: &self.plane,
            mirror: &self.mirror,
            target: &self.anvil_t,
            source: &self.reth,
            source_url: &self.l1.source_url,
            engine: &self.engine,
        }
    }

    fn probes_now(&self) -> BoxFuture<'_, Result<ProbeOutcome>> {
        Box::pin(crate::driver::probes_now(self))
    }

    fn record_scan(&self, walk: &ScanWalk) {
        self.mainnet_metrics.record_scan(
            walk,
            &self.mainnet_metrics.shadow_scanned_blocks,
            &self.mainnet_metrics.shadow_skipped_empty_blocks,
        );
    }

    fn profile_metrics(&self) -> String {
        let mut out = self.mainnet_metrics.render();
        out.push_str(&self.feed_bridge_metrics.render());
        out
    }

    fn status(&self) -> BoxFuture<'_, Result<Self::Status>> {
        Box::pin(crate::http::status_doc(self))
    }
}

/// The top-up loop's authrpc client: the sync runs when the instance
/// supplies both the endpoint and the JWT path (presence is the switch). A
/// configured boundary that cannot be built is a boot error naming itself:
/// silently disabling it would stop the top-up loop while every dashboard
/// still says "enabled".
fn build_engine_api(engine: &EngineSection) -> Result<Option<crate::source_sync::engine_api::EngineClient>> {
    match (&engine.source_sync.engine_url, &engine.source_sync.jwt_path) {
        (Some(url), Some(jwt)) => {
            Ok(Some(crate::source_sync::engine_api::EngineClient::new(url, jwt)?))
        }
        _ => Ok(None),
    }
}
