//! The REST weight budget meter (plan §5): the venue's public API allows
//! 1200 weight/min/IP, with per-endpoint weights (measured: `metaAndAssetCtxs`
//! 20, `l2Book`/`allMids` 2, `clearinghouseState` 2, all other info 20).
//!
//! A rolling 60-second window of spends; a call whose weight does not fit is
//! skipped (never queued) and counted — the poller's next pass retries. The
//! meter is shared by the REST poller and the passthrough reads so neither can
//! starve the other.
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The venue's published budget.
const WINDOW: Duration = Duration::from_secs(60);
const CAPACITY: u64 = 1200;

/// The measured per-endpoint weights (docs, §16.3). A code fact: a weight
/// change is a protocol change, not a deployment knob.
pub mod weight {
    /// `metaAndAssetCtxs`, `meta`, and every unpriced info type.
    pub const HEAVY: u64 = 20;
    /// `l2Book`, `allMids`, `clearinghouseState`, `spotClearinghouseState`,
    /// `orderStatus`, `exchangeStatus`.
    pub const LIGHT: u64 = 2;
}

#[derive(Default)]
pub(crate) struct Budget {
    spends: Mutex<VecDeque<(Instant, u64)>>,
}

impl Budget {
    /// Try to spend `weight`; refused when the rolling window is full. The
    /// caller skips the call (a refusal is backpressure, not an error).
    pub fn try_spend(&self, weight: u64) -> bool {
        self.try_spend_at(Instant::now(), weight)
    }

    /// The weight spent in the current window (the metrics gauge).
    pub fn used(&self) -> u64 {
        self.used_at(Instant::now())
    }

    fn try_spend_at(&self, now: Instant, weight: u64) -> bool {
        let mut spends = self.spends.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict(&mut spends, now);
        let used: u64 = spends.iter().map(|(_, w)| w).sum();
        if used + weight > CAPACITY {
            return false;
        }
        spends.push_back((now, weight));
        true
    }

    fn used_at(&self, now: Instant) -> u64 {
        let mut spends = self.spends.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict(&mut spends, now);
        spends.iter().map(|(_, w)| w).sum()
    }

    fn evict(spends: &mut VecDeque<(Instant, u64)>, now: Instant) {
        while let Some((at, _)) = spends.front() {
            if now.duration_since(*at) >= WINDOW {
                spends.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_caps_and_recovers() {
        let b = Budget::default();
        let t0 = Instant::now();
        assert!(b.try_spend_at(t0, 1000));
        assert!(b.try_spend_at(t0, 200), "1200 fits");
        assert!(!b.try_spend_at(t0, 1), "over capacity is refused");
        assert_eq!(b.used_at(t0), 1200);
        // 61 s later the window is empty again.
        let t1 = t0 + Duration::from_secs(61);
        assert_eq!(b.used_at(t1), 0);
        assert!(b.try_spend_at(t1, 1200));
    }

    #[test]
    fn partial_eviction() {
        let b = Budget::default();
        let t0 = Instant::now();
        assert!(b.try_spend_at(t0, 600));
        let t1 = t0 + Duration::from_secs(30);
        assert!(b.try_spend_at(t1, 600));
        assert!(!b.try_spend_at(t1, 1));
        let t2 = t0 + Duration::from_secs(61);
        assert_eq!(b.used_at(t2), 600, "only the first spend aged out");
        assert!(b.try_spend_at(t2, 600));
    }
}
