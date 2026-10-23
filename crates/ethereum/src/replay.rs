//! Replay loop: re-execute every parent block on M, then verify it.
//!
//! Sequence per block (plan §4.5): set block env -> apply withdrawals ->
//! automine off -> send raws in order (chunked) -> mine 1 -> verify.
use crate::bal;
use crate::blob::{
    available_forms, encode_blob_tx, repair_proofs, select_sidecars, BlobForm, SidecarItem,
};
use crate::ctx::{block_txs, cursor_from, Ctx};
use crate::error::{BlobError, Error, Result};
use crate::rpc::Rpc;
use crate::verify::{verify_block, Verification};
use alloy_consensus::{EthereumTxEnvelope, TxEip4844, TxEip4844Variant};
use alloy_eips::eip2718::Encodable2718;
use alloy_consensus::Transaction as _;
use alloy_network_primitives::TransactionResponse;
use alloy_primitives::{Address, Bytes, Signature, B256, U256, U64};
use mirror_anvil::Call;
use mirror_devnet::metrics::Registry as _;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

pub struct ReplayOutcome {
    pub txs: usize,
    pub replayed: usize,
    /// Always 0: a block whose sidecars are missing is never mined (the replay
    /// holds M and retries), so a "partial" outcome cannot exist.
    pub skipped_blobs: usize,
    pub duration_ms: u64,
    pub verification: Option<Verification>,
}

struct RawTx {
    hash: B256,
    bytes: Bytes,
    /// Blob txs keep their parts so a rejected wrapper form can be retried.
    blob: Option<(TxEip4844, Signature, B256, Vec<SidecarItem>)>,
}

pub async fn replay_block(ctx: &Ctx, n: u64) -> Result<ReplayOutcome> {
    let started = Instant::now();
    let block = ctx.reth.block_by_number(n, true).await?;

    // Chain sanity: M's head must be exactly n-1 (catches rebase races).
    let m_head = ctx.anvil_m.block_number().await?;
    if m_head >= n {
        // A previous attempt completed after its request timed out: verify and
        // advance instead of replaying (and never double-mine).
        tracing::warn!(block = n, head = m_head, "block already mined; verifying late completion");
        return finish_replay(ctx, n, block, started, 0).await;
    }
    if m_head + 1 != n {
        return Err(Error::ReplayOrder { m_head, n });
    }
    // Leftover pool txs from a timed-out attempt would make the resend dirty.
    if !pool_hashes(&ctx.anvil_m).await?.is_empty() {
        tracing::warn!(block = n, "clearing leftover pool txs from a previous attempt");
        clear_pool(&ctx.anvil_m).await?;
    }

    repair_m_state(ctx, n).await?;
    convergence_probes(ctx, n).await?;

    // Block env (the parent header verbatim: timestamp, coinbase, prevRandao,
    // gas limit, base fee).
    let h = &block.header;
    ctx.anvil_m
        .set_next_block_env(h.timestamp, h)
        .await
        .map_err(|source| Error::BlockEnv {
            block: n,
            source: Box::new(source),
        })?;

    if let Some(withdrawals) = &block.withdrawals {
        credit_withdrawals(ctx, n, withdrawals).await?;
    }

    // Raw txs (blob txs get their sidecar from the beacon API). A block that
    // cannot be replayed in full is never mined: M must stay strict.
    let (raws, skipped_blobs) = build_raws(ctx, n, &block).await?;
    if skipped_blobs > 0 {
        // The alarm the operator watches for CL sidecar trouble (the replay
        // holds M and retries; the block is never mined partial or empty).
        ctx.eth_metrics.inc(&ctx.eth_metrics.blob_failures);
        return Err(Error::BlobsUnavailable {
            block: n,
            skipped: skipped_blobs,
        });
    }

    send_with_retry(ctx, n, &raws, block_txs(&block)).await?;
    ctx.anvil_m.anvil_mine(1).await?;

    finish_replay(ctx, n, block, started, skipped_blobs).await
}

/// Convergence repair: force M's state to mainnet's post-state of n-1 from the
/// parent BAL. M cannot reproduce blockhash-dependent values (A17: e.g. Seaport
/// 1.6 counters), and one divergence would cascade forever (invalid txs,
/// different statuses). The BAL is authoritative for every changed entry, so
/// applying it makes M exact again; the per-block verification still measures
/// what execution got right.
async fn repair_m_state(ctx: &Ctx, n: u64) -> Result<()> {
    let repair = ctx.engine.loops.repair_m
        && ctx.last_m_diverged.load(std::sync::atomic::Ordering::Relaxed);
    if !repair {
        if ctx.engine.loops.repair_m {
            tracing::debug!(block = n, "previous M block verified clean; no repair needed");
        }
        return Ok(());
    }
    if let Some(bal) = ctx.reth.block_access_list(n - 1).await? {
        let writes = bal::flatten_bal(&bal);
        let applied = mirror_anvil::apply_writes(
            &ctx.anvil_m,
            &writes,
            ctx.engine.loops.chunk_size,
        )
        .await?;
        ctx.eth_metrics.add(&ctx.eth_metrics.m_repair_writes, applied as u64);
        tracing::debug!(
            block = n,
            writes = writes.len(),
            applied,
            "M state converged from parent BAL"
        );
    } else {
        // No BAL available: a named, bounded degradation (the probes below
        // still measure the state; the next block repairs).
        tracing::debug!(block = n, "no parent BAL; M repair skipped");
    }
    Ok(())
}

/// The convergence probes are the state-level assertion "M == mainnet at n-1":
/// they run for *every* block, repaired or not (a clean block's state is the
/// replay's own product, which is exactly what this measures).
async fn convergence_probes(ctx: &Ctx, n: u64) -> Result<()> {
    let probes =
        mirror_devnet::probes::ProbeRunner::replay(&ctx.engine.verifier.probes)
            .parity(&ctx.reth, n - 1, &ctx.anvil_m)
            .await?;
    ctx.eth_metrics.add(&ctx.eth_metrics.m_convergence_ok, probes.ok as u64);
    ctx.plane.metrics
        .add(&ctx.eth_metrics.m_convergence_failed, probes.failed as u64);
    ctx.plane.metrics
        .set(&ctx.eth_metrics.m_convergence_last_failed, probes.failed as u64);
    Ok(())
}

/// Withdrawals are not txs: credit recipients directly. The credit is
/// idempotent by construction: `setBalance(reth_pre + amount)` always yields
/// the same absolute post-block value, so a retried block can never
/// double-credit.
///
/// The pre-balance must come from **reth**, not from M at N-1: when a block is
/// retried before it is mined, anvil's "N-1" *is* the live state, which already
/// contains the first attempt's credit. Measured once as `withdrawals`
/// divergence (M = reth_post + amount).
async fn credit_withdrawals(
    ctx: &Ctx,
    n: u64,
    withdrawals: &[alloy_rpc_types_eth::Withdrawal],
) -> Result<()> {
    let credits = withdrawal_credits(withdrawals);
    let reads: Vec<Call> = credits
        .keys()
        .map(|address| Call::new("eth_getBalance", (*address, U64::from(n - 1))))
        .collect::<mirror_anvil::Result<_>>()?;
    let mut pre_balances: Vec<U256> = Vec::with_capacity(reads.len());
    for chunk in reads.chunks(ctx.engine.loops.chunk_size) {
        for result in ctx.reth.batch(chunk).await? {
            let value = result.map_err(|payload| Error::PreBalance {
                block: n,
                payload,
            })?;
            pre_balances.push(serde_json::from_str(value.get()).map_err(|_| {
                mirror_anvil::Error::bad_quantity(ctx.reth.url(), "eth_getBalance", value.get())
            })?);
        }
    }
    let mut calls: Vec<Call> = Vec::with_capacity(credits.len());
    for ((address, amount), pre) in credits.iter().zip(pre_balances) {
        calls.push(Call::new("anvil_setBalance", (*address, pre + *amount))?);
    }
    for chunk in calls.chunks(ctx.engine.loops.chunk_size) {
        for result in ctx.anvil_m.batch(chunk).await? {
            if let Err(payload) = result {
                tracing::warn!(error = %payload, "withdrawal credit failed");
            }
        }
    }
    Ok(())
}

/// Verify the mined block and advance the cursor (shared by the normal path and
/// the late-completion recovery path).
async fn finish_replay(
    ctx: &Ctx,
    n: u64,
    block: alloy_rpc_types_eth::Block,
    started: Instant,
    skipped_blobs: usize,
) -> Result<ReplayOutcome> {
    // Full txs: the verifier checks tx order (hashes in position).
    let m_block = ctx.anvil_m.block_by_number(n, true).await?;
    let verification = if ctx.engine.verifier.enabled {
        let v = verify_block(ctx, n, &block, &m_block).await?;
        if v.receipts_unverified {
            ctx.eth_metrics.inc(&ctx.eth_metrics.receipts_unverified);
        }
        ctx.last_m_diverged
            .store(!v.ok, std::sync::atomic::Ordering::Relaxed);
        Some(v)
    } else {
        None
    };

    // 6. cursor + metrics + drift.
    let first_block = ctx
        .cursor_m()?
        .map(|c| c.first_block)
        .unwrap_or(n);
    ctx.set_cursor_m(cursor_from(&m_block, first_block))?;
    ctx.eth_metrics.set(&ctx.eth_metrics.cursor_m, n);
    ctx.set_reth_last_hash(block.header.hash)?;
    ctx.set_reth_last_block(n)?;
    ctx.plane.save_state()?;
    ctx.eth_metrics.inc(&ctx.eth_metrics.blocks_replayed);
    let duration_ms = started.elapsed().as_millis() as u64;
    ctx.eth_metrics.set(&ctx.eth_metrics.last_replay_ms, duration_ms);
    ctx.eth_metrics.set(&ctx.eth_metrics.m_head, n);
    ctx.plane.metrics
        .set(&ctx.eth_metrics.pin_age_blocks, n.saturating_sub(first_block) + 1);

    let outcome = ReplayOutcome {
        txs: crate::ctx::block_txs(&block).len(),
        replayed: crate::ctx::block_txs(&block).len() - skipped_blobs,
        skipped_blobs,
        duration_ms,
        verification,
    };
    log_and_drift(ctx, n, &outcome, &block)?;
    Ok(outcome)
}

/// Build the ordered raw-tx list; blob txs are encoded with a sidecar when one
/// can be fetched (network form), otherwise skipped and the block marked partial.
async fn build_raws(
    ctx: &Ctx,
    n: u64,
    block: &alloy_rpc_types_eth::Block,
) -> Result<(Vec<RawTx>, usize)> {
    let txs = block_txs(block);
    let has_blobs = txs
        .iter()
        .any(|t| matches!(&*t.inner, EthereumTxEnvelope::Eip4844(_)));
    let sources = blob_sidecar_sources(ctx, n, has_blobs).await?;
    let preferred = ctx.blob_form()?;

    let mut out = Vec::with_capacity(txs.len());
    let mut skipped = 0usize;
    for tx in txs {
        match &*tx.inner {
            EthereumTxEnvelope::Eip4844(signed) => {
                let signature = *signed.signature();
                let hash = *signed.hash();
                let TxEip4844Variant::TxEip4844(t) = signed.tx() else {
                    // Already carries a sidecar: encode as-is.
                    out.push(RawTx {
                        hash: tx.tx_hash(),
                        bytes: tx.inner.encoded_2718().into(),
                        blob: None,
                    });
                    continue;
                };
                let blob_tx = BlobTx {
                    tx: t,
                    signature,
                    hash,
                };
                match encode_from_sources(ctx, &blob_tx, &sources, preferred) {
                    Ok((bytes, items)) => out.push(RawTx {
                        hash: tx.tx_hash(),
                        bytes,
                        blob: Some((t.clone(), signature, hash, items)),
                    }),
                    Err(last_err) => {
                        skipped += 1;
                        warn_skipped_blob(&ctx.blob_warned, n, tx.tx_hash(), &last_err);
                    }
                }
            }
            _ => out.push(RawTx {
                hash: tx.tx_hash(),
                bytes: tx.inner.encoded_2718().into(),
                blob: None,
            }),
        }
    }
    Ok((out, skipped))
}

/// Fetch sidecar candidates from every configured source. Beacon root of
/// payload N = parent beacon block root of EL block N+1 (verified against
/// mainnet). Candidates are validated with KZG per tx below, so a source that
/// cannot reconstruct blobs (e.g. a non-supernode after PeerDAS) is skipped
/// instead of poisoning the pool.
async fn blob_sidecar_sources(
    ctx: &Ctx,
    n: u64,
    has_blobs: bool,
) -> Result<Vec<(String, Vec<SidecarItem>)>> {
    if !has_blobs || !ctx.engine.blobs.enabled || !ctx.beacon.enabled() {
        return Ok(Vec::new());
    }
    let Some(next) = ctx.reth.block_by_number_opt(n + 1, false).await? else {
        return Ok(Vec::new());
    };
    let Some(root) = next.header.parent_beacon_block_root else {
        tracing::warn!(block = n, "no parent beacon block root on N+1");
        return Ok(Vec::new());
    };
    let sources = ctx.beacon.blob_sidecar_sources(root).await;
    if sources.is_empty() {
        tracing::warn!(block = n, "no beacon source returned blob sidecars");
    }
    Ok(sources)
}

/// The blob tx being re-encoded: the consensus tx, its recovered signature
/// and its hash (its log identity) — one argument instead of three.
struct BlobTx<'a> {
    tx: &'a TxEip4844,
    signature: Signature,
    hash: B256,
}

/// Encode one blob tx from the first source whose sidecars select, repair and
/// encode: returns the network-form bytes with the repaired items, or the last
/// source's error (reported once per block by the caller).
fn encode_from_sources(
    ctx: &Ctx,
    blob_tx: &BlobTx<'_>,
    sources: &[(String, Vec<SidecarItem>)],
    preferred: Option<BlobForm>,
) -> std::result::Result<(Bytes, Vec<SidecarItem>), String> {
    let mut last_err = String::from("no beacon source returned sidecars");
    for (source, items) in sources {
        match encode_with_source(blob_tx, items, &blob_tx.tx.blob_versioned_hashes, preferred) {
            Ok((bytes, form, fixed)) => {
                ctx.set_blob_form(form).map_err(|e| e.to_string())?;
                return Ok((bytes, fixed));
            }
            Err(e) => {
                // A source that does not hold this tx's sidecar is an empty
                // answer, not a fault; anything else is a validation failure
                // worth a warning.
                if matches!(e, BlobError::NoSidecar { .. }) {
                    tracing::debug!(tx = %blob_tx.hash, source = %source, error = %e, "source does not hold this tx's sidecar");
                } else {
                    tracing::warn!(
                        tx = %blob_tx.hash,
                        source = %source,
                        error = %e,
                        "blob sidecar failed KZG validation - trying next source"
                    );
                }
                last_err = e.to_string();
            }
        }
    }
    Err(last_err)
}

/// One source's attempt: select -> repair proofs -> pick the first encodable
/// form in preference order.
fn encode_with_source(
    blob_tx: &BlobTx<'_>,
    items: &[SidecarItem],
    versioned_hashes: &[B256],
    preferred: Option<BlobForm>,
) -> std::result::Result<(Bytes, BlobForm, Vec<SidecarItem>), BlobError> {
    let selected = select_sidecars(items, versioned_hashes)?;
    // Recompute placeholder blob proofs (Lighthouse on Fulu) and verify every
    // triple before it reaches the pool.
    let fixed = repair_proofs(&selected, versioned_hashes)?;
    let selected: Vec<&SidecarItem> = fixed.iter().collect();
    let forms = available_forms(&selected, versioned_hashes.len());
    let mut order = vec![
        preferred.unwrap_or(BlobForm::Legacy),
        BlobForm::Legacy,
        BlobForm::Eip7594,
        BlobForm::Bare,
    ];
    order.dedup();
    for form in order {
        if !forms.contains(&form) {
            continue;
        }
        if let Ok(bytes) = encode_blob_tx(blob_tx.tx, blob_tx.signature, blob_tx.hash, &selected, form) {
            return Ok((bytes, form, fixed));
        }
    }
    Err(BlobError::Missing {
        what: "no encodable form (no sidecar proofs)",
    })
}

/// One WARN per block: the retry loop calls this again until the beacon
/// ingests the sidecars (with `follow: latest` that is seconds after the
/// block).
fn warn_skipped_blob(blob_warned: &AtomicU64, n: u64, hash: B256, last_err: &str) {
    let first = blob_warned
        .swap(n, std::sync::atomic::Ordering::Relaxed)
        != n;
    if first {
        tracing::warn!(
            block = n,
            tx = %hash,
            error = %last_err,
            "no valid blob sidecar from any source (will retry)"
        );
    } else {
        tracing::debug!(
            block = n,
            tx = %hash,
            error = %last_err,
            "blob sidecar still unavailable"
        );
    }
}

/// Send all raws; on rejection retry once (re-encoding blob txs with the next
/// wrapper form). Aborts the block - without mining - if a retry is needed twice.
async fn send_with_retry(
    ctx: &Ctx,
    n: u64,
    raws: &[RawTx],
    txs: &[alloy_rpc_types_eth::Transaction],
) -> Result<()> {
    let mut current: Vec<Bytes> = raws.iter().map(|r| r.bytes.clone()).collect();

    for attempt in 0..2 {
        let rejected = send_once(&ctx.anvil_m, n, raws, txs, &current).await?;
        if rejected.is_empty() {
            return Ok(());
        }

        // Retry once: only blob txs can be re-encoded (form mismatch); anything
        // else is a hard abort.
        let all_blob = rejected.iter().all(|i| raws[*i].blob.is_some());
        if attempt == 0 && all_blob {
            let next_form = next_blob_form(ctx)?;
            tracing::warn!(
                block_txs = raws.len(),
                rejected = rejected.len(),
                form = next_form.as_str(),
                "retrying block with next blob wrapper form"
            );
            clear_pool(&ctx.anvil_m).await?;
            reencode_blobs(n, raws, &rejected, next_form, &mut current)?;
            ctx.set_blob_form(next_form)?;
            continue;
        }

        clear_pool(&ctx.anvil_m).await?;
        return Err(Error::ReplayAborted {
            block: n,
            rejected: rejected.len(),
        });
    }
    Err(Error::ReplayRetryExhausted { block: n })
}

/// One send pass. Raws go one request at a time: batch items are processed
/// concurrently by the JSON-RPC server, which scrambles pool insertion order
/// (measured: type-4 txs landed 6 positions late). Tx order is load-bearing
/// for replay fidelity, so we pay the round-trips. Returns the indices the
/// node rejected.
async fn send_once(
    anvil_m: &Rpc,
    n: u64,
    raws: &[RawTx],
    txs: &[alloy_rpc_types_eth::Transaction],
    current: &[Bytes],
) -> Result<Vec<usize>> {
    let mut rejected: Vec<usize> = Vec::new();
    let mut nonces = NonceTracker::prepare(anvil_m, txs).await?;
    for (i, raw) in current.iter().enumerate() {
        if let Some(tx) = txs.get(i) {
            if let Err(e) = nonces.before_send(anvil_m, tx).await {
                tracing::warn!(tx = %raws[i].hash, error = %e, "nonce pre-adjust failed");
            }
        }
        match anvil_m.send_raw_batch(std::slice::from_ref(raw)).await {
            Ok(results) => {
                for r in results {
                    if let Err(e) = r {
                        rejected.push(i);
                        tracing::warn!(
                            index = i,
                            tx = %raws[i].hash,
                            error = %e,
                            "raw tx rejected"
                        );
                    }
                }
            }
            // Transport failure: retry the whole block via the caller.
            Err(e) => {
                return Err(Error::TxSend {
                    block: n,
                    index: i,
                    source: e,
                })
            }
        }
    }
    let pool = anvil_m.txpool_status().await;
    tracing::info!(
        sent = current.len(),
        rejected = rejected.len(),
        adjusted = nonces.manipulated.len(),
        pool = ?pool,
        "raw txs sent (pre-mine)"
    );
    nonces.restore(anvil_m).await?;
    Ok(rejected)
}

/// Re-encode the rejected blob txs in the next wrapper form.
fn reencode_blobs(
    n: u64,
    raws: &[RawTx],
    rejected: &[usize],
    form: BlobForm,
    current: &mut [Bytes],
) -> Result<()> {
    for i in rejected {
        // `all_blob` was checked by the caller; the `else` keeps this loop
        // total without an `expect` (a non-blob rejection aborts there).
        let Some((t, sig, hash, items)) = raws[*i].blob.as_ref() else {
            continue;
        };
        let item_refs: Vec<&SidecarItem> = items.iter().collect();
        current[*i] =
            encode_blob_tx(t, *sig, *hash, &item_refs, form).map_err(|source| {
                Error::BlobReencode {
                    block: n,
                    hash: raws[*i].hash,
                    source,
                }
            })?;
    }
    Ok(())
}

fn next_blob_form(ctx: &Ctx) -> Result<BlobForm> {
    Ok(match ctx.blob_form()? {
        Some(BlobForm::Legacy) => BlobForm::Eip7594,
        Some(BlobForm::Eip7594) => BlobForm::Bare,
        Some(BlobForm::Bare) => BlobForm::Legacy,
        None => BlobForm::Legacy,
    })
}

/// The withdrawals' per-recipient credit (gwei → wei), summed per address.
fn withdrawal_credits(withdrawals: &[alloy_rpc_types_eth::Withdrawal]) -> BTreeMap<Address, U256> {
    let mut credits: BTreeMap<Address, U256> = BTreeMap::new();
    for w in withdrawals {
        let wei = U256::from(w.amount) * U256::from(1_000_000_000u64);
        *credits.entry(w.address).or_default() += wei;
    }
    credits
}

/// Every tx currently in the pool (typed `txpool_content`).
async fn pool_hashes(rpc: &Rpc) -> Result<Vec<B256>> {
    let content = rpc.txpool_content().await?;
    let mut hashes: Vec<B256> = Vec::new();
    for side in [content.pending, content.queued] {
        for txs in side.into_values() {
            for tx in txs.into_values() {
                hashes.push(tx.tx_hash());
            }
        }
    }
    Ok(hashes)
}

/// Pool nonce-sequence tracking.
///
/// Anvil's pool keeps txs in *pending* only while their nonces form a
/// contiguous sequence from the account's state nonce; a tx beyond the sequence
/// is parked as *queued* and is never mined. Mainnet blocks can contain
/// legitimate gaps for a sender (EIP-7702 self-delegation advances the nonce by
/// two per tx: measured 0x1e6, 0x1e8, 0x1ea in one block), and the pool does not
/// model the authorization bump, so those txs would be stranded.
///
/// The tracker temporarily moves the sender's nonce to the gap tx's nonce so the
/// pool accepts it, then restores the pre-block nonce before mining - execution
/// re-validates in order against the restored state, with the real auth bumps.
struct NonceTracker {
    base: std::collections::BTreeMap<alloy_primitives::Address, u64>,
    /// Last nonce of the contiguous pending sequence per sender (-1 = none).
    seq_end: std::collections::BTreeMap<alloy_primitives::Address, i64>,
    manipulated: std::collections::BTreeSet<alloy_primitives::Address>,
    /// EIP-7702 authorities of this block's authorizations, recovered from the
    /// signatures. Their nonces bump mid-execution, so a sender listed here
    /// legitimately sits "ahead" of the pre-block state and is not an anomaly.
    auth_bumps: std::collections::BTreeMap<alloy_primitives::Address, usize>,
}

impl NonceTracker {
    async fn prepare(anvil_m: &Rpc, txs: &[alloy_rpc_types_eth::Transaction]) -> Result<Self> {
        let head = anvil_m.block_number().await?;
        let mut base = std::collections::BTreeMap::new();
        for tx in txs {
            let from = tx.from();
            if base.contains_key(&from) {
                continue;
            }
            base.insert(from, anvil_m.nonce_at(from, head).await?);
        }
        let seq_end = base.iter().map(|(a, n)| (*a, *n as i64 - 1)).collect();
        let mut auth_bumps = std::collections::BTreeMap::new();
        for tx in txs {
            if let Some(auths) = tx.authorization_list() {
                for auth in auths {
                    if let Ok(authority) = auth.recover_authority() {
                        *auth_bumps.entry(authority).or_insert(0) += 1;
                    }
                }
            }
        }
        Ok(Self {
            base,
            seq_end,
            manipulated: Default::default(),
            auth_bumps,
        })
    }

    async fn before_send(
        &mut self,
        anvil_m: &Rpc,
        tx: &alloy_rpc_types_eth::Transaction,
    ) -> Result<()> {
        let from = tx.from();
        let nonce = tx.nonce();
        let nonce_i = nonce as i64;
        let end = *self.seq_end.get(&from).unwrap_or(&(nonce_i - 1));
        if nonce_i == end + 1 {
            // Extends the contiguous sequence: the pool keeps it pending.
            self.seq_end.insert(from, nonce_i);
            return Ok(());
        }
        if nonce_i > end + 1 {
            // Gap beyond the sequence: make it current for the pool.
            anvil_m.anvil_set_nonce(from, nonce).await?;
            self.manipulated.insert(from);
            self.seq_end.insert(from, nonce_i);
            match self.auth_bumps.get(&from) {
                Some(bumps) => {
                    // Expected: the sender is an EIP-7702 authority in this
                    // block; its nonce is bumped mid-execution by the
                    // authorization, so the tx is only valid after that point.
                    tracing::info!(
                        sender = %from,
                        nonce,
                        previous_end = end,
                        authorizations = bumps,
                        "nonce ahead of state: EIP-7702 authorization bump within the block"
                    );
                }
                None => {
                    tracing::warn!(
                        sender = %from,
                        nonce,
                        previous_end = end,
                        "nonce gap in block: adjusted state nonce for pool acceptance"
                    );
                }
            }
            return Ok(());
        }
        tracing::warn!(sender = %from, nonce, end, "tx nonce behind pool sequence");
        Ok(())
    }

    async fn restore(&self, anvil_m: &Rpc) -> Result<()> {
        for sender in &self.manipulated {
            if let Some(nonce) = self.base.get(sender) {
                anvil_m.anvil_set_nonce(*sender, *nonce).await?;
            }
        }
        Ok(())
    }
}

/// Drop every tx currently in the pool (M is engine-owned; this keeps retries clean).
async fn clear_pool(rpc: &Rpc) -> Result<()> {
    for h in pool_hashes(rpc).await? {
        if let Err(e) = rpc.anvil_drop_transaction(h).await {
            tracing::debug!(tx = %h, error = %e, "pool drop failed");
        }
    }
    Ok(())
}

/// One verification check line inside the M drift entry.
#[derive(serde::Serialize)]
struct DriftCheck<'a> {
    check: &'a str,
    ok: bool,
    detail: &'a str,
}

/// One M replay anomaly line in drift.jsonl: divergences only — clean blocks
/// live in the metrics (the log used to grow ~20 MB of clean-block telemetry
/// that nobody reads).
#[derive(serde::Serialize)]
struct MDriftEntry<'a> {
    chain: &'static str,
    block: u64,
    mainnet_hash: String,
    txs: usize,
    replayed: usize,
    duration_ms: u64,
    ok: Option<bool>,
    divergence_class: Option<&'static str>,
    divergences: Vec<String>,
    checks: Vec<DriftCheck<'a>>,
}

fn divergence_class_name(class: crate::verify::DivergenceClass) -> &'static str {
    match class {
        crate::verify::DivergenceClass::None => "none",
        crate::verify::DivergenceClass::ValueLevel => "value_level",
        crate::verify::DivergenceClass::Fatal => "fatal",
    }
}

fn log_and_drift(
    ctx: &Ctx,
    n: u64,
    outcome: &ReplayOutcome,
    block: &alloy_rpc_types_eth::Block,
) -> Result<()> {
    // The journal is a *divergence* log: clean blocks are counted in the
    // metrics, not written to disk.
    let clean = outcome
        .verification
        .as_ref()
        .map(|v| v.divergences.is_empty())
        .unwrap_or(false);
    if !clean {
        ctx.drift.append(&drift_entry(n, outcome, block))?;
    }
    log_outcome(ctx, n, outcome);
    if outcome.duration_ms > 10_000 {
        tracing::warn!(block = n, ms = outcome.duration_ms, "slow replay (>10 s)");
    }
    Ok(())
}

/// The drift entry for one replayed block: the verification's verdict and its
/// per-check detail, flattened.
fn drift_entry<'a>(
    n: u64,
    outcome: &'a ReplayOutcome,
    block: &alloy_rpc_types_eth::Block,
) -> MDriftEntry<'a> {
    let v = outcome.verification.as_ref();
    MDriftEntry {
        chain: "m",
        block: n,
        mainnet_hash: format!("0x{:x}", block.header.hash),
        txs: outcome.txs,
        replayed: outcome.replayed,
        duration_ms: outcome.duration_ms,
        ok: v.map(|v| v.ok),
        divergence_class: v.map(|v| divergence_class_name(v.class())),
        divergences: v.map(|v| v.divergences.clone()).unwrap_or_default(),
        checks: v
            .map(|v| {
                v.checks
                    .iter()
                    .map(|(check, ok, detail)| DriftCheck {
                        check,
                        ok: *ok,
                        detail,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Log one replay's outcome and count its divergence: structural divergences
/// in their own counter, value-level ones beside them, and the halt/freeze
/// policy applied through `on_divergence` unless the policy is `Continue`.
fn log_outcome(ctx: &Ctx, n: u64, outcome: &ReplayOutcome) {
    match outcome.verification.as_ref() {
        Some(v) if v.ok => {
            tracing::info!(
                block = n,
                txs = outcome.txs,
                ms = outcome.duration_ms,
                "replayed + verified"
            );
        }
        Some(v) => {
            let structural = matches!(v.class(), crate::verify::DivergenceClass::Fatal);
            if structural {
                ctx.eth_metrics.inc(&ctx.eth_metrics.divergences);
            } else {
                ctx.eth_metrics.add(&ctx.eth_metrics.value_divergences, 1);
            }
            if ctx.engine.loops.on_divergence == mirror_config::OnDivergence::Continue {
                tracing::warn!(
                    block = n,
                    structural,
                    divergences = ?v.divergences,
                    "divergence recorded (state re-converges from the parent BAL before the next block)"
                );
            } else {
                tracing::error!(
                    block = n,
                    structural,
                    divergences = ?v.divergences,
                    "replay verification failed"
                );
                ctx.on_divergence(&format!("block {n}"));
            }
        }
        None => {}
    }
}