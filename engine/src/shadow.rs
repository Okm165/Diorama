//! Shadow registry: tracks the devnet's own writes on T (copy-on-write).
//!
//! COW rules (plan §2.2):
//!   - storage shadows per slot;
//!   - account fields (balance + nonce + code) shadow as one unit;
//!   - the applier skips any parent write whose key is shadowed, so the devnet's
//!     writes are never overwritten by the next parent block.
//!
//! Writes are discovered by watching T's own blocks (traces + tx semantics) and
//! can be added explicitly through the admin API.
use crate::bal::Write;
use alloy_primitives::{Address, U256};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Shadow {
    /// Storage slots the devnet has written.
    #[serde(default)]
    pub slots: BTreeMap<Address, BTreeSet<U256>>,
    /// Accounts whose balance/nonce/code the devnet has written (one unit).
    #[serde(default)]
    pub accounts: BTreeSet<Address>,
    /// Latest value per shadowed storage key (what a re-fork must restore).
    #[serde(default)]
    pub values: BTreeMap<Address, BTreeMap<U256, U256>>,
    /// Latest (balance, nonce) per shadowed account.
    #[serde(default)]
    pub account_values: BTreeMap<Address, (U256, u64)>,
}

impl Shadow {
    pub fn is_shadowed(&self, w: &Write) -> bool {
        match w {
            Write::Storage { address, slot, .. } => self
                .slots
                .get(address)
                .map(|s| s.contains(slot))
                .unwrap_or(false),
            _ => self.accounts.contains(&w.address()),
        }
    }

    /// Mark a single write as shadowed.
    pub fn mark(&mut self, w: &Write) {
        match w {
            Write::Storage { address, slot, value } => {
                self.slots.entry(*address).or_default().insert(*slot);
                // Keep the latest value: it is what a re-fork has to restore.
                self.values.entry(*address).or_default().insert(*slot, *value);
            }
            Write::Balance { address, value } => {
                self.accounts.insert(*address);
                self.account_values.entry(*address).or_default().0 = *value;
            }
            Write::Nonce { address, value } => {
                self.accounts.insert(*address);
                self.account_values.entry(*address).or_default().1 = *value;
            }
            _ => {
                self.accounts.insert(w.address());
            }
        }
    }

    /// Keys that are marked devnet-owned but whose value we never recorded
    /// (marked by an older engine, or a write we could not trace): on a re-fork
    /// they would hold pin-era mainnet values while mainnet's diffs keep
    /// skipping them - the worst of both worlds. Hand them back instead.
    pub fn unrecorded(&mut self) -> (usize, usize) {
        let mut slots = 0usize;
        let mut accounts = 0usize;
        // Collect first (the borrow checker will not let us mutate while
        // iterating the same maps).
        let mut drop_slots: Vec<(Address, U256)> = Vec::new();
        for (address, set) in self.slots.iter() {
            let known = self.values.get(address);
            for slot in set.iter() {
                if known.map(|m| !m.contains_key(slot)).unwrap_or(true) {
                    drop_slots.push((*address, *slot));
                }
            }
        }
        slots += drop_slots.len();
        for (address, slot) in drop_slots {
            if let Some(set) = self.slots.get_mut(&address) {
                set.remove(&slot);
            }
        }
        self.slots.retain(|_, set| !set.is_empty());
        let mut drop_accounts: Vec<Address> = Vec::new();
        for address in self.accounts.iter() {
            if !self.account_values.contains_key(address) {
                drop_accounts.push(*address);
            }
        }
        accounts += drop_accounts.len();
        for address in drop_accounts {
            self.accounts.remove(&address);
        }
        (slots, accounts)
    }

    /// Re-apply the devnet's own writes to a chain that was re-forked under us
    /// (anvil-t's OOM restart, a re-pin). Without this the devnet's positions
    /// vanish silently while their shadow marks keep mainnet's diffs away -
    /// the worst of both worlds. Values are batched; the caller keeps the
    /// shadow marks so the parent diffs still skip these keys.
    pub async fn reapply(&self, rpc: &crate::rpc::Rpc) -> usize {
        let mut calls: Vec<(String, serde_json::Value)> = Vec::new();
        for (address, slots) in &self.values {
            for (slot, value) in slots {
                calls.push((
                    "anvil_setStorageAt".to_string(),
                    serde_json::json!([
                        address,
                        crate::bal::pad_data32(*slot),
                        crate::bal::pad_data32(*value)
                    ]),
                ));
            }
        }
        for (address, (balance, nonce)) in &self.account_values {
            if !balance.is_zero() {
                calls.push((
                    "anvil_setBalance".to_string(),
                    serde_json::json!([address, crate::bal::quant_hex_u256(*balance)]),
                ));
            }
            if *nonce > 0 {
                calls.push((
                    "anvil_setNonce".to_string(),
                    serde_json::json!([address, crate::bal::quant_hex_u64(*nonce)]),
                ));
            }
        }
        let mut applied = 0usize;
        for chunk in calls.chunks(500) {
            match rpc.batch_raw(chunk).await {
                Ok(results) => applied += results.iter().filter(|r| r.is_ok()).count(),
                Err(_) => break,
            }
        }
        applied
    }

    pub fn mark_account_fields(&mut self, address: Address) {
        self.accounts.insert(address);
    }

    pub fn mark_slot(&mut self, address: Address, slot: U256) {
        self.slots.entry(address).or_default().insert(slot);
    }

    pub fn slot_count(&self) -> usize {
        self.slots.values().map(|s| s.len()).sum()
    }

    /// Hand a slot back to mainnet sync. The caller re-syncs its value from
    /// reth, so T does not keep a stale devnet value with no shadow to explain it.
    pub fn unmark_slot(&mut self, address: Address, slot: U256) {
        if let Some(slots) = self.slots.get_mut(&address) {
            slots.remove(&slot);
            if slots.is_empty() {
                self.slots.remove(&address);
            }
        }
    }

    /// Hand an account's balance/nonce/code back to mainnet sync.
    pub fn unmark_account_fields(&mut self, address: Address) {
        self.accounts.remove(&address);
    }

    /// A re-pin starts a new epoch: the devnet's writes are gone with the old
    /// chain, so the shadow set ("these keys belong to the devnet") must go too.
    /// Keeping it would silently stop T from syncing keys it no longer owns.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.accounts.clear();
    }

    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bal::Write;

    #[test]
    fn cow_granularity() {
        let mut s = Shadow::default();
        let a = Address::repeat_byte(0xaa);
        s.mark_slot(a, U256::from(1u64));
        s.mark_account_fields(a);

        assert!(s.is_shadowed(&Write::Storage {
            address: a,
            slot: U256::from(1u64),
            value: U256::ZERO
        }));
        // other slots of the same account still sync
        assert!(!s.is_shadowed(&Write::Storage {
            address: a,
            slot: U256::from(2u64),
            value: U256::ZERO
        }));
        // account fields shadow as one unit
        assert!(s.is_shadowed(&Write::Balance {
            address: a,
            value: U256::ZERO
        }));
        assert!(s.is_shadowed(&Write::Nonce {
            address: a,
            value: 0
        }));
    }
}
