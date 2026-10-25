//! The BAL: the one currency both mirrors compose, COW and apply.
//!
//! Both parent chains end in the same document — EIP-7928's block-level access
//! list — and this module is the generic composer that produces it:
//!
//! - **rung 1 (native)**: when the source records BALs (`Capabilities::
//!   native_bal`), the window's blocks are fetched and merged — reth serves
//!   them; a nanoreth-style archive would too. A block without one falls back
//!   per block to rung 2.
//! - **rung 2 (synthesized)**: the source reports the window's *touches* (the
//!   addresses and logs it can enumerate) plus any changed keys it already
//!   carries values for (the trace rung); the declared scope plans the keys to
//!   read, each is read on the source and the target, and the differing ones
//!   are framed as a BAL (`bal_from_writes`). This is the HyperEVM rung (the
//!   public API enumerates no writes) and reth's trace fallback.
//!
//! Everything after `compose` is shared: [`plan`] is the one call for the
//! whole pipeline half (`compose` → [`crate::cow::filter_bal`] drops the
//! devnet-owned keys → [`mirror_anvil::flatten_bal`] lowers the BAL to the
//! write set), and the engines' apply/scan/parity sequences run unchanged.
//! The window is `(from, to]`; the mainnet composes one parent block at a
//! time, the venue composes everything since its last composed source block.
use crate::closure::{plan_reads, TouchedSet};
use crate::cow::Scope;
use crate::error::Result;
use crate::source::Source;
use alloy_eips::eip7928::{AccountChanges, BlockAccessList};
use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::{Transaction, TransactionReceipt};
use mirror_anvil::{Key, Rpc, Value, Write};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// ERC20 `Transfer(address,address,uint256)`.
const TRANSFER_TOPIC: B256 =
    alloy_primitives::b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
/// ERC20 `transfer(address,uint256)` selector.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// Where a window's changes came from: one metric label, one log field.
///
/// `Bal` = `eth_getBlockAccessList`; `Trace` = the source's trace rung;
/// `Poll` = the envelope rung (touches + declared closures + reads);
/// `None` = nothing was composed (an empty window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum DiffKind {
    Bal,
    Trace,
    Poll,
    None,
}

impl DiffKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bal => "bal",
            Self::Trace => "trace",
            Self::Poll => "poll",
            Self::None => "none",
        }
    }
}

/// A window's changes: the BAL plus the window it covers.
#[derive(Debug, Clone)]
pub struct WindowBal {
    pub from: u64,
    pub to: u64,
    pub kind: DiffKind,
    pub bal: BlockAccessList,
}

impl WindowBal {
    /// The empty window (`to <= from`): the pipeline's no-op input.
    fn empty(from: u64, to: u64) -> Self {
        Self {
            from,
            to,
            kind: DiffKind::None,
            bal: Vec::new(),
        }
    }
}

/// What the composer saw (the venue's poll gauges; the mainnet does not
/// record them). `changed` counts the changes the BAL carries *before* the
/// COW filter.
#[derive(Debug, Clone, Default)]
pub struct ComposeStats {
    pub touched: usize,
    pub planned: usize,
    pub changed: usize,
    pub uncovered: Vec<Address>,
}

impl ComposeStats {
    /// Fold one block's stats into the window's (the per-block fallback path).
    fn absorb(&mut self, other: Self) {
        self.touched += other.touched;
        self.planned += other.planned;
        self.changed += other.changed;
        self.uncovered.extend(other.uncovered);
    }
}

/// A composed window: `(from, to]`. The mainnet composes one parent block at
/// a time ([`Window::single`]); the venue composes everything since its last
/// composed source block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from: u64,
    pub to: u64,
}

impl Window {
    pub fn new(from: u64, to: u64) -> Self {
        Self { from, to }
    }

    /// The one-block window `(n-1, n]` (the mainnet's per-parent apply).
    pub fn single(n: u64) -> Self {
        Self {
            from: n.saturating_sub(1),
            to: n,
        }
    }
}

/// A window's plan: the composed BAL after COW, lowered to the write set the
/// engines apply. One call for the whole shared pipeline half
/// (`compose → cow::filter_bal → flatten_bal`), so the two engines differ
/// only in what they do with it (journal/align/mine vs live parity).
pub struct WindowPlan {
    pub kind: DiffKind,
    /// The composed change count, *before* COW (the `BlockApplied` fact's
    /// `writes`).
    pub total: usize,
    /// The kept changes, still in BAL form (the live-parity check reads it).
    pub bal: BlockAccessList,
    /// The kept changes, flattened (what the engines apply and journal).
    pub writes: Vec<Write>,
    pub filter: crate::cow::FilterStats,
    pub compose: ComposeStats,
}

/// The composer: the source/target/scope triple every composition reads —
/// the family's shared shape, named once. [`Self::compose`] is the generic
/// entry (the rung follows the source's capabilities); [`Self::plan`] is the
/// shared pipeline half (compose → COW → flatten).
pub struct Composer<'a> {
    pub source: &'a dyn Source,
    pub target: &'a dyn Source,
    pub scope: &'a Scope,
}

impl<'a> Composer<'a> {
    /// The generic composer: a window's BAL, however the source can produce
    /// one.
    pub async fn compose(&self, window: Window) -> Result<(WindowBal, ComposeStats)> {
        let Window { from, to } = window;
        if to <= from {
            return Ok((WindowBal::empty(from, to), ComposeStats::default()));
        }
        if self.source.capabilities().native_bal {
            self.native_window(window).await
        } else {
            self.synth_window(window).await
        }
    }

    /// Rung 1: the source records BALs — fetch each block of the window and
    /// merge them. A block without one falls back to rung 2 for that block
    /// alone; the window's kind then names the rung that answered.
    async fn native_window(&self, window: Window) -> Result<(WindowBal, ComposeStats)> {
        let Window { from, to } = window;
        let mut acc: BTreeMap<Address, AccountChanges> = BTreeMap::new();
        let mut stats = ComposeStats::default();
        let mut kind = DiffKind::Bal;
        for n in (from + 1)..=to {
            if let Some(bal) = self.source.bal(n).await? {
                merge_into(&mut acc, bal);
            } else {
                let (bal, s) = self.synthesize(Window::new(n - 1, n)).await?;
                merge_into(&mut acc, bal);
                stats.absorb(s);
                kind = self.source.touched_kind();
            }
        }
        let bal: BlockAccessList = acc.into_values().collect();
        Ok((WindowBal { from, to, kind, bal }, stats))
    }

    /// Rung 2: the source reports the window's touches; the declared scope
    /// plans the keys and the differing reads become the BAL.
    async fn synth_window(&self, window: Window) -> Result<(WindowBal, ComposeStats)> {
        let (bal, stats) = self.synthesize(window).await?;
        Ok((
            WindowBal {
                from: window.from,
                to: window.to,
                kind: self.source.touched_kind(),
                bal,
            },
            stats,
        ))
    }

    /// The shared pipeline half: compose the window, COW it against the
    /// devnet's own writes, and lower it to the write set. The registry is
    /// locked only for the filter step (the compose awaits with the lock
    /// free).
    pub async fn plan(
        &self,
        shadow: &crate::shared::Shared<crate::shadow::Shadow>,
        window: Window,
    ) -> Result<WindowPlan> {
        let (composed, compose) = self.compose(window).await?;
        let total = change_count(&composed.bal);
        let (bal, filter) = {
            let shadow = shadow.lock()?;
            crate::cow::filter_bal(self.scope.watch(), &shadow, composed.bal)
        };
        let writes = mirror_anvil::flatten_bal(&bal);
        Ok(WindowPlan {
            kind: composed.kind,
            total,
            bal,
            writes,
            filter,
            compose,
        })
    }

    /// Rung 2's read side: touches → the declared scope's read plan →
    /// source/target reads → a BAL of the differing keys. Keys the touched
    /// set already carries values for (the trace rung) are not re-read.
    async fn synthesize(&self, window: Window) -> Result<(BlockAccessList, ComposeStats)> {
        let Window { from, to } = window;
        let touched = self.source.touched(from, to).await?;
        let plan = plan_reads(self.scope.closures(), self.scope.watch(), &touched);
        let valued: HashSet<Key> = touched.values.iter().map(Write::key).collect();
        let mut writes: Vec<Write> = touched.values.clone();
        // The read anchors: the source at the window's end (reth serves that
        // block; the public venue RPC serves its latest — the block parameter
        // is ignored, measured), the target at its own head (the devnet
        // chain's height is not the source's: a window end above it is out of
        // range).
        let target_at = self.target.head().await?;
        for key in &plan {
            if valued.contains(key) {
                continue;
            }
            if let Some(w) = self.diff_key(*key, (to, target_at)).await? {
                writes.push(w);
            }
        }
        let stats = ComposeStats {
            touched: touched.addresses.len(),
            planned: plan.len(),
            changed: writes.len(),
            uncovered: uncovered_emitters(self.scope, &plan, &touched),
        };
        Ok((mirror_anvil::bal_from_writes(&writes), stats))
    }

    /// One planned key's read-diff: the source's value at the window's end
    /// against the target's at its own head; a difference becomes a write
    /// (the value the source serves). Equal reads emit nothing.
    async fn diff_key(&self, key: Key, at: (u64, u64)) -> Result<Option<Write>> {
        let (source_at, target_at) = at;
        let source_value = self.source.read_key(key, source_at).await?;
        let target_value = self.target.read_key(key, target_at).await?;
        Ok(if source_value != target_value {
            Write::new(key, source_value)
        } else {
            None
        })
    }
}

/// Log emitters no closure covers and that are not watched accounts: a state
/// change the declared watch model cannot follow. The audit's audible form —
/// counted and warned by the engine, never silent.
fn uncovered_emitters(scope: &Scope, plan: &[Key], touched: &TouchedSet) -> Vec<Address> {
    let planned: HashSet<Address> = plan.iter().map(Key::address).collect();
    let mut uncovered: BTreeSet<Address> = BTreeSet::new();
    for log in &touched.logs {
        if !scope.watch().accounts.contains(&log.address) && !planned.contains(&log.address) {
            uncovered.insert(log.address);
        }
    }
    uncovered.into_iter().collect()
}

/// Merge one block's BAL into the window accumulator (last change wins per
/// key: the post-state readers take the last entry).
fn merge_into(acc: &mut BTreeMap<Address, AccountChanges>, incoming: BlockAccessList) {
    for acct in incoming {
        match acc.entry(acct.address) {
            Entry::Vacant(v) => {
                v.insert(acct);
            }
            Entry::Occupied(mut o) => o.get_mut().merge(acct),
        }
    }
}

/// The number of change entries a BAL carries (what `flatten_bal` emits).
pub fn change_count(bal: &BlockAccessList) -> usize {
    bal.iter().map(changes_in).sum()
}

/// One account's change entries.
pub fn changes_in(acct: &AccountChanges) -> usize {
    acct.storage_post_states().count()
        + usize::from(acct.balance_post_state().is_some())
        + usize::from(acct.nonce_post_state().is_some())
        + usize::from(acct.code_post_state().is_some())
}

/// The envelope rung's touches: the window's txs, receipts and — when the
/// source declares them — HyperEVM's system txs, folded into a touched set.
///
/// The public venue RPC serves no writes, so the touches are what it can
/// enumerate; the declared closures turn them into concrete keys. A chain
/// without `eth_getSystemTxsByBlockNumber` answers an empty list (the client's
/// contract), so the system-tx fold is safe everywhere.
pub async fn envelope_touches(
    rpc: &Rpc,
    from: u64,
    to: u64,
    system_txs: bool,
) -> mirror_anvil::Result<TouchedSet> {
    let mut touched = TouchedSet::default();
    for n in (from + 1)..=to {
        let block = rpc.block_by_number(n, true).await?;
        let txs = crate::scan::block_txs(&block);
        let receipts = rpc.receipts(n).await?;
        let system = if system_txs {
            rpc.system_txs(n).await?
        } else {
            Vec::new()
        };
        absorb_envelope(&mut touched, txs, &receipts, &system);
    }
    Ok(touched)
}

/// Fold one block's envelope into the touched set. Pure (no IO): the tests
/// drive it with the wire shapes the RPCs actually return.
///
/// Scope rule (deliberate): **addresses** collects tx senders/recipients and
/// value participants — the accounts whose *account fields* (balance, nonce,
/// code) a poll reads. Log **topics** are carried separately for the mapping
/// closures (a holder's balance slot is derived from the topic, not from the
/// account list), so an indexed holder does not enter `addresses`.
///
/// HyperEVM's system txs (measured 2026-10-24: `transfer` calldata, gasPrice
/// 0, **no receipt** — `eth_getTransactionReceipt` is null and
/// `eth_getBlockReceipts` omits them) are reconstructed as the `Transfer` log
/// the chain emits: topic1 = the system sender, topic2 = the recipient.
pub fn absorb_envelope(
    touched: &mut TouchedSet,
    txs: &[Transaction],
    receipts: &[TransactionReceipt],
    system_txs: &[Transaction],
) {
    use alloy_consensus::Transaction as _;
    use alloy_network_primitives::TransactionResponse as _;
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

/// Post-write live parity: re-read every change in the BAL from the target and
/// compare it with the value the composition expected. The venue's probe
/// schedule (`ProbeSchedule::LiveParity`); the mainnet's per-block parity is
/// its own sampled check.
pub async fn verify(target: &dyn Source, bal: &BlockAccessList) -> Result<(usize, usize)> {
    let mut ok = 0usize;
    let mut failed = 0usize;
    let at = target.head().await?;
    for acct in bal {
        for (slot, value) in acct.storage_post_states() {
            let key = Key::Storage {
                address: acct.address,
                slot,
            };
            let (o, f) = check(target, at, key, Value::Storage(value)).await?;
            ok += o;
            failed += f;
        }
        if let Some(value) = acct.balance_post_state() {
            let (o, f) = check(target, at, Key::Balance(acct.address), Value::Balance(value)).await?;
            ok += o;
            failed += f;
        }
        if let Some(value) = acct.nonce_post_state() {
            let (o, f) = check(target, at, Key::Nonce(acct.address), Value::Nonce(value)).await?;
            ok += o;
            failed += f;
        }
        if let Some(value) = acct.code_post_state() {
            let (o, f) = check(target, at, Key::Code(acct.address), Value::Code(value.clone())).await?;
            ok += o;
            failed += f;
        }
    }
    Ok((ok, failed))
}

/// One change's post-condition: the target must read back the expected value.
async fn check(target: &dyn Source, at: u64, key: Key, expected: Value) -> Result<(usize, usize)> {
    let got = target.read_key(key, at).await?;
    if got == expected {
        Ok((1, 0))
    } else {
        tracing::warn!(key = ?key, expected = ?expected, found = ?got, "live parity: an applied key did not read back");
        Ok((0, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::closure::TopicMappedSlots;
    use crate::cow::WatchSet;
    use crate::source::Capabilities;
    use alloy_primitives::{address, Bytes, U256};
    use std::collections::HashMap;

    fn tx(value: serde_json::Value) -> Transaction {
        serde_json::from_value(value).expect("transaction wire shape")
    }

    fn receipt(emitter: Address, topics: &[B256]) -> TransactionReceipt {
        serde_json::from_value(serde_json::json!({
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
        tx(serde_json::json!({
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
            "input": Bytes::from(input),
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

        let closures: Vec<Box<dyn crate::closure::KeyClosure>> = vec![Box::new(TopicMappedSlots {
            contract: token,
            base_slot: U256::from(3),
            topic_index: 2, // Transfer(address indexed from, address indexed to, uint256)
        })];
        let plan = crate::closure::plan_reads(&closures, &WatchSet::default(), &touched);
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0],
            Key::Storage {
                address: token,
                slot: crate::closure::mapping_slot(
                    B256::left_padding_from(recipient.as_slice()),
                    U256::from(3),
                ),
            }
        );
    }

    /// A test source: canned capabilities, touches, values, and read answers.
    struct Fake {
        caps: Capabilities,
        bal: Option<BlockAccessList>,
        touched: TouchedSet,
        reads: HashMap<Key, Value>,
        read_keys: std::sync::Mutex<Vec<Key>>,
    }

    impl Source for Fake {
        fn capabilities(&self) -> Capabilities {
            self.caps
        }
        fn head(&self) -> futures_util::future::BoxFuture<'_, mirror_anvil::Result<u64>> {
            Box::pin(async { Ok(10) })
        }
        fn header(
            &self,
            _n: u64,
        ) -> futures_util::future::BoxFuture<'_, mirror_anvil::Result<alloy_rpc_types_eth::Header>> {
            Box::pin(async { Err(mirror_anvil::Error::Corrupt { what: "fake", detail: "header not used".into() }) })
        }
        fn balance_at(
            &self,
            _address: Address,
            _block: u64,
        ) -> futures_util::future::BoxFuture<'_, mirror_anvil::Result<U256>> {
            Box::pin(async { Ok(U256::ZERO) })
        }
        fn nonce_at(
            &self,
            _address: Address,
            _block: u64,
        ) -> futures_util::future::BoxFuture<'_, mirror_anvil::Result<u64>> {
            Box::pin(async { Ok(0) })
        }
        fn storage_at(
            &self,
            _address: Address,
            _slot: U256,
            _block: u64,
        ) -> futures_util::future::BoxFuture<'_, mirror_anvil::Result<B256>> {
            Box::pin(async { Ok(B256::ZERO) })
        }
        fn code_at(
            &self,
            _address: Address,
            _block: u64,
        ) -> futures_util::future::BoxFuture<'_, mirror_anvil::Result<Bytes>> {
            Box::pin(async { Ok(Bytes::new()) })
        }
        fn read_key(
            &self,
            key: Key,
            _block: u64,
        ) -> futures_util::future::BoxFuture<'_, mirror_anvil::Result<Value>> {
            self.read_keys.lock().unwrap().push(key);
            let value = self.reads.get(&key).cloned();
            Box::pin(async move { Ok(value.unwrap_or(Value::Balance(U256::ZERO))) })
        }
        fn bal(
            &self,
            _n: u64,
        ) -> futures_util::future::BoxFuture<'_, mirror_anvil::Result<Option<BlockAccessList>>> {
            let bal = self.bal.clone();
            Box::pin(async move { Ok(bal) })
        }
        fn touched(
            &self,
            _from: u64,
            _to: u64,
        ) -> futures_util::future::BoxFuture<'_, mirror_anvil::Result<TouchedSet>> {
            let touched = self.touched.clone();
            Box::pin(async move { Ok(touched) })
        }
        fn touched_kind(&self) -> DiffKind {
            DiffKind::Poll
        }
    }

    fn caps(native_bal: bool) -> Capabilities {
        Capabilities {
            native_bal,
            traces: false,
            historical: false,
            heads_ws: false,
            system_txs: false,
            system_tx_receipts: false,
        }
    }

    /// Rung 1: a native BAL is taken as-is; no reads happen.
    #[test]
    fn compose_prefers_the_native_rung() {
        let acct = address!("00000000000000000000000000000000000000ee");
        let native: BlockAccessList = vec![balance_change(acct, 99)];
        let (source, target) = bal_pair(native.clone());
        let scope = Scope::default();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (window, stats) = rt
            .block_on(
                Composer {
                    source: &source,
                    target: &target,
                    scope: &scope,
                }
                .compose(Window::new(0, 1)),
            )
            .unwrap();
        assert_eq!(window.kind, DiffKind::Bal);
        assert_eq!(window.bal, native);
        assert_eq!(stats.changed, 0);
        assert!(source.read_keys.lock().unwrap().is_empty());
    }

    /// Rung 2: touches + the declared scope plan keys; each is read on both
    /// sides and only the differing one becomes a change.
    #[test]
    fn compose_synthesizes_and_diffs_both_sides() {
        let watched = address!("00000000000000000000000000000000000000ff");
        let mut touched = TouchedSet::default();
        touched.touch(watched);
        let mut source_reads = HashMap::new();
        source_reads.insert(Key::Balance(watched), Value::Balance(U256::from(500)));
        let mut target_reads = HashMap::new();
        target_reads.insert(Key::Balance(watched), Value::Balance(U256::from(400)));
        let source = Fake {
            caps: caps(false),
            bal: None,
            touched,
            reads: source_reads,
            read_keys: std::sync::Mutex::new(Vec::new()),
        };
        let target = Fake {
            caps: caps(false),
            bal: None,
            touched: TouchedSet::default(),
            reads: target_reads,
            read_keys: std::sync::Mutex::new(Vec::new()),
        };
        let scope = Scope::new(WatchSet::new([watched].into_iter().collect()), Vec::new());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (window, stats) = rt
            .block_on(
                Composer {
                    source: &source,
                    target: &target,
                    scope: &scope,
                }
                .compose(Window::new(0, 1)),
            )
            .unwrap();
        assert_eq!(window.kind, DiffKind::Poll);
        assert_eq!(stats.touched, 1);
        assert_eq!(stats.planned, 3, "balance + nonce + code for the watched account");
        assert_eq!(stats.changed, 1, "only the balance differs");
        let writes = mirror_anvil::flatten_bal(&window.bal);
        assert_eq!(writes.len(), 1);
        assert!(matches!(writes[0], Write::Balance { address, value } if address == watched && value == U256::from(500)));
    }

    /// The trace rung's values are not re-read: the plan skips valued keys.
    #[test]
    fn compose_does_not_re_read_keys_the_source_already_values() {
        let watched = address!("00000000000000000000000000000000000000aa");
        let mut touched = TouchedSet::default();
        touched.touch(watched);
        touched.value(Write::Balance {
            address: watched,
            value: U256::from(7),
        });
        let source = Fake {
            caps: caps(false),
            bal: None,
            touched,
            reads: HashMap::new(),
            read_keys: std::sync::Mutex::new(Vec::new()),
        };
        let target = Fake {
            caps: caps(false),
            bal: None,
            touched: TouchedSet::default(),
            reads: HashMap::new(),
            read_keys: std::sync::Mutex::new(Vec::new()),
        };
        let scope = Scope::new(WatchSet::new([watched].into_iter().collect()), Vec::new());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (window, _) = rt
            .block_on(
                Composer {
                    source: &source,
                    target: &target,
                    scope: &scope,
                }
                .compose(Window::new(0, 1)),
            )
            .unwrap();
        let writes = mirror_anvil::flatten_bal(&window.bal);
        assert_eq!(writes.len(), 1);
        assert!(matches!(writes[0], Write::Balance { value, .. } if value == U256::from(7)));
        // The valued balance was skipped; nonce/code were still planned (the
        // address is a touch), but the zero-equals-zero answers emitted
        // nothing.
        let reads = source.read_keys.lock().unwrap();
        assert!(!reads.contains(&Key::Balance(watched)), "valued keys are not re-read");
    }

    /// One balance change, framed the way the composer frames them.
    fn balance_change(acct: Address, value: u64) -> AccountChanges {
        AccountChanges::new(acct).with_balance_change(alloy_eips::eip7928::BalanceChange::new(
            alloy_eips::eip7928::BlockAccessIndex::new(1),
            U256::from(value),
        ))
    }

    /// A native-BAL source and an empty target (the rung-1 test pair).
    fn bal_pair(native: BlockAccessList) -> (Fake, Fake) {
        (
            Fake {
                caps: caps(true),
                bal: Some(native),
                touched: TouchedSet::default(),
                reads: HashMap::new(),
                read_keys: std::sync::Mutex::new(Vec::new()),
            },
            Fake {
                caps: caps(false),
                bal: None,
                touched: TouchedSet::default(),
                reads: HashMap::new(),
                read_keys: std::sync::Mutex::new(Vec::new()),
            },
        )
    }

    /// The shared pipeline half in one call: compose → COW → flatten. A
    /// devnet-owned account's change is skipped; the kept ones land in both
    /// the filtered BAL (the live-parity read) and the write set (the apply).
    #[test]
    fn plan_composes_filters_and_flattens() {
        let owned = address!("00000000000000000000000000000000000000b1");
        let free = address!("00000000000000000000000000000000000000b2");
        let (source, target) = bal_pair(vec![balance_change(owned, 1), balance_change(free, 2)]);
        let scope = Scope::new(WatchSet::new([owned, free].into_iter().collect()), Vec::new());
        let shadow = crate::shared::Shared::new("test shadow", {
            let mut s = crate::shadow::Shadow::default();
            s.mark_account_fields(owned);
            s
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let plan = rt
            .block_on(
                Composer {
                    source: &source,
                    target: &target,
                    scope: &scope,
                }
                .plan(&shadow, Window::single(1)),
            )
            .unwrap();
        assert_eq!(plan.kind, DiffKind::Bal);
        assert_eq!(plan.total, 2, "the pre-COW change count");
        assert_eq!(plan.filter.skipped_shadowed, 1, "the owned account's fields");
        assert_eq!(plan.writes.len(), 1);
        assert!(
            matches!(plan.writes[0], Write::Balance { address, .. } if address == free),
            "the kept write is the free account's"
        );
        assert_eq!(
            mirror_anvil::flatten_bal(&plan.bal),
            plan.writes,
            "the filtered BAL and the write set agree"
        );
    }
}
