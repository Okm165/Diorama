//! The source seam: where the mirror reads the chain it mirrors.
//!
//! One implementation per profile, chosen at boot (`Arc<dyn Source>` — a
//! boundary, not a hot loop; the two projects never monomorphize each other's
//! types). `RethSource` (the mainnet crate) reads reth (BALs, traces, a
//! 10,064-block state window); the venue's profile composes the spine's
//! [`PinnedSource`] over [`RawSource`] for the HL public RPC (latest-only
//! state, no traces/BAL/proofs — measured 2026-10-24). The trait is exactly
//! what the pipeline and a hand-back ask for: capabilities, the head, a block
//! header, and the three account reads an unshadow restores.
use alloy_primitives::{Address, B256, U256};
// The seam's reads are dynamic (`&dyn Source`): one box per call, the
// standard `futures_util` form. These are per-block and admin-path reads,
// never a hot loop.
use futures_util::future::BoxFuture;
use std::sync::Arc;

/// What a source can serve. Probed once at boot and logged; every read path
/// gates on it — never assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Capabilities {
    /// `eth_getBlockAccessList` (reth; anvil ≥ Amsterdam records its own).
    pub native_bal: bool,
    /// `debug_traceBlockByNumber{prestateTracer, diffMode}`.
    pub traces: bool,
    /// State reads at arbitrary blocks; when false, historical reads are
    /// refused with the numbers, never answered with a newer value.
    pub historical: bool,
    /// `eth_subscribe newHeads` (reth; the HL public RPC documents none).
    pub heads_ws: bool,
    /// `eth_getSystemTxsByBlockNumber` (HyperEVM's Core→EVM completions).
    pub system_txs: bool,
    /// Receipts (and therefore logs) for the system txs. The HL public RPC
    /// serves the txs but **not** their receipts (measured 2026-10-24: a
    /// system tx's receipt is `null` and `eth_getBlockReceipts` returns only
    /// the block's normal txs), so a composer on this source must decode the
    /// system txs' calldata for their touched accounts; a nanoreth archive
    /// serves the receipts.
    pub system_tx_receipts: bool,
}

/// The truth an unshadow restores from, and the pipeline's read side.
pub trait Source: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    /// The source's own head: the anchor before the first apply, when the
    /// devnet has no cursor yet (nothing applied to reflect).
    fn head(&self) -> BoxFuture<'_, mirror_anvil::Result<u64>>;

    /// The block's header: identity (number/hash/parent) plus the context the
    /// applier aligns to (timestamp, coinbase, prevRandao, gas limit, base
    /// fee). The official type — no mirror-owned block shape.
    fn header(&self, n: u64) -> BoxFuture<'_, mirror_anvil::Result<alloy_rpc_types_eth::Header>>;

    fn balance_at(&self, address: Address, block: u64) -> BoxFuture<'_, mirror_anvil::Result<U256>>;

    fn nonce_at(&self, address: Address, block: u64) -> BoxFuture<'_, mirror_anvil::Result<u64>>;

    /// A storage word: the wire form (32 bytes), converted at the write site.
    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<B256>>;

    fn code_at(&self, address: Address, block: u64) -> BoxFuture<'_, mirror_anvil::Result<alloy_primitives::Bytes>>;

    /// Read one key. The default composes the four typed reads, so a source
    /// implements only the primitives it can serve; the composer reads its
    /// plan through this seam (one call shape for every key kind).
    fn read_key(&self, key: mirror_anvil::Key, block: u64) -> BoxFuture<'_, mirror_anvil::Result<mirror_anvil::Value>> {
        Box::pin(async move {
            Ok(match key {
                mirror_anvil::Key::Storage { address, slot } => {
                    let word = self.storage_at(address, slot, block).await?;
                    mirror_anvil::Value::Storage(U256::from_be_bytes(word.0))
                }
                mirror_anvil::Key::Balance(address) => {
                    mirror_anvil::Value::Balance(self.balance_at(address, block).await?)
                }
                mirror_anvil::Key::Nonce(address) => {
                    mirror_anvil::Value::Nonce(self.nonce_at(address, block).await?)
                }
                mirror_anvil::Key::Code(address) => {
                    mirror_anvil::Value::Code(self.code_at(address, block).await?)
                }
            })
        })
    }

    /// Read a plan of keys: the default is the sequential composition (every
    /// source stays correct); the RPC sources override with one mixed batch.
    /// A wide window's plan reaches thousands of keys (every transfer's
    /// holder), and the per-key form cost a round-trip per key against the
    /// public source (measured live: a 300-block catch-up window ground for
    /// minutes).
    fn read_keys(
        &self,
        keys: &[mirror_anvil::Key],
        block: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<Vec<mirror_anvil::Value>>> {
        let keys = keys.to_vec();
        Box::pin(async move {
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                out.push(self.read_key(key, block).await?);
            }
            Ok(out)
        })
    }

    /// The native BAL for block `n`, when the source records one (EIP-7928).
    /// Default: none — the composer falls to the synthesized rung (the
    /// capability flag selects the rung; this answers per block, because a
    /// node can serve some blocks and not others).
    fn bal(&self, n: u64) -> BoxFuture<'_, mirror_anvil::Result<Option<alloy_eips::eip7928::BlockAccessList>>> {
        let _ = n;
        Box::pin(async move { Ok(None) })
    }

    /// The window's touches: the addresses and logs the declared closures
    /// plan from, plus any changed keys the source already carries values for
    /// (the trace rung). Every source implements this — it is the synthesized
    /// rung's data side.
    fn touched(&self, from: u64, to: u64) -> BoxFuture<'_, mirror_anvil::Result<crate::closure::TouchedSet>>;

    /// What this source's touched rung is (`Trace` for a trace-based source,
    /// `Poll` for the envelope rung): the window BAL's `kind` label.
    fn touched_kind(&self) -> crate::bal::DiffKind;
}

impl Capabilities {
    /// Verify the method-presence flags against the live endpoint and return
    /// the flags that disagree with the declaration (empty = all agree). The
    /// engines call this once at boot and log any disagreement: a declared
    /// capability that the endpoint does not serve is a misconfiguration,
    /// named at the only moment it is cheap to see.
    ///
    /// Each probe targets a block that does not exist yet: a method that
    /// exists answers an error that is *not* `-32601` (or a null result), a
    /// method that does not answers exactly that. No traces or BALs are
    /// actually fetched — the probe never pays a full trace's cost.
    ///
    /// Behavioral capabilities (`historical`, `heads_ws`) are not probed:
    /// "the block parameter is ignored" cannot be distinguished from "the
    /// value did not change" by a read. They stay declarations with their
    /// measurements, enforced at the read path (the venue source refuses a
    /// non-latest read rather than silently answering a newer value).
    pub async fn verify_methods(
        &self,
        rpc: &mirror_anvil::Rpc,
        future: u64,
    ) -> Vec<&'static str> {
        use alloy_primitives::U64;
        let mut disagree = Vec::new();

        let bal = rpc
            .call::<serde_json::Value>("eth_getBlockAccessList", (U64::from(future),))
            .await;
        if self.native_bal != method_present(bal) {
            disagree.push("native_bal");
        }

        let trace = rpc
            .call::<serde_json::Value>(
                mirror_anvil::TRACE_BLOCK_METHOD,
                (
                    U64::from(future),
                    serde_json::json!({"tracer": "prestateTracer", "tracerConfig": {"diffMode": true}}),
                ),
            )
            .await;
        if self.traces != method_present(trace) {
            disagree.push("traces");
        }

        let system = rpc
            .call::<serde_json::Value>("eth_getSystemTxsByBlockNumber", (U64::from(future),))
            .await;
        if self.system_txs != method_present(system) {
            disagree.push("system_txs");
        }

        disagree
    }
}

/// Did the method exist? Anything but `-32601` means yes (a missing block is
/// an answer from a method that exists).
fn method_present<T>(r: mirror_anvil::Result<T>) -> bool {
    match r {
        Ok(_) => true,
        Err(e) => !e.is_method_not_found(),
    }
}

/// Probe a source's declared method-presence capabilities against the live
/// endpoint, once at boot (both engines call this for their follow source):
/// a declared capability the endpoint does not serve — or one it serves that
/// the declaration denies — is named at the only moment it is cheap to see,
/// never discovered mid-window. A probe fault is the caller's (a boot read
/// that cannot happen is a boot fault).
pub async fn audit_capabilities(
    what: &str,
    caps: Capabilities,
    rpc: &mirror_anvil::Rpc,
) -> mirror_anvil::Result<()> {
    let head = rpc.block_number().await?;
    let disagree = caps.verify_methods(rpc, head.saturating_add(1_000)).await;
    if !disagree.is_empty() {
        tracing::warn!(
            source = what,
            disagree = ?disagree,
            "declared source capabilities disagree with the endpoint"
        );
    }
    Ok(())
}

/// A source over a raw RPC: the reads are forwarded verbatim and the
/// capabilities are the caller's declaration. For reads that are
/// latest-by-design (the venue's follow reads the public RPC at
/// latest, where the pin guard would refuse every read) and for tools; the
/// pin-exact shape for unshadow restores is [`PinnedSource`] over this.
pub struct RawSource {
    rpc: Arc<mirror_anvil::Rpc>,
    capabilities: Capabilities,
}

impl RawSource {
    /// The handle is shared (`Arc`): one client per endpoint, referenced by
    /// every object that reads through it.
    pub fn new(rpc: Arc<mirror_anvil::Rpc>, capabilities: Capabilities) -> Self {
        Self { rpc, capabilities }
    }

    /// The devnet chain as the composer's target: the synthesis rung reads it
    /// for the diff (traces yes; no BAL — the target is only read, never
    /// composed from).
    pub fn target(rpc: Arc<mirror_anvil::Rpc>) -> Self {
        Self::new(
            rpc,
            Capabilities {
                native_bal: false,
                traces: true,
                historical: true,
                heads_ws: false,
                system_txs: false,
                system_tx_receipts: false,
            },
        )
    }
}

/// The composer's target: the devnet chain's raw reads (T / V) — the one
/// builder both profiles call.
pub fn target(rpc: Arc<mirror_anvil::Rpc>) -> Arc<dyn Source> {
    Arc::new(RawSource::target(rpc))
}

impl Source for RawSource {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn head(&self) -> BoxFuture<'_, mirror_anvil::Result<u64>> {
        Box::pin(self.rpc.block_number())
    }

    fn header(&self, n: u64) -> BoxFuture<'_, mirror_anvil::Result<alloy_rpc_types_eth::Header>> {
        Box::pin(async move { Ok(self.rpc.block_by_number(n, mirror_anvil::Txs::Hashes).await?.header) })
    }

    fn balance_at(&self, address: Address, block: u64) -> BoxFuture<'_, mirror_anvil::Result<U256>> {
        Box::pin(self.rpc.balance_at(address, block))
    }

    fn nonce_at(&self, address: Address, block: u64) -> BoxFuture<'_, mirror_anvil::Result<u64>> {
        Box::pin(self.rpc.nonce_at(address, block))
    }

    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<B256>> {
        Box::pin(self.rpc.storage_at(address, slot, block))
    }

    fn code_at(
        &self,
        address: Address,
        block: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<alloy_primitives::Bytes>> {
        Box::pin(self.rpc.code_at(address, block))
    }

    /// The read plan in one mixed body: the four key kinds map to their RPC
    /// methods (the client owns the mapping and the decode).
    fn read_keys(
        &self,
        keys: &[mirror_anvil::Key],
        block: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<Vec<mirror_anvil::Value>>> {
        let keys = keys.to_vec();
        Box::pin(async move { self.rpc.read_keys(&keys, block).await })
    }

    /// The envelope rung: the generic EVM touches (txs, receipts, and — when
    /// declared — HyperEVM's system txs), no values (the state reads fill
    /// them).
    fn touched(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<crate::closure::TouchedSet>> {
        let system_txs = self.capabilities.system_txs;
        Box::pin(crate::bal::envelope_touches(&self.rpc, from, to, system_txs))
    }

    fn touched_kind(&self) -> crate::bal::DiffKind {
        crate::bal::DiffKind::Poll
    }
}

/// A pinned, latest-only source: the endpoint serves only its *latest* state
/// (the block parameter is ignored — measured), so a state read is exact only
/// while the source's head is still the pin — and the pin *is* the head. The
/// guard refuses with the numbers instead of silently answering from newer
/// state ("never hand back a poisoned value"); a source that declares
/// `historical` (reth's 10,064-block window, an archive) needs no check and
/// passes through. Headers and envelopes are historical-safe by measurement
/// and are never guarded.
pub struct PinnedSource {
    inner: RawSource,
    pin: Arc<std::sync::atomic::AtomicU64>,
}

impl PinnedSource {
    /// The handle is shared (`Arc`): one client per endpoint, referenced by
    /// every object that reads through it.
    pub fn new(
        rpc: Arc<mirror_anvil::Rpc>,
        pin: Arc<std::sync::atomic::AtomicU64>,
        capabilities: Capabilities,
    ) -> Self {
        Self {
            inner: RawSource::new(rpc, capabilities),
            pin,
        }
    }

    /// The pin guard: every state read is only exact while the source's head
    /// is still the pin. The declaration drives it — a source that can serve
    /// historical state needs no check; this one cannot (measured: the block
    /// parameter is ignored, latest is all there is).
    async fn guard(&self) -> mirror_anvil::Result<()> {
        if self.inner.capabilities().historical {
            return Ok(());
        }
        let Some(pin) = crate::pin::resolved_pin(&self.pin) else {
            return Err(mirror_anvil::Error::Corrupt {
                what: "pinned source",
                detail: "the fork pin is not resolved yet".into(),
            });
        };
        let head = self.inner.head().await?;
        if head != pin {
            return Err(mirror_anvil::Error::Corrupt {
                what: "pinned source",
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

impl Source for PinnedSource {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    /// The pin is the head: the source cannot serve history, so its own head
    /// would drift past every read's truth (a latest-by-design reader — the
    /// venue's follow — takes the raw source instead).
    fn head(&self) -> BoxFuture<'_, mirror_anvil::Result<u64>> {
        Box::pin(async move {
            if let Some(pin) = crate::pin::resolved_pin(&self.pin) {
                return Ok(pin);
            }
            self.inner.head().await
        })
    }

    /// Historical-safe on the public source (unlike state reads): the header
    /// is what makes forking and cursor hashes possible at all. No guard.
    fn header(&self, n: u64) -> BoxFuture<'_, mirror_anvil::Result<alloy_rpc_types_eth::Header>> {
        self.inner.header(n)
    }

    fn balance_at(&self, address: Address, block: u64) -> BoxFuture<'_, mirror_anvil::Result<U256>> {
        Box::pin(async move {
            self.guard().await?;
            self.inner.balance_at(address, block).await
        })
    }

    fn nonce_at(&self, address: Address, block: u64) -> BoxFuture<'_, mirror_anvil::Result<u64>> {
        Box::pin(async move {
            self.guard().await?;
            self.inner.nonce_at(address, block).await
        })
    }

    fn storage_at(
        &self,
        address: Address,
        slot: U256,
        block: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<B256>> {
        Box::pin(async move {
            self.guard().await?;
            self.inner.storage_at(address, slot, block).await
        })
    }

    fn code_at(
        &self,
        address: Address,
        block: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<alloy_primitives::Bytes>> {
        Box::pin(async move {
            self.guard().await?;
            self.inner.code_at(address, block).await
        })
    }

    /// One guard for the whole plan (the per-key form paid it per key), then
    /// the batched read.
    fn read_keys(
        &self,
        keys: &[mirror_anvil::Key],
        block: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<Vec<mirror_anvil::Value>>> {
        let keys = keys.to_vec();
        Box::pin(async move {
            self.guard().await?;
            self.inner.read_keys(&keys, block).await
        })
    }

    /// Envelope reads are historical-safe, so the pin guard does not apply.
    fn touched(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, mirror_anvil::Result<crate::closure::TouchedSet>> {
        self.inner.touched(from, to)
    }

    fn touched_kind(&self) -> crate::bal::DiffKind {
        self.inner.touched_kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    fn caps(historical: bool) -> Capabilities {
        Capabilities {
            native_bal: false,
            traces: false,
            historical,
            heads_ws: false,
            system_txs: false,
            system_tx_receipts: false,
        }
    }

    fn pinned(capabilities: Capabilities, pin: u64) -> PinnedSource {
        // The endpoint is never reached by the declaration and unresolved-pin
        // branches: both decide before any read.
        PinnedSource::new(
            Arc::new(mirror_anvil::Rpc::new("http://127.0.0.1:1", Duration::from_secs(1)).unwrap()),
            Arc::new(AtomicU64::new(pin)),
            capabilities,
        )
    }

    /// A source that declares history passes the guard untouched (reth's
    /// window, an archive): the pin never refuses a historical read.
    #[tokio::test]
    async fn a_historical_source_is_never_guarded() {
        assert!(pinned(caps(true), crate::pin::PIN_UNRESOLVED).guard().await.is_ok());
        assert!(pinned(caps(true), 7).guard().await.is_ok());
    }

    /// An unresolved pin refuses before any read, naming the state; a
    /// resolved pin moves the check to the head read (the transport fault
    /// proves the read was attempted, not skipped).
    #[tokio::test]
    async fn an_unresolved_pin_refuses_before_any_read() {
        let err = pinned(caps(false), crate::pin::PIN_UNRESOLVED)
            .guard()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pin is not resolved"), "{err}");
        let err = pinned(caps(false), 7).guard().await.unwrap_err();
        assert!(matches!(err, mirror_anvil::Error::Transport { .. }), "{err}");
    }

    /// The pinned head is the pin, not the source's (the guard's premise): a
    /// resolved pin answers without a read; unresolved falls through to the
    /// source (the transport fault proves the read was attempted).
    #[tokio::test]
    async fn a_pinned_head_is_the_pin_not_the_sources() {
        assert_eq!(pinned(caps(false), 7).head().await.unwrap(), 7);
        let err = pinned(caps(false), crate::pin::PIN_UNRESOLVED)
            .head()
            .await
            .unwrap_err();
        assert!(matches!(err, mirror_anvil::Error::Transport { .. }), "{err}");
    }
}
