//! Shared engine context: RPC clients, watch list, shadow registry, journal,
//! cursor state, metrics.
//!
//! Every mutable registry is a [`mirror_devnet::Shared`] lock: poisoning is a
//! typed fault (`Error::Poisoned`) that the loops propagate and the admin
//! surface answers 503 for — never a per-call panic, never a silent recovery.
use crate::blob::{BeaconClient, BlobForm};
use mirror_config::{ChainL1, EngineSection, FollowHead, Instance};
use crate::drift::Drift;
use crate::error::Result;
use crate::journal::Journal;
use crate::metrics::Metrics;
use crate::rpc::Rpc;
use crate::shadow::Shadow;
use crate::state::{ChainCursor, StateFile};
use alloy_primitives::Address;
use mirror_devnet::admin::AdminSurface;
use mirror_devnet::source::Source;
use mirror_devnet::Shared;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    pub shadow: Shared<Shadow>,
    pub shadow_path: PathBuf,
    pub journal: Journal,
    pub state: Shared<StateFile>,
    pub metrics: Metrics,
    pub drift: Drift,
    pub frozen: AtomicBool,
    pub halted: AtomicBool,
    pub blob_pref: Shared<Option<BlobForm>>,
    /// Set by `/admin/rewind`: parent blocks whose T writes were rolled back
    /// but whose T head is already past them. The normal "late completion"
    /// skip would leave them stale, so the applier re-applies them
    /// (idempotent writes; re-journaling records the rolled-back values as the
    /// new before-values). Cleared once the whole range is re-applied.
    pub reapply_from: Shared<Option<(u64, u64)>>,
    /// Last (head, follow) pair logged as "M ahead of the follow head": the
    /// poll loop runs every second, so the same stall must not spam the log.
    pub last_stall: Shared<(u64, u64)>,
    /// Last block whose missing blob sidecars were logged at WARN: with
    /// `follow: latest` the beacon ingests sidecars seconds after the block, so
    /// the first attempt legitimately misses and the retries must not spam.
    pub blob_warned: std::sync::atomic::AtomicU64,
    /// Seconds the devnet's clock runs ahead of the parent block (`/admin/time`).
    /// The applier aligns to `parent + offset` and clamps to keep timestamps
    /// monotonic, so a jump sticks and mainnet's data keeps flowing - the
    /// financial-dev use case (interest accrual, expiry, funding) that a static
    /// fork gets from `evm_increaseTime` but a live one would lose on the next
    /// parent block.
    pub time_offset: AtomicU64,
    /// Highest T block whose devnet writes have been scanned into the shadow
    /// registry. Shared by the polling shadow loop and the applier's inline
    /// scan (which runs its own prepend blocks *before* the probe parity), so
    /// a block is traced once and the log stays one line per devnet block.
    pub shadow_scanned_upto: AtomicU64,
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
        let shadow_path = engine_section.state_dir.join("shadow.json");
        let shadow = Shadow::load(&shadow_path)?;
        let journal = Journal::new(
            engine_section.journal.dir.clone(),
            engine_section.journal.blocks,
        )?;
        let state = StateFile::open(engine_section.state_dir.join("cursor.json"))?;
        let offset = state.data.time_offset_seconds.unwrap_or(0);
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
            shadow: Shared::new("shadow registry", shadow),
            shadow_path,
            journal,
            // The devnet's clock offset is part of its state contract: T's
            // blocks (and every time-dependent contract on it) carry it, so a
            // restart must not silently drop it back to 0 - T would hold
            // +offset-era state under a mainnet clock (measured: Aave's
            // `lastUpdateTimestamp` lands in the future and every supply
            // panics with an arithmetic underflow).
            time_offset: AtomicU64::new(offset),
            state: Shared::new("cursor state", state),
            metrics: Metrics::default(),
            drift,
            frozen: AtomicBool::new(false),
            halted: AtomicBool::new(false),
            blob_pref: Shared::new("blob form", None),
            reapply_from: Shared::new("reapply range", None),
            blob_warned: std::sync::atomic::AtomicU64::new(0),
            force_rebase: AtomicBool::new(false),
            shadow_scanned_upto: AtomicU64::new(0),
            last_m_diverged: AtomicBool::new(true),
            last_stall: Shared::new("stall log", (0, 0)),
            last_rebase: Shared::new("rebase clock", None),
        })
    }

    pub fn cursor_m(&self) -> Result<Option<ChainCursor>> {
        Ok(self.state.lock()?.data.m.clone())
    }

    pub fn cursor_t(&self) -> Result<Option<ChainCursor>> {
        Ok(self.state.lock()?.data.t.clone())
    }

    pub fn set_cursor_m(&self, c: ChainCursor) -> Result<()> {
        self.state.lock()?.data.m = Some(c);
        Ok(())
    }

    pub fn set_cursor_t(&self, c: ChainCursor) -> Result<()> {
        self.state.lock()?.data.t = Some(c);
        Ok(())
    }

    pub fn set_reth_last_hash(&self, hash: String) -> Result<()> {
        self.state.lock()?.data.reth_last_hash = Some(hash);
        Ok(())
    }

    pub fn set_reth_last_block(&self, n: u64) -> Result<()> {
        self.state.lock()?.data.reth_last_block = Some(n);
        Ok(())
    }

    pub fn reth_last_block(&self) -> Result<Option<u64>> {
        Ok(self.state.lock()?.data.reth_last_block)
    }

    pub fn reth_last_hash(&self) -> Result<Option<String>> {
        Ok(self.state.lock()?.data.reth_last_hash.clone())
    }

    pub fn save_state(&self) -> Result<()> {
        self.state.lock()?.save()?;
        Ok(())
    }

    pub fn save_shadow(&self) -> Result<()> {
        Ok(mirror_devnet::shadow::save_registry(
            &self.shadow,
            &self.shadow_path,
            &self.metrics,
        )?)
    }

    /// The devnet admin surface (plan §11.3), wired to this engine's chain and
    /// registries: the semantics live in `mirror-devnet`, the wiring here.
    pub fn admin(&self) -> AdminSurface<'_> {
        AdminSurface {
            target: &self.anvil_t,
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
            chunk_size: self.engine.loops.chunk_size,
        }
    }

    pub fn is_shadowed(&self, w: &crate::bal::Write) -> Result<bool> {
        Ok(self.shadow.lock()?.is_shadowed(w))
    }

    /// reth follow head (safe/latest/finalized per config).
    pub async fn follow_head(&self) -> Result<Option<u64>> {
        let tag = self.l1.follow.tag();
        let block = self.reth.block_by_tag(tag, false).await?;
        Ok(block.map(|b| b.header.number))
    }

    pub fn on_divergence(&self, reason: &str) {
        self.metrics.inc(&self.metrics.divergences);
        tracing::error!(reason, "DIVERGENCE");
        match self.engine.loops.on_divergence {
            mirror_config::OnDivergence::FreezeM => {
                self.frozen.store(true, Ordering::SeqCst);
                self.metrics.set(&self.metrics.frozen, 1);
            }
            mirror_config::OnDivergence::HaltAll => {
                self.frozen.store(true, Ordering::SeqCst);
                self.halted.store(true, Ordering::SeqCst);
                self.metrics.set(&self.metrics.frozen, 1);
                self.metrics.set(&self.metrics.halted, 1);
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
        let mut st = self.state.lock()?;
        st.data.blob_wrapper = Some(form.as_str().to_string());
        Ok(())
    }

    /// Restore a learned blob form from the persisted state.
    pub fn restore_blob_form(&self) -> Result<()> {
        let persisted = self.state.lock()?.data.blob_wrapper.clone();
        if let Some(s) = persisted {
            if let Some(f) = BlobForm::from_str(&s) {
                *self.blob_pref.lock()? = Some(f);
            }
        }
        Ok(())
    }
}
