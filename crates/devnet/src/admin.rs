//! The devnet admin surface (plan §11.3): deal / shadow / time / rewind /
//! pause / resume — the controls that belong to the devnet itself, independent
//! of which chain the mirror serves. The engine's `http.rs` owns the axum
//! routes; the semantics live here, wired per profile through [`AdminSurface`].
//!
//! `Persisted::t` is the devnet chain's cursor slot (T in the ethereum profile;
//! a profile runs one devnet chain, so the venue profile reuses it for V).
use crate::deal;
use crate::error::{Error, Result};
use crate::journal::Journal;
use crate::metrics::Metrics;
use crate::shadow::{save_registry, Shadow};
use crate::shared::Shared;
use crate::source::Source;
use crate::state::StateFile;
use alloy_primitives::{Address, U256};
use mirror_anvil::Rpc;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The devnet admin surface's wiring: the chain the devnet writes to, the
/// unshadow re-sync source, and the devnet registries. Constructed per request
/// from the engine's context (references only).
pub struct AdminSurface<'a> {
    /// The chain the devnet writes to (T / V).
    pub target: &'a Rpc,
    /// The unshadow re-sync source (reth at the cursor / fork source at the pin).
    pub source: &'a dyn Source,
    pub shadow: &'a Shared<Shadow>,
    pub shadow_path: &'a Path,
    pub journal: &'a Journal,
    pub state: &'a Shared<StateFile>,
    pub metrics: &'a Metrics,
    pub time_offset: &'a AtomicU64,
    pub halted: &'a AtomicBool,
    pub frozen: &'a AtomicBool,
    /// Set by [`AdminSurface::rewind`]: blocks whose writes were rolled back
    /// but whose chain head is already past them. The applier's normal
    /// late-completion skip would leave them stale, so it re-applies the range
    /// (idempotent writes; re-journaling records the rolled-back values as the
    /// new before-values). Cleared once the whole range is re-applied.
    pub reapply_from: &'a Shared<Option<(u64, u64)>>,
    /// The shadow scan's high-water mark (shared with the engine's scan loop).
    pub scanned_upto: &'a AtomicU64,
    pub chunk_size: usize,
    /// Whether this deployment journals the engine's *own* writes (deal), so
    /// `/admin/rewind` can undo them. The venue sets this: it has no parent
    /// stream, so its journal is exactly these writes (keyed by the devnet's
    /// coordinate — the scan cursor, or the head before the first scan). The
    /// mainnet keeps it false: its journal is the applier's parent
    /// before-values, and mixing engine writes in would change what a rewind
    /// means (frozen behavior).
    pub journal_writes: bool,
}

impl AdminSurface<'_> {
    /// `POST /admin/deal` — the unlimited ERC20 faucet.
    pub async fn deal(&self, req: &deal::DealRequest) -> Result<deal::DealOutcome> {
        let journal = self.journal_sink().await?;
        deal::deal(
            self.target,
            self.shadow,
            self.shadow_path,
            self.metrics,
            self.chunk_size,
            journal,
            req,
        )
        .await
    }

    /// Force the shadow scan up to the chain head — the hand-back's pre-step.
    /// A devnet write mined moments ago may not be marked yet, and clearing
    /// before it is marked would let the scanner re-mark the key *with the
    /// devnet's value* right after the re-sync — the poisoned-source race
    /// (measured: an oracle crash tx mined <2 s before the unshadow, re-marked
    /// with the crashed source, and the borrow that followed reverted on the
    /// crashed price). A scan that cannot complete fails the request: a
    /// partial registry would let the race through (marks intact, so the
    /// caller can retry).
    pub async fn scan_to_head(&self) -> Result<()> {
        let head = self.target.block_number().await?;
        let from = self.scanned_upto.load(Ordering::Relaxed);
        if from == 0 {
            self.scanned_upto.store(head, Ordering::Relaxed);
            return Ok(());
        }
        let walked =
            crate::scan::scan_range(self.target, self.shadow, self.scanned_upto, from, head).await?;
        self.metrics.add(&self.metrics.shadow_scanned_blocks, walked);
        Ok(())
    }

    /// `POST /admin/epoch` — re-pin the devnet chain to its fork source's
    /// latest block. One anvil call; the engine's loop notices the reset and
    /// runs the epoch rule (new base = new epoch, same base = restore). On a
    /// transport fault, re-read the fork base: an applied-but-unanswered reset
    /// is a success, not a failure (the retry-discipline rule: never report a
    /// fault for a completed effect).
    pub async fn epoch_repin(target: &Rpc, source_url: &str) -> Result<()> {
        let before = target.fork_config().await?.map(|(_, block)| block);
        if let Err(e) = target.anvil_reset_fork_latest(source_url).await {
            let after = target.fork_config().await?.map(|(_, block)| block);
            if after == before {
                return Err(e.into());
            }
            tracing::warn!(
                error = %e,
                before = ?before,
                after = ?after,
                "anvil_reset answered with a transport fault but the fork base moved; treating it as applied"
            );
        }
        tracing::warn!(
            source = %source_url,
            "the devnet chain was re-pinned to the source's latest block; the engine reconciles on its next pass"
        );
        Ok(())
    }

    /// The journaling sink for the engine's own writes, when this deployment
    /// journals them. The block is the devnet's current coordinate — the scan
    /// cursor, or the head before the first scan — so the write always falls
    /// inside the range `/admin/rewind` walks (a head-anchored key could land
    /// *above* the cursor and be missed by the range).
    async fn journal_sink(&self) -> Result<Option<(&Journal, u64)>> {
        if !self.journal_writes {
            return Ok(None);
        }
        let cursor = self.state.lock()?.data.t.as_ref().map(|c| c.block);
        let block = match cursor {
            Some(block) => block,
            None => self.target.block_number().await?,
        };
        Ok(Some((self.journal, block)))
    }

    /// `POST /admin/shadow` — register devnet-owned keys, or hand them back
    /// and re-sync the source's truth.
    ///
    /// With `clear`, the caller must have flushed its pending write scan first
    /// (the scanner is per-project; see the engine's route for the race this
    /// closes). This surface itself is fail-closed: a source read that fails
    /// aborts the hand-back with the marks intact, so a retry re-reads the same
    /// truth — a swallowed read would hand the key back holding a stale devnet
    /// value with no mark to explain it.
    pub async fn shadow(&self, req: &ShadowRequest) -> Result<ShadowOutcome> {
        if req.all && !req.clear {
            return Err(Error::InvalidShadow {
                reason: "`all` requires `clear` (it collects the keys to hand back)",
            });
        }
        let resynced = if req.clear {
            self.hand_back(req).await?
        } else {
            self.register(req)?;
            0
        };
        let shadow = self.shadow.lock()?;
        Ok(ShadowOutcome {
            address: req.address,
            cleared: req.clear,
            resynced_keys: resynced,
            shadowed_slots: shadow.slot_count(),
            shadowed_accounts: shadow.accounts.len(),
        })
    }

    /// Register the devnet-owned keys (`clear: false`).
    fn register(&self, req: &ShadowRequest) -> Result<()> {
        let mut s = self.shadow.lock()?;
        if req.account_fields {
            s.mark_account_fields(req.address);
        }
        for slot in &req.slots {
            s.mark_slot(req.address, *slot);
        }
        drop(s);
        save_registry(self.shadow, self.shadow_path, self.metrics)
    }

    /// Hand the keys back to parent sync and re-sync the source's truth
    /// (`clear: true`). Fail-closed: a source read that fails aborts with the
    /// marks intact, so a retry re-reads the same truth — a swallowed read
    /// would hand the key back holding a stale devnet value with no mark to
    /// explain it. Returns the number of re-synced keys.
    async fn hand_back(&self, req: &ShadowRequest) -> Result<usize> {
        // `all` hands every shadowed key of the account back to the source.
        let mut slots = req.slots.clone();
        if req.all {
            let s = self.shadow.lock()?;
            if let Some(set) = s.slots.get(&req.address) {
                for slot in set {
                    if !slots.contains(slot) {
                        slots.push(*slot);
                    }
                }
            }
        }
        let account_fields = req.account_fields || req.all;
        // The devnet's state reflects the last applied parent block, so
        // re-sync from *that* block - reading the source's head would leave
        // the devnet holding a future value.
        let anchor = self
            .state
            .lock()?
            .data
            .t
            .as_ref()
            .map(|c| c.block)
            .unwrap_or(0);
        let anchor = if anchor == 0 {
            self.source.head().await?
        } else {
            anchor
        };
        // Read the source's truth *before* touching the marks: a key without a
        // shadow but with a stale devnet value is the most confusing state
        // there is, and a read fault must abort the hand-back (retryable), not
        // skip the key.
        let mut writes: Vec<mirror_anvil::Write> = Vec::new();
        if account_fields {
            writes.push(mirror_anvil::Write::Balance {
                address: req.address,
                value: self.source.balance_at(req.address, anchor).await?,
            });
            writes.push(mirror_anvil::Write::Nonce {
                address: req.address,
                value: self.source.nonce_at(req.address, anchor).await?,
            });
        }
        for slot in &slots {
            writes.push(mirror_anvil::Write::Storage {
                address: req.address,
                slot: *slot,
                value: U256::from_be_bytes(
                    self.source.storage_at(req.address, *slot, anchor).await?.0,
                ),
            });
        }
        {
            let mut s = self.shadow.lock()?;
            if account_fields {
                s.unmark_account_fields(req.address);
            }
            for slot in &slots {
                s.unmark_slot(req.address, *slot);
            }
        }
        // Push the source's values now, then persist the registry. A fault
        // here leaves the marks in memory but not on disk; a restart restores
        // them (conservative: the key stays devnet-owned), and the next request
        // retries - the writes are absolute and idempotent.
        let resynced = mirror_anvil::apply_writes(self.target, &writes, self.chunk_size).await?;
        save_registry(self.shadow, self.shadow_path, self.metrics)?;
        Ok(resynced)
    }

    /// `POST /admin/rewind` — undo the last N applied parent blocks using the
    /// journal. Returns the new cursor block and the number of restored values.
    pub async fn rewind(&self, blocks: u64) -> Result<RewindOutcome> {
        let Some(cursor) = self.state.lock()?.data.t.clone() else {
            return Err(Error::NoCursor);
        };
        if blocks == 0 || blocks > cursor.block {
            return Err(Error::InvalidRewind {
                blocks,
                cursor: cursor.block,
            });
        }
        if blocks > self.journal.keep() {
            // The ring bounds the rewindable window; beyond it the older
            // before-values are pruned, so a partial restore would be a lie.
            return Err(Error::RewindBeyondRing {
                blocks,
                keep: self.journal.keep(),
            });
        }
        let from = cursor.block - blocks + 1;
        // Mark the range for re-apply *before* rolling anything back: the
        // applier's late-completion skip must be disabled for these blocks from
        // the first undo, or a concurrent apply in this window could skip a
        // block that is never re-applied.
        {
            let mut ra = self.reapply_from.lock()?;
            *ra = Some(match *ra {
                Some((f, u)) => (f.min(from), u.max(cursor.block)),
                None => (from, cursor.block),
            });
        }
        // Walk the blocks that actually have before-values (newest first): a
        // quiet block has no file — skipping it is "nothing to restore", not a
        // fault, and a large range costs one directory read, not one probe per
        // block.
        let mut restored = 0usize;
        for b in self.journal.blocks_in(from, cursor.block)? {
            restored += self
                .journal
                .undo_block(b, self.target, self.chunk_size)
                .await?;
        }
        let target = from - 1;
        // Fail-closed on the cursor read: a fault must not silently clear the
        // cursor (the applier would then resume at the chain head + 1 and skip
        // parent blocks); only a *missing* block means "reset under us".
        match self.target.block_by_number_opt(target, false).await? {
            Some(b) => {
                let first = self
                    .state
                    .lock()?
                    .data
                    .t
                    .as_ref()
                    .map(|c| c.first_block)
                    .unwrap_or(target);
                self.state.lock()?.data.t =
                    Some(crate::state::cursor_from(&b, first));
            }
            None => {
                self.state.lock()?.data.t = None;
            }
        }
        self.state.lock()?.save()?;
        // The chain's *head* is still where it was - only the values were
        // rolled back - so the main loop's "diffs already applied"
        // late-completion path would skip the re-application and leave the
        // rewound range stale (measured: T's probes diverged after a rewind
        // drill). The range was marked for re-apply before the first undo.
        tracing::warn!(blocks, restored, "devnet rewound via journal");
        Ok(RewindOutcome {
            cursor: target,
            restored,
        })
    }

    /// `POST /admin/time` — the devnet's clock, in seconds ahead of the parent
    /// chain. A financial product needs this: interest accrual, option expiry,
    /// funding intervals. Unlike a static fork's `evm_increaseTime`, the offset
    /// survives the next parent block (the applier aligns to `parent + offset`)
    /// and the parent's data keeps flowing. Lowering it works, but timestamps
    /// are monotonic: the clamp lets the clock converge back rather than
    /// jumping backwards.
    pub async fn time(&self, seconds: u64) -> Result<TimeOutcome> {
        self.time_offset.store(seconds, Ordering::Relaxed);
        // Persist it: the offset is part of the devnet's state contract (its
        // blocks carry it), so a restart must keep it.
        self.state.lock()?.data.time_offset_seconds = Some(seconds);
        self.state.lock()?.save()?;
        // Informational: the offset is already set and persisted; a head gauge
        // that cannot be read degrades the report, it must not fail the write.
        let head = self.target.block_number().await.unwrap_or(0);
        tracing::warn!(
            offset_seconds = seconds,
            "devnet clock offset set (the devnet diverges from the parent's clock by design)"
        );
        Ok(TimeOutcome {
            offset_seconds: seconds,
            head,
        })
    }

    /// `POST /admin/pause` — hold both loops where they are (plan §4.4
    /// stop-sync). Devnet writes, cheatcodes and reads keep working; nothing
    /// new is applied until [`AdminSurface::resume`]. Useful for a long
    /// manipulation you do not want a parent block to land in the middle of.
    pub fn pause(&self) {
        // A gate read by the loops: SeqCst, like the divergence flags (rule
        // 14 - simplicity over minimal ordering for control gates).
        self.halted.store(true, Ordering::SeqCst);
        self.metrics.set(&self.metrics.halted, 1);
    }

    /// `POST /admin/resume` — clear the halt *and* the divergence freeze.
    /// The freeze is the operator's decision point (`on_divergence`), so
    /// resuming is the operator's acknowledgement that it was investigated:
    /// the two flags are cleared together, as they always have been.
    pub fn resume(&self) {
        self.frozen.store(false, Ordering::SeqCst);
        self.halted.store(false, Ordering::SeqCst);
        self.metrics.set(&self.metrics.frozen, 0);
        self.metrics.set(&self.metrics.halted, 0);
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShadowRequest {
    pub address: Address,
    #[serde(default)]
    pub slots: Vec<U256>,
    #[serde(default)]
    pub account_fields: bool,
    /// Hand these keys back to parent sync (and re-sync their values now).
    #[serde(default)]
    pub clear: bool,
    /// With `clear`: hand back *every* shadowed key of this account - the
    /// "reset this venue to source truth" operation after a sim. Without
    /// `clear` it is a request error (it collects the keys to hand back).
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShadowOutcome {
    pub address: Address,
    pub cleared: bool,
    pub resynced_keys: usize,
    pub shadowed_slots: usize,
    pub shadowed_accounts: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RewindRequest {
    pub blocks: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct RewindOutcome {
    pub cursor: u64,
    pub restored: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimeRequest {
    /// Seconds the devnet's clock runs ahead of the parent chain.
    pub offset_seconds: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct TimeOutcome {
    pub offset_seconds: u64,
    pub head: u64,
}
