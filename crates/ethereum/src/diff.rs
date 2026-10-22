//! Diff applier: keeps T's watch-list state fresh by applying parent BALs.
//!
//! Per parent block (plan §4.2): fetch BAL -> watch list -> skip shadowed (COW)
//! -> pad hex -> read before-values -> batched cheatcode writes -> align block
//! context -> mine -> journal -> sampled parity check.
use crate::bal::{self, Write};
use crate::ctx::{cursor_from, Ctx};
use crate::error::{Error, Result};
use crate::journal::{entry_for_write, BeforeValue, JournalEntry};
use alloy_primitives::U256;
use mirror_devnet::probes::ProbeRunner;
use serde_json::{json, Value};
use std::time::Instant;

pub struct ApplyOutcome {
    pub source: &'static str,
    pub writes: usize,
    pub filtered: usize,
    pub skipped_shadowed: usize,
    pub sampled: usize,
    pub sampled_ok: usize,
    pub duration_ms: u64,
}

#[allow(
    clippy::too_many_lines,
    reason = "R0 freeze: verbatim; the split is a deferred follow-up refactor"
)]
pub async fn apply_block(ctx: &Ctx, n: u64) -> Result<ApplyOutcome> {
    let started = Instant::now();
    let block = ctx.reth.block_by_number(n, false).await?;

    let t_head = ctx.anvil_t.block_number().await?;
    // A journal rewind rolled T's values back without moving its head: those
    // blocks must be re-applied even though `t_head >= n`, otherwise the
    // late-completion skip below leaves the rolled-back values stale.
    let reapply = *ctx.reapply_from.lock()?;
    let needs_reapply = matches!(reapply, Some((from, _)) if n >= from);
    // The journal is the discriminator between "this block's writes are already
    // in" (a timed-out apply that finished late) and "T's head is ahead because
    // devnet txs were mined in their own prepend block". A journal fault is an
    // error, never "no journal": guessing would re-journal already-applied
    // values as before-values.
    let journaled = ctx.journal.has_entry(n)?;
    if t_head >= n && journaled && !needs_reapply {
        // Late completion after a timeout: the writes are already in (the
        // journal has the block). Samples + cursor only.
        tracing::warn!(block = n, head = t_head, "diffs already applied; finalizing late completion");
        let t_block = ctx.anvil_t.block_by_number(n, false).await?;
        let first_block = ctx.cursor_t()?.map(|c| c.first_block).unwrap_or(n);
        ctx.set_cursor_t(cursor_from(&t_block, first_block))?;
        ctx.save_state()?;
        ctx.metrics.inc(&ctx.metrics.blocks_applied);
        ctx.metrics.set(&ctx.metrics.t_head, t_head);
        ctx.metrics.set(&ctx.metrics.cursor_t, n);
        return Ok(ApplyOutcome {
            source: "late",
            writes: 0,
            filtered: 0,
            skipped_shadowed: 0,
            sampled: 0,
            sampled_ok: 0,
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }
    // T's head may be *ahead* of the cursor (devnet prepend blocks) - that is
    // normal. Behind by more than one block is not: that means someone reset T
    // outside the engine (anvil_reset), which the incremental diff applier
    // cannot recover from - alarm and keep retrying instead of corrupting state.
    if t_head + 1 < n {
        return Err(Error::ApplierBehind { t_head, n });
    }

    // 0. Prepend rule (plan, "Prepend rules"): mine the devnet's pending txs in
    //    their own block *before* this parent's diffs land. Then the parent's
    //    block on T carries only the parent's effects, and a devnet tx executes
    //    against the same state its gas was estimated on (measured: anvil's
    //    auto-gas for eth_sendTransaction came out ~5% short when the diffs
    //    landed between estimate and execution, so the tx ran out of gas).
    //    Bounded loop: a tx that arrives *while* this apply is writing diffs
    //    would otherwise be mined together with them, executing against a state
    //    its gas was never estimated on (measured: an Aave borrow estimated
    //    pre-diff came out ~5% short and reverted). Mine until the pool drains,
    //    at most 3 rounds so a steady stream cannot starve the parent block.
    let mut rounds = 0;
    while rounds < 3 {
        let (pending, _queued) = match ctx.anvil_t.txpool_status().await {
            Ok(status) => status,
            Err(e) => {
                // The prepend is a gas-estimate safeguard; a txpool fault skips
                // it (named here, bounded: the next apply tries again).
                tracing::debug!(block = n, error = %e, "prepend skipped: txpool unavailable");
                break;
            }
        };
        if pending == 0 {
            break;
        }
        rounds += 1;
        let last = ctx.anvil_t.block_by_number(ctx.anvil_t.block_number().await?, false).await?;
        if let Err(e) = ctx
            .anvil_t
            .batch_raw(&[(
                "anvil_setNextBlockTimestamp".into(),
                json!([last.header.timestamp + 1]),
            )])
            .await
        {
            // The prepend block's timestamp drifts to anvil's default; the
            // parent's own context alignment below still sets the *applied*
            // block's timestamp, so this is a named, bounded degradation.
            tracing::debug!(block = n, error = %e, "prepend timestamp not set");
        }
        ctx.anvil_t.anvil_mine(1).await?;
        tracing::info!(block = n, pending, round = rounds, "mined devnet txs before the parent diffs");
    }

    // 1. diffs: native BAL first, prestateTracer diff fallback.
    let (writes, source) = if let Some(bal) = ctx.reth.block_access_list(n).await? {
        (bal::flatten_bal(&bal), "bal")
    } else {
        let traces = ctx.reth.trace_block_prestate(n).await?;
        (bal::flatten_trace_block(&traces)?, "trace")
    };
    let total = writes.len();

    // 2. watch list + COW (skip shadowed keys; account fields as one unit).
    let mut filtered: Vec<Write> = Vec::new();
    let mut skipped_shadowed = 0usize;
    let mut not_watched = 0usize;
    for w in writes {
        if !ctx.watchlist.contains(&w.address()) {
            not_watched += 1;
            continue;
        }
        if ctx.is_shadowed(&w)? {
            skipped_shadowed += 1;
            continue;
        }
        filtered.push(w);
    }

    // 3. before-values for the journal (read at T's current head). A read that
    //    fails fails the apply: a journal with a missing before-value would let
    //    a later rewind report success while leaving that key un-restored.
    let mut entries: Vec<JournalEntry> = Vec::with_capacity(filtered.len());
    for w in &filtered {
        let before = match w {
            Write::Storage { address, slot, .. } => {
                let v = ctx.anvil_t.storage_at(*address, *slot, t_head).await?;
                BeforeValue::Storage(U256::from_be_bytes(v.0))
            }
            Write::Balance { address, .. } => {
                BeforeValue::Balance(ctx.anvil_t.balance_at(*address, t_head).await?)
            }
            Write::Nonce { address, .. } => {
                BeforeValue::Nonce(ctx.anvil_t.nonce_at(*address, t_head).await?)
            }
            Write::Code { address, .. } => {
                BeforeValue::Code(ctx.anvil_t.code_at(*address, t_head).await?)
            }
        };
        entries.push(entry_for_write(w, before)?);
    }

    // 4. batched writes (padding is mandatory - measured).
    let applied = mirror_anvil::apply_writes(
        &ctx.anvil_t,
        &filtered,
        ctx.cfg.services.engine.loops.chunk_size,
    )
    .await?;

    // 5. align block context + mine, so `latest` views (Aave-style time
    //    extrapolation) match the parent at N. Always mined: the parent's
    //    effects (diffs + context) must land in their own block, and the cursor
    //    advances once per parent regardless of how many devnet blocks T mined.
    {
        let h = &block.header;
        // Align to the parent, plus the devnet's time offset. Clamped to
        // last+1 so a deliberate `evm_increaseTime` (or a lowered offset)
        // can never make the timestamp non-monotonic and fail the alignment -
        // anvil would then mine with its own default and the context would
        // silently drift (measured).
        let offset = ctx.time_offset.load(std::sync::atomic::Ordering::Relaxed);
        let last_ts = ctx
            .anvil_t
            .block_by_number(ctx.anvil_t.block_number().await?, false)
            .await?
            .header
            .timestamp;
        let ts = (h.timestamp + offset).max(last_ts.saturating_add(1));
        let mut env: Vec<(String, Value)> = vec![
            ("anvil_setNextBlockTimestamp".into(), json!([ts])),
            ("anvil_setCoinbase".into(), json!([h.beneficiary])),
            ("anvil_setNextBlockPrevRandao".into(), json!([h.mix_hash])),
        ];
        if let Some(bf) = h.base_fee_per_gas {
            env.push((
                "anvil_setNextBlockBaseFeePerGas".into(),
                json!([format!("0x{bf:x}")]),
            ));
        }
        for r in ctx.anvil_t.batch_raw(&env).await? {
            if let Err(e) = r {
                tracing::warn!(error = %e, "context alignment failed");
            }
        }
        ctx.anvil_t.anvil_mine(1).await?;
    }

    // 6. journal + prune.
    ctx.journal.record(n, &entries)?;
    ctx.journal.prune(n);

    // 6b. scan this apply's own devnet blocks *before* asserting parity: the
    //     prepend loop may have mined devnet txs whose writes are not marked
    //     shadowed yet (the shadow loop polls every ~2 s), and the probe parity
    //     must skip devnet-owned accounts (copy-on-write) instead of alarming on
    //     the devnet's own writes. Measured: spurious aave_weth_price /
    //     univ3_weth_usdc_slot0 mismatches right after a venue interaction.
    let t_head_now = ctx.anvil_t.block_number().await?;
    if let Err(e) = crate::scan_range(ctx, t_head, t_head_now).await {
        // The marks already in memory are kept and the walk retries from the
        // last clean block (the shadow loop scans every ~2 s), so this is a
        // named, bounded degradation - not a silent skip.
        tracing::warn!(error = %e, "inline shadow scan incomplete");
    }

    // 7. sampled storage parity vs reth at n (drift lens). T's *height* drifts
    //    ahead of the parent height (devnet prepend blocks), so compare T's
    //    latest block against reth at n - reading T by height compared
    //    unrelated blocks and produced ~200 false mismatches per 30 min.
    let t_latest = ctx.anvil_t.block_number().await?;
    let sample: Vec<&Write> = filtered
        .iter()
        .filter(|w| matches!(w, Write::Storage { .. }))
        .take(ctx.cfg.services.engine.verifier.drift_sample_keys)
        .collect();
    let mut sampled_ok = 0usize;
    for w in &sample {
        if let Write::Storage { address, slot, .. } = w {
            match (
                ctx.anvil_t.storage_at(*address, *slot, t_latest).await,
                ctx.reth.storage_at(*address, *slot, n).await,
            ) {
                (Ok(a), Ok(b)) if a == b => sampled_ok += 1,
                (a, b) => tracing::warn!(
                    block = n,
                    address = %address,
                    slot = %slot,
                    "drift sample mismatch: t={a:?} reth={b:?}"
                ),
            }
        }
    }

    // 7b. probe parity for T (the plan's invariant: "probe parity on T after
    //     context alignment"). Devnet-owned targets are skipped - their state
    //     is the devnet's, not mainnet's.
    let probes = ProbeRunner::devnet(
        &ctx.cfg.services.engine.verifier.probes,
        &ctx.shadow,
        ctx.time_offset.load(std::sync::atomic::Ordering::Relaxed) != 0,
    )
    .parity(&ctx.reth, n, &ctx.anvil_t)
    .await?;
    ctx.metrics.add(&ctx.metrics.t_convergence_ok, probes.ok as u64);
    ctx.metrics
        .add(&ctx.metrics.t_convergence_failed, probes.failed as u64);
    ctx.metrics
        .set(&ctx.metrics.t_convergence_last_failed, probes.failed as u64);
    ctx.metrics
        .add(&ctx.metrics.t_probe_skipped, probes.skipped as u64);

    // 8. cursor + metrics + drift.
    let t_block = ctx.anvil_t.block_by_number(n, false).await?;
    let first_block = ctx.cursor_t()?.map(|c| c.first_block).unwrap_or(n);
    ctx.set_cursor_t(cursor_from(&t_block, first_block))?;
    ctx.metrics.set(&ctx.metrics.cursor_t, n);
    ctx.save_state()?;
    ctx.metrics.inc(&ctx.metrics.blocks_applied);
    ctx.metrics.set(&ctx.metrics.bal_writes, applied as u64);
    ctx.metrics
        .set(&ctx.metrics.bal_filtered_out, not_watched as u64);
    ctx.metrics
        .set(&ctx.metrics.bal_skipped_shadowed, skipped_shadowed as u64);
    let duration_ms = started.elapsed().as_millis() as u64;
    ctx.metrics.set(&ctx.metrics.last_apply_ms, duration_ms);
    ctx.metrics.set(&ctx.metrics.t_head, t_latest);

    // The rewound range is fully re-applied: resume normal late-completion skips.
    if let Some((_, until)) = reapply {
        if n >= until {
            *ctx.reapply_from.lock()? = None;
            tracing::info!(block = n, until, "rewound range re-applied");
        }
    }

    let outcome = ApplyOutcome {
        source: if needs_reapply { "reapply" } else { source },
        writes: total,
        filtered: filtered.len(),
        skipped_shadowed,
        sampled: sample.len(),
        sampled_ok,
        duration_ms,
    };
    // Divergence log, not a flight recorder: only anomalies land here (the
    // re-apply of a rewound range, or a sampled-parity mismatch). Counts live
    // in the metrics.
    if needs_reapply || outcome.sampled_ok < outcome.sampled {
        ctx.drift.append(&json!({
            "chain": "t",
            "block": n,
            "source": if needs_reapply { "reapply" } else { source },
            "bal_writes": total,
            "applied": applied,
            "filtered": outcome.filtered,
            "not_watched": not_watched,
            "skipped_shadowed": skipped_shadowed,
            "sampled": outcome.sampled,
            "sampled_ok": outcome.sampled_ok,
            "duration_ms": duration_ms,
        }))?;
    }
    tracing::info!(
        block = n,
        source,
        bal_writes = total,
        applied,
        filtered = outcome.filtered,
        skipped_shadowed,
        sampled_ok = outcome.sampled_ok,
        sampled = outcome.sampled,
        ms = duration_ms,
        "parent diffs applied to T"
    );
    Ok(outcome)
}
