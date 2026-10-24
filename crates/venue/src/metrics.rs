//! The venue profile's metric set (plan §7.8): V's head/pin/epoch, the scan
//! cost, the engine-mined blocks, the at-pin probes. Rendered beside the
//! shared admin gauges (`mirror_devnet::metrics::Metrics::render`) — the split
//! keeps the shared crate's exposition to exactly what both profiles own (the
//! ethereum engine's mirror of this file is `MainnetMetrics`).

mirror_devnet::registry! {
    pub struct VenueMetrics {
        /// V's head.
        gauge head = "venue_head", "anvil-v head";
        /// The last scanned V block.
        gauge cursor = "venue_cursor", "last scanned V block";
        /// V's fork base (the pin).
        gauge fork_pin = "venue_fork_pin", "V's fork base (the pin)";
        /// Blocks since the first scanned block in this epoch.
        gauge pin_age_blocks = "venue_pin_age_blocks", "blocks since the first scanned block in this epoch";
        /// Times V was found re-forked under the driver (a re-pin, an OOM): the
        /// epoch rule ran (clear or restore).
        counter epochs_total = "venue_epochs_total", "times V was found re-forked under the driver (epoch rule ran)";
        /// Writes re-applied by a same-pin restore.
        counter restore_writes_total = "venue_restore_writes_total", "writes re-applied by same-pin restores";
        /// V blocks the shadow scan walked (the harness mines one block per poll;
        /// empty blocks are skipped cheaply — this counter makes the cost visible,
        /// plan §17.4).
        counter scanned_blocks_total = "venue_scanned_blocks_total", "V blocks the shadow scan walked";
        /// The empty share of the walk (one call, no traces).
        counter skipped_empty_blocks_total = "venue_skipped_empty_blocks_total", "empty blocks the shadow scan skipped (one call, no traces)";
        /// V blocks the engine mined because the pool was non-empty: the engine is
        /// V's block producer (T's is the applier), so the bridge's boot txs and
        /// relay mints land without a harness mine.
        counter mined_blocks_total = "venue_mined_blocks_total", "V blocks the engine mined (the pool was non-empty; the engine is V's block producer)";
        counter probe_ok = "venue_probe_ok_total", "at-pin probes matching the fork source";
        counter probe_failed = "venue_probe_failures_total", "at-pin probes diverging from the fork source (devnet-owned skipped)";
        counter probe_skipped = "venue_probe_skipped_total", "probes skipped because their target is devnet-owned";
        /// Probes that diverged in the last at-pin run.
        gauge probe_last_failed = "venue_probe_last_failed", "probes that diverged in the last at-pin run";
        /// The poll composer's last window: addresses touched by the source's
        /// envelopes (txs, receipts, system txs).
        gauge poll_touched = "venue_poll_touched", "addresses touched in the last polled window";
        /// Keys the declared closures planned for that window.
        gauge poll_planned_keys = "venue_poll_planned_keys", "keys the declared closures planned in the last polled window";
        /// Keys whose source value differed from V (the writes applied).
        gauge poll_changed_keys = "venue_poll_changed_keys", "keys the poll found changed (writes emitted)";
        /// Log emitters no closure covers: a state change the declared watch
        /// model cannot follow (the audit signal; should stay 0 in a
        /// complete declaration).
        counter poll_uncovered_logs = "venue_poll_uncovered_logs_total", "logs from contracts no closure covers (declare a closure to follow them)";
        /// The source's head (the HL public API): the follow's target.
        gauge source_head = "venue_source_head", "the HL source's head (the follow's target)";
        /// The highest source block composed onto V (the follow cursor).
        gauge source_followed = "venue_source_followed", "highest source block composed onto V";
        /// Follow windows composed (one per pass with a non-empty window).
        counter follow_windows_total = "venue_follow_windows_total", "follow windows composed";
        /// Keys the follow applied to V (after the COW filter).
        counter follow_applied_total = "venue_follow_applied_total", "keys the follow applied to V";
        /// Keys the follow skipped because the devnet owns them (COW).
        counter follow_skipped_shadowed_total = "venue_follow_skipped_shadowed_total", "keys the follow skipped (devnet-owned, COW)";
        /// Follow windows paused by the gap guard.
        counter follow_gap_pauses_total = "venue_follow_gap_pauses_total", "follow windows paused (gap guard)";
        /// Source-continuity mismatches (a block's parent hash changed under
        /// the follow): a source reorg; the window is recomposed from latest.
        counter source_reorg_total = "venue_source_reorg_total", "source continuity mismatches (a parent hash changed under the follow)";
    }
}

/// The registry convention (`set`/`add`/`inc`) is the shared trait's; this
/// type adds only its own exposition.
impl mirror_devnet::metrics::Registry for VenueMetrics {}

impl VenueMetrics {
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        self.render_declared(&mut out);
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

    /// The venue's own set and the shared admin gauges must not overlap: the
    /// engine renders both (venue first, admin second), and a duplicate series
    /// would be invalid exposition (the mirror of the ethereum-side test).
    #[test]
    fn venue_and_admin_series_do_not_overlap() {
        let venue = VenueMetrics::default();
        let admin = mirror_devnet::metrics::Metrics::default();
        let dupes: Vec<String> = series(&venue.render())
            .intersection(&series(&admin.render()))
            .cloned()
            .collect();
        assert!(dupes.is_empty(), "duplicate series: {dupes:?}");
    }
}
