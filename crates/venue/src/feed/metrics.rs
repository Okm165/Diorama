//! The market plane's metrics: WS health, the rate budget, REST faults, and
//! the per-coin staleness gauges (plan §5: "per-coin staleness metric").
//!
//! The Prometheus conventions are `mirror-devnet`'s (its labeled wrappers:
//! `push_gauge`/`push_counter`), a labeled series per configured market.
use mirror_devnet::metrics::{push_counter, push_gauge};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Default)]
pub(crate) struct FeedMetrics {
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
    /// Devnet-owned book levels (the order lane's consumption records): live
    /// updates keep the consumed delta — the venue analogue of
    /// `mirror_shadowed_slots`.
    owned_levels: AtomicU64,
    /// Live book updates that met a devnet-owned level (plan §4.5.4 I2's skip
    /// counter).
    owned_skips: AtomicU64,
    /// Owned levels pruned because the live book no longer carries the price
    /// (I6: pruned, never re-owned).
    owned_pruned: AtomicU64,
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

    pub fn set_owned_levels(&self, levels: u64) {
        self.owned_levels.store(levels, Ordering::Relaxed);
    }

    pub fn add_owned_skips(&self, n: u64) {
        self.owned_skips.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_owned_pruned(&self, n: u64) {
        self.owned_pruned.fetch_add(n, Ordering::Relaxed);
    }

    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512);
        push_gauge(&mut out, "venue_sim_ws_connected", "market websocket connected (1/0)", &[], self.ws_connected.load(Ordering::Relaxed) as u64);
        push_counter(&mut out, "venue_sim_rest_errors_total", "market REST poller faults", &[], self.rest_errors.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_ws_reconnects_total", "market websocket reconnects", &[], self.ws_reconnects.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_budget_skips_total", "REST calls skipped at the rate budget", &[], self.budget_skips.load(Ordering::Relaxed));
        push_gauge(&mut out, "venue_sim_budget_weight_used", "REST weight spent in the rolling minute", &[], self.budget_used.load(Ordering::Relaxed));
        push_gauge(&mut out, "venue_sim_owned_levels", "book levels the devnet has consumed (live updates keep the consumed delta)", &[], self.owned_levels.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_owned_skips_total", "live book updates that met a devnet-owned level", &[], self.owned_skips.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_owned_pruned_total", "owned levels pruned because the live book moved past them", &[], self.owned_pruned.load(Ordering::Relaxed));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_labeled_series() {
        let m = FeedMetrics::new(&["BTC".into(), "ETH".into()]);
        m.set_stale("BTC", 1500);
        m.set_ws_connected(true);
        let out = m.render();
        assert!(out.contains("venue_sim_stale_ms{coin=\"BTC\"} 1500"), "{out}");
        assert!(out.contains("venue_sim_ws_connected 1"), "{out}");
    }
}
