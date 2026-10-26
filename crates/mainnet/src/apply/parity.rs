//! The apply's measurement: the drift lens (sampled storage keys vs reth at
//! the applied block) and, when the profile probes per block, the convergence
//! probe set.
use crate::ctx::Ctx;
use crate::error::Result;
use mirror_anvil::{Key, Write};
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::probes::ProbeRunner;
use mirror_devnet::spine::ProbeSchedule;
use std::sync::atomic::Ordering;

/// The apply's parity step: the drift lens (sampled storage keys vs reth at
/// n) and — when the profile probes per block — the convergence probe set.
pub(super) async fn parity_step(
    ctx: &Ctx,
    n: u64,
    writes: &[Write],
    t_latest: u64,
) -> Result<(usize, usize)> {
    let (sampled, sampled_ok) = sample_parity(ctx, n, writes, t_latest).await;
    if ctx.mirror.policies().probes == ProbeSchedule::PerBlock {
        t_convergence_probes(ctx, n).await?;
    }
    Ok((sampled, sampled_ok))
}

/// One sampled storage-parity reading vs reth at n (drift lens). T's *height*
/// drifts ahead of the parent height (devnet prepend blocks), so compare T's
/// latest block against reth at n - reading T by height compared unrelated
/// blocks and produced ~200 false mismatches per 30 min. With
/// `verifier.sweep = "full"` every applied storage key is checked, not just
/// the sample. Returns `(sampled, sampled_ok)`.
async fn sample_parity(ctx: &Ctx, n: u64, filtered: &[Write], t_latest: u64) -> (usize, usize) {
    let full = full_sweep(ctx);
    let mut sample: Vec<Key> = filtered.iter().filter_map(Write::storage_key).collect();
    if !full {
        sample.truncate(ctx.engine.verifier.drift_sample_keys);
    }
    if sample.is_empty() {
        return (0, 0);
    }
    // Both sides' reads ride one batch each (≤20 keys per body — the client
    // chunks transparently), concurrently: different endpoints, one body in
    // flight each. A full sweep's sample is every applied storage key; the
    // per-key form cost two round trips per key, serialized.
    let Some(calls) = sample_calls(n, t_latest, &sample) else {
        return (sample.len(), 0);
    };
    // T's side is the local anvil: its own 500-item contract (one body for
    // a full sweep); reth keeps the conservative public cap (its ceiling is
    // not documented here — 1000 answered live once, not relied on).
    let (t_answers, r_answers) = match tokio::try_join!(
        ctx.anvil_t
            .batch_limited(&calls.t, mirror_anvil::ANVIL_BATCH_MAX_ITEMS, 1 << 20),
        ctx.reth.batch(&calls.reth)
    ) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(block = n, keys = sample.len(), error = %e, "drift sample read failed (the batch body)");
            return (sample.len(), 0);
        }
    };
    let mut sampled_ok = 0usize;
    for ((key, t), r) in sample.iter().zip(t_answers).zip(r_answers) {
        if sample_verdict(ctx, n, key, &t, &r) {
            sampled_ok += 1;
        }
    }
    (sample.len(), sampled_ok)
}

/// Both sides' call plans for the drift sample: T at the parent block, reth
/// at its own latest.
struct SampleCalls {
    t: Vec<mirror_anvil::Call>,
    reth: Vec<mirror_anvil::Call>,
}

/// The sample's call plans; `None` when a plan cannot encode (already warned;
/// every sampled key then counts not-ok).
fn sample_calls(n: u64, t_latest: u64, sample: &[Key]) -> Option<SampleCalls> {
    let plan = |at: u64| match mirror_anvil::Rpc::key_calls(sample, at) {
        Ok(calls) => Some(calls),
        Err(e) => {
            tracing::warn!(block = n, keys = sample.len(), error = %e, "drift sample: the call plan failed");
            None
        }
    };
    Some(SampleCalls {
        t: plan(t_latest)?,
        reth: plan(n)?,
    })
}

/// Whether the profile verifies every applied key (the full sweep) or the
/// configured sample — the one spelling of the switch.
fn full_sweep(ctx: &Ctx) -> bool {
    ctx.engine.verifier.sweep == mirror_config::Sweep::Full
}

/// One sampled key's verdict: `true` = the two chains' words match; a drift
/// warns (and, under a full sweep, counts); a read failure warns as itself —
/// never as drift.
fn sample_verdict<E: std::fmt::Display>(
    ctx: &Ctx,
    n: u64,
    key: &Key,
    t: &std::result::Result<Box<serde_json::value::RawValue>, E>,
    r: &std::result::Result<Box<serde_json::value::RawValue>, E>,
) -> bool {
    let full = full_sweep(ctx);
    let Key::Storage { address, slot } = key else {
        // The sample is storage-only by construction; keep the old counting
        // shape if that ever changes.
        return true;
    };
    match (word_of(t), word_of(r)) {
        (Ok(a), Ok(b)) if a == b => true,
        (Ok(a), Ok(b)) => {
            if full {
                ctx.mainnet_metrics.inc(&ctx.mainnet_metrics.full_sweep_mismatches);
            }
            tracing::warn!(
                block = n,
                address = %address,
                slot = %slot,
                "drift sample mismatch: t={a} reth={b}"
            );
            false
        }
        (a, b) => {
            // A read failure is counted (never `sampled_ok`), never confused
            // with drift: the old form logged it as a mismatch and sent
            // operators to drift.jsonl for an RPC hiccup.
            let cause = a.err().or_else(|| b.err()).unwrap_or_default();
            tracing::warn!(block = n, address = %address, slot = %slot, error = %cause, "drift sample read failed");
            false
        }
    }
}

/// One `eth_getStorageAt` answer, decoded: a null or a non-word answer is a
/// read failure, never a zero word.
fn word_of<E: std::fmt::Display>(
    answer: &std::result::Result<Box<serde_json::value::RawValue>, E>,
) -> std::result::Result<alloy_primitives::B256, String> {
    match answer {
        Ok(raw) => serde_json::from_str(raw.get()).map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Probe parity for T (the plan's invariant: "probe parity on T after context
/// alignment"). Devnet-owned targets are skipped - their state is the
/// devnet's, not mainnet's.
async fn t_convergence_probes(ctx: &Ctx, n: u64) -> Result<()> {
    let probes = ProbeRunner::devnet(
        &ctx.engine.verifier.probes,
        &ctx.plane.shadow,
        match ctx.plane.time_offset.load(Ordering::Relaxed) {
            0 => mirror_devnet::probes::ClockOffset::Aligned,
            _ => mirror_devnet::probes::ClockOffset::Offset,
        },
    )
    .parity(&ctx.reth, n, &ctx.anvil_t)
    .await?;
    // The same summary line the venue's at-pin run reports (one shape for
    // both profiles): the per-probe skips stay DEBUG diagnostics.
    mirror_devnet::probes::report(&ctx.plane, ctx.mirror.label(), n, &probes);
    ctx.mainnet_metrics.add(&ctx.mainnet_metrics.t_convergence_ok, probes.ok as u64);
    ctx.mainnet_metrics
        .add(&ctx.mainnet_metrics.t_convergence_failed, probes.failed as u64);
    ctx.mainnet_metrics
        .set(&ctx.mainnet_metrics.t_convergence_last_failed, probes.failed as u64);
    ctx.mainnet_metrics
        .add(&ctx.mainnet_metrics.t_probe_skipped, probes.skipped as u64);
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use mirror_anvil::{Key, Write};

    /// Only storage writes are sampled (the drift lens); account-field writes
    /// carry no storage key (the venue's live parity re-reads the composed
    /// BAL's storage keys the same way).
    #[test]
    fn the_parity_lens_samples_only_storage_writes() {
        let address = Address::repeat_byte(0x11);
        let slot = U256::from(7);
        let storage = Write::Storage {
            address,
            slot,
            value: U256::ZERO,
        };
        assert_eq!(storage.storage_key(), Some(Key::Storage { address, slot }));
        let balance = Write::Balance {
            address,
            value: U256::ZERO,
        };
        assert_eq!(balance.storage_key(), None);
    }
}
