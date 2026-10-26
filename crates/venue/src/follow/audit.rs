//! The uncovered-log audit: logs from contracts no declared closure covers —
//! their state changes cannot be followed until a closure is declared. The
//! metric counts every window; the drift line and the warning carry only the
//! *watched* emitters new to the log — the actionable subset (declare a
//! closure, or unwatch the account). The ambient chain's uncovered logs
//! (every contract the source's blocks touch) stay in the metric: their set
//! churns forever, so a per-emitter line says nothing an operator can act on
//! (measured live: 62 warnings in ten minutes before this split). The
//! applier has no counterpart: reth's native BALs carry every write, so
//! nothing can be uncovered there.
use crate::ctx::Ctx;
use crate::error::Result;
use alloy_primitives::Address;
use diorama_devnet::metrics::Registry as _;

/// The audit's drift line: log emitters no declared closure covers (the
/// metric counts every window; the line carries the emitters new to the log —
/// see [`fresh_emitters`]).
#[derive(serde::Serialize)]
pub(super) struct UncoveredLogsEntry {
    chain: &'static str,
    kind: &'static str,
    emitters: Vec<String>,
}

/// Record the window's uncovered emitters: the counter always, the drift line
/// and the warning only for the emitters new to the log.
pub(super) fn record(ctx: &Ctx, uncovered: &[Address]) -> Result<()> {
    if uncovered.is_empty() {
        return Ok(());
    }
    let metrics = &ctx.venue_metrics;
    metrics.add(&metrics.poll_uncovered_logs, uncovered.len() as u64);
    let watch = ctx.mirror.scope().watch();
    let actionable: Vec<Address> = uncovered
        .iter()
        .copied()
        .filter(|address| watch.accounts.contains(address))
        .collect();
    let fresh = fresh_emitters(&ctx.uncovered_seen, &actionable);
    if !fresh.is_empty() {
        ctx.drift.append(&UncoveredLogsEntry {
            chain: ctx.mirror.label(),
            kind: "uncovered_logs",
            emitters: fresh.clone(),
        })?;
        tracing::warn!(
            emitters = ?fresh,
            "follow: logs from contracts no closure covers (declare a closure to follow them)"
        );
    }
    Ok(())
}

/// The emitters in `current` that the seen set had not carried yet, adding
/// them as it goes. A poisoned lock is recovered by construction (the set is
/// a cache of what was logged; there is no invariant a panic could break).
fn fresh_emitters(
    seen: &std::sync::Mutex<std::collections::HashSet<String>>,
    current: &[Address],
) -> Vec<String> {
    let mut seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    current
        .iter()
        .map(ToString::to_string)
        .filter(|emitter| seen.insert(emitter.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::fresh_emitters;
    use alloy_primitives::Address;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// The audit's bound: the first sighting of an emitter is fresh, its
    /// repeats are not, and a churning set reports only what is new.
    #[test]
    fn fresh_emitters_reports_each_address_once() {
        let seen = Mutex::new(HashSet::new());
        let a = Address::repeat_byte(0xaa);
        let b = Address::repeat_byte(0xbb);
        assert_eq!(fresh_emitters(&seen, &[a, b]).len(), 2);
        assert!(fresh_emitters(&seen, &[a, b]).is_empty());
        assert_eq!(fresh_emitters(&seen, &[b, a]).len(), 0);
        let c = Address::repeat_byte(0xcc);
        let fresh = fresh_emitters(&seen, &[a, c]);
        assert_eq!(fresh, vec![c.to_string()]);
    }
}
