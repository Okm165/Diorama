//! The mainnet profile's metric set: reth/T heads, the applier's counters,
//! the convergence probes, the shadow-scan cost, the source-sync top-up.
//! Rendered beside the shared admin gauges
//! (`mirror_devnet::metrics::Metrics::render`) and the feed bridge's own set —
//! the split keeps the shared crate's exposition to exactly what both
//! profiles own (the venue's mirror of this file is `VenueMetrics`).
use mirror_devnet::metrics::push_metric;
use std::sync::atomic::Ordering;

mirror_devnet::registry! {
    pub struct MainnetMetrics {
        gauge reth_head = "mirror_reth_head", "reth chain head";
        gauge reth_follow_head = "mirror_reth_follow_head", "reth follow head (safe/latest/finalized)";
        gauge source_sync_gap_blocks = "mirror_source_sync_gap_blocks", "CL head execution block - reth head (top-up catch-up progress)";
        /// EIP-8379 top-up: payloads delivered to reth and the remaining gap to the
        /// CL's head (what "self-contained catch-up" looks like in monitoring).
        counter source_sync_delivered = "mirror_source_sync_delivered_total", "execution payloads delivered to reth via the Engine API (EIP-8379 top-up)";
        gauge head = "mirror_head", "the devnet chain's head (T)";
        gauge cursor = "mirror_cursor_block", "last applied parent block";
        /// Times T was found re-forked under the engine (its OOM restart, a re-pin).
        /// The engine re-applies the devnet's recorded writes; the chain history is
        /// gone. A steady increase means T is restarting often (check its RSS).
        counter t_resets_total = "mirror_t_resets_total", "times T was re-forked under the engine (devnet writes re-applied)";
        /// `T's head - T's fork block`: reads below the fork block are served by
        /// reth, whose state window is 10,064 blocks. Past that, any slot T has
        /// not written cannot be read at all - re-pin (make repin) before it.
        gauge t_fork_age_blocks = "mirror_t_fork_age_blocks", "T head - T fork block (reth serves below it; 10,064-block window)";
        gauge pin_age_blocks = "mirror_pin_age_blocks", "blocks since the first processed block in this epoch";
        /// T blocks the shadow scan walked (empty blocks cost one header read; no
        /// traces). The scan-cost reading: it grows ~1 per harness-mined block.
        counter shadow_scanned_blocks = "mirror_shadow_scanned_blocks_total", "T blocks the shadow scan walked (empty blocks cost one header read, no traces)";
        /// The empty share of the walk: one call, no traces (plan §17.4's
        /// empty-block cost reading — the harness mines one block per poll).
        counter shadow_skipped_empty_blocks = "mirror_shadow_skipped_empty_blocks_total", "empty blocks the shadow scan skipped (one call, no traces)";
        gauge last_apply_ms = "mirror_last_apply_seconds", "duration of the last block diff application" / 1000;
        counter blocks_applied = "mirror_blocks_applied_total", "parent blocks applied into T";
        counter reorgs_detected = "mirror_reorgs_detected_total", "upstream reorgs detected (applied diffs self-heal on the new branch)";
        gauge bal_writes = "mirror_bal_writes_last_block", "watch-list writes applied to T in the last applied parent block";
        counter t_convergence_ok = "mirror_t_convergence_ok_total", "devnet probes matching mainnet at the applied parent block";
        counter t_convergence_failed = "mirror_t_convergence_failures_total", "devnet probes diverging from mainnet (devnet-owned accounts skipped)";
        gauge t_convergence_last_failed = "mirror_t_convergence_last_failed", "devnet probes that diverged in the last applied block";
        counter t_probe_skipped = "mirror_t_probe_skipped_total", "probes skipped because their target is devnet-owned (a devnet write to a shared contract hides that contract's probes until it is handed back)";
        gauge bal_filtered_out = "mirror_bal_filtered_out_last_block", "BAL writes in the last parent block filtered out (not watched)";
        gauge bal_skipped_shadowed = "mirror_bal_skipped_shadowed_last_block", "BAL writes in the last parent block skipped (shadowed by devnet)";
        /// Full-sweep parity mismatches (`verifier.full_sweep`): every applied key
        /// is re-read from the source after the apply. Non-zero is a real finding.
        counter full_sweep_mismatches = "mirror_full_sweep_mismatches_total", "full-sweep parity mismatches (verifier.full_sweep)";
    }
}

/// The registry convention (`set`/`add`/`inc`) is the shared trait's; this
/// type adds only its own exposition.
impl mirror_devnet::metrics::Registry for MainnetMetrics {}

impl MainnetMetrics {
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        self.render_declared(&mut out);
        push_metric(&mut out, mirror_devnet::metrics::GAUGE, "mirror_lag_blocks", "follow head - applied cursor", self.reth_follow_head.load(Ordering::Relaxed).saturating_sub(self.cursor.load(Ordering::Relaxed)));
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
        let profile = MainnetMetrics::default();
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
