//! `PollComposer` — the venue profile's diff source: follow the HL public API
//! and turn touched accounts into state reads.
//!
//! The HL public RPC cannot enumerate a block's writes (no BAL, no traces,
//! latest-only state — measured 2026-10-24), so the composer derives them:
//! the window's envelopes (txs, receipts, system txs) give the **touched
//! set**; the venue's declared closures turn it into concrete keys; each key
//! is read from the source (latest) and diffed against V. The output is the
//! shared currency (`Vec<Write>`), so everything after composition is the
//! same pipeline as the mainnet's.
//!
//! Evidence pinned here:
//! - system txs are absent from `eth_getBlockByNumber`'s transactions and
//!   have no receipts on the public RPC (measured): the composer fetches them
//!   via `eth_getSystemTxsByBlockNumber` and reconstructs the `Transfer` log
//!   from the calldata — exactly the log the chain emits (`transfer(recipient,
//!   amount)` called by the system address).
//! - read precompiles never enter this path: the composer reads state, it
//!   does not execute.
//! - the window is `(from, to]` (the shared seam's contract): everything
//!   after the last composed block. The caller bounds it (the driver's gap
//!   policy), so a wide gap is a re-pin decision, never a silent partial poll.
use alloy_consensus::Transaction as _;
use alloy_network_primitives::TransactionResponse as _;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::{Transaction, TransactionReceipt};
use mirror_anvil::{Key, Write};
use mirror_devnet::closure::{plan_reads, KeyClosure, TouchedSet};
use mirror_devnet::diff::{BlockDiff, DiffComposer, DiffKind, WatchSet};
use mirror_devnet::metrics::Registry as _;
use mirror_devnet::source::{Fut, RawSource, Source as _};
use std::collections::HashSet;
use std::sync::Arc;

use crate::metrics::VenueMetrics;

/// ERC20 `Transfer(address,address,uint256)`.
const TRANSFER_TOPIC: B256 =
    alloy_primitives::b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
/// ERC20 `transfer(address,uint256)` selector.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// The venue's composer: reads the source at latest through the shared
/// `Source` seam, diffs against V, emits the changed keys as writes.
pub struct PollComposer {
    /// The source (HL public API). Reads are latest-by-design: the pin guard
    /// of `VenueSource` would refuse them (the source head is never the pin
    /// while following), so this is a `RawSource` with declared capabilities.
    source: Arc<RawSource>,
    /// The target (V): the values the polled reads are diffed against.
    target: Arc<RawSource>,
    /// What the mirror keeps fresh (account fields for these accounts).
    watch: WatchSet,
    /// The declared slot closures (fixed slots + mapping slots from logs).
    closures: Vec<Box<dyn KeyClosure>>,
    metrics: Arc<VenueMetrics>,
}

impl PollComposer {
    pub fn new(
        source: Arc<RawSource>,
        target: Arc<RawSource>,
        watch: WatchSet,
        closures: Vec<Box<dyn KeyClosure>>,
        metrics: Arc<VenueMetrics>,
    ) -> Self {
        Self {
            source,
            target,
            watch,
            closures,
            metrics,
        }
    }

    /// Fetch the window's envelopes and fold them into the touched set.
    async fn touched(&self, from: u64, to: u64) -> mirror_anvil::Result<TouchedSet> {
        // Declared capabilities gate every read path: a source without
        // `eth_getSystemTxsByBlockNumber` (any non-HyperEVM RPC — the local
        // test doubles) simply has no system txs to fold in.
        let system_txs_supported = self.source.capabilities().system_txs;
        let mut touched = TouchedSet::default();
        for n in (from + 1)..=to {
            let block = self.source.rpc().block_by_number(n, true).await?;
            let txs = mirror_devnet::scan::block_txs(&block);
            let receipts = self.source.rpc().receipts(n).await?;
            let system_txs = if system_txs_supported {
                self.source.rpc().system_txs(n).await?
            } else {
                Vec::new()
            };
            absorb_envelope(&mut touched, txs, &receipts, &system_txs);
        }
        Ok(touched)
    }
}

impl DiffComposer for PollComposer {
    fn kind(&self) -> DiffKind {
        DiffKind::Poll
    }

    fn compose(&self, from: u64, to: u64) -> Fut<'_, mirror_anvil::Result<BlockDiff>> {
        Box::pin(async move {
            if to <= from {
                return Ok(empty_diff(to));
            }
            let touched = self.touched(from, to).await?;
            let keys = plan_reads(&self.closures, &self.watch, &touched);
            let writes = self.read_plan(&keys).await?;
            self.record(&touched, &keys, &writes);
            Ok(BlockDiff {
                n: to,
                kind: DiffKind::Poll,
                writes,
            })
        })
    }

    /// Live parity (the venue's probe schedule): re-read the just-applied
    /// keys from V and compare against the values the composition expected —
    /// the poll's own post-condition, every window.
    fn verify<'a>(
        &'a self,
        writes: &'a [Write],
    ) -> Fut<'a, mirror_anvil::Result<Option<(usize, usize)>>> {
        Box::pin(async move {
            if writes.is_empty() {
                return Ok(Some((0, 0)));
            }
            let mut ok = 0usize;
            let mut failed = 0usize;
            let at = self.target.head().await?;
            for w in writes {
                let current = self.target.read_key(w.key(), at).await?;
                if current == w.value() {
                    ok += 1;
                } else {
                    failed += 1;
                    tracing::warn!(
                        key = ?w.key(),
                        expected = ?w.value(),
                        found = ?current,
                        "live parity: an applied key did not read back"
                    );
                }
            }
            Ok(Some((ok, failed)))
        })
    }
}

impl PollComposer {
    /// Read the plan on both sides and keep the differing keys: the
    /// composition's IO half, one function. One read point per side — the
    /// source's head for the truth, V's head for the current values. (Both
    /// sources ignore the block parameter on state reads — latest is the
    /// contract — but passing the observed heads keeps the intent explicit.)
    async fn read_plan(&self, keys: &[Key]) -> mirror_anvil::Result<Vec<Write>> {
        let source_at = self.source.head().await?;
        let target_at = self.target.head().await?;
        let mut writes = Vec::new();
        for key in keys {
            let source = self.source.read_key(*key, source_at).await?;
            let current = self.target.read_key(*key, target_at).await?;
            if source != current {
                if let Some(write) = Write::new(*key, source) {
                    writes.push(write);
                }
            }
        }
        Ok(writes)
    }

    /// Log emitters no closure covers and that are not watched accounts: a
    /// state change the declared watch model cannot follow. The audit's
    /// audible form — counted and warned, never silent.
    fn uncovered_emitters(&self, keys: &[Key], touched: &TouchedSet) -> HashSet<Address> {
        let planned: HashSet<Address> = keys.iter().map(mirror_anvil::Key::address).collect();
        touched
            .logs
            .iter()
            .map(|log| log.address)
            .filter(|address| {
                !self.watch.accounts.contains(address) && !planned.contains(address)
            })
            .collect()
    }

    /// One place for the poll's gauges and the audit warning.
    fn record(&self, touched: &TouchedSet, keys: &[Key], writes: &[Write]) {
        self.metrics
            .set(&self.metrics.poll_touched, touched.addresses.len() as u64);
        self.metrics
            .set(&self.metrics.poll_planned_keys, keys.len() as u64);
        self.metrics
            .set(&self.metrics.poll_changed_keys, writes.len() as u64);
        let uncovered = self.uncovered_emitters(keys, touched);
        if !uncovered.is_empty() {
            self.metrics
                .add(&self.metrics.poll_uncovered_logs, uncovered.len() as u64);
            tracing::warn!(
                emitters = ?uncovered,
                "poll: logs from contracts no closure covers (declare a closure to follow them)"
            );
        }
    }
}

/// The no-work diff (the window is empty or fully composed).
fn empty_diff(to: u64) -> BlockDiff {
    BlockDiff {
        n: to,
        kind: DiffKind::Poll,
        writes: Vec::new(),
    }
}

/// Fold one block's envelope into the touched set. Pure (no IO): the tests
/// drive it with the wire shapes the RPCs actually return.
///
/// Scope rule (deliberate): **addresses** collects tx senders/recipients and
/// value participants — the accounts whose *account fields* (balance, nonce,
/// code) a poll reads. Log **topics** are carried separately for the mapping
/// closures (a holder's balance slot is derived from the topic, not from the
/// account list), so an indexed holder does not enter `addresses`.
pub(crate) fn absorb_envelope(
    touched: &mut TouchedSet,
    txs: &[Transaction],
    receipts: &[TransactionReceipt],
    system_txs: &[Transaction],
) {
    for tx in txs {
        touched.touch(tx.from());
        if let Some(to) = tx.inner.to() {
            touched.touch(to);
        }
    }
    for receipt in receipts {
        for log in receipt.logs() {
            touched.push_log(log.inner.clone());
        }
    }
    for sys in system_txs {
        touched.touch(sys.from());
        let Some(to) = sys.inner.to() else {
            continue;
        };
        touched.touch(to);
        let input = sys.inner.input();
        if input.len() >= 36 && input[..4] == TRANSFER_SELECTOR {
            let recipient = Address::from_slice(&input[16..36]);
            touched.touch(recipient);
            // The reconstructed log: topic1 = the system sender (the `from`
            // the RPC reports), topic2 = the recipient.
            touched.log(
                to,
                vec![
                    TRANSFER_TOPIC,
                    B256::left_padding_from(sys.from().as_slice()),
                    B256::left_padding_from(recipient.as_slice()),
                ],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, U256};
    use mirror_devnet::closure::TopicMappedSlots;
    use serde_json::json;

    fn tx(value: serde_json::Value) -> Transaction {
        serde_json::from_value(value).expect("transaction wire shape")
    }

    fn receipt(emitter: Address, topics: &[B256]) -> TransactionReceipt {
        serde_json::from_value(json!({
            "transactionHash": B256::ZERO,
            "transactionIndex": "0x0",
            "blockHash": B256::ZERO,
            "blockNumber": "0x10",
            "from": Address::ZERO,
            "to": Address::ZERO,
            "cumulativeGasUsed": "0x5208",
            "gasUsed": "0x5208",
            "contractAddress": null,
            "logs": [{
                "address": emitter,
                "topics": topics,
                "data": "0x",
                "blockHash": B256::ZERO,
                "blockNumber": "0x10",
                "transactionHash": B256::ZERO,
                "transactionIndex": "0x0",
                "logIndex": "0x0",
                "removed": false,
            }],
            "logsBloom": "0x".to_string() + &"00".repeat(256),
            "status": "0x1",
            "effectiveGasPrice": "0x0",
            "type": "0x0",
        }))
        .expect("receipt wire shape")
    }

    fn transfer_tx(from: Address, to: Address, recipient: Address) -> Transaction {
        let mut input = vec![0u8; 4 + 32 + 32];
        input[..4].copy_from_slice(&TRANSFER_SELECTOR);
        input[16..36].copy_from_slice(recipient.as_slice());
        tx(json!({
            "hash": B256::ZERO,
            "nonce": "0x1",
            "blockHash": B256::ZERO,
            "blockNumber": "0x10",
            "transactionIndex": "0x0",
            "from": from,
            "to": to,
            "value": "0x0",
            "gasPrice": "0x0",
            "gas": "0x30d40",
            "input": alloy_primitives::Bytes::from(input),
            "v": "0x7f1",
            "r": "0x0",
            "s": "0x0",
            "type": "0x0",
            "chainId": "0x3e7",
        }))
    }

    #[test]
    fn a_normal_envelope_touches_senders_recipients_and_log_emitters() {
        let alice = address!("00000000000000000000000000000000000000a1");
        let pool = address!("00000000000000000000000000000000000000b2");
        let holder = address!("28C6c06298d514Db089934071355E5743bf21d60");
        let normal = transfer_tx(alice, pool, holder);
        let mut touched = TouchedSet::default();
        absorb_envelope(
            &mut touched,
            &[normal],
            &[receipt(
                pool,
                &[
                    TRANSFER_TOPIC,
                    B256::left_padding_from(alice.as_slice()),
                    B256::left_padding_from(holder.as_slice()),
                ],
            )],
            &[],
        );
        assert!(touched.addresses.contains(&alice));
        assert!(touched.addresses.contains(&pool));
        assert_eq!(touched.logs.len(), 1);
        let log = &touched.logs[0];
        assert_eq!(log.address, pool);
        // The holder is a topic, not an account-field read: mapping closures
        // derive its balance slot from the topic.
        assert_eq!(log.topics()[2], B256::left_padding_from(holder.as_slice()));
        assert!(!touched.addresses.contains(&holder));
    }

    #[test]
    fn a_system_transfer_reconstructs_the_transfer_log() {
        let token = address!("00000000000000000000000000000000000000c3");
        let recipient = address!("00000000000000000000000000000000000000d4");
        let system = address!("200000000000000000000000000000000000012b");
        let sys = transfer_tx(system, token, recipient);
        let mut touched = TouchedSet::default();
        absorb_envelope(&mut touched, &[], &[], &[sys]);
        assert!(touched.addresses.contains(&token));
        assert!(touched.addresses.contains(&recipient));
        assert!(touched.addresses.contains(&system));
        let log = &touched.logs[0];
        assert_eq!(log.address, token);
        assert_eq!(log.topics()[0], TRANSFER_TOPIC);
        assert_eq!(log.topics()[1], B256::left_padding_from(system.as_slice()));
        assert_eq!(log.topics()[2], B256::left_padding_from(recipient.as_slice()));
    }

    /// The whole pure chain: system transfer → touched set → declared
    /// closure → read plan with the recipient's balance slot.
    #[test]
    fn a_system_transfer_plans_the_recipients_balance_slot() {
        let token = address!("00000000000000000000000000000000000000c3");
        let recipient = address!("00000000000000000000000000000000000000d4");
        let sys = transfer_tx(
            address!("200000000000000000000000000000000000012b"),
            token,
            recipient,
        );
        let mut touched = TouchedSet::default();
        absorb_envelope(&mut touched, &[], &[], &[sys]);

        let closures: Vec<Box<dyn KeyClosure>> = vec![Box::new(TopicMappedSlots {
            contract: token,
            base_slot: U256::from(3),
            topic_index: 2, // Transfer(address indexed from, address indexed to, uint256)
        })];
        let plan = plan_reads(&closures, &WatchSet::default(), &touched);
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0],
            mirror_anvil::Key::Storage {
                address: token,
                slot: mirror_devnet::closure::mapping_slot(
                    B256::left_padding_from(recipient.as_slice()),
                    U256::from(3),
                ),
            }
        );
    }
}
