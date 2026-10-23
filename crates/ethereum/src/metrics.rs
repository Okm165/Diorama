//! The ethereum profile's metric set (plan §7.8): reth/M/T heads, the
//! applier's counters, the convergence probes, the shadow-scan cost, the
//! source-sync top-up. Rendered beside the shared admin gauges
//! (`mirror_devnet::metrics::Metrics::render`) and the feed bridge's own set —
//! the split keeps the shared crate's exposition to exactly what both
//! profiles own (the venue's mirror of this file is `VenueMetrics`).
use mirror_devnet::metrics::push_metric;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct EthereumMetrics {
    pub reth_head: AtomicU64,
    pub reth_follow_head: AtomicU64,
    pub m_head: AtomicU64,
    pub t_head: AtomicU64,
    pub cursor_m: AtomicU64,
    pub cursor_t: AtomicU64,
    pub blocks_replayed: AtomicU64,
    pub blocks_applied: AtomicU64,
    pub divergences: AtomicU64,
    pub value_divergences: AtomicU64,
    pub bal_writes: AtomicU64,
    pub m_repair_writes: AtomicU64,
    pub m_rebase_skipped: AtomicU64,
    pub m_convergence_ok: AtomicU64,
    pub m_convergence_failed: AtomicU64,
    pub m_convergence_last_failed: AtomicU64,
    pub t_convergence_ok: AtomicU64,
    pub t_convergence_failed: AtomicU64,
    pub t_convergence_last_failed: AtomicU64,
    pub t_probe_skipped: AtomicU64,
    pub bal_filtered_out: AtomicU64,
    pub bal_skipped_shadowed: AtomicU64,
    /// T blocks the shadow scan walked (empty blocks cost one header read; no
    /// traces). The scan-cost reading: it grows ~1 per harness-mined block.
    pub shadow_scanned_blocks: AtomicU64,
    pub last_replay_ms: AtomicU64,
    pub last_apply_ms: AtomicU64,
    pub pin_age_blocks: AtomicU64,
    /// `T's head - T's fork block`: reads below the fork block are served by
    /// reth, whose state window is 10,064 blocks. Past that, any slot T has
    /// not written cannot be read at all - re-pin (make repin) before it.
    pub t_fork_age_blocks: AtomicU64,
    /// Times T was found re-forked under the engine (its OOM restart, a re-pin).
    /// The engine re-applies the devnet's recorded writes; the chain history is
    /// gone. A steady increase means T is restarting often (check its RSS).
    pub t_resets_total: AtomicU64,
    pub blob_failures: AtomicU64,
    pub receipts_unverified: AtomicU64,
    pub reorgs_detected: AtomicU64,
    /// EIP-8379 top-up: payloads delivered to reth and the remaining gap to the
    /// CL's head (what "self-contained catch-up" looks like in monitoring).
    pub source_sync_delivered: AtomicU64,
    pub source_sync_gap_blocks: AtomicU64,
}

#[allow(
    clippy::unused_self,
    reason = "the registry is the namespace; the field is the operand (call sites stay `metrics.set(...)`)"
)]
impl EthereumMetrics {
    pub fn set(&self, field: &AtomicU64, v: u64) {
        field.store(v, Ordering::Relaxed);
    }
    pub fn add(&self, field: &AtomicU64, v: u64) {
        field.fetch_add(v, Ordering::Relaxed);
    }

    pub fn inc(&self, field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }

    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        let g = |out: &mut String, name: &str, help: &str, v: u64| push_metric(out, "gauge", name, help, v);
        let c = |out: &mut String, name: &str, help: &str, v: u64| push_metric(out, "counter", name, help, v);
        g(&mut out, "mirror_reth_head", "reth chain head", self.reth_head.load(Ordering::Relaxed));
        g(&mut out, "mirror_reth_follow_head", "reth follow head (safe/latest/finalized)", self.reth_follow_head.load(Ordering::Relaxed));
        g(&mut out, "mirror_source_sync_gap_blocks", "CL head execution block - reth head (top-up catch-up progress)", self.source_sync_gap_blocks.load(Ordering::Relaxed));
        c(&mut out, "mirror_source_sync_delivered_total", "execution payloads delivered to reth via the Engine API (EIP-8379 top-up)", self.source_sync_delivered.load(Ordering::Relaxed));
        g(&mut out, "mirror_m_head", "anvil-m head", self.m_head.load(Ordering::Relaxed));
        g(&mut out, "mirror_t_head", "anvil-t head", self.t_head.load(Ordering::Relaxed));
        g(&mut out, "mirror_cursor_block", "last replayed parent block", self.cursor_m.load(Ordering::Relaxed));
        g(&mut out, "mirror_cursor_block_t", "last applied parent block", self.cursor_t.load(Ordering::Relaxed));
        g(&mut out, "mirror_lag_blocks", "follow head - replay cursor", self.reth_follow_head.load(Ordering::Relaxed).saturating_sub(self.cursor_m.load(Ordering::Relaxed)));
        c(&mut out, "mirror_t_resets_total", "times T was re-forked under the engine (devnet writes re-applied)", self.t_resets_total.load(Ordering::Relaxed));
        g(&mut out, "mirror_t_fork_age_blocks", "T head - T fork block (reth serves below it; 10,064-block window)", self.t_fork_age_blocks.load(Ordering::Relaxed));
        g(&mut out, "mirror_pin_age_blocks", "blocks since the first processed block in this epoch", self.pin_age_blocks.load(Ordering::Relaxed));
        c(&mut out, "mirror_shadow_scanned_blocks_total", "T blocks the shadow scan walked (empty blocks cost one header read, no traces)", self.shadow_scanned_blocks.load(Ordering::Relaxed));
        g(&mut out, "mirror_last_replay_seconds", "duration of the last block replay", self.last_replay_ms.load(Ordering::Relaxed) / 1000);
        g(&mut out, "mirror_last_apply_seconds", "duration of the last block diff application", self.last_apply_ms.load(Ordering::Relaxed) / 1000);
        c(&mut out, "mirror_blocks_replayed_total", "blocks replayed into M", self.blocks_replayed.load(Ordering::Relaxed));
        c(&mut out, "mirror_blocks_applied_total", "parent blocks applied into T", self.blocks_applied.load(Ordering::Relaxed));
        c(&mut out, "mirror_divergences_total", "fatal verification divergences", self.divergences.load(Ordering::Relaxed));
        c(&mut out, "mirror_value_divergences_total", "value-level divergences (blockhash-dependent; not fatal)", self.value_divergences.load(Ordering::Relaxed));
        c(&mut out, "mirror_reorgs_detected_total", "upstream reorgs detected", self.reorgs_detected.load(Ordering::Relaxed));
        g(&mut out, "mirror_bal_writes_last_block", "watch-list writes applied to T in the last applied parent block", self.bal_writes.load(Ordering::Relaxed));
        c(&mut out, "mirror_m_repair_writes_total", "BAL writes applied to converge M", self.m_repair_writes.load(Ordering::Relaxed));
        c(&mut out, "mirror_m_rebase_skipped_blocks_total", "parent blocks an M re-base skipped (fork base: state exact, tx verification skipped)", self.m_rebase_skipped.load(Ordering::Relaxed));
        c(&mut out, "mirror_m_convergence_ok_total", "post-repair probes matching mainnet", self.m_convergence_ok.load(Ordering::Relaxed));
        c(&mut out, "mirror_m_convergence_failures_total", "post-repair probes diverging (repair incomplete)", self.m_convergence_failed.load(Ordering::Relaxed));
        g(&mut out, "mirror_m_convergence_last_failed", "post-repair probes diverging in the last block", self.m_convergence_last_failed.load(Ordering::Relaxed));
        c(&mut out, "mirror_t_convergence_ok_total", "devnet probes matching mainnet at the applied parent block", self.t_convergence_ok.load(Ordering::Relaxed));
        c(&mut out, "mirror_t_convergence_failures_total", "devnet probes diverging from mainnet (devnet-owned accounts skipped)", self.t_convergence_failed.load(Ordering::Relaxed));
        g(&mut out, "mirror_t_convergence_last_failed", "devnet probes that diverged in the last applied block", self.t_convergence_last_failed.load(Ordering::Relaxed));
        c(&mut out, "mirror_t_probe_skipped_total", "probes skipped because their target is devnet-owned (a devnet write to a shared contract hides that contract's probes until it is handed back)", self.t_probe_skipped.load(Ordering::Relaxed));
        g(&mut out, "mirror_bal_filtered_out_last_block", "BAL writes in the last parent block filtered out (not watched)", self.bal_filtered_out.load(Ordering::Relaxed));
        g(&mut out, "mirror_bal_skipped_shadowed_last_block", "BAL writes in the last parent block skipped (shadowed by devnet)", self.bal_skipped_shadowed.load(Ordering::Relaxed));
        c(&mut out, "mirror_blob_failures_total", "blocks with blob sidecar/replay failures", self.blob_failures.load(Ordering::Relaxed));
        c(&mut out, "mirror_receipts_unverified_total", "blocks replayed without receipts verification", self.receipts_unverified.load(Ordering::Relaxed));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The series names a rendered exposition defines (help/type lines and
    /// label sets stripped).
    fn series(rendered: &str) -> BTreeSet<String> {
        rendered
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| l.split([' ', '{']).next().unwrap_or_default().to_string())
            .collect()
    }

    /// The three sets the engine's `/metrics` composes (the profile's, the
    /// shared admin gauges, the feed bridge's) must not overlap: a duplicate
    /// series is invalid exposition (the venue's
    /// `venue_and_admin_series_do_not_overlap` is the mirror of this test).
    #[test]
    fn profile_admin_and_bridge_series_do_not_overlap() {
        let profile = EthereumMetrics::default();
        let admin = mirror_devnet::metrics::Metrics::default();
        let bridge = crate::feed_bridge::FeedBridgeMetrics::default();
        let mut names = series(&profile.render());
        for other in [series(&admin.render()), series(&bridge.render())] {
            let dupes: Vec<String> = names.intersection(&other).cloned().collect();
            assert!(dupes.is_empty(), "duplicate series: {dupes:?}");
            names.extend(other);
        }
    }
}
