//! The sim's metrics: the series the venue's `/metrics` renders beside the
//! feed's and the bridge's (the registry pattern's third home in this crate).
use mirror_devnet::metrics::{push_counter, push_gauge};
use std::sync::atomic::{AtomicU64, Ordering};

/// The sim's metrics (rendered beside the feed's).
#[derive(Default)]
pub(crate) struct SimMetrics {
    /// Registered (touched) accounts.
    pub accounts: AtomicU64,
    /// The scenario epoch.
    pub epoch: AtomicU64,
    /// Liquidations the waterfall executed (every step; the partial, the
    /// backstop and the ADL counters below carry the individual paths).
    pub liquidations_total: AtomicU64,
    /// Funding payments applied (per position, per accrual).
    pub funding_payments_total: AtomicU64,
    /// Served states that failed the identity re-check (the client's own
    /// gate): any nonzero value is a construction regression to page on.
    pub identity_violations: AtomicU64,
    /// The scenario's explicit `advance` steps (the 1 Hz tick is the
    /// automaton; its work shows in the funding/liquidation counters).
    pub advances_total: AtomicU64,
    /// Orders the matching lane filled (the impact lane's liveness).
    pub orders_filled_total: AtomicU64,
    /// Trigger orders armed / fired by the tick.
    pub triggers_armed_total: AtomicU64,
    pub triggers_fired_total: AtomicU64,
    /// Fills that charged a fee (the fee lane's liveness).
    pub fees_charged_total: AtomicU64,
    /// The waterfall's steps: partial book closes, full book closes, backstop
    /// takeovers and ADL closes.
    pub partial_liquidations_total: AtomicU64,
    pub backstop_liquidations_total: AtomicU64,
    pub adl_closes_total: AtomicU64,
}

impl SimMetrics {
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512);
        push_gauge(&mut out, "venue_sim_accounts", "simulated (touched) accounts", &[], self.accounts.load(Ordering::Relaxed));
        push_gauge(&mut out, "venue_sim_epoch", "scenario epoch (reset bumps it)", &[], self.epoch.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_liquidations_total", "liquidations executed by the sim", &[], self.liquidations_total.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_partial_liquidations_total", "waterfall step 1: partial (20%) book liquidations", &[], self.partial_liquidations_total.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_backstop_liquidations_total", "waterfall step 2: backstop (the vault took the position)", &[], self.backstop_liquidations_total.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_adl_closes_total", "waterfall step 3: ADL closes at the previous mark", &[], self.adl_closes_total.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_funding_payments_total", "funding payments applied", &[], self.funding_payments_total.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_identity_violations_total", "served states that failed the identity re-check (must stay 0)", &[], self.identity_violations.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_advances_total", "scenario advance() steps", &[], self.advances_total.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_orders_filled_total", "orders filled against the real book (the impact lane)", &[], self.orders_filled_total.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_fees_charged_total", "fills that charged a fee (taker and maker sides)", &[], self.fees_charged_total.load(Ordering::Relaxed));
        push_counter(&mut out, "venue_sim_triggers_armed_total", "trigger orders armed (tp/sl)", &[], self.triggers_armed_total.load(Ordering::Relaxed));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sim's series render (the mainnet's bridge counters test pins its
    /// own set the same way).
    #[test]
    fn renders_the_sim_series() {
        let m = SimMetrics::default();
        m.accounts.store(3, std::sync::atomic::Ordering::Relaxed);
        m.liquidations_total.store(1, std::sync::atomic::Ordering::Relaxed);
        let out = m.render();
        assert!(out.contains("venue_sim_accounts 3"), "{out}");
        assert!(out.contains("venue_sim_liquidations_total 1"), "{out}");
        assert!(out.contains("venue_sim_identity_violations_total 0"), "{out}");
    }
}
