//! The bridge's metrics: what crossed the Core↔EVM boundary.
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct BridgeMetrics {
    /// EVM→Core credits applied.
    credits: AtomicU64,
    /// Core→EVM completions submitted.
    completions: AtomicU64,
    /// Actions/effects refused (insufficient balance, unrepresentable amount).
    refusals: AtomicU64,
    /// Action streams that did not decode.
    malformed: AtomicU64,
    /// Blocks whose callTracer failed (internal calls unseen that block).
    trace_faults: AtomicU64,
    /// Decoded actions by id (index = id; ids are ≤ 24 bits — the array is
    /// dense for the ids the docs define, and a rare unknown id saturates the
    /// last slot).
    actions: [AtomicU64; 18],
}

impl BridgeMetrics {
    pub fn inc_credits(&self) {
        self.credits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_completions(&self) {
        self.completions.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_refusals(&self) {
        self.refusals.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_malformed(&self) {
        self.malformed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_trace_faults(&self) {
        self.trace_faults.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count_action(&self, id: u32) {
        let slot = usize::try_from(id).unwrap_or(usize::MAX).min(self.actions.len() - 1);
        self.actions[slot].fetch_add(1, Ordering::Relaxed);
    }

    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        let c = |out: &mut String, name: &str, help: &str, v: u64| {
            mirror_devnet::metrics::push_metric(out, "counter", name, help, v);
        };
        c(&mut out, "venue_bridge_credits_total", "EVM→Core credits applied", self.credits.load(Ordering::Relaxed));
        c(&mut out, "venue_bridge_completions_total", "Core→EVM completions submitted", self.completions.load(Ordering::Relaxed));
        c(&mut out, "venue_bridge_refusals_total", "bridge effects refused", self.refusals.load(Ordering::Relaxed));
        c(&mut out, "venue_bridge_malformed_total", "undecodable CoreWriter action streams", self.malformed.load(Ordering::Relaxed));
        c(&mut out, "venue_bridge_trace_faults_total", "blocks whose callTracer failed", self.trace_faults.load(Ordering::Relaxed));
        for (id, count) in self.actions.iter().enumerate() {
            let v = count.load(Ordering::Relaxed);
            if v > 0 {
                // The per-id series is labeled (one per observed action).
                out.push_str(&format!(
                    "# HELP venue_bridge_actions_total decoded CoreWriter actions by id\n# TYPE venue_bridge_actions_total counter\nvenue_bridge_actions_total{{id=\"{id}\"}} {v}\n"
                ));
            }
        }
        out
    }
}
