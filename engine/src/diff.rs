//! Diff applier: keeps T's watch-list state fresh by applying parent BALs.
//!
//! Per parent block (plan §4.2): fetch BAL -> watch list -> skip shadowed (COW)
//! -> pad hex -> read before-values -> batched cheatcode writes -> align block
//! context -> mine -> journal -> sampled parity check.
use crate::bal::{self, Write};
use crate::ctx::{cursor_from, Ctx};
use crate::rpc::Rpc;
use crate::journal::{entry_for_write, BeforeValue, JournalEntry};
use alloy_primitives::U256;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Instant;

pub struct ApplyOutcome {
    #[allow(dead_code)]
    pub block: u64,
    pub source: &'static str,
    pub writes: usize,
    pub filtered: usize,
    pub skipped_shadowed: usize,
    pub sampled: usize,
    pub sampled_ok: usize,
    pub duration_ms: u64,
}

/// Apply BAL-derived writes to a chain as batched cheatcode calls. Padding is
/// mandatory (measured: anvil rejects odd-length hex with -32602). Returns the
/// number of accepted writes.
pub async fn apply_writes(ctx: &Ctx, target: &Rpc, writes: &[Write]) -> Result<usize> {
    let mut calls: Vec<(String, Value)> = Vec::with_capacity(writes.len());
    for w in writes {
        calls.push(match w {
            Write::Storage { address, slot, value } => (
                "anvil_setStorageAt".into(),
                json!([address, bal::pad_data32(*slot), bal::pad_data32(*value)]),
            ),
            Write::Balance { address, value } => {
                ("anvil_setBalance".into(), json!([address, bal::quant_hex_u256(*value)]))
            }
            Write::Nonce { address, value } => {
                ("anvil_setNonce".into(), json!([address, bal::quant_hex_u64(*value)]))
            }
            Write::Code { address, value } => {
                ("anvil_setCode".into(), json!([address, bal::bytes_hex(value)]))
            }
        });
    }
    let mut applied = 0usize;
    for chunk in calls.chunks(ctx.cfg.loops.chunk_size) {
        for r in target.batch_raw(chunk).await? {
            match r {
                Ok(_) => applied += 1,
                Err(e) => tracing::warn!(error = %e, "bal write failed"),
            }
        }
    }
    Ok(applied)
}

pub async fn apply_block(ctx: &Ctx, n: u64) -> Result<ApplyOutcome> {
    let started = Instant::now();
    let block = ctx.reth.block_by_number(n, false).await?;

    let t_head = ctx.anvil_t.block_number().await?;
    // A journal rewind rolled T's values back without moving its head: those
    // blocks must be re-applied even though `t_head >= n`, otherwise the
    // late-completion skip below leaves the rolled-back values stale.
    let reapply = *ctx.reapply_from.lock().unwrap();
    let needs_reapply = matches!(reapply, Some((from, _)) if n >= from);
    if t_head >= n && !needs_reapply {
        // Late completion after a timeout: samples + cursor only.
        tracing::warn!(block = n, head = t_head, "diffs already applied; finalizing late completion");
        let t_block = ctx.anvil_t.block_by_number(n, false).await?;
        let first_block = ctx.cursor_t().map(|c| c.first_block).unwrap_or(n);
        ctx.set_cursor_t(cursor_from(&t_block, first_block));
        ctx.save_state()?;
        ctx.metrics.inc(&ctx.metrics.blocks_applied);
        ctx.metrics.set(&ctx.metrics.t_head, n);
        return Ok(ApplyOutcome {
            block: n,
            source: "late",
            writes: 0,
            filtered: 0,
            skipped_shadowed: 0,
            sampled: 0,
            sampled_ok: 0,
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }
    if !needs_reapply && t_head + 1 != n {
        bail!("T head {t_head} does not precede block {n}");
    }

    // 1. diffs: native BAL first, prestateTracer diff fallback.
    let (writes, source) = match ctx.reth.block_access_list(n).await? {
        Some(bal) => (bal::flatten_bal(&bal), "bal"),
        None => {
            let traces = ctx.reth.trace_block_prestate(n).await?;
            (bal::flatten_trace_block(&traces)?, "trace")
        }
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
        if ctx.is_shadowed(&w) {
            skipped_shadowed += 1;
            continue;
        }
        filtered.push(w);
    }

    // 3. before-values for the journal (read at T's current head).
    let mut entries: Vec<JournalEntry> = Vec::with_capacity(filtered.len());
    for w in &filtered {
        let before = match w {
            Write::Storage { address, slot, .. } => ctx
                .anvil_t
                .storage_at(*address, *slot, t_head)
                .await
                .ok()
                .map(|v| BeforeValue::Storage(U256::from_be_bytes(v.0))),
            Write::Balance { address, .. } => {
                ctx.anvil_t.balance_at(*address, t_head).await.ok().map(BeforeValue::Balance)
            }
            Write::Nonce { address, .. } => {
                ctx.anvil_t.nonce_at(*address, t_head).await.ok().map(BeforeValue::Nonce)
            }
            Write::Code { address, .. } => {
                ctx.anvil_t.code_at(*address, t_head).await.ok().map(BeforeValue::Code)
            }
        };
        if let Some(b) = before {
            entries.push(entry_for_write(w, b));
        }
    }

    // 4. batched writes (padding is mandatory - measured).
    let applied = apply_writes(ctx, &ctx.anvil_t, &filtered).await?;

    // 5. align block context + mine, so `latest` views (Aave-style time
    //    extrapolation) match the parent at N.
    let h = &block.header;
    let mut env: Vec<(String, Value)> = vec![
        ("anvil_setNextBlockTimestamp".into(), json!([h.timestamp])),
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

    // 6. journal + prune.
    ctx.journal.record(n, &entries)?;
    ctx.journal.prune(n);

    // 7. sampled storage parity vs reth at n (drift lens).
    let sample: Vec<&Write> = filtered
        .iter()
        .filter(|w| matches!(w, Write::Storage { .. }))
        .take(ctx.cfg.verifier.drift_sample_keys)
        .collect();
    let mut sampled_ok = 0usize;
    for w in &sample {
        if let Write::Storage { address, slot, .. } = w {
            match (
                ctx.anvil_t.storage_at(*address, *slot, n).await,
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

    // 8. cursor + metrics + drift.
    let t_block = ctx.anvil_t.block_by_number(n, false).await?;
    let first_block = ctx.cursor_t().map(|c| c.first_block).unwrap_or(n);
    ctx.set_cursor_t(cursor_from(&t_block, first_block));
    ctx.save_state()?;
    ctx.metrics.inc(&ctx.metrics.blocks_applied);
    ctx.metrics.set(&ctx.metrics.bal_writes, applied as u64);
    ctx.metrics
        .set(&ctx.metrics.bal_filtered_out, not_watched as u64);
    ctx.metrics
        .set(&ctx.metrics.bal_skipped_shadowed, skipped_shadowed as u64);
    let duration_ms = started.elapsed().as_millis() as u64;
    ctx.metrics.set(&ctx.metrics.last_apply_ms, duration_ms);
    ctx.metrics.set(&ctx.metrics.t_head, n);

    // The rewound range is fully re-applied: resume normal late-completion skips.
    if let Some((_, until)) = reapply {
        if n >= until {
            *ctx.reapply_from.lock().unwrap() = None;
            tracing::info!(block = n, until, "rewound range re-applied");
        }
    }

    let outcome = ApplyOutcome {
        block: n,
        source: if needs_reapply { "reapply" } else { source },
        writes: total,
        filtered: filtered.len(),
        skipped_shadowed,
        sampled: sample.len(),
        sampled_ok,
        duration_ms,
    };
    ctx.drift.append(&json!({
        "chain": "t",
        "block": n,
        "source": source,
        "bal_writes": total,
        "applied": applied,
        "filtered": outcome.filtered,
        "not_watched": not_watched,
        "skipped_shadowed": skipped_shadowed,
        "sampled": outcome.sampled,
        "sampled_ok": outcome.sampled_ok,
        "duration_ms": duration_ms,
    }));
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
