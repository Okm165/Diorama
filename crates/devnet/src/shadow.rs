//! Shadow registry: tracks the devnet's own writes on T (copy-on-write).
//!
//! COW rules:
//!   - storage shadows per slot;
//!   - account fields (balance + nonce + code) shadow as one unit;
//!   - the applier skips any parent write whose key is shadowed, so the devnet's
//!     writes are never overwritten by the next parent block.
//!
//! Writes are discovered by watching T's own blocks (traces + tx semantics) and
//! can be added explicitly through the admin API.
use diorama_anvil::{Rpc, Write};
use alloy_primitives::{Address, Bytes, U256};
use crate::error::{Error, Result};
use crate::metrics::{Metrics, Registry};
use crate::shared::Shared;
use crate::store;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The artifact's name in errors and load messages: one home.
const ARTIFACT: &str = "shadow registry";

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
    pub(crate) account_values: BTreeMap<Address, AccountValues>,
    /// Whole-account ownership for the devnet's fixtures: the
    /// canonical addresses daloc's deploy replaces via `anvil_setCode` write
    /// through *cheatcodes*, which the tx scanner cannot see — without this,
    /// the next parent diff would clobber the mock. Config carries the list;
    /// it is runtime state, never persisted.
    #[serde(skip)]
    pub fixtures: BTreeSet<Address>,
}

/// The recorded account fields of one shadowed account (any subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "RawAccountValues")]
pub(crate) struct AccountValues {
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
    /// The key-level shadow check (the BAL-typed COW filter asks keys, not
    /// writes).
    pub fn is_shadowed_key(&self, key: &diorama_anvil::Key) -> bool {
        if self.is_fixture(&key.address()) {
            // Whole-account ownership: every write to a fixture address is
            // devnet-owned, marked or not (the fixture deploy is invisible to
            // the scanner).
            return true;
        }
        match key {
            diorama_anvil::Key::Storage { address, slot } => self
                .slots
                .get(address)
                .map(|s| s.contains(slot))
                .unwrap_or(false),
            _ => self.accounts.contains(&key.address()),
        }
    }

    /// Is this a fixture address (whole-account devnet ownership)?
    pub fn is_fixture(&self, address: &Address) -> bool {
        self.fixtures.contains(address)
    }

    /// Install the fixture list (boot: from the instance's
    /// `fixture_addresses`).
    pub fn set_fixtures(&mut self, fixtures: BTreeSet<Address>) {
        self.fixtures = fixtures;
    }

    /// Every address the registry owns, fixtures included (the probe
    /// runner's skip set).
    pub fn owned_addresses(&self) -> BTreeSet<Address> {
        self.slots
            .keys()
            .chain(self.accounts.iter())
            .chain(self.fixtures.iter())
            .copied()
            .collect()
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
        (self.drop_unvalued_slots(), self.drop_unvalued_accounts())
    }

    /// Drop marked slots with no recorded value: they would hold pin-era
    /// source values under a devnet mark. Returns the count dropped.
    fn drop_unvalued_slots(&mut self) -> usize {
        // Collect first (the borrow checker will not let us mutate while
        // iterating the same maps).
        let mut drop_slots: Vec<(Address, U256)> = Vec::new();
        for (address, set) in self.slots.iter() {
            for slot in set.iter() {
                if self.unvalued_slot(address, slot) {
                    drop_slots.push((*address, *slot));
                }
            }
        }
        for (address, slot) in &drop_slots {
            if let Some(set) = self.slots.get_mut(address) {
                set.remove(slot);
            }
        }
        self.slots.retain(|_, set| !set.is_empty());
        drop_slots.len()
    }

    /// A slot mark with no recorded value: it cannot restore anything, so it
    /// is dropped (the same rule the accounts get below).
    fn unvalued_slot(&self, address: &Address, slot: &U256) -> bool {
        self.values
            .get(address)
            .is_none_or(|m| !m.contains_key(slot))
    }

    /// Drop whole-account marks with no recorded value (the same rule as the
    /// slots). Returns the count dropped.
    fn drop_unvalued_accounts(&mut self) -> usize {
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
        for address in &drop_accounts {
            self.accounts.remove(address);
        }
        drop_accounts.len()
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
    pub async fn reapply(&self, rpc: &diorama_anvil::Rpc, chunk_size: usize) -> Result<usize> {
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
        diorama_anvil::apply_writes(rpc, &writes, chunk_size)
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

    /// Any recorded state at all (marks, before-values, account values): the
    /// epoch rule's restore gate and the registry's emptiness test.
    pub fn has_recorded(&self) -> bool {
        self.slot_count() > 0
            || !self.accounts.is_empty()
            || !self.values.is_empty()
            || !self.account_values.is_empty()
    }

    /// The shadowed addresses as the `/status` surface reports them: the
    /// registry's whole ownership set — slot-marked, account-marked and
    /// fixture addresses — lowercase, sorted, deduped (one presentation for
    /// both engines). Deliberately the same set the probe runner's skip
    /// consults ([`Shadow::owned_addresses`]): a consumer of the status (the
    /// scenario's parity probes) must skip exactly what the engine skips
    /// (measured: USDC/WETH/aEthUSDC are fixtures whose supplies freeze at
    /// the pin by design; a status without them had the scenario compare
    /// devnet-owned values against reth).
    pub fn shadowed_addresses(&self) -> Vec<String> {
        self.owned_addresses()
            .iter()
            .map(|a| format!("{a:?}").to_lowercase())
            .collect()
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

    /// Load the registry from its state file ([`crate::store`]'s three
    /// answers; collapsing a fault or corruption into an empty registry would
    /// let the next parent diffs clobber devnet writes whose marks just
    /// vanished). Heals state files from the era when unmark kept recorded
    /// values: orphans are dropped, never re-owned (see `prune_orphans`).
    pub fn load(path: &Path) -> Result<Self> {
        let mut s: Self = store::load(path, ARTIFACT)?.unwrap_or_default();
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
        store::save(path, self, ARTIFACT)
    }
}

/// Persist the registry and update its gauge: the one save path (the shadow
/// loop, hand-backs and deals all go through it, so the gauge never lags a
/// save).
pub(crate) fn save_registry(shadow: &Shared<Shadow>, path: &Path, metrics: &Metrics) -> Result<()> {
    let s = shadow.lock()?;
    s.save(path)?;
    metrics.set(&metrics.shadowed_slots, s.slot_count() as u64);
    metrics.set(&metrics.shadowed_accounts, s.accounts.len() as u64);
    Ok(())
}

/// What a same-pin restore moved, for the caller's log and metrics.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct RestoreStats {
    pub slots: usize,
    pub accounts: usize,
    pub restored: usize,
    pub orphan_slots: usize,
    pub orphan_accounts: usize,
    pub unknown_slots: usize,
    pub unknown_accounts: usize,
}

/// Re-apply the devnet's recorded writes to a chain that was reset at the same
/// pin (T re-forked under the engine; V restarted at the same base).
///
/// Three steps, in order: prune orphaned values (keys whose marks are gone —
/// from an older engine that kept values on unmark), re-apply the recorded
/// values, and hand back marked keys with no recorded value (they would hold
/// pin-era source values under a devnet mark). The caller persists the
/// registry; a fault leaves the marks in memory, so a retry is idempotent.
pub(crate) async fn restore_recorded(
    target: &Rpc,
    shadow: &Shared<Shadow>,
    chunk_size: usize,
) -> Result<RestoreStats> {
    let (orphan_slots, orphan_accounts) = shadow.lock()?.prune_orphans();
    let (slots, accounts) = {
        let s = shadow.lock()?;
        (s.slot_count(), s.accounts.len())
    };
    // Clone out: the re-apply awaits, and a MutexGuard must not be held across
    // it (the loops must stay Send).
    let snapshot = shadow.lock()?.clone();
    let restored = snapshot.reapply(target, chunk_size).await?;
    let (unknown_slots, unknown_accounts) = shadow.lock()?.unrecorded();
    Ok(RestoreStats {
        slots,
        accounts,
        restored,
        orphan_slots,
        orphan_accounts,
        unknown_slots,
        unknown_accounts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `/status` presentation is the registry's: lowercase, sorted,
    /// deduped, slot-marked, account-marked and fixture addresses together —
    /// the same set the probe runner skips.
    #[test]
    fn shadowed_addresses_are_presented_once() {
        let mut s = Shadow::default();
        let a = Address::repeat_byte(0xaa);
        let b = Address::repeat_byte(0xbb);
        let f = Address::repeat_byte(0xcc);
        s.mark_slot(b, U256::from(1u64));
        s.mark_slot(a, U256::from(1u64));
        s.mark_account_fields(a);
        s.set_fixtures(std::iter::once(f).collect());
        let out = s.shadowed_addresses();
        assert_eq!(out.len(), 3, "one entry per address: {out:?}");
        assert_eq!(out[0], format!("{a:?}").to_lowercase());
        assert_eq!(out[1], format!("{b:?}").to_lowercase());
        assert_eq!(out[2], format!("{f:?}").to_lowercase(), "fixtures are owned");
        assert_eq!(out, s.owned_addresses().iter().map(|a| format!("{a:?}").to_lowercase()).collect::<Vec<_>>());
    }

    #[test]
    fn cow_granularity() {
        let mut s = Shadow::default();
        let a = Address::repeat_byte(0xaa);
        s.mark_slot(a, U256::from(1u64));
        s.mark_account_fields(a);
        assert!(s.is_shadowed_key(&diorama_anvil::Key::Storage {
            address: a,
            slot: U256::from(1u64),
        }));
        // other slots of the same account still sync
        assert!(!s.is_shadowed_key(&diorama_anvil::Key::Storage {
            address: a,
            slot: U256::from(2u64),
        }));
        // account fields shadow as one unit
        assert!(s.is_shadowed_key(&diorama_anvil::Key::Balance(a)));
        assert!(s.is_shadowed_key(&diorama_anvil::Key::Nonce(a)));
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

    /// Fixture addresses are whole-account owned: the fixture
    /// deploy writes through cheatcodes the scanner cannot see, so every
    /// write to those addresses must be skipped without a mark — and the
    /// list is runtime state, never persisted.
    #[test]
    fn fixture_addresses_are_owned_whole() {
        let fixture = Address::repeat_byte(0xf1);
        let other = Address::repeat_byte(0xf2);
        let mut s = Shadow::default();
        s.set_fixtures([fixture].into_iter().collect());
        assert!(
            s.is_shadowed_key(&diorama_anvil::Key::Storage {
                address: fixture,
                slot: U256::from(7u64),
            }),
            "any fixture slot is owned"
        );
        assert!(
            s.is_shadowed_key(&diorama_anvil::Key::Balance(fixture)),
            "fixture account fields are owned"
        );
        assert!(
            !s.is_shadowed_key(&diorama_anvil::Key::Storage {
                address: other,
                slot: U256::from(7u64),
            }),
            "non-fixtures need a mark"
        );
        assert!(s.owned_addresses().contains(&fixture));
        // Runtime-only: the persisted form never carries the fixture list.
        let text = serde_json::to_string(&s).unwrap();
        assert!(!text.contains("fixture"), "{text}");
        let reloaded: Shadow = serde_json::from_str(&text).unwrap();
        assert!(reloaded.fixtures.is_empty(), "fixtures come from config");
    }
}
