//! The venue profile's metric set (plan §7.8): V's head/pin/epoch, the scan
//! cost, the engine-mined blocks, the at-pin probes. Rendered beside the
//! shared admin gauges (`mirror_devnet::metrics::Metrics::render`) — the split
//! keeps the shared crate's exposition to exactly what both profiles own (the
//! ethereum engine's mirror of this file is `EthereumMetrics`).
use mirror_devnet::metrics::{counter, gauge};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct VenueMetrics {
    /// V's head.
    pub head: AtomicU64,
    /// The last scanned V block.
    pub cursor: AtomicU64,
    /// V's fork base (the pin).
    pub fork_pin: AtomicU64,
    /// Blocks since the first scanned block in this epoch.
    pub pin_age_blocks: AtomicU64,
    /// Times V was found re-forked under the driver (a re-pin, an OOM): the
    /// epoch rule ran (clear or restore).
    pub epochs_total: AtomicU64,
    /// Writes re-applied by a same-pin restore.
    pub restore_writes_total: AtomicU64,
    /// V blocks the shadow scan walked (the harness mines one block per poll;
    /// empty blocks are skipped cheaply — this counter makes the cost visible,
    /// plan §17.4).
    pub scanned_blocks_total: AtomicU64,
    /// V blocks the engine mined because the pool was non-empty: the engine is
    /// V's block producer (T's is the applier), so the bridge's boot txs and
    /// relay mints land without a harness mine.
    pub mined_blocks_total: AtomicU64,
    pub probe_ok: AtomicU64,
    pub probe_failed: AtomicU64,
    pub probe_skipped: AtomicU64,
    /// Probes that diverged in the last at-pin run.
    pub probe_last_failed: AtomicU64,
}

/// The registry convention (`set`/`add`/`inc`) is the shared trait's; this
/// type adds only its own exposition.
impl mirror_devnet::metrics::Registry for VenueMetrics {}

impl VenueMetrics {
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        gauge(&mut out, "venue_head", "anvil-v head", self.head.load(Ordering::Relaxed));
        gauge(&mut out, "venue_cursor", "last scanned V block", self.cursor.load(Ordering::Relaxed));
        gauge(&mut out, "venue_fork_pin", "V's fork base (the pin)", self.fork_pin.load(Ordering::Relaxed));
        gauge(&mut out, "venue_pin_age_blocks", "blocks since the first scanned block in this epoch", self.pin_age_blocks.load(Ordering::Relaxed));
        counter(&mut out, "venue_epochs_total", "times V was found re-forked under the driver (epoch rule ran)", self.epochs_total.load(Ordering::Relaxed));
        counter(&mut out, "venue_restore_writes_total", "writes re-applied by same-pin restores", self.restore_writes_total.load(Ordering::Relaxed));
        counter(&mut out, "venue_scanned_blocks_total", "V blocks the shadow scan walked", self.scanned_blocks_total.load(Ordering::Relaxed));
        counter(&mut out, "venue_mined_blocks_total", "V blocks the engine mined (the pool was non-empty; the engine is V's block producer)", self.mined_blocks_total.load(Ordering::Relaxed));
        counter(&mut out, "venue_probe_ok_total", "at-pin probes matching the fork source", self.probe_ok.load(Ordering::Relaxed));
        counter(&mut out, "venue_probe_failures_total", "at-pin probes diverging from the fork source (devnet-owned skipped)", self.probe_failed.load(Ordering::Relaxed));
        counter(&mut out, "venue_probe_skipped_total", "probes skipped because their target is devnet-owned", self.probe_skipped.load(Ordering::Relaxed));
        gauge(&mut out, "venue_probe_last_failed", "probes that diverged in the last at-pin run", self.probe_last_failed.load(Ordering::Relaxed));
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
