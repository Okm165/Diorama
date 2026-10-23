//! The epoch rule (plan §6.3), one implementation for both profiles: what
//! happens to the COW registry when the devnet chain was re-forked under the
//! engine.
//!
//! - The fork pin *changed*: the recorded values are stale derivations of the
//!   old base. Drop marks and values — the product re-creates its state from
//!   its own journal. Measured: restoring across a pin change re-applied a
//!   +30d time-travel write as a future `lastUpdateTimestamp` and every Aave
//!   supply panicked with an arithmetic underflow.
//! - The pin is *unchanged*: the recorded writes are still coherent. Prune
//!   orphans, re-apply them, keep the marks.
//!
//! Fail-closed: an unknown pin beside a non-empty registry is a fresh epoch,
//! never a restore — the rule must not restore across a base it cannot prove.
//! A fresh state dir (unknown pin, empty registry) is not a transition: there
//! is nothing to drop, the pin is merely anchored.
//!
//! Either way the journal is dropped (its before-values belong to the old
//! chain: a rewind against the re-forked chain would restore values that never
//! existed there) and `epoch_pin` is re-anchored and persisted. The transition
//! is logged here — both engines report the same event in the same words; the
//! counters stay the profile's (T counts resets *found*, V epoch transitions).
use crate::error::Result;
use crate::plane::Plane;
use crate::shadow;
use mirror_anvil::Rpc;

/// What the epoch rule did, for the caller's metrics.
#[derive(Debug, Clone, Copy)]
pub enum Epoch {
    /// The same pin, or a fresh state dir: the registry needed nothing.
    Steady,
    /// A fresh pin: marks and values were dropped (logged and persisted here).
    Cleared,
    /// The same pin: recorded writes were re-applied (logged and persisted
    /// here).
    Restored { restored: usize },
}

/// Anchor `epoch_pin` for a registry that never had one: a fresh (empty)
/// registry's epoch *is* the current pin, and recording it is what lets a
/// later same-pin restart be recognized as a restore instead of an
/// unprovable epoch. A non-empty registry with an unknown pin is left unknown
/// on purpose — asserting an epoch the state cannot prove is exactly what the
/// rule refuses, and the next reset's decision then fails closed.
///
/// The venue gets this anchor from its boot-time reconcile (the canary forces
/// one on every boot, and it is written last, so a present canary implies a
/// recorded pin). T's epoch rule only runs on a *detected* reset, so its boot
/// calls this explicitly.
pub fn anchor(plane: &Plane, pin: Option<u64>) -> Result<()> {
    let Some(pin) = pin else { return Ok(()) };
    {
        let known = plane.state.lock()?.data.epoch_pin;
        if known.is_some() || plane.shadow.lock()?.has_recorded() {
            return Ok(());
        }
    }
    let mut state = plane.state.lock()?;
    state.data.epoch_pin = Some(pin);
    state.save()?;
    tracing::info!(pin, "epoch pin anchored (fresh registry)");
    Ok(())
}

/// Run the epoch rule against `pin` (the chain's fork base, a source-chain
/// block height; `None` = the chain has no fork config). See the module docs
/// for the contract. A transition is persisted before returning: a crash
/// between the decision and the save must not leave the registry's stale
/// epoch on disk beside an advanced `epoch_pin` (the next boot would read the
/// canary as matching and keep it).
pub async fn reconcile(
    plane: &Plane,
    target: &Rpc,
    chunk_size: usize,
    pin: Option<u64>,
) -> Result<Epoch> {
    let known = plane.state.lock()?.data.epoch_pin;
    let epoch = if known != pin {
        let (slots, accounts) = plane.shadow.lock()?.clear_all();
        if known.is_none() && slots == 0 && accounts == 0 {
            Epoch::Steady
        } else {
            plane.save_shadow()?;
            tracing::warn!(
                pin = ?pin,
                previous = ?known,
                cleared_slots = slots,
                cleared_accounts = accounts,
                "new epoch: the fork base moved; the devnet's writes are gone with it (re-create state from your journal)"
            );
            Epoch::Cleared
        }
    } else if pin.is_some() && plane.shadow.lock()?.has_recorded() {
        let moved = shadow::restore_recorded(target, &plane.shadow, chunk_size).await?;
        plane.save_shadow()?;
        tracing::warn!(
            pin = ?pin,
            restored = moved.restored,
            slots = moved.slots,
            accounts = moved.accounts,
            orphan_slots = moved.orphan_slots,
            orphan_accounts = moved.orphan_accounts,
            unknown_slots = moved.unknown_slots,
            unknown_accounts = moved.unknown_accounts,
            "same-pin restore: re-applied the devnet's recorded writes"
        );
        Epoch::Restored {
            restored: moved.restored,
        }
    } else {
        Epoch::Steady
    };
    if pin.is_some() && known != pin {
        let mut state = plane.state.lock()?;
        state.data.epoch_pin = pin;
        state.save()?;
    }
    // The journal holds before-values of the *old* chain: a rewind against the
    // re-forked chain would restore values that never existed there.
    if let Err(e) = plane.journal.clear() {
        tracing::warn!(error = %e, "clearing the journal after a reset failed");
    }
    Ok(epoch)
}
