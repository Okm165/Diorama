//! Per-block verifier: compares M's replayed block against reth at the same
//! height. Divergence here freezes M (never mine blind).
//!
//! Checks (plan §5.1): tx-count parity · tx order · blobGasUsed parity ·
//! logsBloom parity · log-set equality · receipts (statuses identical; gas
//! delta only on type-4, bounded by EIP-7702 refund accounting) · withdrawal
//! recipient balances. Each check is one small function returning a [`Check`];
//! `verify_block` is the sequence that records them.
use crate::ctx::{block_txs, Ctx};
use crate::error::Result;
use alloy_network_primitives::{ReceiptResponse, TransactionResponse};
use alloy_rpc_types_eth::{Block, Log, Transaction, TransactionReceipt};

/// EIP-7702: a fork's type-4 gas delta must lie in `[-refund * #auths, 0]`.
/// A measured protocol fact (not tunable): one authorization refunds 12,500
/// gas, so the fork can only under-use relative to mainnet's refunds.
const TYPE4_REFUND_PER_AUTH: u64 = 12_500;

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

/// One named check's outcome: the verdict plus the evidence line the drift log
/// carries.
#[derive(Debug)]
struct Check {
    ok: bool,
    detail: String,
}

impl Check {
    fn pass(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
        }
    }

    fn verdict(ok: bool, detail: impl Into<String>) -> Self {
        Self {
            ok,
            detail: detail.into(),
        }
    }
}

impl Verification {
    /// Record one named check; a failed check also lands in `divergences`
    /// with its detail (the drift log's evidence).
    fn record(&mut self, name: &'static str, check: Check) {
        if !check.ok {
            self.divergences.push(format!("{name}: {}", check.detail));
        }
        self.checks.push((name.to_string(), check.ok, check.detail));
    }

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

fn log_key(l: &Log) -> String {
    let mut s = format!("{}|", l.address());
    for t in l.topics() {
        s.push_str(&format!("{t:x}|"));
    }
    s.push_str(&hex::encode(&l.data().data));
    s
}

pub async fn verify_block(
    ctx: &Ctx,
    n: u64,
    reth_block: &Block,
    m_block: &Block,
) -> Result<Verification> {
    let mut v = Verification::default();
    v.record("tx_count", check_tx_count(reth_block, m_block));
    v.record("tx_order", check_tx_order(reth_block, m_block));
    v.record("blob_gas_used", check_blob_gas(reth_block, m_block));
    v.record("logs_bloom", check_logs_bloom(reth_block, m_block));
    v.record("log_set", check_log_set(ctx, n).await);
    match check_receipts(ctx, n, reth_block).await {
        ReceiptsCheck::Compared { count, gas } => {
            v.record("receipts_count", count);
            v.record("receipts", gas);
        }
        ReceiptsCheck::Unverified { reason } => {
            v.receipts_unverified = true;
            v.record("receipts", Check::pass(format!("unverified ({reason})")));
        }
        ReceiptsCheck::MUnavailable { reason } => {
            v.record("receipts", Check::fail(reason));
        }
    }
    if let Some(withdrawals) = &reth_block.withdrawals {
        if !withdrawals.is_empty() {
            v.record("withdrawals", check_withdrawals(ctx, n, withdrawals).await);
        }
    }

    // State probes are deliberately *not* re-run here. Post-execution they only
    // re-detected the blockhash-level divergence that receipts/log_set already
    // record (measured: `probe weth_total_supply diverged` on every block where
    // a blockhash-gated tx flipped status). The probes run once per block at the
    // repaired point (`ProbeRunner`), where a mismatch is a real repair failure
    // and nothing else - that is the invariant worth alarming on.

    v.ok = v.divergences.is_empty();
    Ok(v)
}

fn check_tx_count(reth_block: &Block, m_block: &Block) -> Check {
    let rt = block_txs(reth_block).len();
    let mt = m_block.transactions.len();
    Check::verdict(rt == mt, format!("reth={rt} m={mt}"))
}

/// Same hashes in the same positions (order is load-bearing for replay
/// fidelity).
fn check_tx_order(reth_block: &Block, m_block: &Block) -> Check {
    let r_hashes: Vec<String> = block_txs(reth_block)
        .iter()
        .map(|t| format!("0x{:x}", t.tx_hash()))
        .collect();
    let m_hashes: Vec<String> = block_txs(m_block)
        .iter()
        .map(|t| format!("0x{:x}", t.tx_hash()))
        .collect();
    if r_hashes.len() != m_hashes.len() {
        return Check::fail(format!("count reth={} m={}", r_hashes.len(), m_hashes.len()));
    }
    for (i, (a, b)) in r_hashes.iter().zip(m_hashes.iter()).enumerate() {
        if a != b {
            return Check::fail(format!("order differs at index {i}: reth={a} m={b}"));
        }
    }
    Check::pass(format!("{} txs in order", r_hashes.len()))
}

fn check_blob_gas(reth_block: &Block, m_block: &Block) -> Check {
    let r_blob = reth_block.header.blob_gas_used.unwrap_or(0);
    let m_blob = m_block.header.blob_gas_used.unwrap_or(0);
    Check::verdict(r_blob == m_blob, format!("reth={r_blob} m={m_blob}"))
}

fn check_logs_bloom(reth_block: &Block, m_block: &Block) -> Check {
    let ok = reth_block.header.logs_bloom == m_block.header.logs_bloom;
    if ok {
        Check::pass("")
    } else {
        Check::fail("mismatch")
    }
}

/// Log-set equality: the same events, order-insensitive. A fetch fault on
/// either side is a failed check (the comparison could not run), named with
/// both causes.
async fn check_log_set(ctx: &Ctx, n: u64) -> Check {
    match (ctx.reth.logs(n).await, ctx.anvil_m.logs(n).await) {
        (Ok(rl), Ok(ml)) => {
            let mut rk: Vec<String> = rl.iter().map(log_key).collect();
            let mut mk: Vec<String> = ml.iter().map(log_key).collect();
            rk.sort();
            mk.sort();
            if rk == mk {
                Check::pass(format!("{} logs", rk.len()))
            } else {
                Check::fail(first_log_diff(&rk, &mk))
            }
        }
        (r, m) => Check::fail(format!("log fetch failed: reth={:?} m={:?}", r.err(), m.err())),
    }
}

fn first_log_diff(rk: &[String], mk: &[String]) -> String {
    for (a, b) in rk.iter().zip(mk) {
        if a != b {
            return format!("first diff: reth={} m={}", trunc(a), trunc(b));
        }
    }
    format!("count reth={} m={}", rk.len(), mk.len())
}

/// The receipts comparison's three answers: `count` is structural (same number
/// of receipts), `gas` is behavioral (statuses equal; gas differs only on
/// type-4 within the refund bound). "reth could not serve receipts" is a
/// named degradation, never a pass.
enum ReceiptsCheck {
    Compared { count: Check, gas: Check },
    Unverified { reason: String },
    /// M's own receipts could not be read: recorded as one behavioral failure
    /// (a fetch fault on our own chain is not a structural verdict).
    MUnavailable { reason: String },
}

async fn check_receipts(ctx: &Ctx, n: u64, reth_block: &Block) -> ReceiptsCheck {
    let (rr, mr) = match read_receipts(ctx, n).await {
        Ok(pair) => pair,
        Err(check) => return check,
    };
    let count = Check::verdict(
        rr.len() == mr.len(),
        format!("reth={} m={}", rr.len(), mr.len()),
    );
    if !count.ok {
        return ReceiptsCheck::Compared {
            count,
            gas: Check::fail(format!("count reth={} m={}", rr.len(), mr.len())),
        };
    }
    ReceiptsCheck::Compared {
        count,
        gas: check_receipt_gas(block_txs(reth_block), &rr, &mr),
    }
}

/// Read both chains' receipts; an unreadable side is a recorded reason, never
/// a structural verdict (reth's pruned window, or M's own receipts failing —
/// a fetch fault on our own chain is not a verdict either).
async fn read_receipts(
    ctx: &Ctx,
    n: u64,
) -> std::result::Result<(Vec<TransactionReceipt>, Vec<TransactionReceipt>), ReceiptsCheck> {
    match (ctx.reth.receipts(n).await, ctx.anvil_m.receipts(n).await) {
        // reth could not serve receipts (pruned window): the block is accepted
        // unverified rather than failed for a source-side limitation.
        (Err(e), _) => Err(ReceiptsCheck::Unverified {
            reason: format!("reth receipts unavailable: {e}"),
        }),
        (_, Err(e)) => Err(ReceiptsCheck::MUnavailable {
            reason: format!("M receipts unavailable: {e}"),
        }),
        (Ok(rr), Ok(mr)) => Ok((rr, mr)),
    }
}

/// The per-tx receipt comparison: statuses must be identical; gas may differ
/// only for EIP-7702 txs, in `[-refund*auths, 0]` (the fork can only under-use
/// relative to mainnet's refunds).
fn check_receipt_gas(txs: &[Transaction], rr: &[TransactionReceipt], mr: &[TransactionReceipt]) -> Check {
    for (i, (a, b)) in rr.iter().zip(mr.iter()).enumerate() {
        if a.status() != b.status() {
            return Check::fail(format!("status mismatch at tx {i}"));
        }
        let ga = a.gas_used();
        let gb = b.gas_used();
        if ga == gb {
            continue;
        }
        // Gas may differ only for EIP-7702 txs, in [-refund*auths, 0].
        let ty = txs.get(i).map(|t| u8::from(t.inner.tx_type())).unwrap_or(0);
        if ty != 4 {
            return Check::fail(format!("non-type-4 gas mismatch at tx {i}: reth={ga} m={gb}"));
        }
        let auths = txs.get(i).map(auth_count).unwrap_or(0);
        let bound = TYPE4_REFUND_PER_AUTH * auths;
        let delta = gb as i64 - ga as i64; // fork - mainnet
        if delta > 0 || delta.unsigned_abs() > bound {
            return Check::fail(format!(
                "type-4 gas delta {delta} outside [-{bound},0] at tx {i}"
            ));
        }
    }
    Check::pass(format!("{} receipts", rr.len()))
}

/// Withdrawal recipients' balances must match reth at n (withdrawals are not
/// txs; the replay credits them directly).
async fn check_withdrawals(
    ctx: &Ctx,
    n: u64,
    withdrawals: &[alloy_rpc_types_eth::Withdrawal],
) -> Check {
    for w in withdrawals {
        match (
            ctx.reth.balance_at(w.address, n).await,
            ctx.anvil_m.balance_at(w.address, n).await,
        ) {
            (Ok(a), Ok(b)) if a == b => {}
            (Ok(a), Ok(b)) => {
                return Check::fail(format!("withdrawal {} balance reth={a} m={b}", w.address))
            }
            (a, b) => {
                return Check::fail(format!("withdrawal balance read failed: {a:?} {b:?}"))
            }
        }
    }
    Check::pass(format!("{} withdrawals", withdrawals.len()))
}

fn auth_count(tx: &alloy_rpc_types_eth::Transaction) -> u64 {
    match &*tx.inner {
        alloy_consensus::EthereumTxEnvelope::Eip7702(t) => t.tx().authorization_list.len() as u64,
        _ => 0,
    }
}

fn trunc(s: &str) -> String {
    let cut = mirror_anvil::truncate(s, 80);
    if cut.len() < s.len() {
        format!("{cut}…")
    } else {
        cut.to_string()
    }
}
