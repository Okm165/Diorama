//! The typed write path: [`Write`]s to cheatcode calls, batched.
//!
//! Every write the mirror makes to an anvil chain goes through here (parent
//! diffs, unshadow re-sync, deal, M repair, journal undo, shadow re-apply), so
//! the measured rules are stated once: padding is mandatory (anvil rejects
//! odd-length hex with `-32602`), and `batch_raw` chunks at the anvil
//! contract's 500 items / 1 MB on top of the configured chunk size.
use crate::client::Rpc;
use crate::error::Result;
use crate::pad::{bytes_hex, pad_data32, quant_hex_u256, quant_hex_u64};
use crate::trace::Write;
use serde_json::{json, Value};

/// Apply `writes` to `target` as batched cheatcode calls; returns the number of
/// accepted writes. A per-item refusal is logged and skipped (the caller counts
/// accepted writes); a transport fault is the caller's error.
pub async fn apply_writes(target: &Rpc, writes: &[Write], chunk_size: usize) -> Result<usize> {
    let mut calls: Vec<(String, Value)> = Vec::with_capacity(writes.len());
    for w in writes {
        calls.push(match w {
            Write::Storage { address, slot, value } => (
                "anvil_setStorageAt".into(),
                json!([address, pad_data32(*slot), pad_data32(*value)]),
            ),
            Write::Balance { address, value } => {
                ("anvil_setBalance".into(), json!([address, quant_hex_u256(*value)]))
            }
            Write::Nonce { address, value } => {
                ("anvil_setNonce".into(), json!([address, quant_hex_u64(*value)]))
            }
            Write::Code { address, value } => {
                ("anvil_setCode".into(), json!([address, bytes_hex(value)]))
            }
        });
    }
    let mut applied = 0usize;
    for chunk in calls.chunks(chunk_size.max(1)) {
        for r in target.batch_raw(chunk).await? {
            match r {
                Ok(_) => applied += 1,
                Err(e) => tracing::warn!(error = %e, "cheatcode write failed"),
            }
        }
    }
    Ok(applied)
}
