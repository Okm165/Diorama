//! The reconnect shape (plan §11.3 rule 3): bounded exponential backoff with a
//! reset on progress — one definition for every client that re-establishes a
//! session (the venue feed's websocket today; a REST lane or a bridge
//! reconnect tomorrow), never re-derived per client.
//!
//! The caller owns the sleep, deliberately: that is where its own shutdown
//! races the backoff, and a helper that slept could not be cancelled. The
//! arithmetic and its bounds live here — and carry the unit test the
//! hand-rolled copies never had.
use std::time::Duration;

/// The outcome a backoffed attempt reports: progress resets the delay, a
/// stall doubles it. Named, so a call site reads (`record(Progress::Stalled)`)
/// instead of a bare `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Progressed,
    Stalled,
}

/// Bounded exponential backoff: `base` for the first failed attempt, doubling
/// per consecutive failure, never past `cap`; progress resets it to `base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    current: Duration,
}

impl Backoff {
    /// `base` is the first failure's delay; `cap` bounds every later one.
    pub const fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            current: base,
        }
    }

    /// The delay for the attempt just made — sleep this before retrying.
    pub const fn delay(&self) -> Duration {
        self.current
    }

    /// Record the attempt's outcome: progress (a session that served) resets
    /// the delay to `base`; a failure doubles it, never past `cap`.
    pub fn record(&mut self, progress: Progress) {
        self.current = match progress {
            Progress::Progressed => self.base,
            Progress::Stalled => (self.current * 2).min(self.cap),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_to_the_cap_and_resets_on_progress() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(5));
        // The first failure sleeps the base, then doubles up to the cap.
        let mut delays = Vec::new();
        for _ in 0..6 {
            delays.push(backoff.delay().as_secs());
            backoff.record(Progress::Stalled);
        }
        assert_eq!(delays, vec![1, 2, 4, 5, 5, 5]);
        // One session that made progress starts the sequence over.
        backoff.record(Progress::Progressed);
        assert_eq!(backoff.delay(), Duration::from_secs(1));
    }
}
