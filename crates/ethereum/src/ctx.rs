//! Engine context: the profile's clients and config, plus the devnet plane
//! (registry, journal, cursor state, admin gauges and flags) as one
//! [`Plane`] — the same bundle the venue engine embeds.
//!
//! Every mutable registry is a [`mirror_devnet::Shared`] lock: poisoning is a
//! typed fault (`Error::Poisoned`) that the loops propagate and the admin
//! surface answers 503 for — never a per-call panic, never a silent recovery.
use crate::blob::{BeaconClient, BlobForm};
use crate::drift::Drift;
use crate::error::Result;
use crate::metrics::EthereumMetrics;
use crate::rpc::Rpc;
use alloy_primitives::Address;
use mirror_config::{ChainL1, EngineSection, FollowHead, Instance};
use mirror_devnet::admin::AdminSurface;
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::plane::Plane;
use mirror_devnet::source::Source;
use mirror_devnet::state::ChainCursor;
use mirror_devnet::Shared;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Block → cursor conversion (the shim keeps the frozen `crate::ctx::cursor_from`
/// path resolving; the code lives in `mirror-devnet`).
pub use mirror_devnet::state::cursor_from;

/// Transactions of a full block — chain-generic, so it lives in
/// `mirror-devnet::scan` (the venue profile scans V with the same code).
pub use mirror_devnet::scan::block_txs;

pub struct Ctx {
    /// The ethereum chain's section (the source, follow policy, fork offset).
    pub l1: ChainL1,
    /// The engine's section (the run path moved it out of the instance: the
    /// context carries exactly the sections it reads).
    pub engine: EngineSection,
    pub reth: Rpc,
    pub anvil_m: Rpc,
    pub anvil_t: Rpc,
    /// The unshadow re-sync source (reth at the applied cursor): one
    /// implementation per profile, chosen at boot (rule 15).
    pub source: Arc<dyn Source>,
    pub beacon: BeaconClient,
    /// Engine API client (reth authrpc) for the top-up loop; None when disabled
    /// or unconfigured.
    pub engine_api: Option<crate::engine_api::EngineClient>,
    pub watchlist: HashSet<Address>,
    /// The devnet plane: the shadow registry, the journal, the cursor state,
    /// the shared admin gauges and flags — the same bundle the venue engine
    /// embeds (one home for the wiring that must not drift).
    pub plane: Plane,
    /// The profile's own set (reth/M/T heads, the applier's counters, the
    /// probes); rendered beside the shared admin gauges.
    pub eth_metrics: EthereumMetrics,
    /// The feed bridge's counters (shared with the bridge task; /metrics
    /// renders them whether or not the bridge is configured).
    pub feed_bridge_metrics: Arc<crate::feed_bridge::FeedBridgeMetrics>,
    pub drift: Drift,
    /// The learned blob wrapper form (persisted in the plane's state file).
    pub blob_pref: Shared<Option<BlobForm>>,
    /// Last (head, follow) pair logged as "M ahead of the follow head": the
    /// poll loop runs every second, so the same stall must not spam the log.
    pub last_stall: Shared<(u64, u64)>,
    /// Last block whose missing blob sidecars were logged at WARN: with
    /// `follow: latest` the beacon ingests sidecars seconds after the block, so
    /// the first attempt legitimately misses and the retries must not spam.
    pub blob_warned: std::sync::atomic::AtomicU64,
    /// Set by the replay loop when the same block has failed long enough that
    /// the cause cannot be transient (measured: Lighthouse prunes blob sidecars
    /// after ~1 epoch, so a hold longer than that can never be served and M
    /// would wait forever). `next_block_for` honours it by re-basing M at a
    /// fresh pin, the same path a receding follow head takes.
    pub force_rebase: AtomicBool,
    /// Did the *last* M block's verification find a divergence? The parent-BAL
    /// repair only runs when it did: M's state is then the replay's own product
    /// (the verification means something) and clean blocks cost no RPC churn.
    /// Starts `true` so the first block after a restart is repaired.
    pub last_m_diverged: AtomicBool,
    /// When M was last re-based (rate-limits `anvil_reset` attempts to 1/min).
    pub last_rebase: Shared<Option<std::time::Instant>>,
}

impl Ctx {
    /// Build the context. Every boundary that must exist for the engine to be
    /// correct is fallible here (clients, state files, the drift log): a
    /// missing boundary is a boot error naming it, never a degraded start.
    #[allow(
        clippy::similar_names,
        reason = "M/T are the domain's chain names (M = verified replay chain, T = devnet chain)"
    )]
    pub fn new(
        mut cfg: Instance,
        reth_url: &str,
        anvil_m_url: &str,
        anvil_t_url: &str,
    ) -> Result<Self> {
        let (l1, engine_section) = cfg.take_ethereum()?;
        let timeout = Duration::from_secs(engine_section.reth.request_timeout_secs);
        let watchlist = cfg.watch_set();
        let plane = Plane::open(
            &engine_section.state_dir,
            engine_section.journal.dir.clone(),
            engine_section.journal.blocks,
            &engine_section.fixture_addresses,
        )?;
        let drift = Drift::new(engine_section.state_dir.join("drift.jsonl"))?;
        let beacon = BeaconClient::new(engine_section.blobs.beacon_urls.clone())?;
        let reth = Rpc::new(reth_url, timeout)?;
        let source: Arc<dyn Source> = Arc::new(crate::eth_source::EthereumSource::new(reth.clone()));
        let engine_api = match (
            &engine_section.source_sync.engine_url,
            &engine_section.source_sync.jwt_path,
        ) {
            (Some(url), Some(jwt)) if engine_section.source_sync.enabled => {
                // A configured boundary that cannot be built is a boot error
                // naming itself: silently disabling it would stop the top-up
                // loop while every dashboard still says "enabled".
                Some(crate::engine_api::EngineClient::new(url, jwt)?)
            }
            _ => None,
        };
        Ok(Self {
            l1,
            engine: engine_section,
            reth,
            anvil_m: Rpc::new(anvil_m_url, timeout)?,
            anvil_t: Rpc::new(anvil_t_url, timeout)?,
            source,
            beacon,
            engine_api,
            watchlist,
            plane,
            eth_metrics: EthereumMetrics::default(),
            feed_bridge_metrics: Arc::new(crate::feed_bridge::FeedBridgeMetrics::default()),
            drift,
            blob_pref: Shared::new("blob form", None),
            last_stall: Shared::new("stall log", (0, 0)),
            blob_warned: std::sync::atomic::AtomicU64::new(0),
            force_rebase: AtomicBool::new(false),
            last_m_diverged: AtomicBool::new(true),
            last_rebase: Shared::new("rebase clock", None),
        })
    }

    pub fn cursor_m(&self) -> Result<Option<ChainCursor>> {
        Ok(self.plane.state.lock()?.data.m.clone())
    }

    pub fn cursor_t(&self) -> Result<Option<ChainCursor>> {
        Ok(self.plane.state.lock()?.data.t.clone())
    }

    pub fn set_cursor_m(&self, c: ChainCursor) -> Result<()> {
        self.plane.state.lock()?.data.m = Some(c);
        Ok(())
    }

    pub fn set_cursor_t(&self, c: ChainCursor) -> Result<()> {
        self.plane.state.lock()?.data.t = Some(c);
        Ok(())
    }

    pub fn set_reth_last_hash(&self, hash: alloy_primitives::B256) -> Result<()> {
        self.plane.state.lock()?.data.reth_last_hash = Some(hash);
        Ok(())
    }

    pub fn set_reth_last_block(&self, n: u64) -> Result<()> {
        self.plane.state.lock()?.data.reth_last_block = Some(n);
        Ok(())
    }

    pub fn reth_last_block(&self) -> Result<Option<u64>> {
        Ok(self.plane.state.lock()?.data.reth_last_block)
    }

    pub fn reth_last_hash(&self) -> Result<Option<alloy_primitives::B256>> {
        Ok(self.plane.state.lock()?.data.reth_last_hash)
    }

    /// The devnet admin surface, wired to this engine's chain and re-sync
    /// source (the semantics live in `mirror-devnet`; the wiring is the
    /// profile's: T's chain, reth-as-source, the configured chunk size, and a
    /// journal that holds the applier's parent before-values, not the engine's
    /// own writes).
    pub fn admin(&self) -> AdminSurface<'_> {
        self.plane.admin(
            &self.anvil_t,
            self.source.as_ref(),
            self.engine.loops.chunk_size,
            false,
        )
    }

    pub fn is_shadowed(&self, w: &crate::bal::Write) -> Result<bool> {
        Ok(self.plane.shadow.lock()?.is_shadowed(w))
    }

    /// reth follow head (safe/latest/finalized per config).
    pub async fn follow_head(&self) -> Result<Option<u64>> {
        let tag = self.l1.follow.tag();
        let block = self.reth.block_by_tag(tag, false).await?;
        Ok(block.map(|b| b.header.number))
    }

    pub fn on_divergence(&self, reason: &str) {
        self.eth_metrics.inc(&self.eth_metrics.divergences);
        tracing::error!(reason, "DIVERGENCE");
        match self.engine.loops.on_divergence {
            mirror_config::OnDivergence::FreezeM => {
                self.plane.frozen.store(true, Ordering::SeqCst);
                self.plane.metrics.set(&self.plane.metrics.frozen, 1);
            }
            mirror_config::OnDivergence::HaltAll => {
                self.plane.frozen.store(true, Ordering::SeqCst);
                self.plane.halted.store(true, Ordering::SeqCst);
                self.plane.metrics.set(&self.plane.metrics.frozen, 1);
                self.plane.metrics.set(&self.plane.metrics.halted, 1);
            }
            mirror_config::OnDivergence::Continue => {}
        }
    }

    pub fn follow_tag(&self) -> &'static str {
        match self.l1.follow {
            FollowHead::Safe => "safe",
            FollowHead::Latest => "latest",
            FollowHead::Finalized => "finalized",
        }
    }

    pub fn blob_form(&self) -> Result<Option<BlobForm>> {
        Ok(*self.blob_pref.lock()?)
    }

    pub fn set_blob_form(&self, form: BlobForm) -> Result<()> {
        *self.blob_pref.lock()? = Some(form);
        let mut st = self.plane.state.lock()?;
        st.data.blob_wrapper = Some(form.as_str().to_string());
        Ok(())
    }

    /// Restore a learned blob form from the persisted state.
    pub fn restore_blob_form(&self) -> Result<()> {
        let persisted = self.plane.state.lock()?.data.blob_wrapper.clone();
        if let Some(s) = persisted {
            if let Some(f) = BlobForm::from_str(&s) {
                *self.blob_pref.lock()? = Some(f);
            }
        }
        Ok(())
    }
}
