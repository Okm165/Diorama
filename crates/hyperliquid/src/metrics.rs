//! The venue profile's metric set (plan §7.8): V's head/pin/epoch, the scan
//! cost, the engine-mined blocks, the at-pin probes. Rendered beside the
//! shared admin gauges (`mirror_devnet::metrics::Metrics::render`) — the split
//! keeps the shared crate's exposition to exactly what both profiles own (the
//! ethereum engine's mirror of this file is `EthereumMetrics`).

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
