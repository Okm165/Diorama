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
/// `verifier.full_sweep` every applied storage key is checked, not just the
/// sample. Returns `(sampled, sampled_ok)`.
async fn sample_parity(ctx: &Ctx, n: u64, filtered: &[Write], t_latest: u64) -> (usize, usize) {
    let full = ctx.engine.verifier.full_sweep;
    let mut sample: Vec<Key> = filtered.iter().filter_map(storage_key).collect();
    if !full {
        sample.truncate(ctx.engine.verifier.drift_sample_keys);
    }
    let mut sampled_ok = 0usize;
    for key in &sample {
        if sample_key(ctx, n, t_latest, key, full).await {
            sampled_ok += 1;
        }
    }
    (sample.len(), sampled_ok)
}

/// The key of a storage write (`None` for account-field writes: only storage
/// is sampled).
fn storage_key(w: &Write) -> Option<Key> {
    match w {
        Write::Storage { address, slot, .. } => Some(Key::Storage {
            address: *address,
            slot: *slot,
        }),
        _ => None,
    }
}

/// Compare one storage key at the two chains' own anchors: T's latest block
/// against reth at n. Equal reads are the reading; a mismatch (or a fault on
/// either side) is the drift lens's warn — and, under `full_sweep`, a
/// counted mismatch.
async fn sample_key(ctx: &Ctx, n: u64, t_latest: u64, key: &Key, full: bool) -> bool {
    let Key::Storage { address, slot } = key else {
        return true;
    };
    match (
        ctx.anvil_t.storage_at(*address, *slot, t_latest).await,
        ctx.reth.storage_at(*address, *slot, n).await,
    ) {
        (Ok(a), Ok(b)) if a == b => true,
        (a, b) => {
            if full {
                ctx.mainnet_metrics.inc(&ctx.mainnet_metrics.full_sweep_mismatches);
            }
            tracing::warn!(
                block = n,
                address = %address,
                slot = %slot,
                "drift sample mismatch: t={a:?} reth={b:?}"
            );
            false
        }
    }
}

/// Probe parity for T (the plan's invariant: "probe parity on T after context
/// alignment"). Devnet-owned targets are skipped - their state is the
/// devnet's, not mainnet's.
async fn t_convergence_probes(ctx: &Ctx, n: u64) -> Result<()> {
    let probes = ProbeRunner::devnet(
        &ctx.engine.verifier.probes,
        &ctx.plane.shadow,
        ctx.plane.time_offset.load(Ordering::Relaxed) != 0,
    )
    .parity(&ctx.reth, n, &ctx.anvil_t)
    .await?;
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
    use super::storage_key;
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
        assert_eq!(storage_key(&storage), Some(Key::Storage { address, slot }));
        let balance = Write::Balance {
            address,
            value: U256::ZERO,
        };
        assert_eq!(storage_key(&balance), None);
    }
}
