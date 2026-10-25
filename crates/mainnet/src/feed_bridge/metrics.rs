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
        let series = |out: &mut String, name: &str, help: &str, kind: &str, values: &Mutex<Vec<(String, u64)>>| {
            let values = values.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for (feed, v) in values.iter() {
                out.push_str(&format!(
                    "# HELP {name} {help}\n# TYPE {name} {kind}\n{name}{{feed=\"{feed}\"}} {v}\n"
                ));
            }
        };
        let counter = mirror_devnet::metrics::COUNTER;
        series(&mut out, "feed_bridge_updates_total", "answers published to mock aggregators", counter, &self.updates);
        series(&mut out, "feed_bridge_pauses_total", "feeds paused on a scenario's write", counter, &self.pauses);
        mirror_devnet::metrics::push_metric(
            &mut out,
            counter,
            "feed_bridge_pass_errors_total",
            "feed bridge passes that failed (the feed's last_error names the cause)",
            self.pass_errors.load(Ordering::Relaxed),
        );
        out
    }
}

