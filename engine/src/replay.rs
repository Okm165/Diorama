//! Replay loop: re-execute every parent block on M, then verify it.
//!
//! Sequence per block (plan §4.5): set block env -> apply withdrawals ->
//! automine off -> send raws in order (chunked) -> mine 1 -> verify.
use crate::bal;
use crate::blob::{available_forms, encode_blob_tx, select_sidecars, BlobForm, SidecarItem};
use crate::ctx::{block_txs, cursor_from, Ctx};
use crate::rpc::{Rpc, RpcError};
use crate::verify::{verify_block, Verification};
use alloy_consensus::{EthereumTxEnvelope, TxEip4844, TxEip4844Variant};
use alloy_eips::eip2718::Encodable2718;
use alloy_consensus::Transaction as _;
use alloy_network_primitives::TransactionResponse;
use alloy_primitives::{Bytes, Signature, B256, U256};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use serde_json::Value;
use std::time::Instant;

pub struct ReplayOutcome {
    pub block: u64,
    pub txs: usize,
    pub replayed: usize,
    pub skipped_blobs: usize,
    pub partial: bool,
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
        bail!("M head {m_head} does not precede block {n}");
    }
    // Leftover pool txs from a timed-out attempt would make the resend dirty.
    if !pool_hashes(&ctx.anvil_m).await.is_empty() {
        tracing::warn!(block = n, "clearing leftover pool txs from a previous attempt");
        clear_pool(ctx, &ctx.anvil_m).await?;
    }

    // 0. convergence repair: force M's state to mainnet's post-state of n-1
    //    from the parent BAL. M cannot reproduce blockhash-dependent values
    //    (A17: e.g. Seaport 1.6 counters), and one divergence would cascade
    //    forever (invalid txs, different statuses). The BAL is authoritative
    //    for every changed entry, so applying it makes M exact again; the
    //    per-block verification below still measures what execution got right.
    if ctx.cfg.loops.repair_m {
        match ctx.reth.block_access_list(n - 1).await? {
            Some(bal) => {
                let writes = bal::flatten_bal(&bal);
                let applied = crate::diff::apply_writes(ctx, &ctx.anvil_m, &writes).await?;
                // DIAGNOSTIC (temporary): confirm a storage write landed.
                if let Some(bal::Write::Storage { address, slot, value }) =
                    writes.iter().find(|w| matches!(w, bal::Write::Storage { .. }))
                {
                    let head = ctx.anvil_m.block_number().await?;
                    match ctx.anvil_m.storage_at(*address, *slot, head).await {
                        Ok(got) => tracing::info!(
                            address = %address,
                            slot = %slot,
                            want = %value,
                            got = %got,
                            "repair readback"
                        ),
                        Err(e) => tracing::warn!(error = %e, "repair readback failed"),
                    }
                }
                ctx.metrics.add(&ctx.metrics.m_repair_writes, applied as u64);
                tracing::debug!(
                    block = n,
                    writes = writes.len(),
                    applied,
                    "M state converged from parent BAL"
                );
                let (ok, failed) = crate::verify::convergence_check(ctx, n - 1).await;
                ctx.metrics.add(&ctx.metrics.m_convergence_ok, ok as u64);
                ctx.metrics.add(&ctx.metrics.m_convergence_failed, failed as u64);
                ctx.metrics.set(&ctx.metrics.m_convergence_last_failed, failed as u64);
            }
            None => tracing::debug!(block = n, "no parent BAL; M repair skipped"),
        }
    }

    // 1. block env (all five context fields aligned to the parent).
    let h = &block.header;
    let mut env: Vec<(String, Value)> = vec![
        ("anvil_setNextBlockTimestamp".into(), json!([h.timestamp])),
        ("anvil_setCoinbase".into(), json!([h.beneficiary])),
        ("anvil_setNextBlockPrevRandao".into(), json!([h.mix_hash])),
        ("anvil_setBlockGasLimit".into(), json!([format!("0x{:x}", h.gas_limit)])),
    ];
    if let Some(bf) = h.base_fee_per_gas {
        env.push((
            "anvil_setNextBlockBaseFeePerGas".into(),
            json!([format!("0x{bf:x}")]),
        ));
    }
    for r in ctx.anvil_m.batch_raw(&env).await? {
        if let Err(e) = r {
            bail!("block env failed: {e}");
        }
    }

    // 2. withdrawals are not txs: credit recipients directly. The credit is
    //    idempotent (pre-balance + aggregated amount, via setBalance) so a
    //    retried block never double-credits.
    if let Some(withdrawals) = &block.withdrawals {
        let mut per_address: std::collections::BTreeMap<alloy_primitives::Address, U256> =
            std::collections::BTreeMap::new();
        for w in withdrawals {
            let wei = U256::from(w.amount) * U256::from(1_000_000_000u64);
            *per_address.entry(w.address).or_default() += wei;
        }
        let mut calls: Vec<(String, Value)> = Vec::with_capacity(per_address.len());
        for (address, amount) in &per_address {
            let pre = ctx.anvil_m.balance_at(*address, n - 1).await?;
            calls.push((
                "anvil_setBalance".to_string(),
                json!([address, bal::quant_hex_u256(pre + *amount)]),
            ));
        }
        for chunk in calls.chunks(ctx.cfg.loops.chunk_size) {
            for r in ctx.anvil_m.batch_raw(chunk).await? {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "withdrawal credit failed");
                }
            }
        }
    }

    // 3. raw txs (blob txs get their sidecar from the beacon API). A block
    //    that cannot be replayed in full is never mined: M must stay strict.
    let (raws, skipped_blobs) = build_raws(ctx, n, &block).await?;
    if skipped_blobs > 0 {
        bail!(
            "block {n}: {skipped_blobs} blob tx(s) have no sidecar (beacon sources unavailable); not mining a partial block"
        );
    }

    // 4. send + retry-once + mine.
    send_with_retry(ctx, &raws, block_txs(&block)).await?;
    ctx.anvil_m.anvil_mine(1).await.context("mining M block")?;

    finish_replay(ctx, n, block, started, skipped_blobs).await
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
    let verification = if ctx.cfg.verifier.enabled {
        let v = verify_block(ctx, n, &block, &m_block).await?;
        ctx.metrics
            .set(&ctx.metrics.probes_ok, v.probes_ok as u64);
        ctx.metrics
            .set(&ctx.metrics.probes_failed, v.probes_failed as u64);
        if v.receipts_unverified {
            ctx.metrics.inc(&ctx.metrics.receipts_unverified);
        }
        Some(v)
    } else {
        None
    };

    // 6. cursor + metrics + drift.
    let first_block = ctx
        .cursor_m()
        .map(|c| c.first_block)
        .unwrap_or(n);
    ctx.set_cursor_m(cursor_from(&m_block, first_block));
    ctx.set_reth_last_hash(format!("0x{:x}", block.header.hash));
    ctx.set_reth_last_block(n);
    ctx.save_state()?;
    ctx.metrics.inc(&ctx.metrics.blocks_replayed);
    let duration_ms = started.elapsed().as_millis() as u64;
    ctx.metrics.set(&ctx.metrics.last_replay_ms, duration_ms);
    ctx.metrics.set(&ctx.metrics.m_head, n);
    ctx.metrics
        .set(&ctx.metrics.pin_age_blocks, n.saturating_sub(first_block) + 1);

    let partial = skipped_blobs > 0;
    let outcome = ReplayOutcome {
        block: n,
        txs: crate::ctx::block_txs(&block).len(),
        replayed: crate::ctx::block_txs(&block).len() - skipped_blobs,
        skipped_blobs,
        partial,
        duration_ms,
        verification,
    };
    log_and_drift(ctx, n, &outcome, &block);
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

    // Fetch sidecars once per block. Beacon root of payload N = parent beacon
    // block root of EL block N+1 (verified against mainnet).
    let mut sidecars: Vec<SidecarItem> = Vec::new();
    if has_blobs && ctx.cfg.blobs.enabled && ctx.beacon.enabled() {
        if let Some(next) = ctx.reth.block_by_number_opt(n + 1, false).await? {
            if let Some(root) = next.header.parent_beacon_block_root {
                match ctx.beacon.blob_sidecars(root).await {
                    Ok(items) => sidecars = items,
                    Err(e) => tracing::warn!(block = n, error = %e, "blob sidecar fetch failed"),
                }
            } else {
                tracing::warn!(block = n, "no parent beacon block root on N+1");
            }
        }
    }

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
                match select_sidecars(&sidecars, &t.blob_versioned_hashes) {
                    Ok(items) => {
                        let forms = available_forms(&items, t.blob_versioned_hashes.len());
                        let preferred = ctx.blob_form().unwrap_or(BlobForm::Legacy);
                        let mut order =
                            vec![preferred, BlobForm::Legacy, BlobForm::Eip7594, BlobForm::Bare];
                        order.dedup();
                        let mut encoded: Option<(BlobForm, Bytes)> = None;
                        for form in order {
                            if !forms.contains(&form) {
                                continue;
                            }
                            if let Ok(b) = encode_blob_tx(t, signature, hash, &items, form) {
                                encoded = Some((form, b));
                                break;
                            }
                        }
                        match encoded {
                            Some((form, bytes)) => {
                                ctx.set_blob_form(form);
                                out.push(RawTx {
                                    hash: tx.tx_hash(),
                                    bytes,
                                    blob: Some((
                                        t.clone(),
                                        signature,
                                        hash,
                                        items.into_iter().cloned().collect(),
                                    )),
                                });
                            }
                            None => {
                                skipped += 1;
                                tracing::warn!(block = n, tx = %tx.tx_hash(), "no encodable blob form");
                            }
                        }
                    }
                    Err(e) => {
                        skipped += 1;
                        tracing::warn!(block = n, tx = %tx.tx_hash(), error = %e, "blob sidecar missing");
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

/// Send all raws; on rejection retry once (re-encoding blob txs with the next
/// wrapper form). Aborts the block - without mining - if a retry is needed twice.
async fn send_with_retry(
    ctx: &Ctx,
    raws: &[RawTx],
    txs: &[alloy_rpc_types_eth::Transaction],
) -> Result<()> {
    let mut current: Vec<Bytes> = raws.iter().map(|r| r.bytes.clone()).collect();

    for attempt in 0..2 {
        let mut rejected: Vec<usize> = Vec::new();
        // Raws are sent one request at a time: batch items are processed
        // concurrently by the JSON-RPC server, which scrambles pool insertion
        // order (measured: type-4 txs landed 6 positions late). Tx order is
        // load-bearing for replay fidelity, so we pay the round-trips.
        let mut nonces = NonceTracker::prepare(ctx, txs).await?;
        for (i, raw) in current.iter().enumerate() {
            if let Some(tx) = txs.get(i) {
                if let Err(e) = nonces.before_send(ctx, tx).await {
                    tracing::warn!(tx = %raws[i].hash, error = %e, "nonce pre-adjust failed");
                }
            }
            match ctx.anvil_m.send_raw_batch(std::slice::from_ref(raw)).await {
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
                Err(e) => {
                    // Transport failure: retry the whole block via the caller.
                    bail!("raw tx send failed at index {i}: {e}");
                }
            }
        }
        let pool = ctx.anvil_m.txpool_status().await;
        tracing::info!(
            sent = current.len(),
            rejected = rejected.len(),
            adjusted = nonces.manipulated.len(),
            pool = ?pool,
            "raw txs sent (pre-mine)"
        );
        nonces.restore(ctx).await?;
        if rejected.is_empty() {
            return Ok(());
        }

        // Retry once: only blob txs can be re-encoded (form mismatch); anything
        // else is a hard abort.
        let all_blob = rejected.iter().all(|i| raws[*i].blob.is_some());
        if attempt == 0 && all_blob {
            let next_form = next_blob_form(ctx);
            tracing::warn!(
                block_txs = raws.len(),
                rejected = rejected.len(),
                form = next_form.as_str(),
                "retrying block with next blob wrapper form"
            );
            clear_pool(ctx, &ctx.anvil_m).await?;
            for i in &rejected {
                let (t, sig, hash, items) = raws[*i].blob.as_ref().expect("blob parts");
                let item_refs: Vec<&SidecarItem> = items.iter().collect();
                let bytes = encode_blob_tx(t, *sig, *hash, &item_refs, next_form)
                    .with_context(|| format!("re-encode blob tx {}", raws[*i].hash))?;
                current[*i] = bytes;
            }
            ctx.set_blob_form(next_form);
            continue;
        }

        clear_pool(ctx, &ctx.anvil_m).await?;
        let e = rejected
            .first()
            .map(|i| format!("tx {}", raws[*i].hash))
            .unwrap_or_default();
        bail!("block replay aborted: {} raw tx(s) rejected ({e})", rejected.len());
    }
    bail!("block replay aborted after retry")
}

fn next_blob_form(ctx: &Ctx) -> BlobForm {
    match ctx.blob_form() {
        Some(BlobForm::Legacy) => BlobForm::Eip7594,
        Some(BlobForm::Eip7594) => BlobForm::Bare,
        Some(BlobForm::Bare) => BlobForm::Legacy,
        None => BlobForm::Legacy,
    }
}

/// Every tx currently in the pool.
async fn pool_hashes(rpc: &Rpc) -> Vec<B256> {
    let content: Value = rpc
        .call_raw("txpool_content", json!([]))
        .await
        .unwrap_or(Value::Null);
    let mut hashes: Vec<B256> = Vec::new();
    for side in ["pending", "queued"] {
        if let Some(accounts) = content.get(side).and_then(Value::as_object) {
            for txs in accounts.values().filter_map(Value::as_object) {
                for tx in txs.values() {
                    if let Some(h) = tx.get("hash").and_then(Value::as_str) {
                        if let Ok(h) = h.parse::<B256>() {
                            hashes.push(h);
                        }
                    }
                }
            }
        }
    }
    hashes
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
}

impl NonceTracker {
    async fn prepare(ctx: &Ctx, txs: &[alloy_rpc_types_eth::Transaction]) -> Result<Self> {
        let head = ctx.anvil_m.block_number().await?;
        let mut base = std::collections::BTreeMap::new();
        for tx in txs {
            let from = tx.from();
            if base.contains_key(&from) {
                continue;
            }
            base.insert(from, ctx.anvil_m.nonce_at(from, head).await?);
        }
        let seq_end = base.iter().map(|(a, n)| (*a, *n as i64 - 1)).collect();
        Ok(Self {
            base,
            seq_end,
            manipulated: Default::default(),
        })
    }

    async fn before_send(
        &mut self,
        ctx: &Ctx,
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
            ctx.anvil_m.anvil_set_nonce(from, nonce).await?;
            self.manipulated.insert(from);
            self.seq_end.insert(from, nonce_i);
            tracing::warn!(
                sender = %from,
                nonce,
                previous_end = end,
                "nonce gap in block: adjusted state nonce for pool acceptance"
            );
            return Ok(());
        }
        tracing::warn!(sender = %from, nonce, end, "tx nonce behind pool sequence");
        Ok(())
    }

    async fn restore(&self, ctx: &Ctx) -> Result<()> {
        for sender in &self.manipulated {
            if let Some(nonce) = self.base.get(sender) {
                ctx.anvil_m.anvil_set_nonce(*sender, *nonce).await?;
            }
        }
        Ok(())
    }
}

/// Drop every tx currently in the pool (M is engine-owned; this keeps retries clean).
async fn clear_pool(_ctx: &Ctx, rpc: &Rpc) -> Result<()> {
    for h in pool_hashes(rpc).await {
        let _ = rpc.anvil_drop_transaction(h).await;
    }
    Ok(())
}

fn log_and_drift(ctx: &Ctx, n: u64, outcome: &ReplayOutcome, block: &alloy_rpc_types_eth::Block) {
    let v = outcome.verification.as_ref();
    let entry = json!({
        "chain": "m",
        "block": n,
        "mainnet_hash": format!("0x{:x}", block.header.hash),
        "txs": outcome.txs,
        "replayed": outcome.replayed,
        "skipped_blobs": outcome.skipped_blobs,
        "partial": outcome.partial,
        "duration_ms": outcome.duration_ms,
        "ok": v.map(|v| v.ok),
        "divergence_class": v.map(|v| match v.class() {
            crate::verify::DivergenceClass::None => "none",
            crate::verify::DivergenceClass::ValueLevel => "value_level",
            crate::verify::DivergenceClass::Fatal => "fatal",
        }),
        "divergences": v.map(|v| v.divergences.clone()).unwrap_or_default(),
        "checks": v.map(|v| v.checks.iter().map(|(k, ok, d)| json!({"check": k, "ok": ok, "detail": d})).collect::<Vec<_>>()).unwrap_or_default(),
    });
    ctx.drift.append(&entry);

    match v {
        Some(v) if v.ok => {
            tracing::info!(
                block = n,
                txs = outcome.txs,
                ms = outcome.duration_ms,
                "replayed + verified"
            );
        }
        Some(v) if outcome.partial => {
            // Blob txs were skipped because no sidecar source had them (e.g.
            // Lighthouse has not backfilled that far yet). Expected to fail
            // tx-count/order/blob-gas/receipts checks; recorded, not escalated.
            ctx.metrics.inc(&ctx.metrics.blob_failures);
            tracing::warn!(
                block = n,
                skipped_blobs = outcome.skipped_blobs,
                divergences = ?v.divergences,
                "partial replay (blob txs skipped) - not escalating to freeze"
            );
        }
        Some(v) => {
            let structural = matches!(v.class(), crate::verify::DivergenceClass::Fatal);
            if structural {
                ctx.metrics.inc(&ctx.metrics.divergences);
            } else {
                ctx.metrics.add(&ctx.metrics.value_divergences, 1);
            }
            match ctx.cfg.loops.on_divergence {
                crate::config::OnDivergence::Continue => tracing::warn!(
                    block = n,
                    structural,
                    divergences = ?v.divergences,
                    "divergence recorded (state re-converges from the parent BAL before the next block)"
                ),
                _ => {
                    tracing::error!(
                        block = n,
                        structural,
                        divergences = ?v.divergences,
                        "replay verification failed"
                    );
                    ctx.on_divergence(&format!("block {n}"));
                }
            }
        }
        None => {}
    }
    if outcome.duration_ms > 10_000 {
        tracing::warn!(block = n, ms = outcome.duration_ms, "slow replay (>10 s)");
    }
}

/// Fetch a reth block for the loop's continuity check (used by the driver).
pub async fn reth_block_hash(rpc: &Rpc, n: u64) -> Result<Option<String>> {
    Ok(rpc
        .block_by_number_opt(n, false)
        .await?
        .map(|b| format!("0x{:x}", b.header.hash)))
}

#[allow(dead_code)]
fn _unused(e: RpcError) -> anyhow::Error {
    anyhow!(e.to_string())
}
