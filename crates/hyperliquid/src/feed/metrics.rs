//! The market plane's metrics: WS health, the rate budget, REST faults, and
//! the per-coin staleness gauges (plan §5: "per-coin staleness metric").
//!
//! The hand-rolled Prometheus conventions are `mirror-devnet`'s; the one
//! extension here is labels (`{coin="BTC"}`), which only the per-coin gauges
//! need — a labeled series per configured market.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Default)]
pub struct FeedMetrics {
    /// WS client connected (1/0).
    ws_connected: AtomicBool,
    /// REST poller transport/parse faults.
    rest_errors: AtomicU64,
    /// WS reconnects since boot.
    ws_reconnects: AtomicU64,
    /// Calls skipped because the rate budget was full.
    budget_skips: AtomicU64,
    /// Weight spent in the rolling window (the poller publishes it).
    budget_used: AtomicU64,
    /// Milliseconds since each coin's last live context update.
    stale_ms: BTreeMap<String, AtomicU64>,
}

impl FeedMetrics {
    pub fn new(markets: &[String]) -> Self {
        Self {
            stale_ms: markets.iter().map(|m| (m.clone(), AtomicU64::new(0))).collect(),
            ..Self::default()
        }
    }

    pub fn set_ws_connected(&self, connected: bool) {
        self.ws_connected.store(connected, Ordering::Relaxed);
    }

    pub fn inc_rest_errors(&self) {
        self.rest_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_ws_reconnects(&self) {
        self.ws_reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_budget_skips(&self) {
        self.budget_skips.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_budget_used(&self, used: u64) {
        self.budget_used.store(used, Ordering::Relaxed);
    }

    pub fn set_stale(&self, coin: &str, ms: u64) {
        if let Some(slot) = self.stale_ms.get(coin) {
            slot.store(ms, Ordering::Relaxed);
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512);
        push_gauge(&mut out, "venue_sim_ws_connected", "market websocket connected (1/0)", &[], self.ws_connected.load(Ordering::Relaxed) as u64);
        push_counter(&mut out, "venue_sim_rest_errors_total", "market REST poller faults", &[], self.rest_errors.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_ws_reconnects_total", "market websocket reconnects", &[], self.ws_reconnects.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_budget_skips_total", "REST calls skipped at the rate budget", &[], self.budget_skips.load(Ordering::Relaxed));
        push_gauge(&mut out, "venue_sim_budget_weight_used", "REST weight spent in the rolling minute", &[], self.budget_used.load(Ordering::Relaxed));
        // One family, one HELP/TYPE, then every series (strict scrapers
        // reject a repeated header — the verifier caught it).
        if !self.stale_ms.is_empty() {
            out.push_str("# HELP venue_sim_stale_ms ms since the coin's last live context update\n");
            out.push_str("# TYPE venue_sim_stale_ms gauge\n");
        }
        for (coin, ms) in &self.stale_ms {
            out.push_str(&format!(
                "venue_sim_stale_ms{{coin=\"{coin}\"}} {}\n",
                ms.load(Ordering::Relaxed)
            ));
        }
        out
    }
}

fn push_gauge(out: &mut String, name: &str, help: &str, labels: &[(&str, &str)], value: u64) {
    push_metric(out, "gauge", name, help, labels, value);
}

fn push_counter(out: &mut String, name: &str, help: &str, labels: &[(&str, &str)], value: u64) {
    push_metric(out, "counter", name, help, labels, value);
}

fn push_metric(out: &mut String, kind: &str, name: &str, help: &str, labels: &[(&str, &str)], value: u64) {
    let series = if labels.is_empty() {
        name.to_string()
    } else {
        let l = labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!("{name}{{{l}}}")
    };
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{series} {value}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_labeled_staleness() {
        let m = FeedMetrics::new(&["BTC".into(), "ETH".into()]);
        m.set_stale("BTC", 1500);
        m.set_ws_connected(true);
        let out = m.render();
        assert!(out.contains("venue_sim_stale_ms{coin=\"BTC\"} 1500"), "{out}");
        assert!(out.contains("venue_sim_ws_connected 1"), "{out}");
    }
}
