//! The feed bridge's counters (its own set: the profile's `MainnetMetrics` and
//! the shared admin gauges are separate, and the three must not overlap —
//! tested in `crate::metrics`).
use std::sync::atomic::Ordering;
use std::sync::Mutex;

/// The bridge's metrics.
#[derive(Default)]
pub(crate) struct FeedBridgeMetrics {
    updates: Mutex<Vec<(String, u64)>>,
    pauses: Mutex<Vec<(String, u64)>>,
    /// Pass faults, whatever the stage: a source read, a target read, a
    /// publish, a decimals mismatch. The feed's `last_error` names the cause
    /// (measured: an uninstalled target is *waiting*, not a fault - the
    /// counter only moves when a pass actually fails).
    pass_errors: std::sync::atomic::AtomicU64,
}

impl FeedBridgeMetrics {
    pub fn inc_pass_errors(&self) {
        self.pass_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// One answer published to a feed's mock aggregator.
    pub fn record_update(&self, feed: &str) {
        Self::count(&self.updates, feed);
    }

    /// One feed paused because a scenario owns its value.
    pub fn record_pause(&self, feed: &str) {
        Self::count(&self.pauses, feed);
    }

    fn count(field: &Mutex<Vec<(String, u64)>>, name: &str) {
        let mut v = field.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match v.iter_mut().find(|(n, _)| n == name) {
            Some((_, c)) => *c += 1,
            None => v.push((name.to_string(), 1)),
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512);
        let series = |out: &mut String, name: &str, help: &str, values: &Mutex<Vec<(String, u64)>>| {
            let values = values.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for (feed, v) in values.iter() {
                diorama_devnet::metrics::push_counter(out, name, help, &[("feed", feed.as_str())], *v);
            }
        };
        series(&mut out, "feed_bridge_updates_total", "answers published to mock aggregators", &self.updates);
        series(&mut out, "feed_bridge_pauses_total", "feeds paused on a scenario's write", &self.pauses);
        diorama_devnet::metrics::push_counter(
            &mut out,
            "feed_bridge_pass_errors_total",
            "feed bridge passes that failed (the feed's last_error names the cause)",
            &[],
            self.pass_errors.load(Ordering::Relaxed),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bridge's counters render their labeled series (the venue's
    /// `feed/metrics.rs` pins its own the same way).
    #[test]
    fn renders_labeled_series() {
        let m = FeedBridgeMetrics::default();
        m.record_update("ETH/USD");
        m.record_update("ETH/USD");
        m.record_pause("BTC/USD");
        m.inc_pass_errors();
        let out = m.render();
        assert!(out.contains("feed_bridge_updates_total{feed=\"ETH/USD\"} 2"), "{out}");
        assert!(out.contains("feed_bridge_pauses_total{feed=\"BTC/USD\"} 1"), "{out}");
        assert!(out.contains("feed_bridge_pass_errors_total 1"), "{out}");
    }
}
