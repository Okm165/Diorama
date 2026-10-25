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
    /// V head minus the processed cursor: the plan §7.8's `venue_bridge_pending`
    /// intent — a growing lag is a stuck bridge, a zero lag is caught up.
    lag_blocks: AtomicU64,
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

    pub fn inc_refusals_by(&self, n: u64) {
        self.refusals.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_malformed(&self) {
        self.malformed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_trace_faults(&self) {
        self.trace_faults.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_lag_blocks(&self, lag: u64) {
        self.lag_blocks.store(lag, Ordering::Relaxed);
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
        mirror_devnet::metrics::push_metric(
            &mut out,
            "gauge",
            "venue_bridge_lag_blocks",
            "V head minus the bridge's processed cursor (growing = stuck)",
            self.lag_blocks.load(Ordering::Relaxed),
        );
        // One family, one HELP/TYPE, then the observed series.
        if self.actions.iter().any(|c| c.load(Ordering::Relaxed) > 0) {
            out.push_str("# HELP venue_bridge_actions_total decoded CoreWriter actions by id\n");
            out.push_str("# TYPE venue_bridge_actions_total counter\n");
        }
        for (id, count) in self.actions.iter().enumerate() {
            let v = count.load(Ordering::Relaxed);
            if v > 0 {
                out.push_str(&format!("venue_bridge_actions_total{{id=\"{id}\"}} {v}\n"));
            }
        }
        out
    }
}
