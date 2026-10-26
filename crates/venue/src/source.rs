//! The venue profile's source facts: what the HL public RPC can serve
//! (measured 2026-10-24), declared once and audited at boot. The read shape —
//! the pin guard, the head anchor — is the spine's
//! [`diorama_devnet::source::PinnedSource`], chosen in `profile.rs`; the
//! follow reads the raw source at latest by design.
//!
//! The truth is the fork source **at the pin**: V's base state *is* the
//! source's state at that block, and the public source cannot serve history
//! (its `eth_call` returns the *latest* state — measured: V's fork at block N
//! differs from the source's answer at N, while V's state IS the source's
//! state at N). A read is therefore only exact while the source's head is
//! still the pin; once the source has advanced, the read is *refused* with
//! the numbers rather than silently returning a newer value (the "never hand
//! back a poisoned value" rule — the spine's guard).
//!
//! The registry is **not** a substitute: its `values` are the *devnet's*
//! latest writes (what a re-fork must restore), not the source's
//! before-state. The before-state lives in the **journal** (each engine write
//! records it) — so a selective hand-back could serve a key's *oldest*
//! journal entry, gated on the ring still covering the epoch
//! (`first_block + keep > head`). That slice is planned, not built: today the
//! refusal is fail-closed, and the wholesale hand-back is the epoch itself
//! (the re-pin's re-fork restores the source's whole state — measured: the
//! pin is seconds stale on a 1s-block source, so the selective window is only
//! reliable against a scratch/slow source).
use diorama_devnet::source::{Capabilities, Capability, PinnedSource, RawSource, Source};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// What the public venue source can serve (measured 2026-10-24): no BAL, no
/// traces, no proofs, no websocket JSON-RPC; state reads are latest-only (the
/// block parameter is ignored), and system transactions (Core→EVM completions)
/// are exposed through their own RPC method. The follow source declares the
/// same capabilities on its raw source (the mainnet's `source.rs` declares
/// its own the same way: one `declared()` per source).
pub(crate) fn declared() -> Capabilities {
    // Measured 2026-10-24: the public RPC serves the system txs but their
    // receipts are null (and absent from eth_getBlockReceipts); a nanoreth
    // archive serves them.
    Capabilities::of([Capability::SystemTxs])
}

/// The profile's truth source: the pin-guarded decorator over the raw reads
/// (the guard refuses once the source's head has moved past the pin).
pub(crate) fn pinned(rpc: Arc<diorama_anvil::Rpc>, pin: Arc<AtomicU64>) -> Arc<dyn Source> {
    Arc::new(PinnedSource::new(rpc, pin, declared()))
}

/// The profile's follow source: the raw reads at latest (the follow is
/// latest-by-design; the guard would refuse its reads).
pub(crate) fn follow(rpc: Arc<diorama_anvil::Rpc>) -> Arc<dyn Source> {
    Arc::new(RawSource::new(rpc, declared()))
}

#[cfg(test)]
mod tests {
    use super::{declared, Capabilities, Capability};

    /// The public source's declaration, pinned flag for flag: the boot audit
    /// compares the live endpoint against exactly this, and the composer
    /// depends on each one (no BAL, no traces, latest-only reads, system txs
    /// exposed, their receipts absent — measured 2026-10-24). The mainnet's
    /// `the_reth_source_declares_the_measured_capabilities` pins its own the
    /// same way.
    #[test]
    fn the_public_source_declares_the_measured_capabilities() {
        assert_eq!(
            declared(),
            Capabilities::of([Capability::SystemTxs]),
            "the declaration the boot audit compares against (measured 2026-10-24)"
        );
    }
}
