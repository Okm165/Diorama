//! The venue's reset detector: the chain-instance canary.
//!
//! V is a fork-mode anvil, and a fresh fork cannot carry the canary word — so
//! "the canary reads back the recorded pin" means "this is still the chain
//! instance the registry was built against". It is the only reliable reset
//! signal here: heights below the fork resolve to the *source's* real blocks
//! (a re-pin to a later base keeps every cursor hash valid — measured live:
//! the driver kept scanning a stale range), and a same-pin restart leaves no
//! other trace. One storage read per pass; one write per reconcile.
//!
//! The detector is the venue's implementation of the spine's
//! [`ResetDetector`]: the mainnet mirror's is `CursorHash` (in
//! `crates/mainnet/src/reset.rs`), this one is the pin canary — the
//! divergence is declared in `crates/venue/src/profile.rs` and this module
//! holds its mechanics.
use alloy_primitives::{Address, B256, U256};
use mirror_anvil::Rpc;
use mirror_devnet::shared::Shared;
use futures_util::future::BoxFuture;
use mirror_devnet::spine::{ResetDetector, ResetSignal};
use mirror_devnet::state::StateFile;

/// The canary word's address (a scratch address with no code) and slot.
pub(crate) const CANARY_ADDRESS: Address =
    alloy_primitives::address!("00000000000000000000000000000000ca11ca11");
pub(crate) const CANARY_SLOT: U256 = U256::ZERO;

/// The pin-canary detector: the recorded chain instance's storage word must
/// read back the pin the registry was built against.
pub struct PinCanary {
    /// The cursor state the expected pin lives in (the plane's state file).
    state: Shared<StateFile>,
}

impl PinCanary {
    pub fn new(state: Shared<StateFile>) -> Self {
        Self { state }
    }
}

impl ResetDetector for PinCanary {
    fn detect<'a>(&'a self, target: &'a Rpc) -> BoxFuture<'a, mirror_devnet::Result<ResetSignal>> {
        Box::pin(async move {
            let expected = self.state.epoch_pin()?;
            let found = read_canary(target).await?;
            Ok(if found == expected {
                ResetSignal::Steady
            } else {
                ResetSignal::PinMismatch { expected, found }
            })
        })
    }
}

/// Read the canary word: `Some(pin)` when the recorded instance wrote it,
/// `None` when the storage is empty (a fresh fork) or carries a word that is
/// not a stamped pin — a read fault is the caller's error.
// TRUST: the fork pin is the trusted root (A1/A2 — the base's state is the
// source's, unverifiable by construction). The guards are this canary and the
// epoch rule: a moved base clears marks *and* values; a same-pin reset
// restores the recorded writes; a fresh chain cannot carry the canary word.
pub async fn read_canary(v: &Rpc) -> mirror_devnet::Result<Option<u64>> {
    let word = v.storage_at_latest(CANARY_ADDRESS, CANARY_SLOT).await?;
    Ok(decode_canary(U256::from_be_bytes(word.0)))
}

/// Mark this chain instance (see [`CANARY_ADDRESS`]).
pub async fn write_canary(v: &Rpc, pin: u64) -> mirror_devnet::Result<()> {
    let value = B256::from(encode_canary(pin));
    v.anvil_set_storage_at(CANARY_ADDRESS, CANARY_SLOT, value)
        .await?;
    Ok(())
}

/// The canary word's encoding: `pin + 1`. A raw zero would make a genesis pin
/// (0 — a legitimate fork base) indistinguishable from "never stamped", so a
/// chain forked at block 0 could never read its canary back (measured: the
/// driver reconciled on every pass, rewriting the word forever).
fn encode_canary(pin: u64) -> U256 {
    U256::from(pin + 1)
}

/// Decode the canary word: `None` for a zero word (never stamped) or a
/// foreign word that does not fit a block number.
fn decode_canary(value: U256) -> Option<u64> {
    u64::try_from(value).ok()?.checked_sub(1)
}

#[cfg(test)]
mod tests {
    use super::{decode_canary, encode_canary};
    use alloy_primitives::U256;

    /// The encoding's whole point: a genesis pin round-trips (a raw word
    /// cannot — zero means unstamped), and unstamped/foreign words decode to
    /// `None` (the epoch rule then reconciles).
    #[test]
    fn the_canary_round_trips_including_genesis() {
        assert_eq!(decode_canary(encode_canary(0)), Some(0));
        assert_eq!(decode_canary(encode_canary(1)), Some(1));
        assert_eq!(decode_canary(encode_canary(20_000_000)), Some(20_000_000));
        assert_eq!(decode_canary(U256::ZERO), None, "unstamped");
        assert_eq!(decode_canary(U256::MAX), None, "a foreign word");
    }
}
