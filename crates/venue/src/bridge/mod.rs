//! The venue bridge: V's Core↔EVM boundary, mirrored (plan §7.4).
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
use crate::sim::{Sim, TokenRef};
use crate::wire::UserAbstraction;
use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, U256};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_rpc_types_trace::geth::{CallFrame, GethTrace, TraceResult};
use corewriter::{CoreAction, CORE_WRITER};
use metrics::BridgeMetrics;
use mirror_anvil::Rpc;
use rust_decimal::Decimal;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use system::{TokenInfo, Transfer};

/// The bridge's own cursor and the persisted state it needs. The cursor lives
/// in the sim's state file (one state root per service).
pub struct Bridge {
    anvil_v: Rpc,
    sim: Arc<Sim>,
    feed: Arc<MarketFeed>,
    /// The last head whose timestamp was observed (one header read per new
    /// block; the sim's served `time` follows the venue chain's clock).
    observed_head: std::sync::atomic::AtomicU64,
    /// The last scanned V block (0 = not anchored yet); persisted in the
    /// sim's state so a restart does not lose the boundary activity mined
    /// meanwhile.
    cursor: std::sync::atomic::AtomicU64,
    pub metrics: Arc<BridgeMetrics>,
}

impl Bridge {
    pub fn new(anvil_v: Rpc, sim: Arc<Sim>, feed: Arc<MarketFeed>, metrics: Arc<BridgeMetrics>) -> Self {
        let cursor = sim.bridge_cursor().unwrap_or(0);
        Self {
            anvil_v,
            sim,
            feed,
            observed_head: std::sync::atomic::AtomicU64::new(0),
            cursor: std::sync::atomic::AtomicU64::new(cursor),
            metrics,
        }
    }

    /// Run forever: one scan pass per second.
    pub async fn run(self) {
        loop {
            if let Err(e) = self.pass().await {
                tracing::warn!(error = %e, "venue bridge pass failed; will retry");
            }
            tokio::time::sleep(Duration::from_millis(1_000)).await;
        }
    }

    /// One pass: anchor (pin-aware), then scan every new block.
    pub async fn pass(&self) -> Result<()> {
        let head = self.anvil_v.block_number().await?;
        self.observe_head_time(head).await?;
        let pin = self.anvil_v.fork_config().await?.map(|(_, block)| block).unwrap_or(0);
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
        self.metrics.set_lag_blocks(head.saturating_sub(self.cursor.load(Ordering::Relaxed)));
        Ok(())
    }

    /// Keep the sim's served `time` on the venue chain's clock: one header
    /// read per new head (their harness jumps every chain forward; a venue
    /// read must not look stale afterwards).
    async fn observe_head_time(&self, head: u64) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self.observed_head.load(Ordering::Relaxed) == head {
            return Ok(());
        }
        let block = self.anvil_v.block_by_number(head, mirror_anvil::Txs::Hashes).await?;
        self.sim.observe_chain_time(block.header.timestamp.saturating_mul(1_000));
        self.observed_head.store(head, Ordering::Relaxed);
        Ok(())
    }

    /// Decode one block's boundary activity and apply it.
    async fn scan_block(&self, n: u64) -> Result<()> {
        let block = self.anvil_v.block_by_number(n, mirror_anvil::Txs::Full).await?;
        if block.transactions.is_empty() {
            return Ok(());
        }
        let Some(spot) = self.feed.spot_meta() else {
            tracing::warn!(block = n, "bridge has no spotMeta yet; the block is re-scanned");
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
        let receipts = self.anvil_v.receipts(n).await?;
        for receipt in &receipts {
            for log in receipt.logs() {
                // HYPE's boundary is the system contract's `Received` event
                // (docs): a tx value sent to `0x2222…2` credits Core HYPE.
                if log.address() == system::HYPE_SYSTEM {
                    self.apply_hype_credit(log.data(), tokens)?;
                    continue;
                }
                let Some(transfer) = system::transfer_of(log.data()) else {
                    continue;
                };
                self.apply_credit(&transfer, log.address(), tokens).await?;
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
        match self.anvil_v.trace_block_calls(n).await {
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
    // only a Transfer log from the token's linked EVM contract counts, and the
    // Core amount is the log's exact value converted by the pinned decimals.
    /// EVM→Core credit from one transfer log (the emitting contract must be
    /// the token's linked EVM contract).
    async fn apply_credit(&self, transfer: &Transfer, emitter: Address, tokens: &[TokenInfo]) -> Result<()> {
        let Some((sender, token, amount)) = system::credit_of(transfer, tokens, emitter) else {
            return Ok(());
        };
        self.sim.credit_spot(
            sender,
            TokenRef { index: token.index, coin: &token.name },
            amount,
        )?;
        self.metrics.inc_credits();
        tracing::info!(
            from = %sender,
            token = %token.name,
            amount = %amount,
            "bridge: EVM→Core credit"
        );
        Ok(())
    }

    /// The EVM→Core HYPE credit (the system contract's `Received` event).
    /// The emitter check is the address itself: only the real system contract
    /// can emit a credit, never a look-alike.
    fn apply_hype_credit(&self, data: &alloy_primitives::LogData, tokens: &[TokenInfo]) -> Result<()> {
        let Some(token) = tokens.iter().find(|t| t.name == "HYPE") else {
            return Ok(());
        };
        let Some((user, amount)) = system::hype_received_of(data, token) else {
            return Ok(());
        };
        self.sim.credit_spot(
            user,
            TokenRef {
                index: token.index,
                coin: &token.name,
            },
            amount,
        )?;
        self.metrics.inc_credits();
        tracing::info!(from = %user, amount = %amount, "bridge: EVM→Core HYPE credit");
        Ok(())
    }

    /// Apply one decoded action to the sim.
    async fn apply_action(&self, caller: Address, action: CoreAction, tokens: &[TokenInfo]) -> Result<()> {
        self.metrics.count_action(action.id());
        match action {
            CoreAction::UsdClassTransfer { ntl, to_perp } => {
                self.usd_class_transfer(caller, ntl, to_perp, tokens)?;
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
                (coin, self.sim.cancel(caller, &self.coin_of(*asset).unwrap_or_default(), *oid, false))
            }
            CoreAction::CancelByCloid { asset, cloid } => {
                let Some(coin) = self.coin_of(*asset) else {
                    self.metrics.inc_refusals();
                    tracing::warn!(account = %caller, asset, "bridge: cancelByCloid for an unknown asset refused");
                    return;
                };
                (
                    coin,
                    self.sim.cancel_by_cloid(caller, &self.coin_of(*asset).unwrap_or_default(), alloy_primitives::B128::from(*cloid), false),
                )
            }
            _ => return,
        };
        match outcome {
            Ok(()) => tracing::info!(account = %caller, coin = %coin, "bridge: CoreWriter cancel executed"),
            Err(e) => {
                self.metrics.inc_refusals();
                tracing::warn!(account = %caller, coin = %coin, error = %e, "bridge: CoreWriter cancel refused");
            }
        }
    }

    /// The asset index -> coin map the CoreWriter actions address (`None` =
    /// an index the live meta does not carry: a named refusal upstream).
    fn coin_of(&self, asset: u32) -> Option<String> {
        self.feed.meta()?.universe.get(asset as usize).map(|a| a.name.clone())
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
        to_perp: bool,
        tokens: &[TokenInfo],
    ) -> Result<()> {
        let Some(usdc) = system::by_name(tokens, "USDC") else {
            return Err(Error::Warming);
        };
        let Some(amount) = system::core_to_human(ntl, usdc.wei_decimals) else {
            self.metrics.inc_refusals();
            tracing::warn!(wei = ntl, "bridge: usdClassTransfer amount is not representable; refused");
            return Ok(());
        };
        let applied = self.sim.usd_class_transfer(
            caller,
            TokenRef { index: usdc.index, coin: &usdc.name },
            amount,
            to_perp,
        )?;
        if applied {
            tracing::info!(account = %caller, amount = %amount, to_perp, "bridge: USD class transfer");
        } else {
            self.metrics.inc_refusals();
            tracing::warn!(account = %caller, amount = %amount, to_perp, "bridge: USD class transfer refused (insufficient balance)");
        }
        Ok(())
    }

    /// The `sendAsset` dex guard: this sim models the spot dex, so a perp-dex
    /// move (multi-dex is deferred — docs/INTEGRATION.md) is refused by name
    /// and counted — never applied as a spot send. `true` when the action was
    /// refused.
    fn refuse_off_spot_send_asset(&self, caller: Address, source_dex: u32, destination_dex: u32) -> bool {
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
        if let Some(info) = system::by_index(tokens, u32::try_from(token).unwrap_or(u32::MAX)) {
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
            TokenRef { index: info.index, coin: &info.name },
            amount,
        )?;
        if !applied {
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
    async fn send_asset(
        &self,
        caller: Address,
        destination: Address,
        info: &TokenInfo,
        wei: u64,
    ) -> Result<()> {
        let Some(amount) = system::core_to_human(wei, info.wei_decimals) else {
            self.metrics.inc_refusals();
            tracing::warn!(wei, "bridge: sendAsset amount is not representable; refused");
            return Ok(());
        };
        if destination == system::system_address(u64::from(info.index)) {
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
                TokenRef { index: info.index, coin: &info.name },
                amount,
            )?;
            if !applied {
                self.metrics.inc_refusals();
                tracing::warn!(account = %caller, amount = %amount, "bridge: sendAsset refused (insufficient balance)");
            }
        }
        Ok(())
    }

    /// The completion tx: `transfer(caller, amount)` on the linked ERC20 from
    /// the system address (auto-impersonate). It lands with the devnet's next
    /// mined block, like the real chain's system tx.
    async fn complete_core_to_evm(&self, recipient: &Address, info: &TokenInfo, amount: Decimal) -> Result<()> {
        let Some((erc20, extra)) = info.evm else {
            return Err(system::no_evm_link(info));
        };
        let evm_decimals = info.wei_decimals as i32 + extra;
        let Some(value) = system::human_to_evm(amount, evm_decimals) else {
            self.metrics.inc_refusals();
            tracing::warn!(amount = %amount, "bridge: Core→EVM amount is not representable in EVM wei; refused");
            return Ok(());
        };
        let from = system::system_address(u64::from(info.index));
        // The system address pays gas on V (the real chain's system txs do
        // not); fund it once so the pool's balance check passes.
        if self.anvil_v.balance_at(from, 0).await.unwrap_or(U256::ZERO).is_zero() {
            self.anvil_v
                .anvil_set_balance(from, U256::from(1_000_000_000_000_000_000u64))
                .await?;
        }
        let tx = TransactionRequest::default()
            .from(from)
            .to(erc20)
            .input(system::transfer_calldata(*recipient, value).into());
        let hash = self.anvil_v.send_transaction(tx).await?;
        self.metrics.inc_completions();
        tracing::info!(
            to = %recipient,
            token = %info.name,
            amount = %amount,
            tx = %hash,
            "bridge: Core→EVM completion submitted"
        );
        Ok(())
    }
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
        is_buy,
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
        is_buy: *is_buy,
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
            is_buy: true,
            limit_px: 10_150_000_000,
            sz: 250_000_000,
            reduce_only: false,
            tif: 3,
            cloid: 0,
        };
        let order = core_order(Some(&meta), &action, crate::sim::order::Tif::Ioc)
            .expect("a modeled order");
        assert_eq!(order.coin, "ETH");
        assert!(order.is_buy);
        assert_eq!(order.size, "2.5".parse().unwrap());
        assert_eq!(order.limit_px, Some("101.5".parse().unwrap()));
        assert_eq!(order.tif, crate::sim::order::Tif::Ioc);
        // An index the meta does not carry is a refusal, not a guess.
        let oob = CoreAction::LimitOrder {
            asset: 9,
            is_buy: true,
            limit_px: 1,
            sz: 1,
            reduce_only: false,
            tif: 3,
            cloid: 0,
        };
        assert!(core_order(Some(&meta), &oob, crate::sim::order::Tif::Ioc).is_none());
        assert!(core_order(None, &action, crate::sim::order::Tif::Ioc).is_none());
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
        assert_eq!(out[0].0, Address::repeat_byte(0x02), "attributed to the caller");
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
            is_buy: true,
            limit_px: 100,
            sz: 1,
            reduce_only: false,
            tif,
            cloid,
        };
        let mut calls = [
            order(2, 1),
            cancel(1),
            CoreAction::UsdClassTransfer { ntl: 1, to_perp: true },
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
        assert!(matches!(calls[3], CoreAction::CancelByOid { oid: 2, .. }), "cancels keep the proposed order");
        assert!(matches!(calls[4], CoreAction::LimitOrder { tif: 2, .. }));
        assert!(
            matches!(calls[5], CoreAction::LimitOrder { tif: 3, .. }),
            "the book-sending orders close the block, in the proposed order"
        );
    }
}
