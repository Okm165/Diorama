//! The devnet admin surface (plan §11.3): deal / shadow / time / rewind /
//! pause / resume — the controls that belong to the devnet itself, independent
//! of which chain the mirror serves. The engine's `http.rs` owns the axum
//! routes; the semantics live here, wired per profile through [`AdminSurface`].
//!
//! `Persisted::t` is the devnet chain's cursor slot (T in the mainnet profile;
//! a profile runs one devnet chain, so the venue profile reuses it for V).
use crate::deal;
use crate::error::{Error, Result};
use crate::journal::Journal;
use crate::metrics::Registry;
use crate::plane::Plane;
use crate::source::Source;
use alloy_primitives::{Address, U256};
use mirror_anvil::Rpc;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

/// The devnet admin surface's wiring: the devnet plane (registry, journal,
/// cursor state, gauges and flags) plus the profile's own facts — the chain
/// the devnet writes to, the unshadow re-sync source, the write chunk size,
/// and whether this deployment journals the engine's own writes. Constructed
/// per request from the engine's context (references only).
pub struct AdminSurface<'a> {
    /// The devnet plane both engines embed (one home for the registries).
    pub plane: &'a Plane,
    /// The chain the devnet writes to (T / V).
    pub target: &'a Rpc,
    /// The unshadow re-sync source (reth at the cursor / fork source at the pin).
    pub source: &'a dyn Source,
    pub chunk_size: usize,
    /// What this deployment's journal holds (see [`crate::engine::JournalScope`]):
    /// only `OwnWrites` journals the engine's own writes (the deal), so
    /// `/admin/rewind` can undo them.
    pub journal_scope: crate::engine::JournalScope,
}

impl AdminSurface<'_> {
    /// `POST /admin/deal` — the unlimited ERC20 faucet.
    pub async fn deal(&self, req: &deal::DealRequest) -> Result<deal::DealOutcome> {
        let journal = self.journal_sink().await?;
        deal::deal(self, journal, req).await
    }

    /// Force the shadow scan up to the chain head — the hand-back's pre-step.
    /// A devnet write mined moments ago may not be marked yet, and clearing
    /// before it is marked would let the scanner re-mark the key *with the
    /// devnet's value* right after the re-sync — the poisoned-source race
    /// (measured: an oracle crash tx mined <2 s before the unshadow, re-marked
    /// with the crashed source, and the borrow that followed reverted on the
    /// crashed price). A scan that cannot complete fails the request: a
    /// partial registry would let the race through (marks intact, so the
    /// caller can retry). Returns the blocks walked — the profile records
    /// them in its own scan-cost counter.
    pub async fn scan_to_head(&self) -> Result<crate::scan::ScanWalk> {
        let head = self.target.block_number().await?;
        let pin = self.target.fork_block().await?.unwrap_or(0);
        // A fresh mark starts at the head: the hand-back's job is the recent
        // devnet writes, not history (the registry was persisted).
        crate::scan::scan_to(self.plane, self.target, pin, head, head).await
    }

    /// `POST /admin/epoch` — re-pin the devnet chain to its fork source's
    /// latest block. One anvil call; the engine's loop notices the reset and
    /// runs the epoch rule (new base = new epoch, same base = restore). On a
    /// transport fault, re-read the fork base: an applied-but-unanswered reset
    /// is a success, not a failure (the retry-discipline rule: never report a
    /// fault for a completed effect).
    pub async fn epoch_repin(target: &Rpc, source_url: &str) -> Result<EpochResponse> {
        let before = fork_base(target).await?;
        if let Err(e) = target.anvil_reset_fork_latest(source_url).await {
            confirm_reset(target, before, e).await?;
        }
        tracing::warn!(
            source = %source_url,
            "the devnet chain was re-pinned to the source's latest block; the engine reconciles on its next pass"
        );
        Ok(EpochResponse {
            reset: true,
            source: source_url.to_string(),
        })
    }

    /// The journaling sink for the engine's own writes, when this deployment
    /// journals them. The block is the devnet's current coordinate — the scan
    /// cursor, or the head before the first scan — so the write always falls
    /// inside the range `/admin/rewind` walks (a head-anchored key could land
    /// *above* the cursor and be missed by the range).
    async fn journal_sink(&self) -> Result<Option<(&Journal, u64)>> {
        if self.journal_scope != crate::engine::JournalScope::OwnWrites {
            return Ok(None);
        }
        let cursor = self.plane.state.cursor()?.as_ref().map(|c| c.block);
        let block = match cursor {
            Some(block) => block,
            None => self.target.block_number().await?,
        };
        Ok(Some((&self.plane.journal, block)))
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
        if req.clear && req.slots.is_empty() && !req.account_fields && !req.all {
            return Err(Error::InvalidShadow {
                reason: "`clear` needs keys to hand back: name `slots`, `account_fields`, \
                         or `all` (a bare clear hands back nothing and looks like success)",
            });
        }
        let resynced = if req.clear {
            self.hand_back(req).await?
        } else {
            self.register(req)?;
            0
        };
        let shadow = self.plane.shadow.lock()?;
        Ok(ShadowOutcome {
            address: req.address,
            cleared: req.clear,
            resynced_keys: resynced,
            shadowed_slots: shadow.slot_count(),
            shadowed_accounts: shadow.accounts.len(),
        })
    }

    /// `POST /admin/shadow` — the hand-back with its race guard, shared by
    /// both engines so neither can forget it. A `clear` request flushes the
    /// engine's pending write scan first (the scanner is per-project: on T it
    /// is the applier's devnet-block scan, on V the fork driver's), then hands
    /// the keys back. Without the flush the hand-back can read a source value
    /// that predates the devnet's own writes and re-mark the stale value.
    /// Returns the outcome and the blocks the pre-step scan walked (0 when
    /// there was nothing to flush); the profile records the count.
    pub async fn shadow_hand_back(&self, req: &ShadowRequest) -> Result<(ShadowOutcome, crate::scan::ScanWalk)> {
        let walk = if req.clear {
            self.scan_to_head().await?
        } else {
            crate::scan::ScanWalk {
                walked: 0,
                empty: 0,
                shadowed: 0,
            }
        };
        Ok((self.shadow(req).await?, walk))
    }

    /// Register the devnet-owned keys (`clear: false`).
    fn register(&self, req: &ShadowRequest) -> Result<()> {
        let mut s = self.plane.shadow.lock()?;
        if req.account_fields {
            s.mark_account_fields(req.address);
        }
        for slot in &req.slots {
            s.mark_slot(req.address, *slot);
        }
        drop(s);
        self.plane.save_shadow()
    }

    /// Hand the keys back to parent sync and re-sync the source's truth
    /// (`clear: true`). Fail-closed: a source read that fails aborts with the
    /// marks intact, so a retry re-reads the same truth — a swallowed read
    /// would hand the key back holding a stale devnet value with no mark to
    /// explain it. Returns the number of re-synced keys.
    async fn hand_back(&self, req: &ShadowRequest) -> Result<usize> {
        let (slots, account_fields) = self.hand_back_keys(req)?;
        let anchor = self.hand_back_anchor().await?;
        // Read the source's truth *before* touching the marks: a key without a
        // shadow but with a stale devnet value is the most confusing state
        // there is, and a read fault must abort the hand-back (retryable), not
        // skip the key.
        let writes = self.source_truth(req, &slots, account_fields, anchor).await?;
        self.unmark_keys(req, &slots, account_fields)?;
        // Push the source's values now, then persist the registry. A fault
        // here leaves the marks in memory but not on disk; a restart restores
        // them (conservative: the key stays devnet-owned), and the next request
        // retries - the writes are absolute and idempotent.
        let resynced = mirror_anvil::apply_writes(self.target, &writes, self.chunk_size).await?;
        self.plane.save_shadow()?;
        Ok(resynced)
    }

    /// The keys a hand-back covers: the request's own, plus the account's whole
    /// set when `all` is named.
    fn hand_back_keys(&self, req: &ShadowRequest) -> Result<(Vec<U256>, bool)> {
        let mut slots = req.slots.clone();
        if req.all {
            let s = self.plane.shadow.lock()?;
            if let Some(set) = s.slots.get(&req.address) {
                let missing: Vec<U256> = set
                    .iter()
                    .filter(|slot| !slots.contains(slot))
                    .copied()
                    .collect();
                slots.extend(missing);
            }
        }
        Ok((slots, req.account_fields || req.all))
    }

    /// The block to re-sync from: the devnet's state reflects the last applied
    /// parent block, so reading the source's *head* would leave the devnet
    /// holding a future value. No cursor (a fresh chain) reads the head.
    async fn hand_back_anchor(&self) -> Result<u64> {
        let anchor = self
            .plane
            .state
            .lock()?
            .data
            .devnet
            .as_ref()
            .map(|c| c.block)
            .unwrap_or(0);
        if anchor == 0 {
            Ok(self.source.head().await?)
        } else {
            Ok(anchor)
        }
    }

    /// The source's values for the handed-back keys, read before any mark is
    /// touched (a fault here aborts with the marks intact — the retry re-reads
    /// the same truth).
    async fn source_truth(
        &self,
        req: &ShadowRequest,
        slots: &[U256],
        account_fields: bool,
        anchor: u64,
    ) -> Result<Vec<mirror_anvil::Write>> {
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
        for slot in slots {
            writes.push(mirror_anvil::Write::Storage {
                address: req.address,
                slot: *slot,
                value: U256::from_be_bytes(
                    self.source.storage_at(req.address, *slot, anchor).await?.0,
                ),
            });
        }
        Ok(writes)
    }

    /// Drop the handed-back keys' marks (the account's whole field set when
    /// `account_fields`).
    fn unmark_keys(&self, req: &ShadowRequest, slots: &[U256], account_fields: bool) -> Result<()> {
        let mut s = self.plane.shadow.lock()?;
        if account_fields {
            s.unmark_account_fields(req.address);
        }
        for slot in slots {
            s.unmark_slot(req.address, *slot);
        }
        Ok(())
    }

    /// `POST /admin/rewind` — undo the last N applied parent blocks using the
    /// journal. Returns the new cursor block and the number of restored values.
    pub async fn rewind(&self, blocks: u64) -> Result<RewindOutcome> {
        let (cursor, from) = self.rewind_plan(blocks)?;
        self.mark_reapply(from, cursor)?;
        let restored = self.restore_blocks(from, cursor).await?;
        self.reset_cursor_to(from - 1).await?;
        // The chain's *head* is still where it was - only the values were
        // rolled back - so the main loop's "diffs already applied"
        // late-completion path would skip the re-application and leave the
        // rewound range stale (measured: T's probes diverged after a rewind
        // drill). The range was marked for re-apply before the first undo.
        tracing::warn!(blocks, restored, "devnet rewound via journal");
        Ok(RewindOutcome {
            cursor: from - 1,
            restored,
        })
    }

    /// The rewind's static checks: the ring bounds the window (beyond it the
    /// older before-values are pruned, so a partial restore would be a lie),
    /// and the request must fit the cursor. Both are *static* properties of
    /// the request — checked before the state reads so the same request gets
    /// the same answer on every chain (the shared surface's symmetry;
    /// measured: with the cursor check first, a chain without a cursor
    /// answered "no cursor" while its peer answered "beyond the ring").
    /// Returns the cursor block and the first block to restore.
    fn rewind_plan(&self, blocks: u64) -> Result<(u64, u64)> {
        if blocks > self.plane.journal.keep() {
            return Err(Error::RewindBeyondRing {
                blocks,
                keep: self.plane.journal.keep(),
            });
        }
        let Some(cursor) = self.plane.state.cursor()? else {
            return Err(Error::NoCursor);
        };
        if blocks == 0 || blocks > cursor.block {
            return Err(Error::InvalidRewind {
                blocks,
                cursor: cursor.block,
            });
        }
        Ok((cursor.block, cursor.block - blocks + 1))
    }

    /// Mark the range for re-apply *before* rolling anything back: the
    /// applier's late-completion skip must be disabled for these blocks from
    /// the first undo, or a concurrent apply in this window could skip a block
    /// that is never re-applied.
    fn mark_reapply(&self, from: u64, upto: u64) -> Result<()> {
        let mut ra = self.plane.reapply_from.lock()?;
        *ra = Some(match *ra {
            Some((f, u)) => (f.min(from), u.max(upto)),
            None => (from, upto),
        });
        Ok(())
    }

    /// Walk the blocks that actually have before-values (newest first): a
    /// quiet block has no file — skipping it is "nothing to restore", not a
    /// fault, and a large range costs one directory read, not one probe per
    /// block. Returns the values restored.
    async fn restore_blocks(&self, from: u64, upto: u64) -> Result<usize> {
        let mut restored = 0usize;
        for b in self.plane.journal.blocks_in(from, upto)? {
            restored += self
                .plane
                .journal
                .undo_block(b, self.target, self.chunk_size)
                .await?;
        }
        Ok(restored)
    }

    /// Rewrite the cursor at the restored block. Fail-closed on the read: a
    /// fault must not silently clear the cursor (the applier would then resume
    /// at the chain head + 1 and skip parent blocks); only a *missing* block
    /// means "reset under us".
    async fn reset_cursor_to(&self, target: u64) -> Result<()> {
        if let Some(b) = self.target.block_by_number_opt(target, false).await? {
            crate::state::advance_cursor(&self.plane.state, &b, target)?;
        } else {
            self.plane.state.clear_cursor()?;
            self.plane.save_state()?;
        }
        Ok(())
    }

    /// `POST /admin/rewind` — the route's response: the request's block count
    /// plus the outcome, assembled once for both engines (the wire shape is
    /// part of the shared surface's symmetry).
    pub async fn rewind_response(&self, blocks: u64) -> Result<RewindResponse> {
        let out = self.rewind(blocks).await?;
        Ok(RewindResponse {
            rewound_blocks: blocks,
            restored_values: out.restored,
            cursor: out.cursor,
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
        self.plane.time_offset.store(seconds, Ordering::Relaxed);
        // Persist it: the offset is part of the devnet's state contract (its
        // blocks carry it), so a restart must keep it.
        self.plane.state.set_time_offset_seconds(seconds)?;
        self.plane.state.lock()?.save()?;
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
        self.plane.halted.store(true, Ordering::SeqCst);
        self.plane.metrics.set(&self.plane.metrics.halted, 1);
    }

    /// `POST /admin/resume` — clear the `/admin/pause` halt. The halt is the
    /// operator's gate (both engines share it), so resuming is the operator's
    /// acknowledgement that it was investigated.
    pub fn resume(&self) {
        self.plane.halted.store(false, Ordering::SeqCst);
        self.plane.metrics.set(&self.plane.metrics.halted, 0);
    }
}

/// The chain's fork base, when it is a fork (the re-pin's before/after probe).
async fn fork_base(target: &Rpc) -> Result<Option<u64>> {
    Ok(target.fork_config().await?.map(|(_, block)| block))
}

/// An applied-but-unanswered reset is a success, not a failure (the
/// retry-discipline rule: never report a fault for a completed effect). A
/// fork base that did not move means the reset never landed: the fault stands.
async fn confirm_reset(target: &Rpc, before: Option<u64>, e: mirror_anvil::Error) -> Result<()> {
    let after = fork_base(target).await?;
    if after == before {
        return Err(e.into());
    }
    tracing::warn!(
        error = %e,
        before = ?before,
        after = ?after,
        "anvil_reset answered with a transport fault but the fork base moved; treating it as applied"
    );
    Ok(())
}

/// `/admin/epoch`'s wire shape (both engines): the re-pin applied, and the
/// source it re-pinned to. Built by [`AdminSurface::epoch_repin`], so the two
/// engines' handlers serve one shared type — nothing per-crate to drift — and
/// the sim's `epoch_new` reads it back as the same type (the channel is ours
/// on both ends).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EpochResponse {
    pub reset: bool,
    pub source: String,
}

/// `/admin/rewind`'s wire shape (both engines): `cursor` is the profile's
/// devnet-chain cursor after the rewind — T's on the mainnet, V's on the
/// venue — named as `/status` names it (one fact, one name).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RewindResponse {
    pub rewound_blocks: u64,
    pub restored_values: usize,
    pub cursor: u64,
}

/// `/admin/pause` / `/admin/resume`'s wire shapes (both engines).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PauseResponse {
    pub paused: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResumeResponse {
    pub resumed: bool,
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
