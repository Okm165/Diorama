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
//!   the ones it models (USD class transfer, spot send, sendAsset, add API
//!   wallet, set abstraction), counts the rest (orders are Phase 2 matching);
//! - **Core→EVM**: a `sendAsset` naming a token's system address is completed
//!   the way the chain does it — a `transfer(recipient, amount)` call on the
//!   linked ERC20 **from** the system address (V runs auto-impersonate).
//!
//! The scan is anchored like the driver's: blocks at or below the fork pin
//! are the *source's* real blocks and are never scanned (their transfers are
//! not the devnet's); a re-pin re-anchors the cursor to the new pin.
pub mod corewriter;
pub mod metrics;
pub mod system;

use crate::error::{Error, Result};
use crate::feed::MarketFeed;
use crate::sim::Sim;
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
    v: Rpc,
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
    pub fn new(v: Rpc, sim: Arc<Sim>, feed: Arc<MarketFeed>, metrics: Arc<BridgeMetrics>) -> Self {
        let cursor = sim.bridge_cursor().unwrap_or(0);
        Self {
            v,
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
        let head = self.v.block_number().await?;
        self.observe_head_time(head).await?;
        let pin = self.v.fork_config().await?.map(|(_, block)| block).unwrap_or(0);
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
        let block = self.v.block_by_number(head, false).await?;
        self.sim.observe_chain_time(block.header.timestamp.saturating_mul(1_000));
        self.observed_head.store(head, Ordering::Relaxed);
        Ok(())
    }

    /// Decode one block's boundary activity and apply it.
    async fn scan_block(&self, n: u64) -> Result<()> {
        let block = self.v.block_by_number(n, true).await?;
        if block.transactions.is_empty() {
            return Ok(());
        }
        let Some(spot) = self.feed.spot_meta() else {
            tracing::warn!(block = n, "bridge has no spotMeta yet; the block is re-scanned");
            return Err(Error::Warming);
        };
        let tokens = system::token_registry(&spot);

        // EVM→Core: transfers to system addresses, from the receipts' logs.
        let receipts = self.v.receipts(n).await?;
        for receipt in &receipts {
            for log in receipt.logs() {
                let Some(transfer) = system::transfer_of(log.topics(), &log.data().data) else {
                    continue;
                };
                self.apply_credit(&transfer, log.address(), &tokens).await?;
            }
        }

        // CoreWriter: top-level txs plus contract-internal calls.
        let calls = self.corewriter_calls(n, &block).await?;
        for (caller, action) in calls {
            self.apply_action(caller, action, &tokens).await?;
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
        match self.v.trace_block_calls(n).await {
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
        self.sim.credit_spot(sender, token.index, &token.name, amount)?;
        self.metrics.inc_credits();
        tracing::info!(
            from = %sender,
            token = %token.name,
            amount = %amount,
            "bridge: EVM→Core credit"
        );
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
                self.spot_send(caller, destination, token, wei, tokens)?;
            }
            CoreAction::SendAsset {
                destination,
                token,
                wei,
                ..
            } => {
                self.send_asset(caller, destination, token, wei, tokens).await?;
            }
            CoreAction::AddApiWallet { wallet, name } => {
                self.add_api_wallet(caller, wallet, &name)?;
            }
            CoreAction::SetAbstraction { user, abstraction } => {
                self.set_abstraction(user, abstraction)?;
            }
            // Orders and cancels need the matching lane (Phase 2): counted.
            CoreAction::LimitOrder { .. }
            | CoreAction::CancelByOid { .. }
            | CoreAction::CancelByCloid { .. }
            | CoreAction::Unmodeled { .. } => {}
            CoreAction::Malformed => self.metrics.inc_malformed(),
        }
        Ok(())
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
        let amount = system::core_to_human(ntl, usdc.wei_decimals);
        let applied =
            self.sim
                .usd_class_transfer(caller, usdc.index, &usdc.name, amount, to_perp)?;
        if applied {
            tracing::info!(account = %caller, amount = %amount, to_perp, "bridge: USD class transfer");
        } else {
            self.metrics.inc_refusals();
            tracing::warn!(account = %caller, amount = %amount, to_perp, "bridge: USD class transfer refused (insufficient balance)");
        }
        Ok(())
    }

    /// `spotSend`: spot→spot inside Core.
    fn spot_send(
        &self,
        caller: Address,
        destination: Address,
        token: u64,
        wei: u64,
        tokens: &[TokenInfo],
    ) -> Result<()> {
        let Some(info) = system::by_index(tokens, u32::try_from(token).unwrap_or(u32::MAX)) else {
            self.metrics.inc_refusals();
            return Ok(());
        };
        let amount = system::core_to_human(wei, info.wei_decimals);
        let applied = self
            .sim
            .spot_send(caller, destination, info.index, &info.name, amount)?;
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

    /// `setAbstraction`: 1 = disabled, 2 = unified account, 3 = portfolio
    /// margin; any other code is counted and never applied.
    fn set_abstraction(&self, user: Address, abstraction: u8) -> Result<()> {
        let mode = match abstraction {
            1 => UserAbstraction::Disabled,
            2 => UserAbstraction::UnifiedAccount,
            3 => UserAbstraction::PortfolioMargin,
            _ => {
                self.metrics.inc_refusals();
                return Ok(());
            }
        };
        self.sim.set_mode(user, mode)?;
        tracing::info!(account = %user, ?mode, "bridge: abstraction set");
        Ok(())
    }

    /// `sendAsset`: spot→spot inside Core, or Core→EVM when the destination
    /// is the token's system address (completed by a system-address transfer).
    async fn send_asset(
        &self,
        caller: Address,
        destination: Address,
        token: u64,
        wei: u64,
        tokens: &[TokenInfo],
    ) -> Result<()> {
        let Some(info) = system::by_index(tokens, u32::try_from(token).unwrap_or(u32::MAX)) else {
            self.metrics.inc_refusals();
            return Ok(());
        };
        let amount = system::core_to_human(wei, info.wei_decimals);
        if destination == system::system_address(token) {
            // Core→EVM: debit the caller, then complete on the EVM side.
            let Some(_after) = self.sim.try_debit_spot(caller, info.index, amount)? else {
                self.metrics.inc_refusals();
                tracing::warn!(account = %caller, amount = %amount, "bridge: Core→EVM refused (insufficient balance)");
                return Ok(());
            };
            self.complete_core_to_evm(&caller, info, amount).await?;
        } else {
            let applied = self.sim.spot_send(caller, destination, info.index, &info.name, amount)?;
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
        if self.v.balance_at(from, 0).await.unwrap_or(U256::ZERO).is_zero() {
            self.v
                .anvil_set_balance(from, U256::from(1_000_000_000_000_000_000u64))
                .await?;
        }
        let tx = TransactionRequest::default()
            .from(from)
            .to(erc20)
            .input(system::transfer_calldata(*recipient, value).into());
        let hash = self.v.send_transaction(tx).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn usd_transfer_input(ntl: u64) -> Vec<u8> {
        let mut input = vec![0x01, 0x00, 0x00, 0x07];
        input.extend_from_slice(&U256::from(ntl).to_be_bytes::<32>());
        input.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
        input
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
}
