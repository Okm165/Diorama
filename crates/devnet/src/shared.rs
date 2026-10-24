//! State shared across tasks, with a named poison policy.
//!
//! `std::sync::Mutex` poisoning is a corruption signal: a panic inside a
//! critical section may have left the value mid-update. The devnet registries
//! are the mirror's memory of what it owns, so neither silent recovery nor a
//! per-call panic is acceptable (boundary-discipline: corrupt is named where
//! it is detected): every lock returns [`Error::Poisoned`] and the caller
//! decides — the engine's loops propagate it (the mirror halts loudly);
//! read-only admin handlers answer 503.
use crate::error::{Error, Result};
use std::sync::{Arc, Mutex, MutexGuard};

pub struct Shared<T> {
    what: &'static str,
    value: Arc<Mutex<T>>,
}

impl<T> Clone for Shared<T> {
    /// A clone shares the protected value (the `Arc`), not a copy of it: the
    /// reset detectors hold the same state file the plane writes.
    fn clone(&self) -> Self {
        Self {
            what: self.what,
            value: Arc::clone(&self.value),
        }
    }
}

impl<T> Shared<T> {
    /// `what` names the protected value in every poisoning fault it raises.
    pub fn new(what: &'static str, value: T) -> Self {
        Self {
            what,
            value: Arc::new(Mutex::new(value)),
        }
    }

    /// Lock; poisoning is a typed fault, not a panic.
    pub fn lock(&self) -> Result<MutexGuard<'_, T>> {
        self.value
            .lock()
            .map_err(|_| Error::Poisoned { what: self.what })
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("what", &self.what)
            .field("value", &self.value)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_is_a_typed_fault_not_a_panic() {
        let shared = Shared::new("test registry", 1u32);
        // Poison it: panic while holding the guard.
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = shared.lock().unwrap();
            panic!("mid-update");
        }));
        assert!(poisoned.is_err());
        // The next lock is a named error, and the process is still alive.
        let err = shared.lock().unwrap_err();
        assert!(matches!(
            err,
            Error::Poisoned {
                what: "test registry"
            }
        ));
        assert!(err.to_string().contains("test registry"), "{err}");
    }
}
