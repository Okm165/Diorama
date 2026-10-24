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
use alloy_primitives::{keccak256, Address, B256, U256};
use mirror_anvil::Key;
use std::collections::{HashMap, HashSet};

/// The addresses a source block touched, and the indexed log topics that
/// carry mapping keys. Built by the venue composer from block envelopes +
/// receipts + system transactions; `log_topics` is what lets a mapping
/// closure derive per-holder slots the account list alone cannot.
#[derive(Debug, Clone, Default)]
pub struct TouchedSet {
    pub addresses: HashSet<Address>,
    /// `(emitter, indexed topics)` per log, in block order.
    pub log_topics: Vec<(Address, Vec<B256>)>,
}

impl TouchedSet {
    pub fn touch(&mut self, address: Address) {
        self.addresses.insert(address);
    }

    pub fn log(&mut self, emitter: Address, topics: Vec<B256>) {
        self.addresses.insert(emitter);
        self.log_topics.push((emitter, topics));
    }

    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty() && self.log_topics.is_empty()
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
/// verified live against WETH's `balanceOf` (see the module docs).
pub fn mapping_slot(key: B256, base_slot: U256) -> U256 {
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(key.as_slice());
    preimage[32..].copy_from_slice(&base_slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(preimage).0)
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
            .log_topics
            .iter()
            .filter(|(emitter, topics)| {
                *emitter == self.contract && topics.len() > self.topic_index
            })
            .map(|(_, topics)| Key::Storage {
                address: self.contract,
                slot: mapping_slot(topics[self.topic_index], self.base_slot),
            })
            .collect()
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
}
