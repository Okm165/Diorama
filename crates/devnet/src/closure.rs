//! The venue's declared closure: from a touched set to concrete keys.
//!
//! The mainnet profile never needs this: the BAL enumerates every changed
//! slot, so "all slots of a watched account" is a filter. The HL public RPC
//! cannot enumerate anything (no BAL, no traces, latest-only state), so the
//! venue mirror's watch set must be *declared and enumerable* — and the
//! declaration is generated from the data the source does serve: block
//! envelopes (tx senders/recipients), system transactions (Core→EVM
//! credits), and receipt logs (emitters + indexed topics).
//!
//! Two closures cover the venue's layouts, both pinned by evidence:
//!
//! - [`FixedSlots`]: declared slots per contract (oracle slots, fee-growth
//!   accumulators, …) — config, not discovery.
//! - [`TopicMappedSlots`]: Solidity mapping slots from log topics,
//!   `slot = keccak256(pad32(key) ‖ pad32(base))`. Live-verified against
//!   WETH (slot 3): holder `0x28C6…1d60` ⇒ slot `0xdca77c…a94e`, and the
//!   storage word equals `balanceOf` exactly.
use alloy_primitives::{Address, B256, Bytes, Log, U256};
use mirror_anvil::{Key, Write};
use std::collections::{HashMap, HashSet};

/// The window's touches as a source can report them: the addresses and logs
/// the declared closures plan from, plus any changed keys the source already
/// carries values for (the trace rung; empty on the envelope rung). Built by
/// [`crate::bal::envelope_touches`] and by the trace-based sources.
#[derive(Debug, Clone, Default)]
pub struct TouchedSet {
    pub addresses: HashSet<Address>,
    /// The window's logs, in block order.
    pub logs: Vec<Log>,
    /// Changed keys the source already knows the post-values of (the trace
    /// rung): the synthesis does not re-read them.
    pub values: Vec<Write>,
}

impl TouchedSet {
    pub fn touch(&mut self, address: Address) {
        self.addresses.insert(address);
    }

    /// Record a log by its parts (the reconstructed system-transfer path).
    pub fn log(&mut self, emitter: Address, topics: Vec<B256>) {
        self.addresses.insert(emitter);
        self.logs
            .push(Log::new_unchecked(emitter, topics, Bytes::new()));
    }

    /// Adopt a log a receipt already carries (no re-shaping).
    pub fn push_log(&mut self, log: Log) {
        self.addresses.insert(log.address);
        self.logs.push(log);
    }

    /// Record a changed key the source already carries the value of (the
    /// trace rung). Deliberately not an address touch: the value is complete
    /// for the key, so the read plan must not re-read it.
    pub fn value(&mut self, write: Write) {
        self.values.push(write);
    }
}

/// Touched set → concrete keys to read. One closure list per venue mirror
/// (the declared watch model); the composer unions their outputs with the
/// watch set before reading.
pub trait KeyClosure: Send + Sync {
    fn keys_for(&self, touched: &TouchedSet) -> Vec<Key>;
}

/// Declared fixed slots per contract.
#[derive(Debug, Clone, Default)]
pub struct FixedSlots {
    pub slots: HashMap<Address, Vec<U256>>,
}

impl FixedSlots {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn declare(&mut self, contract: Address, slots: impl IntoIterator<Item = U256>) {
        self.slots.entry(contract).or_default().extend(slots);
    }
}

impl KeyClosure for FixedSlots {
    fn keys_for(&self, touched: &TouchedSet) -> Vec<Key> {
        let mut out = Vec::new();
        for address in &touched.addresses {
            if let Some(slots) = self.slots.get(address) {
                out.extend(slots.iter().map(|slot| Key::Storage {
                    address: *address,
                    slot: *slot,
                }));
            }
        }
        out
    }
}

/// Solidity mapping slot: `keccak256(pad32(key) ‖ pad32(base_slot))`.
///
/// This is the layout every Solidity mapping has (the compiler hashes the
/// concatenation of the key and the slot, both left-padded to 32 bytes);
/// verified live against WETH's `balanceOf` (see the module docs). The
/// preimage rule lives once, in `mirror_anvil::mapping_slot_word`.
pub fn mapping_slot(key: B256, base_slot: U256) -> U256 {
    mirror_anvil::mapping_slot_word(key, base_slot)
}

/// Mapping slots derived from one indexed log topic (ERC20/ERC721 balances,
/// allowances, …): for every log emitted by `contract` with at least
/// `topic_index + 1` topics, the topic at `topic_index` is the mapping key.
#[derive(Debug, Clone, Copy)]
pub struct TopicMappedSlots {
    pub contract: Address,
    pub base_slot: U256,
    pub topic_index: usize,
}

impl KeyClosure for TopicMappedSlots {
    fn keys_for(&self, touched: &TouchedSet) -> Vec<Key> {
        touched
            .logs
            .iter()
            .filter(|log| log.address == self.contract && log.topics().len() > self.topic_index)
            .map(|log| Key::Storage {
                address: self.contract,
                slot: mapping_slot(log.topics()[self.topic_index], self.base_slot),
            })
            .collect()
    }
}

/// The read plan: every key the poll must read, deduped, deterministic.
///
/// This is the composer's *decision* half — pure, no IO, no clock — so it is
/// unit-testable without a chain (functional core; the reads and the writes
/// are the imperative shell). Scope rules, evidence-backed:
///
/// - **Closures are authoritative for slots.** They only emit keys for
///   touched addresses (fixed) or configured contracts (mapped), so they are
///   the venue's slot declaration; filtering them through the watch set
///   again would drop exactly the derived keys the closure exists to produce.
/// - **Account fields follow the account**: balance/nonce/code are read only
///   for touched addresses the watch set names (the source cannot say which
///   field changed, so all three are read; unchanged values produce no
///   write).
/// - **Order**: closure order (config order), then touched addresses sorted —
///   deterministic plans make logs and tests stable.
pub fn plan_reads(
    closures: &[Box<dyn KeyClosure>],
    watch: &crate::cow::WatchSet,
    touched: &TouchedSet,
) -> Vec<Key> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for closure in closures {
        push_new(&mut out, &mut seen, closure.keys_for(touched));
    }
    let mut accounts: Vec<Address> = touched.addresses.iter().copied().collect();
    accounts.sort_unstable();
    for address in accounts {
        if watch.accounts.contains(&address) {
            push_new(&mut out, &mut seen, account_field_keys(address));
        }
    }
    out
}

/// The three account-field keys as one unit (they shadow together).
fn account_field_keys(address: Address) -> [Key; 3] {
    [
        Key::Balance(address),
        Key::Nonce(address),
        Key::Code(address),
    ]
}

/// Append `keys` in order, skipping ones already seen: the plan is deduped,
/// declaration order first.
fn push_new(out: &mut Vec<Key>, seen: &mut HashSet<Key>, keys: impl IntoIterator<Item = Key>) {
    for key in keys {
        if seen.insert(key) {
            out.push(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    /// The live vector (probed 2026-10-24 against mainnet via anvil):
    /// WETH slot 3, holder `0x28C6c06298d514Db089934071355E5743bf21d60`.
    #[test]
    fn mapping_slot_matches_the_live_weth_vector() {
        let holder = address!("28C6c06298d514Db089934071355E5743bf21d60");
        let key = B256::left_padding_from(holder.as_slice());
        let slot = mapping_slot(key, U256::from(3));
        let expected: U256 = "0xdca77c2adfd7db987f63f9968b5a29d8cf2d5bee6727fb1e132443d4c5e6a94e"
            .parse()
            .unwrap();
        assert_eq!(slot, expected);
    }

    #[test]
    fn fixed_slots_only_answer_for_touched_contracts() {
        let oracle = address!("00000000000000000000000000000000000000aa");
        let untouched = address!("00000000000000000000000000000000000000bb");
        let mut closure = FixedSlots::new();
        closure.declare(oracle, [U256::from(1), U256::from(2)]);
        closure.declare(untouched, [U256::from(9)]);

        let mut touched = TouchedSet::default();
        touched.touch(oracle);
        let keys = closure.keys_for(&touched);
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|k| k.address() == oracle));
    }

    #[test]
    fn topic_mapped_slots_read_the_indexed_holder() {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let holder = address!("28C6c06298d514Db089934071355E5743bf21d60");
        let closure = TopicMappedSlots {
            contract: weth,
            base_slot: U256::from(3),
            topic_index: 1, // Transfer(address indexed from, address indexed to, uint256)
        };

        let mut touched = TouchedSet::default();
        // A Transfer log: topic0 = the event signature, topic1 = from.
        touched.log(
            weth,
            vec![
                B256::from(alloy_primitives::keccak256(b"Transfer(address,address,uint256)")),
                B256::left_padding_from(holder.as_slice()),
                B256::left_padding_from(holder.as_slice()),
            ],
        );
        let keys = closure.keys_for(&touched);
        assert_eq!(keys.len(), 1);
        let expected: U256 = "0xdca77c2adfd7db987f63f9968b5a29d8cf2d5bee6727fb1e132443d4c5e6a94e"
            .parse()
            .unwrap();
        assert_eq!(
            keys[0],
            Key::Storage {
                address: weth,
                slot: expected,
            }
        );
    }

    #[test]
    fn a_log_from_another_contract_is_ignored() {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let other = address!("00000000000000000000000000000000000000cc");
        let closure = TopicMappedSlots {
            contract: weth,
            base_slot: U256::from(3),
            topic_index: 1,
        };
        let mut touched = TouchedSet::default();
        touched.log(other, vec![B256::ZERO, B256::ZERO]);
        assert!(closure.keys_for(&touched).is_empty());
    }

    #[test]
    fn plan_reads_scopes_account_fields_and_dedups_slots() {
        use crate::cow::WatchSet;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let oracle = address!("00000000000000000000000000000000000000aa");
        let holder = address!("28C6c06298d514Db089934071355E5743bf21d60");
        let watched = address!("00000000000000000000000000000000000000bb");

        let mut watch = WatchSet::default();
        watch.accounts.insert(watched);
        let mut fixed = FixedSlots::new();
        fixed.declare(oracle, [U256::from(1)]);
        let closures: Vec<Box<dyn KeyClosure>> = vec![
            Box::new(fixed),
            Box::new(TopicMappedSlots {
                contract: weth,
                base_slot: U256::from(3),
                topic_index: 1,
            }),
        ];

        let mut touched = TouchedSet::default();
        touched.touch(oracle);
        touched.touch(watched);
        touched.log(
            weth,
            vec![
                B256::ZERO,
                B256::left_padding_from(holder.as_slice()),
                B256::ZERO,
            ],
        );

        let plan = plan_reads(&closures, &watch, &touched);
        // oracle fixed slot + weth mapping slot + 3 account fields.
        assert_eq!(plan.len(), 5);
        assert!(plan.contains(&Key::Storage {
            address: oracle,
            slot: U256::from(1)
        }));
        assert!(plan.contains(&Key::Balance(watched)));
        assert!(plan.contains(&Key::Nonce(watched)));
        assert!(plan.contains(&Key::Code(watched)));

        // A second log for the same holder adds no key.
        touched.log(
            weth,
            vec![
                B256::ZERO,
                B256::left_padding_from(holder.as_slice()),
                B256::ZERO,
            ],
        );
        assert_eq!(plan_reads(&closures, &watch, &touched).len(), 5);
    }

    #[test]
    fn plan_reads_reads_no_fields_for_untouched_watched_accounts() {
        use crate::cow::WatchSet;
        let watched = address!("00000000000000000000000000000000000000bb");
        let other = address!("00000000000000000000000000000000000000cc");
        let mut watch = WatchSet::default();
        watch.accounts.insert(watched);
        let closures: Vec<Box<dyn KeyClosure>> = Vec::new();

        let mut touched = TouchedSet::default();
        touched.touch(other);
        assert!(plan_reads(&closures, &watch, &touched).is_empty());

        touched.touch(watched);
        assert_eq!(plan_reads(&closures, &watch, &touched).len(), 3);
    }
}
