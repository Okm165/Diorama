//! The bridge's publish policy: when a confirmed value follows the source,
//! when a publish is in flight, and when a divergence is a scenario's write
//! (pause, never overwrite). Pure — the rules are testable without a chain.
/// One feed's value state: what the chain confirmed the bridge wrote, and
/// what a publish in flight should land as. Both are needed: between the
/// `updateAnswer` tx and its mining, the target still holds the *previous*
/// value — comparing only against "last written" would read that as a
/// scenario's write and pause the bridge on itself.
///
/// `pending_since_ms` is the publish's send time: an anvil pool is not
/// durable (and a same-nonce sender can displace ours — measured: daloc's
/// deploy sharing the writer account wedged a feed in `pending` forever), so
/// a publish that does not land within the confirmation window is re-sent.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct ValueState {
    pub(crate) confirmed: Option<i128>,
    pub(crate) pending: Option<i128>,
    pub(crate) pending_since_ms: Option<u64>,
}

/// The pass decision, pure so the pause rule is testable without a chain.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The target now holds the pending publish: record it as confirmed.
    Confirm,
    /// A publish is in flight (or the value is current): nothing to do.
    WaitPending,
    SkipUnchanged,
    Publish(i128),
    Pause { found: i128 },
}

/// The rules, in order:
/// 1. the target holds the pending publish → confirm it;
/// 2. the target holds the confirmed value (or nothing was ever confirmed —
///    first contact): publish the source's value when it moved, wait while a
///    publish is in flight *within the confirmation window*, re-publish when
///    it did not land in time, skip when nothing moved;
/// 3. anything else on the target → a scenario wrote it: pause, never
///    overwrite a scenario's value.
pub(crate) fn step(state: ValueState, target_now: i128, source_now: i128, now_ms: u64, window_ms: u64) -> Step {
    if state.pending == Some(target_now) {
        return Step::Confirm;
    }
    let ours = state.confirmed.is_none() || state.confirmed == Some(target_now);
    if !ours {
        return Step::Pause { found: target_now };
    }
    if let Some(since) = state.pending_since_ms {
        if now_ms.saturating_sub(since) < window_ms {
            return Step::WaitPending;
        }
        // The pending publish never landed (dropped, displaced by a
        // same-nonce sender, or reverted): re-send the *current* source
        // value. The effect is idempotent — a new round with the same answer
        // is exactly what a retry should leave behind.
        return Step::Publish(source_now);
    }
    if state.confirmed == Some(source_now) {
        return Step::SkipUnchanged;
    }
    Step::Publish(source_now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(confirmed: Option<i128>, pending: Option<i128>) -> ValueState {
        // A pending state's send time defaults to 0 (fresh unless the test
        // says otherwise by advancing `now`).
        ValueState {
            confirmed,
            pending,
            pending_since_ms: pending.map(|_| 0),
        }
    }

    const WINDOW: u64 = 60_000;

    #[test]
    fn the_pause_rule_never_overwrites_a_scenario() {
        let now = 1_000;
        // The bridge confirmed 2000; the target now holds 1500 (a crash): pause.
        assert_eq!(step(state(Some(2000), None), 1500, 2000, now, WINDOW), Step::Pause { found: 1500 });
        // The target still holds the bridge's value; the source moved: publish.
        assert_eq!(step(state(Some(2000), None), 2000, 2100, now, WINDOW), Step::Publish(2100));
        // Nothing moved: no tx.
        assert_eq!(step(state(Some(2000), None), 2000, 2000, now, WINDOW), Step::SkipUnchanged);
        // First pass (never wrote): publish the source value, no pause.
        assert_eq!(step(ValueState::default(), 999, 2000, now, WINDOW), Step::Publish(2000));
    }

    /// Between the publish and its mining the target holds the *previous*
    /// value: that must read as "in flight", never as a scenario's write.
    #[test]
    fn a_publish_in_flight_is_not_a_scenario_write() {
        let now = 1_000;
        assert_eq!(step(state(Some(2000), Some(2100)), 2000, 2100, now, WINDOW), Step::WaitPending);
        // Mined: confirmed.
        assert_eq!(step(state(Some(2000), Some(2100)), 2100, 2100, now, WINDOW), Step::Confirm);
        // After Confirm the next pass sees the source unchanged: skip.
        assert_eq!(step(state(Some(2100), None), 2100, 2100, now, WINDOW), Step::SkipUnchanged);
        // A crash during the flight is still caught (neither value).
        assert_eq!(step(state(Some(2000), Some(2100)), 1500, 2100, now, WINDOW), Step::Pause { found: 1500 });
    }

    /// A publish that never lands is re-sent once the confirmation window
    /// passes — an anvil pool is not durable, and a same-nonce sender can
    /// displace the tx (measured: daloc's deploy sharing the writer account
    /// wedged a feed in `pending` forever).
    #[test]
    fn a_stale_pending_publish_is_re_sent() {
        let in_flight = state(Some(2000), Some(2100));
        // Within the window: wait.
        assert_eq!(step(in_flight, 2000, 2100, 1_000, WINDOW), Step::WaitPending);
        // Past the window, target still not holding it: re-publish the
        // *current* source value.
        assert_eq!(step(in_flight, 2000, 2200, 1_000 + WINDOW, WINDOW), Step::Publish(2200));
        // Past the window but the target does hold it: confirm wins.
        assert_eq!(step(in_flight, 2100, 2200, 1_000 + WINDOW, WINDOW), Step::Confirm);
    }
}
