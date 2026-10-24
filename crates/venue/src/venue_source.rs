//! `VenueSource` — the unshadow re-sync truth for the venue profile.
//!
//! The mainnet profile re-syncs a handed-back key from reth at the applied
//! cursor; the venue's truth is the fork source **at the pin**: V's base state
//! *is* the source's state at that block.
//!
//! Fail-closed, because the public source cannot serve history: its `eth_call`
//! returns the *latest* state (measured: V's fork at block N differs from the
//! source's answer at N, while V's state IS the source's state at N — the
//! block parameter is ignored, or the retention window is shorter than a
//! re-pin interval). A read is therefore only exact while the source's head is
//! still the pin; once the source has advanced, the read is *refused* with the
//! numbers rather than silently returning a newer value (the "never hand back
//! a poisoned value" rule).
//!
//! The registry is **not** a substitute: its `values` are the *devnet's*
//! latest writes (what a re-fork must restore), not the source's before-state.
//! The before-state lives in the **journal** (each engine write records it) —
//! so a selective hand-back could serve a key's *oldest* journal entry, gated
//! on the ring still covering the epoch (`first_block + keep > head`). That
//! slice is planned, not built: today the refusal above is fail-closed, and
//! the wholesale hand-back is the epoch itself (the re-pin's re-fork restores
//! the source's whole state — measured: the pin is seconds stale on a
//! 1s-block source, so the selective window is only reliable against a
//! scratch/slow source).
use alloy_primitives::{Address, B256, U256};
use mirror_anvil::{Error, Rpc};
use mirror_devnet::source::{Capabilities, Fut, Source};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct VenueSource {
    rpc: Arc<Rpc>,
    /// V's fork pin (0 = not yet resolved: the driver sets it during boot's
    /// reconcile, before the HTTP surface serves).
    pin: Arc<AtomicU64>,
}

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

impl VenueSource {
    /// The handle is shared (`Arc`): one client per endpoint, referenced by
    /// every object that reads through it.
    pub fn new(rpc: Arc<Rpc>, pin: Arc<AtomicU64>) -> Self {
        Self { rpc, pin }
    }

    /// The pin guard: every read is only exact while the source's head is
    /// still the pin. The declaration drives it — a source that can serve
    /// historical state (an archive) needs no check; this one cannot
    /// (measured: the block parameter is ignored, latest is all there is).
    async fn guard(&self) -> Result<(), Error> {
        if self.capabilities().historical {
            return Ok(());
        }
        let pin = self.pin.load(Ordering::Relaxed);
        if pin == 0 {
            return Err(Error::Corrupt {
                what: "venue source",
                detail: "the fork pin is not resolved yet".into(),
            });
        }
        let head = self.rpc.block_number().await?;
        if head != pin {
            return Err(Error::Corrupt {
                what: "venue source",
                detail: format!(
                    "the source advanced past the pin (pin {pin}, head {head}): it \
                     serves only its latest state for eth_call (measured), so the \
                     pin's truth cannot be read back. A re-pin restores the whole \
                     source state (the re-fork is the wholesale hand-back); a \
                     *selective* hand-back is possible only while the source's head \
                     is still the pin — seconds on a 1s-block source"
                ),
            });
        }
        Ok(())
    }
}

// TRUST: the source's answers are venue-reported, never storage-proven. The
// guard is the fail-closed pin check below: while the source head differs from
// the pin (the public RPC serves latest regardless of the block parameter —
// measured), reads are refused instead of served from the wrong state.

impl Source for VenueSource {
    /// What the public venue source can serve (measured 2026-10-24): no BAL,
    /// no traces, no proofs, no websocket JSON-RPC; state reads are
    /// latest-only (the block parameter is ignored), and system transactions
    /// (Core→EVM completions) are exposed through their own RPC method.
    fn capabilities(&self) -> Capabilities {
        public_capabilities()
    }

    /// The source's anchor: the pin. Before the pin is resolved (a boot race
    /// the bin's ordering rules out) the source's head is the only truthful
    /// answer left; it is never the steady-state path.
    fn head(&self) -> Fut<'_, mirror_anvil::Result<u64>> {
        Box::pin(async move {
            let pin = self.pin.load(Ordering::Relaxed);
            if pin != 0 {
                return Ok(pin);
            }
            self.rpc.block_number().await
        })
    }

    /// Block headers are historical-safe on the public source (unlike state
    /// reads): `eth_getBlockByNumber` serves old blocks, which is what makes
    /// forking and cursor hashes possible at all. No pin guard here.
    fn header(&self, n: u64) -> Fut<'_, mirror_anvil::Result<alloy_rpc_types_eth::Header>> {
        Box::pin(async move {
            let block = self.rpc.block_by_number(n, false).await?;
            Ok(block.header)
        })
    }

    fn balance_at(&self, address: Address, block: u64) -> Fut<'_, mirror_anvil::Result<U256>> {
        Box::pin(async move {
            self.guard().await?;
            self.rpc.balance_at(address, block).await
        })
    }

    fn nonce_at(&self, address: Address, block: u64) -> Fut<'_, mirror_anvil::Result<u64>> {
        Box::pin(async move {
            self.guard().await?;
            self.rpc.nonce_at(address, block).await
        })
    }

    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> Fut<'_, mirror_anvil::Result<B256>> {
        Box::pin(async move {
            self.guard().await?;
            self.rpc.storage_at(address, slot, block).await
        })
    }

    fn code_at(
        &self,
        address: Address,
        block: u64,
    ) -> Fut<'_, mirror_anvil::Result<alloy_primitives::Bytes>> {
        Box::pin(async move {
            self.guard().await?;
            self.rpc.code_at(address, block).await
        })
    }

    /// The envelope rung: the same generic EVM touches the public API serves
    /// (txs, receipts, and the HyperEVM system txs — `Capabilities` says so).
    /// Envelope reads are historical-safe, so the pin guard does not apply.
    fn touched(
        &self,
        from: u64,
        to: u64,
    ) -> Fut<'_, mirror_anvil::Result<mirror_devnet::closure::TouchedSet>> {
        let system_txs = self.capabilities().system_txs;
        Box::pin(mirror_devnet::bal::envelope_touches(
            &self.rpc,
            from,
            to,
            system_txs,
        ))
    }

    fn touched_kind(&self) -> mirror_devnet::bal::DiffKind {
        mirror_devnet::bal::DiffKind::Poll
    }
}
