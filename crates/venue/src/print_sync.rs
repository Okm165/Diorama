//! `print_sync` — keep the mirror's on-chain spot prints in step with the sim's
//! own oracle.
//!
//! Why this exists
//! ---------------
//! A venue-sim mirror seeds each `MirrorPriceFeed` once at deploy; only a
//! scenario control moves it. The consumer (the indexer) refuses any round that
//! diverges more than 1500 bps from the sim venue's own `oraclePx`, and any
//! round that moves more than 2000 bps from the previous served answer; on a
//! refusal it keeps the previous served price. A long-running sim walks its
//! oracle, so a seeded print drifts out of the bound and the asset becomes
//! unpricable (measured 2026-11-21: the x-* set sat 4–12 % high after a day,
//! xSOXL 29 % — refused every round, held at the deploy print).
//!
//! The loop reads the sim's oracle in-process (the same value `/info` serves),
//! reads the feed's current answer and decimals off V, and steps the print
//! toward the oracle through the engine's journaled `/admin/feed` control —
//! clamped per pass and with a deadband, so both of the consumer's bounds hold
//! by construction. A scenario control that moves a feed is followed, not
//! fought: the next pass steps from whatever the chain holds.
//!
//! The step function is pure and pinned by a table; the loop is the IO shell.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::Address;
use diorama_anvil::Rpc;
use diorama_config::PrintSyncSection;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::feed::MarketFeed;

/// The answer to write this pass, or `None` when the print is inside the
/// deadband (or either side is not a positive price — a foreign feed layout,
/// which the caller must not guess at).
///
/// `deadband_bps` is measured against the current answer (the served value the
/// consumer compares), and the step is capped at `max_step_bps` of the same
/// base — the consumer's own two bounds are 1500/2000 bps, so a policy well
/// under them leaves margin for a rounding unit.
#[must_use]
pub(crate) fn next_print(current: i128, oracle: i128, deadband_bps: u16, max_step_bps: u16) -> Option<i128> {
    if current <= 0 || oracle <= 0 {
        return None;
    }
    let delta = oracle - current;
    let magnitude = delta.unsigned_abs();
    let base = current.unsigned_abs();
    if magnitude.saturating_mul(10_000) <= base.saturating_mul(u128::from(deadband_bps)) {
        return None;
    }
    let cap = base.saturating_mul(u128::from(max_step_bps)) / 10_000;
    let step = magnitude.min(cap) as i128;
    if step == 0 {
        return None;
    }
    let next = if delta < 0 { current - step } else { current + step };
    (next > 0 && next != current).then_some(next)
}

/// A venue Decimal at the feed's own decimals, as the integer the feed stores.
fn scale(price: Decimal, decimals: u32) -> Option<i128> {
    price.checked_mul(Decimal::from(10u32.checked_pow(decimals)?))?.to_i128()
}

/// Start the follow loop when `[services.venue_sim.print_sync]` is configured.
///
/// Refuses (loudly, once) when the two endpoints it needs are absent: the
/// engine admin to write through and anvil V to read the current answer.
pub(crate) fn spawn(sim_section: &diorama_config::VenueSimSection, feed: &Arc<MarketFeed>, http: reqwest::Client) {
    let Some(sync) = &sim_section.print_sync else {
        return;
    };
    let (Some(admin_base), Some(anvil_url)) = (&sim_section.engine_admin_url, &sim_section.anvil_url) else {
        tracing::warn!(
            "print_sync is configured but engine_admin_url/anvil_url is unset — the print follow is disabled"
        );
        return;
    };
    let feeds = parse_feeds(sync);
    if feeds.len() != sync.feeds.len() {
        tracing::warn!(
            configured = sync.feeds.len(),
            parsed = feeds.len(),
            "print_sync: an entry has an unparseable feed address and was dropped"
        );
    }
    if feeds.is_empty() {
        return;
    }
    let url = format!("{}/admin/feed", admin_base.trim_end_matches('/'));
    let anvil_url = anvil_url.clone();
    let interval = Duration::from_millis(sync.interval_ms);
    let (deadband_bps, max_step_bps) = (sync.deadband_bps, sync.max_step_bps);
    let timeout = Duration::from_secs(sim_section.rpc_timeout_secs);
    let feed = Arc::clone(feed);
    tokio::spawn(async move {
        let Ok(rpc) = Rpc::new(&anvil_url, timeout) else {
            tracing::warn!("print_sync: anvil V client build failed — the print follow is disabled");
            return;
        };
        let mut decimals: HashMap<Address, u32> = HashMap::new();
        loop {
            for (coin, address) in &feeds {
                follow_one(&rpc, &feed, &http, &url, &mut decimals, coin, *address, deadband_bps, max_step_bps).await;
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// One feed's pass: read the oracle and the chain's answer, and step the print
/// when the drift clears the deadband. Every early return is "nothing to do or
/// nothing this loop may safely touch" — a foreign layout, a coin the sim does
/// not carry, or a drift inside the deadband.
#[allow(clippy::too_many_arguments)] // one pass's inputs, named; a struct would hide the call contract
async fn follow_one(
    rpc: &Rpc,
    feed: &MarketFeed,
    http: &reqwest::Client,
    url: &str,
    decimals: &mut HashMap<Address, u32>,
    coin: &str,
    address: Address,
    deadband_bps: u16,
    max_step_bps: u16,
) {
    let Some(oracle) = feed.oracle(coin) else { return };
    let scale_by = match decimals.get(&address) {
        Some(d) => *d,
        None => match diorama_devnet::feeds::read_feed_decimals(rpc, address).await {
            Ok(Some(d)) => {
                decimals.insert(address, u32::from(d));
                u32::from(d)
            }
            // A feed whose layout cannot be read is not this loop's to guess at.
            _ => return,
        },
    };
    let Some(oracle) = scale(oracle, scale_by) else { return };
    let Ok(Some(current)) = diorama_devnet::feeds::read_feed_answer(rpc, address).await else {
        return;
    };
    let Some(answer) = next_print(current, oracle, deadband_bps, max_step_bps) else {
        return;
    };
    let request = diorama_devnet::feeds::FeedControlRequest {
        feed: address,
        mode: diorama_devnet::feeds::FeedMode::Set,
        pct: None,
        answer: Some(answer),
    };
    match http.post(url).json(&request).send().await {
        Ok(response) if response.status().is_success() => {
            tracing::info!(
                coin = %coin,
                before = current,
                oracle,
                answer,
                "print_sync: stepped the mirror print toward the sim oracle"
            );
        }
        Ok(response) => {
            tracing::warn!(
                coin = %coin,
                status = %response.status(),
                "print_sync: the engine refused a feed write"
            );
        }
        Err(e) => tracing::warn!(coin = %coin, error = %e, "print_sync: engine admin unreachable"),
    }
}

fn parse_feeds(section: &PrintSyncSection) -> Vec<(String, Address)> {
    section
        .feeds
        .iter()
        .filter_map(|f| Address::from_str(&f.feed).ok().map(|a| (f.coin.clone(), a)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::next_print;

    /// The decision table: the deadband holds a quiet feed quiet, a real drift
    /// steps toward the oracle, and a step larger than the cap is clamped — the
    /// consumer's two bounds (1_500 bps divergence, 2_000 bps per round) hold by
    /// construction with the policy's defaults (100/1_500).
    #[test]
    fn the_print_step_holds_the_deadband_and_clamps_the_cap() {
        // Inside the deadband: no write (a price's own dust must not churn rounds).
        assert_eq!(next_print(3_201_200_000, 3_210_000_000, 100, 1_500), None);
        // Exactly at the deadband: still held (<=, not <).
        assert_eq!(next_print(10_000_000_000, 10_010_000_000, 100, 1_500), None);
        // A small drift: the exact oracle is written.
        assert_eq!(next_print(32_012_000_000, 32_500_000_000, 100, 1_500), Some(32_500_000_000));
        assert_eq!(next_print(32_012_000_000, 31_500_000_000, 100, 1_500), Some(31_500_000_000));
        // A drift beyond the cap: one bounded step (15 % of the current answer).
        let step = next_print(10_000_000_000, 6_000_000_000, 100, 1_500).expect("a step");
        assert_eq!(step, 8_500_000_000, "100 down by at most 15 % per pass");
        let step = next_print(10_000_000_000, 14_000_000_000, 100, 1_500).expect("a step");
        assert_eq!(step, 11_500_000_000, "100 up by at most 15 % per pass");
        // A foreign layout is never guessed at.
        assert_eq!(next_print(0, 100, 100, 1_500), None);
        assert_eq!(next_print(100, 0, 100, 1_500), None);
        assert_eq!(next_print(-5, 100, 100, 1_500), None);
        // A cap small enough to round to zero changes nothing.
        assert_eq!(next_print(100, 10_000, 0, 1), None);
    }

    /// A correcting pass converges: repeated capped steps reach the oracle and
    /// then stop (no infinite churn around the deadband).
    #[test]
    fn repeated_steps_converge_and_then_hold() {
        let mut answer = 10_000_000_000i128;
        let oracle = 6_000_000_000i128;
        let mut passes = 0;
        while let Some(next) = next_print(answer, oracle, 100, 1_500) {
            answer = next;
            passes += 1;
            assert!(passes < 100, "the cap must converge");
        }
        assert!(
            (answer - oracle).unsigned_abs() * 10_000 <= answer.unsigned_abs() * 100,
            "converged inside the deadband"
        );
    }
}
