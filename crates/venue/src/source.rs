//! The venue profile's source facts: what the HL public RPC can serve
//! (measured 2026-10-24), declared once and audited at boot. The read shape —
//! the pin guard, the head anchor — is the spine's
//! [`mirror_devnet::source::PinnedSource`], chosen in `profile.rs`; the
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
use mirror_devnet::source::Capabilities;

/// What the public venue source can serve (measured 2026-10-24): no BAL, no
/// traces, no proofs, no websocket JSON-RPC; state reads are latest-only (the
/// block parameter is ignored), and system transactions (Core→EVM completions)
/// are exposed through their own RPC method. The follow source declares the
/// same capabilities on its raw source.
pub fn public_capabilities() -> Capabilities {
    Capabilities {
        native_bal: false,
        traces: false,
        historical: false,
        heads_ws: false,
        system_txs: true,
        // Measured 2026-10-24: the public RPC serves the system txs but their
        // receipts are null (and absent from eth_getBlockReceipts); a nanoreth
        // archive serves them.
        system_tx_receipts: false,
    }
}
