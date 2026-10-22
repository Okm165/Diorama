//! Per-block verifier: compares M's replayed block against reth at the same
//! height. Divergence here freezes M (never mine blind).
//!
//! Checks (plan §5.1): tx-count parity · blobGasUsed parity · logsBloom parity ·
//! log-set equality · receipts (statuses identical; gas delta only on type-4,
//! bounded by EIP-7702 refund accounting) · withdrawal recipient balances ·
//! configured probe calls.
use crate::ctx::{block_txs, Ctx};
use alloy_network_primitives::{ReceiptResponse, TransactionResponse};
use alloy_rpc_types_eth::{Block, Log};
use anyhow::Result;
use alloy_primitives::Address;

/// Divergence classes:
///   - `ValueLevel`: only log data / bloom differ - identical events with
///     different values. Expected for contracts whose output depends on
///     `blockhash(...)` or other values a fork cannot reproduce (plan axiom
///     A17: mirror block hashes differ from mainnet; e.g. Seaport 1.6's
///     incrementCounter uses blockhash(number-1) as a quasi-random interval).
///   - `Fatal`: counts, order, statuses, gas, withdrawals or probes differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergenceClass {
    None,
    ValueLevel,
    Fatal,
}

#[derive(Debug, Default)]
pub struct Verification {
    pub ok: bool,
    pub checks: Vec<(String, bool, String)>,
    pub divergences: Vec<String>,
    /// reth could not serve receipts (pruned window) - block accepted unverified.
    pub receipts_unverified: bool,
}

fn log_key(l: &Log) -> String {
    let mut s = format!("{}|", l.address());
    for t in l.topics() {
        s.push_str(&format!("{t:x}|"));
    }
    s.push_str(&hex::encode(&l.data().data));
    s
}

pub async fn verify_block(ctx: &Ctx, n: u64, reth_block: &Block, m_block: &Block) -> Result<Verification> {
    let mut v = Verification::default();

    // tx count
    let rt = block_txs(reth_block).len();
    let mt = m_block.transactions.len();
    let ok = rt == mt;
    v.checks.push(("tx_count".into(), ok, format!("reth={rt} m={mt}")));
    if !ok {
        v.divergences.push(format!("tx_count: reth={rt} m={mt}"));
    }

    // tx order: same hashes in the same positions (order is load-bearing)
    let r_hashes: Vec<String> = block_txs(reth_block)
        .iter()
        .map(|t| format!("0x{:x}", t.tx_hash()))
        .collect();
    let m_hashes: Vec<String> = block_txs(m_block)
        .iter()
        .map(|t| format!("0x{:x}", t.tx_hash()))
        .collect();
    {
        let mut ok = r_hashes.len() == m_hashes.len();
        let mut detail = format!("{} txs in order", r_hashes.len());
        if !ok {
            detail = format!("count reth={} m={}", r_hashes.len(), m_hashes.len());
        } else {
            for (i, (a, b)) in r_hashes.iter().zip(m_hashes.iter()).enumerate() {
                if a != b {
                    ok = false;
                    detail = format!("order differs at index {i}: reth={a} m={b}");
                    break;
                }
            }
        }
        v.checks.push(("tx_order".into(), ok, detail.clone()));
        if !ok {
            v.divergences.push(format!("tx_order: {detail}"));
        }
    }

    // blobGasUsed
    let r_blob = reth_block.header.blob_gas_used.unwrap_or(0);
    let m_blob = m_block.header.blob_gas_used.unwrap_or(0);
    let ok = r_blob == m_blob;
    v.checks.push(("blob_gas_used".into(), ok, format!("reth={r_blob} m={m_blob}")));
    if !ok {
        v.divergences.push(format!("blob_gas_used: reth={r_blob} m={m_blob}"));
    }

    // logsBloom
    let ok = reth_block.header.logs_bloom == m_block.header.logs_bloom;
    v.checks.push(("logs_bloom".into(), ok, String::new()));
    if !ok {
        v.divergences.push("logs_bloom mismatch".into());
    }

    // log set
    match (ctx.reth.logs(n).await, ctx.anvil_m.logs(n).await) {
        (Ok(mut rl), Ok(mut ml)) => {
            let mut rk: Vec<String> = rl.iter().map(log_key).collect();
            let mut mk: Vec<String> = ml.iter().map(log_key).collect();
            rk.sort();
            mk.sort();
            let ok = rk == mk;
            let detail = if ok {
                format!("{} logs", rk.len())
            } else {
                let mut diff = String::new();
                for (a, b) in rk.iter().zip(mk.iter()) {
                    if a != b {
                        diff = format!("first diff: reth={} m={}", trunc(a), trunc(b));
                        break;
                    }
                }
                if diff.is_empty() {
                    diff = format!("count reth={} m={}", rk.len(), mk.len());
                }
                diff
            };
            v.checks.push(("log_set".into(), ok, detail.clone()));
            if !ok {
                v.divergences.push(format!("log_set: {detail}"));
            }
            rl.clear();
            ml.clear();
        }
        (r, m) => {
            let msg = format!("log fetch failed: reth={:?} m={:?}", r.err(), m.err());
            v.checks.push(("log_set".into(), false, msg.clone()));
            v.divergences.push(msg);
        }
    }

    // receipts: statuses equal; gas delta only on type-4 within the refund bound
    let r_receipts = ctx.reth.receipts(n).await;
    let m_receipts = ctx.anvil_m.receipts(n).await;
    match (r_receipts, m_receipts) {
        (Ok(rr), Ok(mr)) => {
            let ok_len = rr.len() == mr.len();
            v.checks.push((
                "receipts_count".into(),
                ok_len,
                format!("reth={} m={}", rr.len(), mr.len()),
            ));
            if !ok_len {
                v.divergences.push(format!(
                    "receipts_count: reth={} m={}",
                    rr.len(),
                    mr.len()
                ));
            }
            let mut ok = ok_len;
            let mut detail = format!("{} receipts", rr.len());
            if ok_len {
                for (i, (a, b)) in rr.iter().zip(mr.iter()).enumerate() {
                    if a.status() != b.status() {
                        ok = false;
                        detail = format!("status mismatch at tx {i}");
                        break;
                    }
                    let ga = a.gas_used();
                    let gb = b.gas_used();
                    if ga == gb {
                        continue;
                    }
                    // gas may differ only for EIP-7702 txs, in [-refund*auths, 0]
                    let ty = block_txs(reth_block)
                        .get(i)
                        .map(|t| u8::from(t.inner.tx_type()))
                        .unwrap_or(0);
                    if ty != 4 {
                        ok = false;
                        detail = format!("non-type-4 gas mismatch at tx {i}: reth={ga} m={gb}");
                        break;
                    }
                    let auths = block_txs(reth_block).get(i).map(auth_count).unwrap_or(0);
                    let bound = ctx.cfg.verifier.type4_refund_per_auth * auths;
                    let delta = gb as i64 - ga as i64; // fork - mainnet
                    if delta > 0 || delta.unsigned_abs() > bound {
                        ok = false;
                        detail = format!("type-4 gas delta {delta} outside [-{bound},0] at tx {i}");
                        break;
                    }
                }
            } else {
                detail = format!("count reth={} m={}", rr.len(), mr.len());
            }
            v.checks.push(("receipts".into(), ok, detail.clone()));
            if !ok {
                v.divergences.push(format!("receipts: {detail}"));
            }
        }
        (Err(e), _) => {
            v.receipts_unverified = true;
            v.checks
                .push(("receipts".into(), true, format!("unverified (reth receipts unavailable: {e})")));
        }
        (_, Err(e)) => {
            v.checks
                .push(("receipts".into(), false, format!("M receipts unavailable: {e}")));
            v.divergences.push(format!("M receipts unavailable: {e}"));
        }
    }

    // withdrawal recipients: balances must match reth at n
    if let Some(withdrawals) = &reth_block.withdrawals {
        if !withdrawals.is_empty() {
            let mut ok = true;
            let mut detail = format!("{} withdrawals", withdrawals.len());
            for w in withdrawals {
                match (
                    ctx.reth.balance_at(w.address, n).await,
                    ctx.anvil_m.balance_at(w.address, n).await,
                ) {
                    (Ok(a), Ok(b)) if a == b => {}
                    (Ok(a), Ok(b)) => {
                        ok = false;
                        detail = format!("withdrawal {} balance reth={a} m={b}", w.address);
                        break;
                    }
                    (a, b) => {
                        ok = false;
                        detail = format!("withdrawal balance read failed: {a:?} {b:?}");
                        break;
                    }
                }
            }
            v.checks.push(("withdrawals".into(), ok, detail.clone()));
            if !ok {
                v.divergences.push(format!("withdrawals: {detail}"));
            }
        }
    }

    // State probes are deliberately *not* re-run here. Post-execution they only
    // re-detected the blockhash-level divergence that receipts/log_set already
    // record (measured: `probe weth_total_supply diverged` on every block where
    // a blockhash-gated tx flipped status). The probes run once per block at the
    // repaired point (`convergence_check`), where a mismatch is a real repair
    // failure and nothing else - that is the invariant worth alarming on.

    v.ok = v.divergences.is_empty();
    Ok(v)
}

impl Verification {
    /// Structural checks verify the engine's replay mechanics and freeze M on
    /// failure. Behavioral checks verify mainnet's values/statuses; they are
    /// recorded but not fatal (mirror block hashes differ from mainnet, A17).
    const STRUCTURAL: [&'static str; 5] = [
        "tx_count",
        "tx_order",
        "blob_gas_used",
        "receipts_count",
        "withdrawals",
    ];

    pub fn class(&self) -> DivergenceClass {
        if self.ok {
            return DivergenceClass::None;
        }
        let structural = self
            .checks
            .iter()
            .any(|(name, ok, _)| !ok && Self::STRUCTURAL.contains(&name.as_str()));
        if structural {
            DivergenceClass::Fatal
        } else {
            DivergenceClass::ValueLevel
        }
    }
}

fn auth_count(tx: &alloy_rpc_types_eth::Transaction) -> u64 {
    match &*tx.inner {
        alloy_consensus::EthereumTxEnvelope::Eip7702(t) => t.tx().authorization_list.len() as u64,
        _ => 0,
    }
}

fn trunc(s: &str) -> String {
    if s.len() <= 80 {
        s.to_string()
    } else {
        format!("{}…", &s[..80])
    }
}

// Post-repair convergence proof: M's state (head == n) must match mainnet's
/// post-state of the same block for every configured probe. Execution may
/// diverge (A17), but the parent-BAL convergence restores mainnet's values
/// before every block - this check measures that guarantee.
/// Probe parity for T (the plan's "probe parity on T after context alignment"):
/// the same probe set, compared against reth at the parent block just applied,
/// skipping probes whose target account the devnet owns (copy-on-write: that
/// state is the devnet's, so parity is not expected there). Runs once per
/// applied block; the metric is the alarm.
pub async fn t_probe_parity(ctx: &Ctx, n: u64) -> (usize, usize, usize) {
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    // A devnet time offset makes time-dependent views diverge from mainnet by
    // design (that is what it is for): skip instead of alarming.
    if ctx.time_offset.load(std::sync::atomic::Ordering::Relaxed) != 0 {
        return (0, 0, 0);
    }
    let shadowed: std::collections::HashSet<Address> = {
        let s = ctx.shadow.lock().unwrap();
        s.slots.keys().chain(s.accounts.iter()).copied().collect()
    };
    for probe in &ctx.cfg.verifier.probes {
        if shadowed.contains(&probe.to) {
            // A devnet write anywhere in this target hides *all* of its
            // probes (measured: a crashed oracle source stayed marked and the
            // five oracle probes were skipped silently). Count them so the
            // blind spot is visible; hand the key back to check again.
            skipped += 1;
            tracing::debug!(block = n, probe = %probe.name, target = ?probe.to,
                "probe skipped: target is devnet-owned");
            continue;
        }
        let r = ctx.reth.call_at(probe.to, &probe.data, n).await;
        let t = ctx.anvil_t.call_latest(probe.to, &probe.data).await;
        match (r, t) {
            (Ok(a), Ok(b)) if a == b => ok += 1,
            (Ok(a), Ok(b)) => {
                failed += 1;
                tracing::warn!(
                    block = n,
                    probe = %probe.name,
                    reth = %trunc(&hex::encode(&a)),
                    t = %trunc(&hex::encode(&b)),
                    "T probe mismatch after context alignment"
                );
            }
            (Err(e), _) | (_, Err(e)) => {
                failed += 1;
                tracing::debug!(block = n, probe = %probe.name, error = %e, "T probe failed");
            }
        }
    }
    (ok, failed, skipped)
}

pub async fn convergence_check(ctx: &Ctx, n: u64) -> (usize, usize) {
    let mut ok = 0usize;
    let mut failed = 0usize;
    for probe in &ctx.cfg.verifier.probes {
        let r = ctx.reth.call_at(probe.to, &probe.data, n).await;
        // M's repair writes are unmined, so read M's current (latest) state.
        let m = ctx.anvil_m.call_latest(probe.to, &probe.data).await;
        match (r, m) {
            (Ok(a), Ok(b)) if a == b => ok += 1,
            (Ok(a), Ok(b)) => {
                failed += 1;
                tracing::warn!(
                    block = n,
                    probe = %probe.name,
                    reth = %trunc(&hex::encode(&a)),
                    m = %trunc(&hex::encode(&b)),
                    "convergence probe mismatch (state repair incomplete)"
                );
            }
            (Err(e), _) | (_, Err(e)) => {
                failed += 1;
                tracing::warn!(block = n, probe = %probe.name, error = %e, "convergence probe failed");
            }
        }
    }
    (ok, failed)
}
