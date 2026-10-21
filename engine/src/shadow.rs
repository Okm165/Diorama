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
            Write::Storage { address, slot, .. } => {
                self.slots.entry(*address).or_default().insert(*slot);
            }
            _ => {
                self.accounts.insert(w.address());
            }
        }
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
