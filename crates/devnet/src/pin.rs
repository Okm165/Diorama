//! The fork-pin vocabulary: a chain fork's pin as one atomic word, with the
//! unresolved sentinel. One home for both mirrors — the venue's driver
//! resolves it at boot (and on every re-pin), the pinned source reads it, and
//! the HTTP surface reports it.
use std::sync::atomic::{AtomicU64, Ordering};

/// The pin handle's "not resolved yet" sentinel. A fork base is a real block
/// number and 0 is a legitimate genesis fork, so the sentinel must not be 0
/// (the canary's zero-word problem, same class — see the reset detectors).
pub const PIN_UNRESOLVED: u64 = u64::MAX;

/// The pin handle's resolved value: `None` while [`PIN_UNRESOLVED`]. One
/// accessor, so no reader compares against the raw sentinel.
pub fn resolved_pin(pin: &AtomicU64) -> Option<u64> {
    match pin.load(Ordering::Relaxed) {
        PIN_UNRESOLVED => None,
        p => Some(p),
    }
}

/// The chain's own fork pin, read once (the selftests' one call): a node
/// without a pin is the named error, never a silent default.
pub async fn fork_pin(rpc: &diorama_anvil::Rpc, chain: &'static str) -> crate::Result<u64> {
    rpc.fork_block()
        .await?
        .ok_or(crate::Error::NoForkPin { chain })
}

#[cfg(test)]
mod tests {
    use super::{resolved_pin, PIN_UNRESOLVED};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The sentinel never leaks as a pin: unresolved reads `None`, and a real
    /// block (genesis included) reads `Some`.
    #[test]
    fn the_pin_sentinel_never_leaks() {
        let pin = AtomicU64::new(PIN_UNRESOLVED);
        assert_eq!(resolved_pin(&pin), None);
        pin.store(0, Ordering::Relaxed);
        assert_eq!(resolved_pin(&pin), Some(0));
        pin.store(49_000_000, Ordering::Relaxed);
        assert_eq!(resolved_pin(&pin), Some(49_000_000));
    }
}
