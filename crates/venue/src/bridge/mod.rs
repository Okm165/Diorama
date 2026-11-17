//! The venue bridge: V's Core↔EVM boundary, mirrored.
//!
//! V is a fork of the real HyperEVM, so the *contracts* at the boundary are
//! real (CoreWriter `0x3333…`, the system addresses) but HyperCore is not
//! there to act on them. This bridge is the missing half:
//!
//! - **EVM→Core**: an ERC20 transfer to a token's system address credits the
//!   sender's Core spot balance (from the `Transfer` log — the docs' exact
//!   rule);
//! - **CoreWriter actions**: every call to `0x3333…3333` (top-level *and*
//!   contract-internal, via the callTracer — a contract's action is
//!   attributed to the contract) decodes to a typed action; the sim applies
//!   the ones it models (limit orders, cancels by oid and cloid, USD class
//!   transfer, spot send, sendAsset, add API wallet, set abstraction), counts
//!   the rest;
//! - **Core→EVM**: a `sendAsset` naming a token's system address is completed
//!   the way the chain does it — a `transfer(recipient, amount)` call on the
//!   linked ERC20 **from** the system address (V runs auto-impersonate).
//!
//! The scan is anchored like the driver's: blocks at or below the fork pin
//! are the *source's* real blocks and are never scanned (their transfers are
//! not the devnet's); a re-pin re-anchors the cursor to the new pin.
pub(crate) mod corewriter;
pub(crate) mod metrics;
pub(crate) mod system;

use crate::error::{Error, Result};
use crate::feed::MarketFeed;
use crate::sim::order::CancelSpeed;
use crate::sim::{Applied, ClassTransfer, Direction};
use crate::sim::{Sim, TokenRef};
use crate::wire::UserAbstraction;
use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, U256};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_rpc_types_trace::geth::{CallFrame, GethTrace, TraceResult};
use corewriter::{CoreAction, CORE_WRITER};
use diorama_anvil::Rpc;
use metrics::BridgeMetrics;
use rust_decimal::Decimal;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use system::{TokenInfo, Transfer};

/// The bridge's declared deployment policy: the scan cadence and the devnet
/// gas stipend for the Core↔EVM system address.
#[derive(Debug, Clone, Copy)]
pub struct BridgePolicy {
    pub poll: Duration,
    pub system_gas_stipend: U256,
}

/// The bridge's own cursor and the persisted state it needs. The cursor lives
/// in the sim's state file (one state root per service).
pub struct Bridge {
    target_rpc: Rpc,
    sim: Arc<Sim>,
    feed: Arc<MarketFeed>,
    /// The scan cadence (the deployment's `bridge_poll_interval_ms`).
    poll: Duration,
    /// The devnet gas stipend for the Core↔EVM system address.
    system_gas_stipend: U256,
    /// The last head whose timestamp was observed (one header read per new
    /// block; the sim's served `time` follows the venue chain's clock).
    observed_head: std::sync::atomic::AtomicU64,
    /// The last scanned V block (0 = not anchored yet); persisted in the
    /// sim's state so a restart does not lose the boundary activity mined
    /// meanwhile.
    cursor: std::sync::atomic::AtomicU64,
    /// The last figures each account was projected with (spot total, spot
    /// hold, perp withdrawable): the projection writes only on change, so a
    /// quiet book costs no tx.
    projected: std::sync::Mutex<std::collections::HashMap<Address, [u64; 3]>>,
    /// The reason each account's figures could not be computed, keyed by
    /// address. A projection refusal is a **state**, not an event: a fixture
    /// account holding a market the served meta does not carry is refused on
    /// every pass (measured: ~1/s for `xyz:NVDA`), so the log fires on the
    /// transition into and out of the refusal, not on each retry.
    refusals: std::sync::Mutex<std::collections::HashMap<Address, String>>,
    pub metrics: Arc<BridgeMetrics>,
}

impl Bridge {
    pub fn new(
        target_rpc: Rpc,
        sim: Arc<Sim>,
        feed: Arc<MarketFeed>,
        metrics: Arc<BridgeMetrics>,
        policy: BridgePolicy,
    ) -> Result<Self> {
        // A poisoned sim state must not read as "not anchored": that would
        // re-anchor at the head and skip the boundary activity mined while
        // the service was down.
        let cursor = sim.bridge_cursor()?;
        Ok(Self {
            target_rpc,
            sim,
            feed,
            poll: policy.poll,
            system_gas_stipend: policy.system_gas_stipend,
            observed_head: std::sync::atomic::AtomicU64::new(0),
            cursor: std::sync::atomic::AtomicU64::new(cursor),
            projected: std::sync::Mutex::new(std::collections::HashMap::new()),
            refusals: std::sync::Mutex::new(std::collections::HashMap::new()),
            metrics,
        })
    }

    /// Run forever: one scan pass per configured interval.
    pub async fn run(self) {
        loop {
            if let Err(e) = self.pass().await {
                tracing::warn!(error = %e, "venue bridge pass failed; will retry");
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// One pass: anchor (pin-aware), then scan every new block.
    pub async fn pass(&self) -> Result<()> {
        let head = self.target_rpc.block_number().await?;
        self.observe_head_time(head).await?;
        let pin = self
            .target_rpc
            .fork_config()
            .await?
            .map(|fork| fork.block)
            .unwrap_or(0);
        let mut from = self.cursor.load(Ordering::Relaxed);
        if from == 0 || from < pin || from > head {
            // First pass, a re-pin, or a chain reset under us: anchor at the
            // devnet's own range (never scan the source's blocks).
            let anchor = head.max(pin);
            from = anchor;
            self.cursor.store(anchor, Ordering::Relaxed);
            self.sim.set_bridge_cursor(anchor)?;
            tracing::info!(block = anchor, pin, head, "venue bridge anchored");
        }
        for n in (from + 1)..=head {
            self.scan_block(n).await?;
            self.cursor.store(n, Ordering::Relaxed);
            self.sim.set_bridge_cursor(n)?;
            // One atomic write per block: the cursor and the block's effects
            // land together, so a crash mid-block re-scans instead of
            // half-applying (a duplicate credit is a silent balance lie).
            self.sim.save_if_dirty()?;
        }
        // The Core-read precompile mocks are projections of the sim. Every
        // block's effects are now durable in the sim, so this is the one
        // point the projection is refreshed from. The pass's own head is the
        // block the projection's presence checks read at — a pinned read, not
        // a genesis read (see the gate's note).
        self.project_precompiles(head).await?;
        self.metrics
            .set_lag_blocks(head.saturating_sub(self.cursor.load(Ordering::Relaxed)));
        Ok(())
    }

    /// Project the sim's Core figures onto the mirror's read-precompile mocks
    /// (`0x0801` spot balance, `0x0803` perp withdrawable).
    ///
    /// **Why.** The mocks are separate Solidity ledgers that nothing wrote:
    /// a draw that credited the sim's spot class was invisible to
    /// `freeSpotBalance()`, the owner's recall reverted `ZeroAmount`, the
    /// close stayed `RecallPending` forever, and a healthy matured book could
    /// never be settled (measured 2026-11-12: 67.892481 USDC credited in the
    /// sim and served by the gateway, zero reported by the mock). The sim is
    /// the single source of truth; the mocks are its write-through
    /// projection, refreshed here once per pass after the block's effects are
    /// durable.
    ///
    /// The figures are the ones the bridge's own Core→EVM clamp uses
    /// (`available_after_maintenance`): `hold = total − available`, so
    /// `freeSpotBalance()` reports exactly what the venue will release and a
    /// recall cannot over-recall into a bridge refusal. A production-like
    /// chain (no mock code) is a no-op: the real precompiles serve the reads.
    async fn project_precompiles(&self, head: u64) -> Result<()> {
        let Some(spot) = self.feed.spot_meta() else {
            return Ok(()); // the feed is warming; the pass already refused the block
        };
        let tokens = system::token_registry(&spot);
        let Some(usdc) = tokens.iter().find(|t| t.name == "USDC") else {
            tracing::warn!("bridge: no USDC in the spot registry; the precompile projection is skipped this pass");
            return Ok(());
        };
        // **The mocks' presence is read at the pass's head, never at block 0.**
        // The first cut read genesis, where a forked anvil has no mock code —
        // the deployment's mocks are installed after the fork pin — so the
        // gate read "not installed" every pass and the projection silently
        // no-op'd: no writer funding, no mock write, and the exact W-B1
        // symptom (mock reports 0 while the sim holds the balance) survived
        // its own fix. Measured live 2026-11-12: writer nonce 0 with 54
        // simulated accounts holding spot USDC, until the read moved to the
        // pass's anchored head.
        let spot_installed = !self
            .target_rpc
            .code_at(system::SPOT_BALANCE_MOCK, head)
            .await?
            .is_empty();
        let withdrawable_installed = !self
            .target_rpc
            .code_at(system::WITHDRAWABLE_MOCK, head)
            .await?
            .is_empty();
        if !spot_installed && !withdrawable_installed {
            return Ok(());
        }
        let meta = self.feed.margin_meta();
        let changed = self.changed_figures(usdc, meta.as_ref(), head).await?;
        if changed.is_empty() {
            return Ok(());
        }
        if self
            .target_rpc
            .balance_at(system::PRECOMPILE_WRITER, head)
            .await?
            .is_zero()
        {
            self.target_rpc
                .anvil_set_balance(system::PRECOMPILE_WRITER, self.system_gas_stipend)
                .await?;
        }
        for (address, figures) in changed {
            self.write_projection(
                address,
                figures,
                u64::from(usdc.index),
                spot_installed,
                withdrawable_installed,
            )
            .await?;
        }
        Ok(())
    }

    /// One account's mock writes plus the cache advance. The cache moves only
    /// after both sends ack (a faulted write retries the whole account next
    /// pass), so the projection never claims a write the chain refused.
    #[allow(clippy::too_many_arguments)]
    async fn write_projection(
        &self,
        address: Address,
        figures: [u64; 3],
        token_index: u64,
        spot_installed: bool,
        withdrawable_installed: bool,
    ) -> Result<()> {
        let [total, hold, withdrawable] = figures;
        if spot_installed {
            self.send_projection(
                system::SPOT_BALANCE_MOCK,
                system::set_balance_calldata(address, token_index, total, hold),
            )
            .await?;
        }
        if withdrawable_installed {
            self.send_projection(
                system::WITHDRAWABLE_MOCK,
                system::set_withdrawable_calldata(address, withdrawable),
            )
            .await?;
        }
        self.projected
            .lock()
            .expect("bridge projection cache")
            .insert(address, figures);
        tracing::info!(
            account = %address,
            spot_total_wei8 = total,
            spot_hold_wei8 = hold,
            withdrawable_usd6 = withdrawable,
            "bridge: precompile projection written"
        );
        Ok(())
    }

    /// One mock write, sent like the Core→EVM completion: a funded plain tx
    /// waited to its receipt, so a REVERTED write is an error here and the
    /// projection cache is not advanced past it. (The hash alone proves
    /// admission: a reverted tx still mines with status 0x0 — measured on
    /// the feed bridge, which re-published for hours on a reverting write.)
    async fn send_projection(&self, mock: Address, data: alloy_primitives::Bytes) -> Result<()> {
        let tx = TransactionRequest::default()
            .from(system::PRECOMPILE_WRITER)
            .to(mock)
            .input(data.into());
        self.target_rpc.send_transaction_confirmed(tx).await?;
        tracing::debug!(mock = %mock, "bridge: precompile mock write confirmed");
        Ok(())
    }

    /// The accounts whose projected figures changed this pass: every sim
    /// account is re-read (outside the projection lock — the lock guards only
    /// the diff write-back, so a projection tx that faults retries next pass)
    /// and compared against the cache.
    async fn changed_figures(
        &self,
        usdc: &system::TokenInfo,
        meta: Option<&crate::wire::Meta>,
        _head: u64,
    ) -> Result<Vec<(Address, [u64; 3])>> {
        let mut current = std::collections::HashMap::new();
        for address in self.sim.account_addresses()? {
            match self.core_figures(address, usdc, meta) {
                Ok(figures) => {
                    current.insert(address, figures);
                    if self
                        .refusals
                        .lock()
                        .expect("bridge refusal cache")
                        .remove(&address)
                        .is_some()
                    {
                        tracing::info!(
                            account = %address,
                            "bridge: precompile projection figures available again"
                        );
                    }
                }
                Err(e) => self.note_refusal(address, &e.to_string()),
            }
        }
        let projected = self.projected.lock().expect("bridge projection cache");
        Ok(current
            .into_iter()
            .filter(|(address, figures)| projected.get(address) != Some(figures))
            .collect())
    }

    /// One account's unpriceable figures, logged once per distinct cause.
    ///
    /// The account stays out of this pass (a projection that omits an
    /// unpriceable leg would understate maintenance), but a permanently
    /// unpriceable fixture must not read as a 1 Hz fault: the reason is a
    /// state, so the log fires on the transition into and out of it.
    fn note_refusal(&self, address: Address, reason: &str) {
        self.metrics.inc_refusals();
        let first = {
            let mut refusals = self.refusals.lock().expect("bridge refusal cache");
            match refusals.get(&address) {
                Some(known) if known == reason => false,
                _ => {
                    refusals.insert(address, reason.to_string());
                    true
                }
            }
        };
        if first {
            tracing::warn!(
                account = %address,
                error = %reason,
                "bridge: precompile projection figures unavailable; the account is not projected \
                 until the cause clears"
            );
        } else {
            tracing::debug!(
                account = %address,
                error = %reason,
                "bridge: precompile projection figures still unavailable"
            );
        }
    }

    /// One account's projected figures, in the units the mocks store:
    /// `[spot total (core wei), spot hold (core wei), perp withdrawable (USD
    /// 1e6)]`. Zeros for an account the sim no longer holds, so the projection
    /// can never report a balance the venue does not.
    fn core_figures(
        &self,
        address: Address,
        token: &system::TokenInfo,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<[u64; 3]> {
        let Some(account) = self.sim.account(address)? else {
            return Ok([0; 3]);
        };
        let entry = account.spot.get(&token.index);
        let total = entry.map_or(Decimal::ZERO, |e| e.total);
        let hold =
            match crate::sim::account::available_after_maintenance(&account, &self.feed, meta)?
                .into_iter()
                .find(|(t, _)| *t == token.index)
                .map(|(_, available)| available)
            {
                Some(available) if available < total => total - available,
                _ => entry.map_or(Decimal::ZERO, |e| e.hold),
            };
        let view = crate::sim::account::view(&account, &self.feed, meta)?;
        Ok([
            core_units(total, token.wei_decimals)?,
            core_units(hold, token.wei_decimals)?,
            core_units(view.withdrawable, system::PERP_USD_DECIMALS)?,
        ])
    }

    /// Keep the sim's served `time` on the venue chain's clock: one header
    /// read per new head (their harness jumps every chain forward; a venue
    /// read must not look stale afterwards).
    async fn observe_head_time(&self, head: u64) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self.observed_head.load(Ordering::Relaxed) == head {
            return Ok(());
        }
        let block = self
            .target_rpc
            .block_by_number(head, diorama_anvil::Txs::Hashes)
            .await?;
        self.sim
            .observe_chain_time(block.header.timestamp.saturating_mul(1_000));
        self.observed_head.store(head, Ordering::Relaxed);
        Ok(())
    }

    /// Decode one block's boundary activity and apply it.
    async fn scan_block(&self, n: u64) -> Result<()> {
        let block = self
            .target_rpc
            .block_by_number(n, diorama_anvil::Txs::Full)
            .await?;
        if block.transactions.is_empty() {
            return Ok(());
        }
        let Some(spot) = self.feed.spot_meta() else {
            tracing::warn!(
                block = n,
                "bridge has no spotMeta yet; the block is re-scanned"
            );
            return Err(Error::Warming);
        };
        let tokens = system::token_registry(&spot);

        // The docs' pre-existence rule (interaction timings): the snapshot is
        // taken *before* this block's credits, so an EVM→Core transfer that
        // initializes an account in the same block still cannot act.
        let pre_block = self.sim.account_addresses()?;

        // EVM→Core: the docs' intra-block step 3.
        self.apply_credits(n, &tokens).await?;

        // CoreWriter: top-level txs plus contract-internal calls. The Core
        // sorts a block's actions by category before applying them (docs:
        // non-order actions, then cancels, then actions that send Gtc or Ioc
        // orders; within a category, the proposed order) — the bridge
        // reproduces that order. The consensus batches inside a block are not
        // visible from the EVM side; one batch per block is the usual case.
        let mut calls = self.corewriter_calls(n, &block).await?;
        calls.sort_by_key(|(_, action)| action_rank(action));
        let (calls, refused) = callers_eligible(&pre_block, calls);
        if refused > 0 {
            self.metrics.inc_refusals_by(refused as u64);
            tracing::warn!(
                block = n,
                refused,
                "CoreWriter actions refused: the account must exist on HyperCore before the block (docs: interaction timings)"
            );
        }
        for (caller, action) in calls {
            self.apply_action(caller, action, &tokens).await?;
        }
        Ok(())
    }

    /// The block's EVM→Core side (the docs' intra-block step 3: "EVM -> Core
    /// transfers are processed" before the CoreWriter actions): HYPE's system
    /// contract (`Received`) and every token's system-address transfers,
    /// from the receipts' logs.
    async fn apply_credits(&self, n: u64, tokens: &[TokenInfo]) -> Result<()> {
        let receipts = self.target_rpc.receipts(n).await?;
        for receipt in &receipts {
            // The logs decoded once, with their emitters, so a helper-hop
            // credit can look back at the same transaction's funding leg.
            let logs: Vec<(Address, &alloy_primitives::LogData)> = receipt
                .logs()
                .iter()
                .map(|log| (log.address(), log.data()))
                .collect();
            for (index, log) in receipt.logs().iter().enumerate() {
                // HYPE's boundary is the system contract's `Received` event
                // (docs): a tx value sent to `0x2222…2` credits Core HYPE.
                if log.address() == system::HYPE_SYSTEM {
                    self.apply_hype_credit(log.data(), tokens)?;
                    continue;
                }
                let Some(transfer) = system::transfer_of(log.data()) else {
                    continue;
                };
                self.apply_credit(&transfer, log.address(), &logs[..index], tokens)
                    .await?;
            }
        }
        Ok(())
    }

    /// Every CoreWriter call in the block with its Core account (the calling
    /// address). The callTracer includes the top-level frames, so traces are
    /// the single source when available; a trace fault degrades to top-level
    /// txs only (counted — contract-internal calls are missed that block).
    async fn corewriter_calls(
        &self,
        n: u64,
        block: &alloy_rpc_types_eth::Block,
    ) -> Result<Vec<(Address, CoreAction)>> {
        match self.target_rpc.trace_block_calls(n).await {
            Ok(traces) => {
                let mut out = Vec::new();
                for trace in traces {
                    collect_internal(&trace, &mut out);
                }
                Ok(out)
            }
            Err(e) => {
                self.metrics.inc_trace_faults();
                tracing::warn!(block = n, error = %e, "callTracer unavailable; contract-internal CoreWriter calls are not seen this block");
                let mut out = Vec::new();
                for tx in block.transactions.txns() {
                    if tx.to() == Some(CORE_WRITER) {
                        out.push((tx.inner.signer(), corewriter::decode_calldata(tx.input())));
                    }
                }
                Ok(out)
            }
        }
    }

    // TRUST: value in = value recorded (token-admission-policy). The guards:
    // a Transfer log from the token's linked EVM contract credits its sender;
    // an entry point's own self-transfer is credited to the same-transaction
    // funding depositor; the Core amount is the log's exact value converted by
    // the pinned decimals.
    /// EVM→Core credit from one transfer log (the linked token's own transfer,
    /// or an entry-point self-transfer with a same-transaction funding leg).
    async fn apply_credit(
        &self,
        transfer: &Transfer,
        emitter: Address,
        prior: &[(Address, &alloy_primitives::LogData)],
        tokens: &[TokenInfo],
    ) -> Result<()> {
        // The linked token's own log is the ordinary deposit. When a bridge
        // helper moves the tokens on, the helper's self-transfer is resolved
        // back to the depositor that funded it earlier in the same
        // transaction. Both rules refuse a self-transfer that no funding leg
        // backs; the linked token's own self-transfer is never credited (the
        // measured 2026-10-31 USDC deposit credited the token contract
        // itself).
        let credit = system::credit_of(transfer, tokens, emitter)
            .or_else(|| system::hop_credit_of(transfer, prior, tokens, emitter));
        let Some(credit) = credit else {
            if transfer.from == emitter {
                tracing::warn!(
                    from = %transfer.from,
                    to = %transfer.to,
                    value = %transfer.value,
                    "bridge: EVM→Core transfer originates at its emitter, and no earlier same-transaction transfer into the emitter funds it — refusing to credit"
                );
            }
            return Ok(());
        };
        self.sim.credit_spot(
            credit.sender,
            TokenRef {
                index: credit.token.index,
                coin: &credit.token.name,
            },
            credit.amount,
        )?;
        self.metrics.inc_credits();
        tracing::info!(
            from = %credit.sender,
            token = %credit.token.name,
            amount = %credit.amount,
            "bridge: EVM→Core credit"
        );
        Ok(())
    }

    /// The EVM→Core HYPE credit (the system contract's `Received` event).
    /// The emitter check is the address itself: only the real system contract
    /// can emit a credit, never a look-alike.
    fn apply_hype_credit(
        &self,
        data: &alloy_primitives::LogData,
        tokens: &[TokenInfo],
    ) -> Result<()> {
        let Some(token) = tokens.iter().find(|t| t.name == "HYPE") else {
            return Ok(());
        };
        let Some(credit) = system::hype_received_of(data, token) else {
            return Ok(());
        };
        self.sim.credit_spot(
            credit.sender,
            TokenRef {
                index: token.index,
                coin: &token.name,
            },
            credit.amount,
        )?;
        self.metrics.inc_credits();
        tracing::info!(from = %credit.sender, amount = %credit.amount, "bridge: EVM→Core HYPE credit");
        Ok(())
    }

    /// Apply one decoded action to the sim.
    async fn apply_action(
        &self,
        caller: Address,
        action: CoreAction,
        tokens: &[TokenInfo],
    ) -> Result<()> {
        self.metrics.count_action(action.id());
        match action {
            CoreAction::UsdClassTransfer { ntl, direction } => {
                self.usd_class_transfer(caller, ntl, direction, tokens)?;
            }
            CoreAction::SpotSend {
                destination,
                token,
                wei,
            } => {
                if let Some(info) = self.token_info(tokens, token) {
                    self.spot_send(caller, destination, info, wei)?;
                }
            }
            CoreAction::SendAsset {
                destination,
                source_dex,
                destination_dex,
                token,
                wei,
                ..
            } => {
                if self.refuse_off_spot_send_asset(caller, source_dex, destination_dex) {
                    return Ok(());
                }
                if let Some(info) = self.token_info(tokens, token) {
                    self.send_asset(caller, destination, info, wei).await?;
                }
            }
            CoreAction::AddApiWallet { wallet, name } => {
                self.add_api_wallet(caller, wallet, &name)?;
            }
            CoreAction::SetAbstraction { user, abstraction } => {
                self.set_abstraction(user, abstraction);
            }
            order @ CoreAction::LimitOrder { .. } => self.limit_order(caller, &order),
            action @ (CoreAction::CancelByOid { .. } | CoreAction::CancelByCloid { .. }) => {
                self.core_cancel(caller, &action);
            }
            CoreAction::Unmodeled { .. } => {}
            CoreAction::Malformed => self.metrics.inc_malformed(),
        }
        Ok(())
    }

    /// The CoreWriter cancels (ids 10/11): resolve the asset, then the sim's
    /// own cancel — the venue's refusal string rides the log.
    fn core_cancel(&self, caller: Address, action: &CoreAction) {
        let (coin, outcome) = match action {
            CoreAction::CancelByOid { asset, oid } => {
                let Some(coin) = self.coin_of(*asset) else {
                    self.metrics.inc_refusals();
                    tracing::warn!(account = %caller, asset, "bridge: cancel for an unknown asset refused");
                    return;
                };
                (
                    coin.clone(),
                    self.sim.cancel(caller, &coin, *oid, CancelSpeed::Normal),
                )
            }
            CoreAction::CancelByCloid { asset, cloid } => {
                let Some(coin) = self.coin_of(*asset) else {
                    self.metrics.inc_refusals();
                    tracing::warn!(account = %caller, asset, "bridge: cancelByCloid for an unknown asset refused");
                    return;
                };
                (
                    coin.clone(),
                    self.sim.cancel_by_cloid(
                        caller,
                        &coin,
                        alloy_primitives::B128::from(*cloid),
                        CancelSpeed::Normal,
                    ),
                )
            }
            _ => return,
        };
        match outcome {
            Ok(()) => {
                tracing::info!(account = %caller, coin = %coin, "bridge: CoreWriter cancel executed");
            }
            Err(e) => {
                self.metrics.inc_refusals();
                tracing::warn!(account = %caller, coin = %coin, error = %e, "bridge: CoreWriter cancel refused");
            }
        }
    }

    /// The asset index -> coin map the CoreWriter actions address (`None` =
    /// an index the live meta does not carry: a named refusal upstream).
    fn coin_of(&self, asset: u32) -> Option<String> {
        self.feed
            .meta()?
            .universe
            .get(asset as usize)
            .map(|a| a.name.clone())
    }

    /// The CoreWriter limit order (id 1): prices and sizes arrive as 10^8 ×
    /// the human value and the tif byte is the docs' own (1 `Alo`, 2 `Gtc`,
    /// 3 `Ioc`). The order fills against the real book through the same lane
    /// as `/exchange` and `/control/order` — one lane, three doors. An
    /// `Alo`/`Gtc` order rests in the sim's book; a refused fill (no depth,
    /// no margin, unknown asset) is counted, never retried.
    fn limit_order(&self, caller: Address, action: &CoreAction) {
        let CoreAction::LimitOrder { asset, tif, .. } = action else {
            return;
        };
        let Some(tif) = crate::sim::order::Tif::from_corewriter(*tif) else {
            self.metrics.inc_malformed();
            tracing::warn!(account = %caller, tif, "bridge: CoreWriter order with an unknown tif byte");
            return;
        };
        let Some(order) = core_order(self.feed.meta().as_ref(), action, tif) else {
            self.metrics.inc_refusals();
            tracing::warn!(account = %caller, asset, "bridge: CoreWriter order for an unknown asset refused");
            return;
        };
        match self.sim.place(&self.feed, caller, &order) {
            Ok(placed) => {
                if let Some(fill) = &placed.fill {
                    tracing::info!(account = %caller, coin = %order.coin, sz = %fill.total_sz, avg_px = %fill.avg_px, "bridge: CoreWriter order filled");
                }
                if let Some(oid) = placed.resting {
                    tracing::info!(account = %caller, coin = %order.coin, oid, "bridge: CoreWriter order resting");
                }
            }
            Err(e) => {
                self.metrics.inc_refusals();
                tracing::warn!(account = %caller, coin = %order.coin, error = %e, "bridge: CoreWriter order refused");
            }
        }
    }

    /// `usdClassTransfer`: USDC between the spot and perp wallets.
    fn usd_class_transfer(
        &self,
        caller: Address,
        ntl: u64,
        direction: Direction,
        tokens: &[TokenInfo],
    ) -> Result<()> {
        let Some(usdc) = system::by_name(tokens, "USDC") else {
            return Err(Error::Warming);
        };
        // Action 7 carries the *perp class's* USD (1e6), not the spot token's
        // core wei: the margin agent's recall reads the perp class's
        // withdrawable precompile (1e6) and converts ×100 only for the
        // spot-class bridge leg (`HLConversions.perpToWei`). Decoding at the
        // spot scale read the recall 1/100 small — the measured
        // 0.15-of-15.0 leg that stranded the recovery sweep.
        let Some(amount) = usd_class_amount(ntl) else {
            self.metrics.inc_refusals();
            tracing::warn!(
                wei = ntl,
                "bridge: usdClassTransfer amount is not representable; refused"
            );
            return Ok(());
        };
        let applied = self.sim.usd_class_transfer(
            caller,
            &ClassTransfer {
                token: TokenRef {
                    index: usdc.index,
                    coin: &usdc.name,
                },
                amount,
                direction,
            },
            &self.feed,
        )?;
        match applied {
            Applied::Applied => {
                tracing::info!(account = %caller, amount = %amount, ?direction, "bridge: USD class transfer");
            }
            Applied::Refused => {
                self.metrics.inc_refusals();
                tracing::warn!(account = %caller, amount = %amount, ?direction, "bridge: USD class transfer refused (insufficient balance)");
            }
        }
        Ok(())
    }

    /// The `sendAsset` dex guard: this sim models the spot dex, so a perp-dex
    /// move (multi-dex is deferred — docs/INTEGRATION.md) is refused by name
    /// and counted — never applied as a spot send. `true` when the action was
    /// refused.
    fn refuse_off_spot_send_asset(
        &self,
        caller: Address,
        source_dex: u32,
        destination_dex: u32,
    ) -> bool {
        if source_dex == corewriter::SPOT_DEX && destination_dex == corewriter::SPOT_DEX {
            return false;
        }
        self.metrics.inc_refusals();
        tracing::warn!(
            caller = %caller,
            source_dex,
            destination_dex,
            "bridge: sendAsset outside the spot dex refused (multi-dex is deferred)"
        );
        true
    }

    /// The registry entry for a wire token index; an unknown index is counted
    /// and refused (the bridge never guesses a token).
    fn token_info<'a>(&self, tokens: &'a [TokenInfo], token: u64) -> Option<&'a TokenInfo> {
        if let Some(info) = system::by_index(tokens, u32::try_from(token).ok()?) {
            return Some(info);
        }
        self.metrics.inc_refusals();
        None
    }

    /// `spotSend`: spot→spot inside Core.
    fn spot_send(
        &self,
        caller: Address,
        destination: Address,
        info: &TokenInfo,
        wei: u64,
    ) -> Result<()> {
        let Some(amount) = system::core_to_human(wei, info.wei_decimals) else {
            self.metrics.inc_refusals();
            tracing::warn!(wei, "bridge: spotSend amount is not representable; refused");
            return Ok(());
        };
        let applied = self.sim.spot_send(
            caller,
            destination,
            TokenRef {
                index: info.index,
                coin: &info.name,
            },
            amount,
        )?;
        if matches!(applied, Applied::Refused) {
            self.metrics.inc_refusals();
            tracing::warn!(account = %caller, amount = %amount, "bridge: spot send refused (insufficient balance)");
        }
        Ok(())
    }

    /// `addApiWallet`: register the agent on the account.
    fn add_api_wallet(&self, caller: Address, wallet: Address, name: &str) -> Result<()> {
        self.sim.note_agent(caller, wallet, name)?;
        tracing::info!(account = %caller, wallet = %wallet, name, "bridge: API wallet registered");
        Ok(())
    }

    /// `setAbstraction`: 1 = disabled, 2 = unified account; any other code is
    /// counted and never applied. 3 (portfolio margin) reaches the sim, which
    /// refuses it by name — its cross-asset portfolio is the venue's own
    /// accounting — and the refusal is counted here, never a block failure.
    fn set_abstraction(&self, user: Address, abstraction: u8) {
        let mode = match abstraction {
            1 => UserAbstraction::Disabled,
            2 => UserAbstraction::UnifiedAccount,
            3 => UserAbstraction::PortfolioMargin,
            _ => {
                self.metrics.inc_refusals();
                return;
            }
        };
        match self.sim.set_mode(user, mode) {
            Ok(()) => tracing::info!(account = %user, ?mode, "bridge: abstraction set"),
            Err(e) => {
                self.metrics.inc_refusals();
                tracing::warn!(account = %user, error = %e, "bridge: abstraction refused");
            }
        }
    }

    /// `sendAsset`: spot→spot inside Core, or Core→EVM when the destination
    /// is the token's system address (completed by a system-address transfer).
    #[allow(clippy::too_many_lines)] // one rendering/transition table: splitting it would hide the field-to-line mapping
    async fn send_asset(
        &self,
        caller: Address,
        destination: Address,
        info: &TokenInfo,
        wei: u64,
    ) -> Result<()> {
        let Some(amount) = system::core_to_human(wei, info.wei_decimals) else {
            self.metrics.inc_refusals();
            tracing::warn!(
                wei,
                "bridge: sendAsset amount is not representable; refused"
            );
            return Ok(());
        };
        if destination == system::system_address(u64::from(info.index)) {
            if !self.core_to_evm_allowed(caller, info, amount)? {
                return Ok(());
            }
            // Core→EVM: debit the caller, then complete on the EVM side.
            let Some(_after) = self.sim.try_debit_spot(caller, info.index, amount)? else {
                self.metrics.inc_refusals();
                tracing::warn!(account = %caller, amount = %amount, "bridge: Core→EVM refused (insufficient balance)");
                return Ok(());
            };
            self.complete_core_to_evm(&caller, info, amount).await?;
        } else {
            let applied = self.sim.spot_send(
                caller,
                destination,
                TokenRef {
                    index: info.index,
                    coin: &info.name,
                },
                amount,
            )?;
            if matches!(applied, Applied::Refused) {
                self.metrics.inc_refusals();
                tracing::warn!(account = %caller, amount = %amount, "bridge: sendAsset refused (insufficient balance)");
            }
        }
        Ok(())
    }

    /// The Core→EVM transfer bound: the venue's rule bounds the move by the
    /// account's **spot available after maintenance**, not by the spot balance
    /// and not by the perp class's `withdrawable`. A control-patched or
    /// insolvent account can still hold spot cash while its equity is below
    /// the transfer floor; 4a withdrew 0.55 with `withdrawable=0` one block
    /// before its deficit deposit. But a unified account's spot principal is a
    /// **separate ledger** from the perp wallet (the live `cf16` recovery:
    /// 240.510247 spot against 0.000000455 perp), so the perp figure is the
    /// wrong bound for a spot→EVM bridge — it refused the whole recovery leg.
    /// The collateral token is bounded by `tokenToAvailableAfterMaintenance`;
    /// other spot tokens are holdings, bounded by the balance alone.
    fn core_to_evm_allowed(
        &self,
        caller: Address,
        info: &TokenInfo,
        amount: Decimal,
    ) -> Result<bool> {
        if info.index != crate::sim::account::COLLATERAL_TOKEN {
            return Ok(true);
        }
        let Some(account) = self.sim.account(caller)? else {
            self.metrics.inc_refusals();
            tracing::warn!(account = %caller, amount = %amount, "bridge: Core→EVM refused (unknown account)");
            return Ok(false);
        };
        let meta = self.feed.margin_meta();
        if let Some(bound) =
            crate::sim::account::spot_bridge_bound(&account, &self.feed, meta.as_ref(), info.index)?
        {
            if amount > bound {
                self.metrics.inc_refusals();
                tracing::warn!(
                    account = %caller,
                    amount = %amount,
                    available = %bound,
                    "bridge: Core→EVM refused (amount exceeds spot available after maintenance)"
                );
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The completion tx: `transfer(caller, amount)` on the linked ERC20 from
    /// the system address (auto-impersonate). It lands with the devnet's next
    /// mined block, like the real chain's system tx.
    async fn complete_core_to_evm(
        &self,
        recipient: &Address,
        info: &TokenInfo,
        amount: Decimal,
    ) -> Result<()> {
        let Some(evm) = info.evm else {
            return Err(system::no_evm_link(info));
        };
        let evm_decimals = info.wei_decimals as i32 + evm.extra_decimals;
        let Some(value) = system::human_to_evm(amount, evm_decimals) else {
            self.metrics.inc_refusals();
            tracing::warn!(amount = %amount, "bridge: Core→EVM amount is not representable in EVM wei; refused");
            return Ok(());
        };
        let from = system::system_address(u64::from(info.index));
        // The system address pays gas on V (the real chain's system txs do
        // not); fund it once, at the deployment's declared stipend, so the
        // pool's balance check passes. A balance-read fault propagates: it
        // must not be read as "empty" and funded over a broken RPC.
        if self.target_rpc.balance_at(from, 0).await?.is_zero() {
            self.target_rpc
                .anvil_set_balance(from, self.system_gas_stipend)
                .await?;
        }
        let tx = TransactionRequest::default()
            .from(from)
            .to(evm.address)
            .input(system::transfer_calldata(*recipient, value).into());
        // Waited to the receipt: the completion's cursor may only advance on
        // a write that actually landed (see send_projection).
        let receipt = self.target_rpc.send_transaction_confirmed(tx).await?;
        self.metrics.inc_completions();
        tracing::info!(
            to = %recipient,
            token = %info.name,
            amount = %amount,
            tx = %receipt.transaction_hash,
            "bridge: Core→EVM completion confirmed"
        );
        Ok(())
    }
}

/// Action 7's `ntl` as the human decimal the sim keeps: the *perp class's*
/// USD (1e6) — `system::PERP_USD_DECIMALS`, not the spot token's core wei.
/// Pure, so the unit rule is testable without a chain.
fn usd_class_amount(ntl: u64) -> Option<Decimal> {
    system::core_to_human(ntl, system::PERP_USD_DECIMALS)
}

/// A human decimal as the mocks' raw unit, floored like every other
/// Core→EVM conversion (`system::human_to_evm`). `None` becomes an error at
/// the caller: an unrepresentable figure must refuse the projection, never
/// silently write zero.
fn core_units(amount: Decimal, decimals: u32) -> Result<u64> {
    let decimals = i32::try_from(decimals).map_err(|_| Error::Overflow {
        what: "precompile projection decimals",
    })?;
    let raw = system::human_to_evm(amount, decimals).ok_or(Error::Overflow {
        what: "precompile projection amount",
    })?;
    u64::try_from(raw).map_err(|_| Error::Overflow {
        what: "precompile projection amount exceeds u64",
    })
}

/// The Core's intra-block action order (docs: "Within a block, actions are
/// sorted within each consensus batch as 1. Actions that do not send GTC or
/// IOC orders to any book 2. Cancels 3. Actions that send at least one GTC or
/// IOC … Within each category, actions are sorted in the order they were
/// proposed"). An `Alo` (post-only, tif byte 1) cannot match, so it sorts
/// with the non-order actions; `Gtc`/`Ioc` (bytes 2/3) are the book-sending
/// category. `sort_by_key` is stable, so each category keeps the proposed
/// order.
/// The docs' pre-existence rule (interaction timings): "the account
/// performing the CoreWriter action must exist on HyperCore before the EVM
/// block is built. An EVM -> Core transfer to initialize the account in the
/// same block will still result in the CoreWriter action being rejected."
/// The bridge snapshots the accounts before the block's credits and filters
/// the calls through this — pure, so the rule is testable without a chain.
fn callers_eligible(
    pre_block: &std::collections::HashSet<Address>,
    calls: Vec<(Address, CoreAction)>,
) -> (Vec<(Address, CoreAction)>, usize) {
    let mut refused = 0;
    let eligible = calls
        .into_iter()
        .filter(|(caller, _)| {
            let known = pre_block.contains(caller);
            if !known {
                refused += 1;
            }
            known
        })
        .collect();
    (eligible, refused)
}

fn action_rank(action: &CoreAction) -> u8 {
    match action {
        CoreAction::CancelByOid { .. } | CoreAction::CancelByCloid { .. } => 1,
        CoreAction::LimitOrder { tif, .. } if *tif == 2 || *tif == 3 => 2,
        _ => 0,
    }
}

/// Collect CoreWriter calls from a trace tree (frame order = execution order).
fn collect_internal(trace: &TraceResult, out: &mut Vec<(Address, CoreAction)>) {
    if let Some(GethTrace::CallTracer(frame)) = trace.success() {
        walk_frame(frame, out);
    }
}

fn walk_frame(frame: &CallFrame, out: &mut Vec<(Address, CoreAction)>) {
    if frame.to == Some(CORE_WRITER) && !frame.input.is_empty() {
        out.push((frame.from, corewriter::decode_calldata(&frame.input)));
    }
    for child in &frame.calls {
        walk_frame(child, out);
    }
}

/// The CoreWriter order's units (docs, pinned): prices and sizes are 10^8 ×
/// the human value, and the asset index resolves through the live meta
/// (`None` = an index the meta does not carry — a named refusal upstream).
/// The tif comes in already mapped (`Tif::from_corewriter`, one home).
fn core_order(
    meta: Option<&crate::wire::Meta>,
    action: &CoreAction,
    tif: crate::sim::order::Tif,
) -> Option<crate::sim::order::Order> {
    let CoreAction::LimitOrder {
        asset,
        side,
        limit_px,
        sz,
        reduce_only,
        cloid,
        ..
    } = action
    else {
        return None;
    };
    let coin = meta?.universe.get(*asset as usize)?.name.clone();
    let scale = Decimal::from(100_000_000u64);
    Some(crate::sim::order::Order {
        coin,
        is_buy: side.is_buy(),
        size: Decimal::from(*sz) / scale,
        limit_px: Some(Decimal::from(*limit_px) / scale),
        tif,
        reduce_only: *reduce_only,
        // The CoreWriter payload carries the cloid as a uint128 (docs,
        // action 1); zero means "none" (the wire's absent value).
        cloid: (*cloid != 0).then(|| alloy_primitives::B128::from(*cloid)),
        // The CoreWriter has no trigger variant: limit orders only.
        trigger: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The projection's unit rule: the mocks store raw core wei / raw USD
    /// 1e6, floored like every other Core→EVM conversion, and an
    /// unrepresentable figure is refused rather than written as zero.
    #[test]
    fn core_units_floor_at_the_mock_unit_and_refuse_the_unrepresentable() {
        use rust_decimal::Decimal;
        // 67.892481 USDC at 8 decimals — the measured live case.
        let live: Decimal = "67.892481".parse().unwrap();
        assert_eq!(core_units(live, 8).expect("representable"), 6_789_248_100);
        // Sub-wei remainder floors (the chain's own rule).
        let dust: Decimal = "0.0000000019".parse().unwrap();
        assert_eq!(core_units(dust, 8).expect("representable"), 0);
        // The perp class's USD 1e6 scale.
        let withdrawable: Decimal = "13104.514502".parse().unwrap();
        assert_eq!(
            core_units(withdrawable, system::PERP_USD_DECIMALS).expect("representable"),
            13_104_514_502,
        );
        // A negative or absurd figure refuses; it is never laundered into 0.
        let negative: Decimal = "-1".parse().unwrap();
        assert!(core_units(negative, 8).is_err());
        let absurd: Decimal = "100000000000000000000000".parse().unwrap();
        assert!(core_units(absurd, 8).is_err());
    }

    fn usd_transfer_input(ntl: u64) -> Vec<u8> {
        let mut input = vec![0x01, 0x00, 0x00, 0x07];
        input.extend_from_slice(&U256::from(ntl).to_be_bytes::<32>());
        input.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
        input
    }

    /// The CoreWriter order's units are the docs' own: 10^8 × human for both
    /// price and size, resolved against the live meta.
    #[test]
    fn corewriter_orders_scale_by_1e8() {
        let mut meta: crate::wire::Meta = serde_json::from_str(r#"{"universe": []}"#).unwrap();
        meta.universe.push(crate::wire::AssetInfo {
            name: "ETH".into(),
            sz_decimals: 4,
            max_leverage: 20,
            margin_table_id: None,
            only_isolated: None,
            is_delisted: None,
            margin_mode: None,
        });
        let action = CoreAction::LimitOrder {
            asset: 0,
            side: crate::sim::order::Side::Buy,
            limit_px: 10_150_000_000,
            sz: 250_000_000,
            reduce_only: false,
            tif: 3,
            cloid: 0,
        };
        let order =
            core_order(Some(&meta), &action, crate::sim::order::Tif::Ioc).expect("a modeled order");
        assert_eq!(order.coin, "ETH");
        assert!(order.is_buy);
        assert_eq!(order.size, "2.5".parse().unwrap());
        assert_eq!(order.limit_px, Some("101.5".parse().unwrap()));
        assert_eq!(order.tif, crate::sim::order::Tif::Ioc);
        // An index the meta does not carry is a refusal, not a guess.
        let oob = CoreAction::LimitOrder {
            asset: 9,
            side: crate::sim::order::Side::Buy,
            limit_px: 1,
            sz: 1,
            reduce_only: false,
            tif: 3,
            cloid: 0,
        };
        assert!(core_order(Some(&meta), &oob, crate::sim::order::Tif::Ioc).is_none());
        assert!(core_order(None, &action, crate::sim::order::Tif::Ioc).is_none());
    }

    /// Action 7's unit rule: `ntl` is the *perp class's* USD (1e6), not the
    /// spot token's core wei (8). The margin agent's recall sends the clamped
    /// perp-class figure (the `0x0803` withdrawable precompile's own unit) and
    /// converts ×100 only for the spot-class bridge leg — so a 15.0 USDC
    /// recall arrives as `ntl = 15_000_000` and must apply as 15.0. The old
    /// spot-wei reading produced the measured 0.15-of-15.0 leg (R5-02) that
    /// stranded the recovery sweep.
    #[test]
    fn usd_class_amount_is_perp_class_usd_not_spot_wei() {
        assert_eq!(usd_class_amount(15_000_000), Some("15.0".parse().unwrap()));
        assert_eq!(
            usd_class_amount(7_950_068),
            Some("7.950068".parse().unwrap())
        );
        // The defect's reading, pinned as the contrast: the same raw at the
        // spot token's 8-dec scale is 1/100 of the request.
        assert_eq!(
            system::core_to_human(15_000_000, 8),
            Some("0.15".parse().unwrap())
        );
        // Sub-wei dust still converts (the venue floors nothing here — the
        // amount is the caller's exact perp-class figure).
        assert_eq!(usd_class_amount(1), Some("0.000001".parse().unwrap()));
    }

    /// The pre-existence rule is pure: a caller outside the pre-block set is
    /// refused (and counted); a known caller passes.
    #[test]
    fn corewriter_actions_require_a_pre_existing_account() {
        use std::collections::HashSet;
        let known = Address::repeat_byte(0x0a);
        let fresh = Address::repeat_byte(0x0b);
        let pre: HashSet<Address> = [known].into_iter().collect();
        let (eligible, refused) = callers_eligible(
            &pre,
            vec![
                (known, CoreAction::Malformed),
                (fresh, CoreAction::Malformed),
            ],
        );
        assert_eq!(refused, 1, "the same-block initialization still refuses");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].0, known);
    }

    #[test]
    fn internal_frames_are_walked_in_order() {
        let outer = CallFrame {
            from: Address::repeat_byte(0x01),
            to: Some(Address::repeat_byte(0x02)),
            input: Default::default(),
            calls: vec![
                CallFrame {
                    from: Address::repeat_byte(0x02),
                    to: Some(CORE_WRITER),
                    input: usd_transfer_input(1).into(),
                    calls: vec![],
                    ..Default::default()
                },
                CallFrame {
                    from: Address::repeat_byte(0x02),
                    to: Some(CORE_WRITER),
                    input: usd_transfer_input(2).into(),
                    calls: vec![],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut out = Vec::new();
        walk_frame(&outer, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].0,
            Address::repeat_byte(0x02),
            "attributed to the caller"
        );
        assert_eq!(out[0].1.id(), 7);
        assert_eq!(out[1].1.id(), 7);
    }
    /// The Core's intra-block action order (docs): non-order actions, then
    /// cancels, then Gtc/Ioc orders — a stable sort, so each category keeps
    /// the proposed order. An Alo is post-only and sorts with the non-orders.
    #[test]
    fn intra_block_actions_sort_by_category() {
        let cancel = |oid: u64| CoreAction::CancelByOid { asset: 0, oid };
        let order = |tif: u8, cloid: u128| CoreAction::LimitOrder {
            asset: 0,
            side: crate::sim::order::Side::Buy,
            limit_px: 100,
            sz: 1,
            reduce_only: false,
            tif,
            cloid,
        };
        let mut calls = [
            order(2, 1),
            cancel(1),
            CoreAction::UsdClassTransfer {
                ntl: 1,
                direction: Direction::ToPerp,
            },
            order(1, 2),
            order(3, 3),
            cancel(2),
        ];
        calls.sort_by_key(action_rank);
        assert!(
            matches!(calls[0], CoreAction::UsdClassTransfer { .. }),
            "a non-order action leads"
        );
        assert!(
            matches!(calls[1], CoreAction::LimitOrder { tif: 1, .. }),
            "the Alo is post-only: it sorts with the non-orders"
        );
        assert!(matches!(calls[2], CoreAction::CancelByOid { oid: 1, .. }));
        assert!(
            matches!(calls[3], CoreAction::CancelByOid { oid: 2, .. }),
            "cancels keep the proposed order"
        );
        assert!(matches!(calls[4], CoreAction::LimitOrder { tif: 2, .. }));
        assert!(
            matches!(calls[5], CoreAction::LimitOrder { tif: 3, .. }),
            "the book-sending orders close the block, in the proposed order"
        );
    }
}
