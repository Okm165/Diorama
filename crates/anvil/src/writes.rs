//! The typed write path: [`Write`]s to cheatcode calls, batched.
//!
//! Every write the mirror makes to an anvil chain goes through here (parent
//! diffs, unshadow re-sync, deal, M repair, journal undo, shadow re-apply), so
//! the measured wire rules are stated once, as types: storage keys/values are
//! full 32-byte words (`B256` serializes with leading zeros; anvil rejects
//! odd-length or quantized hex with `-32602`), balances/nonces are quantities
//! (`U256`/`U64` serialize minimal-hex), and code is bytes. Batching chunks at
//! the anvil contract's 500 items / 1 MB on top of the configured chunk size.
use crate::client::{call, Call, Rpc};
use crate::error::Result;
use crate::trace::Write;
use alloy_primitives::{B256, U64};

/// Apply `writes` to `target` as batched cheatcode calls; returns the number of
/// accepted writes. A per-item refusal is logged and skipped (the caller counts
/// accepted writes); a transport fault is the caller's error.
pub async fn apply_writes(target: &Rpc, writes: &[Write], chunk_size: usize) -> Result<usize> {
    let calls = writes.iter().map(write_call).collect::<Result<Vec<_>>>()?;
    let mut applied = 0usize;
    for chunk in calls.chunks(chunk_size.max(1)) {
        for result in target.batch(chunk).await? {
            match result {
                Ok(_) => applied += 1,
                Err(e) => tracing::warn!(error = %e, "cheatcode write failed"),
            }
        }
    }
    Ok(applied)
}

/// One write as its cheatcode call (the measured wire shape per method).
fn write_call(write: &Write) -> Result<Call> {
    match write {
        Write::Storage {
            address,
            slot,
            value,
        } => call(
            "anvil_setStorageAt",
            (
                *address,
                B256::from(slot.to_be_bytes::<32>()),
                B256::from(value.to_be_bytes::<32>()),
            ),
        ),
        Write::Balance { address, value } => call("anvil_setBalance", (*address, *value)),
        Write::Nonce { address, value } => {
            call("anvil_setNonce", (*address, U64::from(*value)))
        }
        Write::Code { address, value } => call("anvil_setCode", (*address, value.clone())),
    }
}
