//! EIP-7928 BAL flattening: the spec's `AccountChanges` into the write set.
//!
//! Measured rule (do not rediscover): "last change per key = final value" is
//! guaranteed by the spec ordering; `AccountChanges::storage_post_states()`
//! implements exactly that. The trace fallback's flattener lives beside this
//! one in [`crate::trace`] — both speak [`crate::Write`], the one currency.
//!
//! The inverse ([`bal_from_writes`]) makes the unification literal: any
//! composer that can produce a write set can hand the shared pipeline a
//! **BAL**, so the pipeline's input is one type for both mirrors — the
//! native BAL on reth, a synthesized one on the venue's polled state. The
//! round trip is lossless and passes the spec crate's own structural
//! validator (tested below).
use alloy_eips::eip7928::{
    AccountChanges, BalanceChange, BlockAccessIndex, BlockAccessList, CodeChange, NonceChange,
    SlotChanges, StorageChange,
};
use crate::Write;
use std::collections::BTreeMap;

/// Flatten a BAL into post-state writes (last change per key).
pub fn flatten_bal(bal: &BlockAccessList) -> Vec<Write> {
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

/// Build a spec-shaped BAL from a write set: one account entry per address
/// (sorted), one change per key at `BlockAccessIndex(1)` — the synthetic
/// "one transaction" framing, because a polled diff has no per-tx
/// attribution (index 1 with `transaction_count = 1` satisfies the spec's
/// index bound). The round trip through [`flatten_bal`] is lossless and the
/// result passes the spec crate's own structural validator (tested), so a
/// composer whose source has no native BAL can still hand the shared
/// pipeline a BAL without changing what the pipeline does.
pub fn bal_from_writes(writes: &[Write]) -> BlockAccessList {
    let index = BlockAccessIndex::new(1);
    let mut accounts: BTreeMap<alloy_primitives::Address, AccountChanges> = BTreeMap::new();
    for w in writes {
        let single = match w {
            Write::Storage {
                address,
                slot,
                value,
            } => AccountChanges::with_capacity(*address, 1).with_storage_change(
                SlotChanges::with_capacity(*slot, 1)
                    .with_change(StorageChange::new(index, *value)),
            ),
            Write::Balance { address, value } => AccountChanges::with_capacity(*address, 1)
                .with_balance_change(BalanceChange::new(index, *value)),
            Write::Nonce { address, value } => AccountChanges::with_capacity(*address, 1)
                .with_nonce_change(NonceChange::new(index, *value)),
            Write::Code { address, value } => AccountChanges::with_capacity(*address, 1)
                .with_code_change(CodeChange::new(index, value.clone())),
        };
        accounts
            .entry(w.address())
            .and_modify(|acct| acct.merge(single.clone()))
            .or_insert(single);
    }
    let mut bal: BlockAccessList = accounts.into_values().collect();
    for acct in &mut bal {
        acct.sort();
    }
    bal
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, U256};

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

    /// The unification proof: any write set can become a BAL that the spec
    /// crate's own validator accepts, and the shared flattener returns
    /// exactly the original writes.
    #[test]
    fn bal_from_writes_round_trips_and_passes_the_spec_validator() {
        let a = address!("00000000000000000000000000000000000000aa");
        let b = address!("00000000000000000000000000000000000000bb");
        let writes = vec![
            Write::Storage {
                address: a,
                slot: U256::from(1),
                value: U256::from(0x2a),
            },
            Write::Storage {
                address: a,
                slot: U256::from(2),
                value: U256::ZERO,
            },
            Write::Balance {
                address: a,
                value: U256::from(100),
            },
            Write::Nonce {
                address: b,
                value: 7,
            },
            Write::Code {
                address: b,
                value: alloy_primitives::Bytes::from_static(&[0x60, 0x00]),
            },
        ];
        let bal = bal_from_writes(&writes);

        // The spec crate's own structural validator (ordering, uniqueness,
        // non-empty slot lists, index bounds) accepts the synthetic BAL.
        alloy_eips::eip7928::validate_block_access_list(&bal, 1).unwrap();
        // Each address appears exactly once.
        assert_eq!(bal.len(), 2);

        // Lossless round trip (compared as sets: entry order is not part of
        // the contract).
        let mut want: Vec<String> = writes.iter().map(|w| format!("{w:?}")).collect();
        let mut got: Vec<String> = flatten_bal(&bal).iter().map(|w| format!("{w:?}")).collect();
        want.sort();
        got.sort();
        assert_eq!(got, want);
    }
}
