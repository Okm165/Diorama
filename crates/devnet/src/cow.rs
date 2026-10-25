//! COW: what a mirror keeps fresh (the scope) and the one filter that skips
//! what the devnet already owns.
//!
//! The name is the OS analogy — the devnet's writes shadow the parent's the
//! way copy-on-write pages shadow a fork's; there is no page sharing here,
//! the mechanism is the shadow registry plus this filter.
//!
//! The scope is per-mirror data — the watched accounts (both mirrors) plus the
//! declared closures (the venue's polled rung) — while the filter is one
//! implementation over the shared currency (a BAL's changes). Account fields
//! shadow as one unit; storage shadows per slot; fixture addresses are
//! whole-account devnet-owned (the deploy is invisible to the scanner).
use crate::closure::KeyClosure;
use crate::shadow::Shadow;
use alloy_eips::eip7928::{
    AccountChanges, BalanceChange, BlockAccessIndex, BlockAccessList, CodeChange, NonceChange,
    SlotChanges,
};
use alloy_primitives::{Address, U256};
use mirror_anvil::Key;
use std::collections::{HashMap, HashSet};

/// What the mirror keeps fresh.
///
/// The mainnet profile watches whole accounts (every slot of a watched
/// account arrives through the BAL, so `accounts` is the whole set); the venue
/// cannot enumerate touched slots, so it watches accounts for the account
/// fields and a declared slot closure on top. Whole-account fixture ownership
/// is not here: it belongs to the shadow registry (marks without values).
#[derive(Debug, Clone, Default)]
pub struct WatchSet {
    pub accounts: HashSet<Address>,
    /// Declared slots (venue closure; empty on mainnet).
    pub slots: HashMap<Address, HashSet<U256>>,
}

/// Declare `slots` for `contract` (extend, never replace: a second
/// declaration adds): the one body the watch set and the venue's fixed
/// closures share — their maps differ in the value collection, not in the
/// rule.
pub fn declare_slots<C: Extend<U256> + Default>(
    map: &mut HashMap<Address, C>,
    contract: Address,
    slots: impl IntoIterator<Item = U256>,
) {
    map.entry(contract).or_default().extend(slots);
}

impl WatchSet {
    pub fn new(accounts: HashSet<Address>) -> Self {
        Self {
            accounts,
            slots: HashMap::new(),
        }
    }

    /// Declare slots for a contract (the venue closure): whenever the account
    /// is touched, its declared slots are watched. The plan is the narrower
    /// set; the watch set's job is the COW filter and the audit.
    pub fn declare_slots(&mut self, contract: Address, slots: impl IntoIterator<Item = U256>) {
        declare_slots(&mut self.slots, contract, slots);
    }

    /// Is this write's key watched? Account fields follow the account; a
    /// storage write is watched when its account is (mainnet: all slots) or
    /// its slot is declared (venue closure).
    pub fn contains(&self, w: &mirror_anvil::Write) -> bool {
        self.contains_key(&w.key())
    }

    /// The key-level form (the read plan and the COW filter ask keys).
    pub fn contains_key(&self, key: &Key) -> bool {
        match key {
            Key::Storage { address, slot } => {
                self.accounts.contains(address)
                    || self
                        .slots
                        .get(address)
                        .is_some_and(|slots| slots.contains(slot))
            }
            other => self.accounts.contains(&other.address()),
        }
    }
}

/// The mirror's declared scope: the accounts/slots it keeps fresh and the
/// closures that turn touches into concrete keys. Per-mirror data, one shape.
#[derive(Default)]
pub struct Scope {
    watch: WatchSet,
    closures: Vec<Box<dyn KeyClosure>>,
}

impl Scope {
    pub fn new(watch: WatchSet, closures: Vec<Box<dyn KeyClosure>>) -> Self {
        Self { watch, closures }
    }

    /// The scope from config: the watch accounts plus the instance's declared
    /// closures (an empty section degenerates to the watch-only scope — the
    /// mainnet profile's case).
    pub fn from_config(
        accounts: HashSet<Address>,
        closures: &mirror_config::ClosuresSection,
    ) -> Self {
        Self::new(
            crate::closure::watch_set(accounts, closures),
            crate::closure::closures(closures),
        )
    }

    pub fn watch(&self) -> &WatchSet {
        &self.watch
    }

    pub fn closures(&self) -> &[Box<dyn KeyClosure>] {
        &self.closures
    }
}

/// The COW filter's counts (per change entry: one slot, or one account field).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilterStats {
    pub not_watched: usize,
    pub skipped_shadowed: usize,
}

/// Filter a BAL's changes: keep the watch set's, skip devnet-owned keys (COW:
/// account fields count as one unit, whole-account fixtures included). The
/// output is a change document — storage reads and per-tx indices are not part
/// of what the pipeline writes, so the kept changes are re-framed at index 1
/// (the same framing [`mirror_anvil::bal_from_writes`] uses).
pub fn filter_bal(watch: &WatchSet, shadow: &Shadow, bal: BlockAccessList) -> (BlockAccessList, FilterStats) {
    let mut out: BlockAccessList = Vec::new();
    let mut stats = FilterStats::default();
    for acct in bal {
        if let Some(kept) = filter_account(watch, shadow, &acct, &mut stats) {
            out.push(kept);
        }
    }
    out.sort_by_key(|acct| acct.address);
    (out, stats)
}

/// One account's changes: a fixture drops whole (the deploy is invisible to
/// the scanner, so every change of a fixture address is devnet-owned);
/// otherwise the watch/shadow decision runs per key.
fn filter_account(
    watch: &WatchSet,
    shadow: &Shadow,
    acct: &AccountChanges,
    stats: &mut FilterStats,
) -> Option<AccountChanges> {
    let address = acct.address;
    if shadow.is_fixture(&address) {
        stats.skipped_shadowed += crate::bal::changes_in(acct);
        return None;
    }
    let mut kept = AccountChanges::with_capacity(address, acct.storage_changes.len());
    for slot in &acct.storage_changes {
        let key = Key::Storage {
            address,
            slot: slot.slot,
        };
        if keep(watch, shadow, &key, stats) {
            kept.storage_changes.push(SlotChanges::new(
                slot.slot,
                slot.changes.last().cloned().into_iter().collect(),
            ));
        }
    }
    for (key, field) in account_fields(acct, address) {
        if !keep(watch, shadow, &key, stats) {
            continue;
        }
        match field {
            KeptField::Balance(change) => kept.balance_changes.push(change),
            KeptField::Nonce(change) => kept.nonce_changes.push(change),
            KeptField::Code(change) => kept.code_changes.push(change),
        }
    }
    if kept.has_changes() {
        Some(kept)
    } else {
        None
    }
}

/// One kept account field (the three shapes `filter_account` keeps).
enum KeptField {
    Balance(BalanceChange),
    Nonce(NonceChange),
    Code(CodeChange),
}

/// The account's changed fields as `(key, change)` pairs, one shape for all
/// three (the decision is the same for each).
fn account_fields(acct: &AccountChanges, address: Address) -> Vec<(Key, KeptField)> {
    let mut out = Vec::new();
    if let Some(value) = acct.balance_post_state() {
        out.push((
            Key::Balance(address),
            KeptField::Balance(BalanceChange::new(BlockAccessIndex::new(1), value)),
        ));
    }
    if let Some(value) = acct.nonce_post_state() {
        out.push((
            Key::Nonce(address),
            KeptField::Nonce(NonceChange::new(BlockAccessIndex::new(1), value)),
        ));
    }
    if let Some(value) = acct.code_post_state() {
        out.push((
            Key::Code(address),
            KeptField::Code(CodeChange::new(BlockAccessIndex::new(1), value.clone())),
        ));
    }
    out
}

/// The watch/shadow decision for one key: `true` keeps it, and the stats
/// record why when it does not.
fn keep(watch: &WatchSet, shadow: &Shadow, key: &Key, stats: &mut FilterStats) -> bool {
    if !watch.contains_key(key) {
        stats.not_watched += 1;
        false
    } else if shadow.is_shadowed_key(key) {
        stats.skipped_shadowed += 1;
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Bytes};

    fn storage(acct: Address, slot: u64, value: u64) -> AccountChanges {
        AccountChanges::new(acct).with_storage_change(
            SlotChanges::with_capacity(U256::from(slot), 1).with_change(
                alloy_eips::eip7928::StorageChange::new(
                    BlockAccessIndex::new(1),
                    U256::from(value),
                ),
            ),
        )
    }

    #[test]
    fn filter_keeps_watched_slots_and_skips_devnet_owned_ones() {
        let token = address!("00000000000000000000000000000000000000aa");
        let other = address!("00000000000000000000000000000000000000bb");
        let mut watch = WatchSet::default();
        watch.declare_slots(token, [U256::from(1)]);
        let mut shadow = Shadow::default();
        shadow.slots.entry(token).or_default().insert(U256::from(1));
        let bal: BlockAccessList = vec![
            storage(token, 1, 42),  // declared but devnet-owned -> skipped
            storage(token, 2, 43),  // not declared -> not watched
            storage(other, 1, 44),  // account not watched -> not watched
        ];
        let (filtered, stats) = filter_bal(&watch, &shadow, bal);
        assert!(filtered.is_empty());
        assert_eq!(stats.skipped_shadowed, 1);
        assert_eq!(stats.not_watched, 2);
    }

    #[test]
    fn filter_keeps_account_fields_as_one_unit_and_drops_fixtures() {
        let watched = address!("00000000000000000000000000000000000000cc");
        let fixture = address!("00000000000000000000000000000000000000dd");
        let watch = WatchSet::new([watched, fixture].into_iter().collect());
        let mut shadow = Shadow::default();
        shadow.set_fixtures([fixture].into_iter().collect());
        let bal: BlockAccessList = vec![
            AccountChanges::new(watched)
                .with_balance_change(BalanceChange::new(BlockAccessIndex::new(1), U256::from(7)))
                .with_nonce_change(NonceChange::new(BlockAccessIndex::new(1), 3)),
            AccountChanges::new(fixture).with_code_change(CodeChange::new(
                BlockAccessIndex::new(1),
                Bytes::from_static(&[0x60, 0x00]),
            )),
        ];
        let (filtered, stats) = filter_bal(&watch, &shadow, bal);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].address, watched);
        assert_eq!(filtered[0].balance_changes.len(), 1);
        assert_eq!(filtered[0].nonce_changes.len(), 1);
        assert_eq!(stats.skipped_shadowed, 1, "the fixture's code change");
        assert_eq!(stats.not_watched, 0);
    }
}
