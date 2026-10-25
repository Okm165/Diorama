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
        let known = plane.state.epoch_pin()?;
        if known.is_some() || plane.shadow.lock()?.has_recorded() {
            return Ok(());
        }
    }
    plane.state.set_epoch_pin(Some(pin))?;
    plane.save_state()?;
    tracing::info!(pin, "epoch pin anchored (fresh registry)");
    Ok(())
}

/// Whether the registry holds anything a reconcile must witness: a recorded
/// epoch pin or recorded writes. A fresh state dir holds neither — its boot
/// is an *anchor*, not a transition (the rule's own fresh case below is the
/// negation of this), so a caller that reports the transition (the venue's
/// reset fact) stays silent for it, exactly like the mainnet's boot anchor.
pub fn holds_state(plane: &Plane) -> Result<bool> {
    Ok(plane.state.epoch_pin()?.is_some() || plane.shadow.lock()?.has_recorded())
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
    let known = plane.state.epoch_pin()?;
    let epoch = if known != pin {
        clear_for_new_pin(plane, known, pin)?
    } else if pin.is_some() && plane.shadow.lock()?.has_recorded() {
        restore_same_pin(plane, target, chunk_size, pin).await?
    } else {
        Epoch::Steady
    };
    if pin.is_some() && known != pin {
        plane.state.set_epoch_pin(pin)?;
        plane.save_state()?;
    }
    // The journal holds before-values of the *old* chain: a rewind against the
    // re-forked chain would restore values that never existed there.
    if let Err(e) = plane.journal.clear() {
        tracing::warn!(error = %e, "clearing the journal after a reset failed");
    }
    Ok(epoch)
}

/// The pin moved: the devnet's writes belong to the old chain. Classify by
/// what the registry *held* before the clear — `clear_all` reports the marked
/// counts, and a registry can hold recorded values with no marks (healed on
/// load, but never assumed); reporting that as "nothing to clear" would drop
/// it silently and uncounted.
fn clear_for_new_pin(plane: &Plane, known: Option<u64>, pin: Option<u64>) -> Result<Epoch> {
    let (had, slots, accounts) = {
        let mut shadow = plane.shadow.lock()?;
        let had = shadow.has_recorded();
        let (slots, accounts) = shadow.clear_all();
        (had, slots, accounts)
    };
    if known.is_none() && !had {
        return Ok(Epoch::Steady);
    }
    plane.save_shadow()?;
    tracing::warn!(
        pin = ?pin,
        previous = ?known,
        cleared_slots = slots,
        cleared_accounts = accounts,
        "new epoch: the fork base moved; the devnet's writes are gone with it (re-create state from your journal)"
    );
    Ok(Epoch::Cleared)
}

/// The pin is unchanged and the registry holds writes: re-apply them over the
/// re-forked chain (the same-pin restore).
async fn restore_same_pin(
    plane: &Plane,
    target: &Rpc,
    chunk_size: usize,
    pin: Option<u64>,
) -> Result<Epoch> {
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
    Ok(Epoch::Restored {
        restored: moved.restored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Journal, JournalEntry};
    use crate::metrics::Metrics;
    use crate::shadow::Shadow;
    use crate::shared::Shared;
    use crate::state::StateFile;
    use alloy_primitives::{Address, U256};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::time::Duration;

    /// A plane on a fresh temp dir (per test: they may run concurrently).
    fn test_plane(name: &str) -> Plane {
        let dir = crate::test_dir(&format!("epoch-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        Plane {
            shadow: Shared::new("shadow registry", Shadow::default()),
            shadow_path: dir.join("shadow.json"),
            journal: Journal::new(dir.join("journal"), 16).unwrap(),
            state: Shared::new(
                crate::CURSOR_ARTIFACT,
                StateFile::open(dir.join("cursor.json")).unwrap(),
            ),
            metrics: Metrics::default(),
            facts: crate::facts::FactBus::default(),
            halted: AtomicBool::new(false),
            gap_paused: AtomicBool::new(false),
            time_offset: AtomicU64::new(0),
            shadow_scanned_upto: AtomicU64::new(0),
            reapply_from: Shared::new("reapply range", None),
        }
    }

    /// Never connected to: the decision paths under test do not touch the
    /// chain (only the same-pin restore does, and the drill covers that live).
    fn target() -> Rpc {
        Rpc::new("http://127.0.0.1:9", Duration::from_secs(1)).unwrap()
    }

    fn storage_write(addr: Address, slot: U256, value: U256) -> mirror_anvil::Write {
        mirror_anvil::Write::Storage {
            address: addr,
            slot,
            value,
        }
    }

    /// The decision table's non-network paths: the anchor, the fail-closed
    /// clear (including a values-only registry), the changed-pin clear, the
    /// same-pin steady, and the journal dropped on every reconcile.
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the epoch rule's decision table: every row in one place, so a new pin class cannot land without its verdict"
    )]
    async fn epoch_decisions_are_fail_closed() {
        let rpc = target();
        let addr = Address::repeat_byte(0x11);
        let slot = U256::from(3);

        // 1. Anchor: an empty registry's epoch is the current pin; a
        //    non-empty one is left unknown (nothing is asserted unprovably).
        let empty = test_plane("anchor-empty");
        anchor(&empty, Some(100)).unwrap();
        assert_eq!(empty.state.epoch_pin().unwrap(), Some(100));
        let held = test_plane("anchor-held");
        held.shadow
            .lock()
            .unwrap()
            .mark(&storage_write(addr, slot, U256::from(7)));
        anchor(&held, Some(100)).unwrap();
        assert_eq!(held.state.epoch_pin().unwrap(), None);

        // 2. A fresh state dir (unknown pin, empty registry) is not a
        //    transition: the pin is merely anchored.
        let fresh = test_plane("fresh");
        assert!(matches!(
            reconcile(&fresh, &rpc, 8, Some(100)).await.unwrap(),
            Epoch::Steady
        ));
        assert_eq!(fresh.state.epoch_pin().unwrap(), Some(100));

        // 3. Unknown pin beside a *values-only* registry: cleared, never
        //    reported as "nothing to clear" (the values are stale derivations
        //    of a base the state cannot prove).
        let values_only = test_plane("values-only");
        values_only.shadow.lock().unwrap().values.insert(
            addr,
            std::collections::BTreeMap::from([(slot, U256::from(7))]),
        );
        assert!(matches!(
            reconcile(&values_only, &rpc, 8, Some(100)).await.unwrap(),
            Epoch::Cleared
        ));
        assert!(!values_only.shadow.lock().unwrap().has_recorded());

        // 4. A changed pin clears and re-anchors; the same pin with an empty
        //    registry is steady.
        let moved = test_plane("moved");
        moved.state.set_epoch_pin(Some(1)).unwrap();
        moved
            .shadow
            .lock()
            .unwrap()
            .mark(&storage_write(addr, slot, U256::from(7)));
        assert!(matches!(
            reconcile(&moved, &rpc, 8, Some(100)).await.unwrap(),
            Epoch::Cleared
        ));
        assert_eq!(moved.state.epoch_pin().unwrap(), Some(100));
        assert!(matches!(
            reconcile(&moved, &rpc, 8, Some(100)).await.unwrap(),
            Epoch::Steady
        ));

        // 5. The journal is dropped on every reconcile (its before-values
        //    belong to the old chain).
        moved
            .journal
            .record(
                1,
                &[JournalEntry::Storage {
                    address: addr,
                    slot,
                    before: U256::from(1),
                }],
            )
            .unwrap();
        assert!(!moved.journal.blocks_in(1, 1).unwrap().is_empty());
        reconcile(&moved, &rpc, 8, Some(100)).await.unwrap();
        assert!(moved.journal.blocks_in(1, 1).unwrap().is_empty());
    }
}
