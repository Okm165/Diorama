//! The EIP-7928 BAL flattening. The measured padding rules and the prestateTracer
//! fallback live in `mirror-anvil` (anvil-facing formats); the symbols the
//! frozen engine still reaches through `crate::bal::*` are re-exported here
//! (R1 extraction — zero call-site churn; the write encoder now lives in
//! `mirror_anvil::apply_writes`).
//!
//! Measured rule (do not rediscover): "last change per key = final value" is
//! guaranteed by the spec ordering; `AccountChanges::storage_post_states()`
//! implements exactly that.
use alloy_eips::eip7928::BlockAccessList;

pub use mirror_anvil::{flatten_trace_block, Write};

/// Flatten a BAL into post-state writes (last change per key).
pub(crate) fn flatten_bal(bal: &BlockAccessList) -> Vec<Write> {
    let mut out = Vec::new();
    for acct in bal.iter() {
        if !acct.has_changes() {
            continue;
        }
        for (slot, value) in acct.storage_post_states() {
            out.push(Write::Storage {
                address: acct.address,
                slot,
                value,
            });
        }
        if let Some(value) = acct.balance_post_state() {
            out.push(Write::Balance {
                address: acct.address,
                value,
            });
        }
        if let Some(value) = acct.nonce_post_state() {
            out.push(Write::Nonce {
                address: acct.address,
                value,
            });
        }
        if let Some(value) = acct.code_post_state() {
            out.push(Write::Code {
                address: acct.address,
                value: value.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    #[test]
    fn bal_flatten_uses_post_states() {
        // Two writes to the same slot: only the last must survive.
        let bal: BlockAccessList = serde_json::from_str(
            // Canonical eth_getBlockAccessList shape ({index, value}); alloy's
            // serde renames also accept blockAccessIndex/newValue aliases.
            r#"[{
              "address":"0x00000000000000000000000000000000000000aa",
              "storageChanges":[
                {"slot":"0x1","changes":[{"index":1,"value":"0x2a"},{"index":2,"value":"0x2b"}]}
              ],
              "storageReads":["0x9"],
              "balanceChanges":[{"index":1,"value":"0x64"}],
              "nonceChanges":[],
              "codeChanges":[]
            }]"#,
        )
        .unwrap();
        let writes = flatten_bal(&bal);
        assert_eq!(writes.len(), 2);
        let storage = writes
            .iter()
            .find(|w| matches!(w, Write::Storage { .. }))
            .unwrap();
        match storage {
            Write::Storage { value, .. } => assert_eq!(*value, U256::from(0x2bu64)),
            _ => unreachable!(),
        }
        let balance = writes
            .iter()
            .find(|w| matches!(w, Write::Balance { .. }))
            .unwrap();
        match balance {
            Write::Balance { value, .. } => assert_eq!(*value, U256::from(100u64)),
            _ => unreachable!(),
        }
    }
}
