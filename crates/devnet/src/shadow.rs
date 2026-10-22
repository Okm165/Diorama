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
use mirror_anvil::Write;
use alloy_primitives::{Address, Bytes, U256};
use crate::error::{Error, Result};
use crate::metrics::Metrics;
use crate::shared::Shared;
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
    /// Latest recorded field values per shadowed account. Each field is
    /// optional: a marked account whose balance was never *recorded* must not
    /// have a default zero restored over a real value on a re-fork.
    #[serde(default)]
    pub account_values: BTreeMap<Address, AccountValues>,
}

/// The recorded account fields of one shadowed account (any subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "RawAccountValues")]
pub struct AccountValues {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<U256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Bytes>,
}

impl AccountValues {
    /// Is any field recorded? (A mark with nothing recorded is handed back.)
    fn any(&self) -> bool {
        self.balance.is_some() || self.nonce.is_some() || self.code.is_some()
    }
}

/// The persisted form: today's map, or the older `[balance, nonce]` tuple.
/// Old files must keep loading (the registry is a state file, not a cache).
#[derive(Deserialize)]
#[serde(untagged)]
enum RawAccountValues {
    Legacy((U256, u64)),
    Current(RawCurrent),
}

#[derive(Deserialize)]
struct RawCurrent {
    #[serde(default)]
    balance: Option<U256>,
    #[serde(default)]
    nonce: Option<u64>,
    #[serde(default)]
    code: Option<Bytes>,
}

impl From<RawAccountValues> for AccountValues {
    fn from(raw: RawAccountValues) -> Self {
        match raw {
            RawAccountValues::Legacy((balance, nonce)) => Self {
                balance: Some(balance),
                nonce: Some(nonce),
                code: None,
            },
            RawAccountValues::Current(c) => Self {
                balance: c.balance,
                nonce: c.nonce,
                code: c.code,
            },
        }
    }
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
                self.account_values.entry(*address).or_default().balance = Some(*value);
            }
            Write::Nonce { address, value } => {
                self.accounts.insert(*address);
                self.account_values.entry(*address).or_default().nonce = Some(*value);
            }
            Write::Code { address, value } => {
                self.accounts.insert(*address);
                self.account_values.entry(*address).or_default().code = Some(value.clone());
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
            let recorded = self
                .account_values
                .get(address)
                .map(AccountValues::any)
                .unwrap_or(false);
            if !recorded {
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
    /// the worst of both worlds. Values are batched through the one write path;
    /// the caller keeps the shadow marks so the parent diffs still skip these
    /// keys.
    ///
    /// A transport fault aborts the restore with a typed error: a partial
    /// restore that reported success would leave T holding some devnet values
    /// and some pin-era ones with no way to tell which (the caller retries or
    /// halts; the marks are still in place either way).
    pub async fn reapply(&self, rpc: &mirror_anvil::Rpc, chunk_size: usize) -> Result<usize> {
        let mut writes: Vec<Write> = Vec::new();
        for (address, slots) in &self.values {
            for (slot, value) in slots {
                writes.push(Write::Storage {
                    address: *address,
                    slot: *slot,
                    value: *value,
                });
            }
        }
        for (address, v) in &self.account_values {
            // Recorded zeros restore as zeros: "never recorded" is the absent
            // field, not a zero value (a drained account must come back
            // drained, not at its pin-era balance).
            if let Some(balance) = v.balance {
                writes.push(Write::Balance {
                    address: *address,
                    value: balance,
                });
            }
            if let Some(nonce) = v.nonce {
                writes.push(Write::Nonce {
                    address: *address,
                    value: nonce,
                });
            }
            if let Some(code) = &v.code {
                writes.push(Write::Code {
                    address: *address,
                    value: code.clone(),
                });
            }
        }
        mirror_anvil::apply_writes(rpc, &writes, chunk_size)
            .await
            .map_err(Error::from)
    }

    pub fn mark_account_fields(&mut self, address: Address) {
        self.accounts.insert(address);
    }

    pub fn mark_slot(&mut self, address: Address, slot: U256) {
        self.slots.entry(address).or_default().insert(slot);
    }

    pub fn slot_count(&self) -> usize {
        self.slots.values().map(std::collections::BTreeSet::len).sum()
    }

    /// Hand a slot back to mainnet sync. The caller re-syncs its value from
    /// reth, so T does not keep a stale devnet value with no shadow to explain
    /// it - and the recorded value goes too: a later re-fork must not restore
    /// (or re-own) a key that was deliberately handed back.
    pub fn unmark_slot(&mut self, address: Address, slot: U256) {
        if let Some(slots) = self.slots.get_mut(&address) {
            slots.remove(&slot);
            if slots.is_empty() {
                self.slots.remove(&address);
            }
        }
        if let Some(values) = self.values.get_mut(&address) {
            values.remove(&slot);
            if values.is_empty() {
                self.values.remove(&address);
            }
        }
    }

    /// Hand an account's balance/nonce/code back to mainnet sync (recorded
    /// values go with the mark, like [`Self::unmark_slot`]).
    pub fn unmark_account_fields(&mut self, address: Address) {
        self.accounts.remove(&address);
        self.account_values.remove(&address);
    }

    /// Drop values whose keys are not marked. The maps hold only devnet
    /// writes, and a write is recorded together with its mark - so a value
    /// without a mark is an orphan from an older engine (which left values
    /// behind when a key was handed back). Re-marking it would resurrect a
    /// value the devnet explicitly gave up (measured: a crashed oracle source
    /// came back this way); restore paths must prune instead.
    pub fn prune_orphans(&mut self) -> (usize, usize) {
        let orphan_slots: Vec<Address> = self
            .values
            .keys()
            .filter(|a| !self.slots.contains_key(*a))
            .copied()
            .collect();
        let mut slots = 0usize;
        for a in orphan_slots {
            if let Some(v) = self.values.remove(&a) {
                slots += v.len();
            }
        }
        let orphan_accounts: Vec<Address> = self
            .account_values
            .keys()
            .filter(|a| !self.accounts.contains(*a))
            .copied()
            .collect();
        let accounts = orphan_accounts.len();
        for a in orphan_accounts {
            self.account_values.remove(&a);
        }
        (slots, accounts)
    }

    /// A *new epoch*: the fork base moved (a fresh pin after a restart or a
    /// re-pin), so the devnet's writes are gone with the old base. Marks *and*
    /// values go: the recorded values were derived from the old base (plan
    /// 2.2/6.3 - "derived values go stale when the base advances"), and
    /// replaying them onto a moved base mixes eras (measured: a +30d
    /// time-travel write came back as a future `lastUpdateTimestamp` and every
    /// Aave supply panicked with an arithmetic underflow). The product
    /// re-creates its positions from its own journal (plan 3.6).
    pub fn clear_all(&mut self) -> (usize, usize) {
        let slots = self.slot_count();
        let accounts = self.accounts.len();
        self.slots.clear();
        self.accounts.clear();
        self.values.clear();
        self.account_values.clear();
        (slots, accounts)
    }

    /// Load the registry from its state file.
    ///
    /// Three answers (boundary-discipline): **absent** is a fact — first boot
    /// starts empty; an **IO fault** is not emptiness; **corrupt** bytes die
    /// here, at the load point, with the path named. Collapsing either failure
    /// into an empty registry would let the next parent diffs clobber devnet
    /// writes whose marks just vanished.
    pub fn load(path: &Path) -> Result<Self> {
        let mut s = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| Error::Corrupt {
                what: "shadow registry",
                path: path.to_path_buf(),
                source,
            })?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(source) => {
                return Err(Error::Io {
                    what: "shadow registry",
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        // Heal state files from the era when unmark kept recorded values:
        // orphans are dropped, never re-owned (see `prune_orphans`).
        let (slots, accounts) = s.prune_orphans();
        if slots > 0 || accounts > 0 {
            tracing::warn!(
                orphan_slots = slots,
                orphan_accounts = accounts,
                "shadow state had orphan values (keys handed back earlier); pruned"
            );
        }
        Ok(s)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                what: "shadow registry directory",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(|source| Error::Encode {
            what: "shadow registry",
            source,
        })?;
        std::fs::write(&tmp, bytes).map_err(|source| Error::Io {
            what: "shadow registry",
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| Error::Io {
            what: "shadow registry",
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Persist the registry and update its gauge: the one save path (the shadow
/// loop, hand-backs and deals all go through it, so the gauge never lags a
/// save).
pub fn save_registry(shadow: &Shared<Shadow>, path: &Path, metrics: &Metrics) -> Result<()> {
    let s = shadow.lock()?;
    s.save(path)?;
    metrics.set(&metrics.shadowed_slots, s.slot_count() as u64);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirror_anvil::Write;

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

    #[test]
    fn prune_orphans_drops_handed_back_values() {
        // A re-fork restores the value maps. Values whose keys are still
        // marked are devnet writes (restored, not touched here); values whose
        // keys are *not* marked are orphans from the era when unmark kept
        // them - restoring or re-owning those would resurrect a value the
        // devnet handed back (measured: a crashed oracle source).
        let mut s = Shadow::default();
        let a = Address::repeat_byte(0xaa); // owned: mark + value
        let b = Address::repeat_byte(0xbb); // orphan: value, no mark
        let c = Address::repeat_byte(0xcc); // marked, no recorded value
        s.mark_slot(a, U256::from(1u64));
        s.values.entry(a).or_default().insert(U256::from(1u64), U256::from(7u64));
        s.mark_account_fields(a);
        s.account_values.insert(
            a,
            AccountValues {
                balance: Some(U256::from(5u64)),
                nonce: Some(3),
                code: None,
            },
        );
        s.values.entry(b).or_default().insert(U256::from(2u64), U256::from(9u64));
        s.account_values.insert(
            b,
            AccountValues {
                balance: Some(U256::from(1u64)),
                nonce: Some(1),
                code: None,
            },
        );
        s.mark_slot(c, U256::from(3u64));

        let (slots, accounts) = s.prune_orphans();
        assert_eq!((slots, accounts), (1, 1)); // b's slot + b's fields dropped
        assert!(!s.values.contains_key(&b) && !s.account_values.contains_key(&b));
        assert!(s.values.contains_key(&a) && s.account_values.contains_key(&a)); // a kept
        // c is marked without a value: `unrecorded` hands it back.
        let (unknown_slots, unknown_accounts) = s.unrecorded();
        assert_eq!((unknown_slots, unknown_accounts), (1, 0));
    }

    #[test]
    fn clear_all_drops_marks_and_values() {
        let mut s = Shadow::default();
        let a = Address::repeat_byte(0xee);
        s.mark_slot(a, U256::from(1u64));
        s.mark_account_fields(a);
        s.values.entry(a).or_default().insert(U256::from(1u64), U256::from(7u64));
        s.account_values.insert(
            a,
            AccountValues {
                balance: Some(U256::from(5u64)),
                nonce: Some(3),
                code: None,
            },
        );

        let (slots, accounts) = s.clear_all();
        assert_eq!((slots, accounts), (1, 1));
        assert!(s.slots.is_empty() && s.accounts.is_empty());
        assert!(s.values.is_empty() && s.account_values.is_empty());
    }

    #[test]
    fn unmark_drops_the_recorded_value() {
        let mut s = Shadow::default();
        let a = Address::repeat_byte(0xdd);
        s.mark_slot(a, U256::from(1u64));
        s.values.entry(a).or_default().insert(U256::from(1u64), U256::from(7u64));
        s.mark_account_fields(a);
        s.account_values.insert(
            a,
            AccountValues {
                balance: Some(U256::from(5u64)),
                nonce: Some(3),
                code: None,
            },
        );

        s.unmark_slot(a, U256::from(1u64));
        s.unmark_account_fields(a);
        assert!(!s.values.contains_key(&a) && !s.account_values.contains_key(&a));
        assert_eq!(s.prune_orphans(), (0, 0)); // nothing left to prune
    }

    /// The pre-code registry stored `[balance, nonce]`: old state files must
    /// keep loading (the registry is a state file, not a cache).
    #[test]
    fn legacy_account_values_parse() {
        let a = Address::repeat_byte(0xab);
        let json = format!(
            r#"{{"accounts":["{a:?}"],"account_values":{{"{a:?}":[5,3]}}}}"#
        );
        let s: Shadow = serde_json::from_str(&json).unwrap();
        let v = s.account_values.get(&a).expect("legacy tuple loaded");
        assert_eq!(v.balance, Some(U256::from(5u64)));
        assert_eq!(v.nonce, Some(3));
        assert!(v.code.is_none());
    }

    /// A recorded zero restores as a zero; only an absent field means "never
    /// recorded" (the old format could not tell them apart).
    #[test]
    fn recorded_zero_is_a_value() {
        let a = Address::repeat_byte(0xcd);
        let json = format!(
            r#"{{"accounts":["{a:?}"],"account_values":{{"{a:?}":{{"balance":"0x0"}}}}}}"#
        );
        let mut s: Shadow = serde_json::from_str(&json).unwrap();
        assert!(s.account_values.get(&a).unwrap().any());
        assert_eq!(s.unrecorded(), (0, 0)); // kept: a recorded zero is a value
        // round-trips as a map, not as a legacy tuple
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"balance\":\"0x0\""), "{text}");
        s.unmark_account_fields(a);
        assert_eq!(s.prune_orphans(), (0, 0));
    }
}
