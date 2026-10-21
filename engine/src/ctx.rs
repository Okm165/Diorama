//! Shared engine context: RPC clients, watch list, shadow registry, journal,
//! cursor state, metrics.
use crate::blob::{BeaconClient, BlobForm};
use crate::config::{Config, FollowHead};
use crate::drift::Drift;
use crate::journal::Journal;
use crate::metrics::Metrics;
use crate::rpc::Rpc;
use crate::shadow::Shadow;
use crate::state::{ChainCursor, StateFile};
use alloy_primitives::Address;
use anyhow::{Context as _, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Transactions of a full block (the engine always requests full=true).
pub fn block_txs(block: &alloy_rpc_types_eth::Block) -> &[alloy_rpc_types_eth::Transaction] {
    block.transactions.as_transactions().unwrap_or(&[])
}

pub struct Ctx {
    pub cfg: Config,
    pub reth: Rpc,
    pub anvil_m: Rpc,
    pub anvil_t: Rpc,
    pub beacon: BeaconClient,
    pub watchlist: HashSet<Address>,
    pub shadow: Mutex<Shadow>,
    pub shadow_path: PathBuf,
    pub journal: Journal,
    pub state: Mutex<StateFile>,
    pub metrics: Metrics,
    pub drift: Drift,
    pub frozen: AtomicBool,
    pub halted: AtomicBool,
    pub blob_pref: Mutex<Option<BlobForm>>,
}

impl Ctx {
    pub fn new(cfg: Config, reth_url: &str, anvil_m_url: &str, anvil_t_url: &str) -> Result<Self> {
        let timeout = Duration::from_secs(cfg.reth.request_timeout_secs);
        let watchlist = cfg.watch_set();
        let shadow_path = cfg.state_dir.join("shadow.json");
        let shadow = Shadow::load(&shadow_path);
        let journal = Journal::new(cfg.journal.dir.clone(), cfg.journal.blocks);
        let state = StateFile::open(cfg.state_dir.join("cursor.json"));
        let drift = Drift::new(cfg.state_dir.join("drift.jsonl"));
        let beacon = BeaconClient::new(cfg.blobs.beacon_urls.clone());
        Ok(Self {
            reth: Rpc::new(reth_url, timeout),
            anvil_m: Rpc::new(anvil_m_url, timeout),
            anvil_t: Rpc::new(anvil_t_url, timeout),
            beacon,
            watchlist,
            shadow: Mutex::new(shadow),
            shadow_path,
            journal,
            state: Mutex::new(state),
            metrics: Metrics::default(),
            drift,
            frozen: AtomicBool::new(false),
            halted: AtomicBool::new(false),
            blob_pref: Mutex::new(None),
            cfg,
        })
    }

    pub fn cursor_m(&self) -> Option<ChainCursor> {
        self.state.lock().unwrap().data.m.clone()
    }

    pub fn cursor_t(&self) -> Option<ChainCursor> {
        self.state.lock().unwrap().data.t.clone()
    }

    pub fn set_cursor_m(&self, c: ChainCursor) {
        self.state.lock().unwrap().data.m = Some(c);
    }

    pub fn set_cursor_t(&self, c: ChainCursor) {
        self.state.lock().unwrap().data.t = Some(c);
    }

    pub fn set_reth_last_hash(&self, hash: String) {
        self.state.lock().unwrap().data.reth_last_hash = Some(hash);
    }

    pub fn set_reth_last_block(&self, n: u64) {
        self.state.lock().unwrap().data.reth_last_block = Some(n);
    }

    pub fn reth_last_block(&self) -> Option<u64> {
        self.state.lock().unwrap().data.reth_last_block
    }

    pub fn reth_last_hash(&self) -> Option<String> {
        self.state.lock().unwrap().data.reth_last_hash.clone()
    }

    pub fn save_state(&self) -> Result<()> {
        self.state.lock().unwrap().save()
    }

    pub fn save_shadow(&self) -> Result<()> {
        let s = self.shadow.lock().unwrap();
        s.save(&self.shadow_path)?;
        self.metrics
            .set(&self.metrics.shadowed_slots, s.slot_count() as u64);
        Ok(())
    }

    pub fn is_shadowed(&self, w: &crate::bal::Write) -> bool {
        self.shadow.lock().unwrap().is_shadowed(w)
    }

    #[allow(dead_code)]
    pub fn mark_shadow(&self, w: &crate::bal::Write) {
        self.shadow.lock().unwrap().mark(w);
    }

    /// reth follow head (safe/latest/finalized per config).
    pub async fn follow_head(&self) -> Result<Option<u64>> {
        let tag = self.cfg.reth.follow.tag();
        let block = self.reth.block_by_tag(tag, false).await?;
        Ok(block.map(|b| b.header.number))
    }

    pub fn on_divergence(&self, reason: &str) {
        self.metrics.inc(&self.metrics.divergences);
        tracing::error!(reason, "DIVERGENCE");
        match self.cfg.loops.on_divergence {
            crate::config::OnDivergence::FreezeM => {
                self.frozen.store(true, Ordering::SeqCst);
                self.metrics.set(&self.metrics.frozen, 1);
            }
            crate::config::OnDivergence::HaltAll => {
                self.frozen.store(true, Ordering::SeqCst);
                self.halted.store(true, Ordering::SeqCst);
                self.metrics.set(&self.metrics.frozen, 1);
                self.metrics.set(&self.metrics.halted, 1);
            }
            crate::config::OnDivergence::Continue => {}
        }
    }

    pub fn resume(&self) {
        self.frozen.store(false, Ordering::SeqCst);
        self.halted.store(false, Ordering::SeqCst);
        self.metrics.set(&self.metrics.frozen, 0);
        self.metrics.set(&self.metrics.halted, 0);
    }

    pub fn follow_tag(&self) -> &'static str {
        match self.cfg.reth.follow {
            FollowHead::Safe => "safe",
            FollowHead::Latest => "latest",
            FollowHead::Finalized => "finalized",
        }
    }

    pub fn blob_form(&self) -> Option<BlobForm> {
        *self.blob_pref.lock().unwrap()
    }

    pub fn set_blob_form(&self, form: BlobForm) {
        *self.blob_pref.lock().unwrap() = Some(form);
        let mut st = self.state.lock().unwrap();
        st.data.blob_wrapper = Some(form.as_str().to_string());
    }

    /// Restore a learned blob form from the persisted state.
    pub fn restore_blob_form(&self) {
        if let Some(s) = self.state.lock().unwrap().data.blob_wrapper.clone() {
            if let Some(f) = BlobForm::from_str(&s) {
                *self.blob_pref.lock().unwrap() = Some(f);
            }
        }
    }
}

/// Convenience: build a `ChainCursor` from a block.
pub fn cursor_from(block: &alloy_rpc_types_eth::Block, first_block: u64) -> ChainCursor {
    ChainCursor {
        block: block.header.number,
        hash: format!("0x{:x}", block.header.hash),
        first_block,
    }
}

/// Read a block's hash (for cursor validation).
pub async fn block_hash(rpc: &Rpc, n: u64) -> Result<Option<String>> {
    let b = rpc.block_by_number_opt(n, false).await.with_context(|| format!("block {n}"))?;
    Ok(b.map(|b| format!("0x{:x}", b.header.hash)))
}
