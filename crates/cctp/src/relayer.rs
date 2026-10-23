//! The relayer: signed messages → `receiveMessage(message, attestation)` on
//! the destination chain (§15.4).
//!
//! Design rules this module keeps:
//!
//! - **The chain is the dedup truth** (the keeper-artery rule): before sending
//!   (and on every revert), `usedNonces(nonce)` is read on the destination —
//!   a consumed nonce means the mint already happened, whatever our record
//!   says. A relay is idempotent across crashes.
//! - **Outcome-unknown is a state, not a failure**: a submitted tx is recorded
//!   ([`MessageRecord::relay_tx`]); the next tick checks its receipt first and
//!   re-sends only when the tx left the pool without a receipt (an anvil
//!   restart loses its pool). Blind re-sends cannot happen.
//! - **Parked, never dropped**: a mined revert (or repeated node-side send
//!   refusals) parks the record with the reason; `/health` and `/metrics` keep
//!   it visible for an operator. Transport faults never park — they retry.
//! - **Gas alerts** (daloc's defaults: 0.1 ETH low / 0.01 ETH critical):
//!   per-chain balance logs + gauges (the bin's boot top-up is the devnet
//!   funding step; the alerts guard later drops).
use crate::chains::{by_domain, Chain, MESSAGE_TRANSMITTER_V2};
use crate::error::{Error, Result};
use crate::metrics::CctpMetrics;
use crate::store::{MessageRecord, RelayState, Store};
use crate::transfer::{
    receipt, receive_message_calldata, wait_for_receipt, IMessageTransmitterV2, TxSender,
};
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::TransactionReceipt;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// Node-side send refusals before a message is parked. A JSON-RPC error is
/// the node's answer (a deterministic refusal, e.g. an estimate revert); a
/// transport fault is an outage and must never park a message — it retries.
const MAX_SEND_ATTEMPTS: u32 = 5;

pub struct Relayer {
    pub chains: Vec<Chain>,
    pub store: Arc<Store>,
    pub sender: Arc<TxSender>,
    pub metrics: Arc<CctpMetrics>,
    /// Gas-alert thresholds (daloc's defaults, wei).
    pub balance_low: U256,
    pub balance_critical: U256,
    /// How long one tick waits for a submitted tx's receipt before leaving it
    /// for the next tick.
    pub receipt_timeout: Duration,
    /// Last head seen per chain (parallel to `chains`): a head below the
    /// watermark is a rewind (a snapshot revert) that un-minted relayed
    /// messages — their records are parked, never re-relayed (a stale
    /// derivation, I6; `POST /v2/reattest/{nonce}` is the deliberate way
    /// back).
    pub heads: Vec<std::sync::atomic::AtomicU64>,
}

impl Relayer {
    /// The steady loop: one pass per interval; per-message failures are logged
    /// and counted, never fatal.
    pub async fn run(self, poll: Duration) {
        let mut ticker = tokio::time::interval(poll);
        loop {
            ticker.tick().await;
            if let Err(e) = self.tick().await {
                tracing::warn!(error = %e, "cctp relayer pass failed; will retry");
            }
        }
    }

    /// One pass: gas alerts, destination-rewind reconciliation, then every
    /// SIGNED record once.
    pub async fn tick(&self) -> Result<usize> {
        if let Err(e) = self.gas_alerts().await {
            tracing::warn!(error = %e, "cctp gas alert read failed");
        }
        if let Err(e) = self.reconcile_rewinds().await {
            tracing::warn!(error = %e, "cctp destination rewind check failed");
        }
        let signed = self.store.with(|state| {
            Ok(state
                .messages
                .iter()
                .filter(|m| m.state == RelayState::Signed)
                .cloned()
                .collect::<Vec<_>>())
        })?;
        let mut confirmed = 0usize;
        for record in &signed {
            self.metrics.relay_attempts.fetch_add(1, Ordering::Relaxed);
            match self.relay(record).await {
                Ok(true) => confirmed += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(nonce = %record.nonce, error = %e, "cctp relay pass failed for a message; will retry");
                }
            }
        }
        Ok(confirmed)
    }

    /// A destination head below its watermark is a rewind: any relayed
    /// message whose nonce is no longer consumed has lost its mint — park it
    /// (the destination's rollback is a deliberate test action; re-minting
    /// into it would fight the revert). The chain stays the dedup truth: a
    /// still-consumed nonce means the mint predates the rewind and stands.
    async fn reconcile_rewinds(&self) -> Result<()> {
        for (index, chain) in self.chains.iter().enumerate() {
            let head = chain.rpc.block_number().await?;
            let watermark = self.heads[index].load(Ordering::Relaxed);
            let rewound = watermark > 0 && head < watermark;
            if watermark > 0 && !rewound {
                self.heads[index].store(head, Ordering::Relaxed);
                continue;
            }
            if rewound {
                tracing::warn!(
                    chain = chain.name,
                    head,
                    watermark,
                    "destination rewound; reconciling relayed messages"
                );
            }
            // The full pass also runs at boot (`watermark == 0`): a restart
            // after a revert or a re-pin must reconcile, not only a live
            // regression. After the first tick the watermark gates it.
            let relayed: Vec<MessageRecord> = self.store.with(|state| {
                Ok(state
                    .messages
                    .iter()
                    .filter(|m| {
                        m.destination_domain == chain.domain && m.state == RelayState::Relayed
                    })
                    .cloned()
                    .collect::<Vec<_>>())
            })?;
            for record in relayed {
                if self.used_nonce(chain, record.nonce).await? != U256::ZERO {
                    continue;
                }
                self.park(
                    record.nonce,
                    format!(
                        "destination rewound or re-pinned: the mint is no longer on-chain at block {head} (re-run the transfer, or reattest to re-arm)"
                    ),
                )?;
            }
            self.heads[index].store(head, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Read the relay key's balance on every chain and raise the alerts.
    pub async fn gas_alerts(&self) -> Result<()> {
        for chain in &self.chains {
            let balance = chain.native_balance(self.sender.address()).await?;
            self.metrics.set_balance(chain.name, balance);
            if balance < self.balance_critical {
                self.metrics.gas_critical.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    chain = chain.name,
                    relayer = %self.sender.address(),
                    balance = %balance,
                    critical = %self.balance_critical,
                    "cctp relayer gas CRITICAL"
                );
            } else if balance < self.balance_low {
                self.metrics.gas_low.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    chain = chain.name,
                    relayer = %self.sender.address(),
                    balance = %balance,
                    low = %self.balance_low,
                    "cctp relayer gas low"
                );
            }
        }
        Ok(())
    }

    /// Relay one SIGNED record. `Ok(true)` = confirmed (or reconciled as
    /// already consumed); `Ok(false)` = still in flight, re-checked next tick.
    async fn relay(&self, record: &MessageRecord) -> Result<bool> {
        let chain = by_domain(&self.chains, record.destination_domain)?;
        // Hook-carrying messages are consumed through the wrapper (their
        // `destinationCaller` pins it, §15.4); plain ones by this bridge's
        // key. A message pinned to someone else is served by the Iris but
        // never relayable here: park it instead of paying for the real
        // revert.
        let decoded = record.decoded()?;
        let caller = decoded.header.destination_caller;
        let (target, consumer) = if decoded.body.hook_data.is_empty() {
            (MESSAGE_TRANSMITTER_V2, self.sender.address())
        } else {
            let wrapper = self
                .store
                .with(|state| Ok(state.hooks.get(&record.destination_domain).copied()))?
                .ok_or_else(|| Error::BadRequest {
                    reason: format!(
                        "message {} carries a hook but domain {} has no wrapper (the boot step deploys one)",
                        record.nonce, record.destination_domain
                    ),
                })?;
            (wrapper, wrapper)
        };
        if caller != B256::ZERO && caller != consumer.into_word() {
            self.park(
                record.nonce,
                format!("destinationCaller {caller} is not this bridge's consumer ({consumer})"),
            )?;
            return Ok(false);
        }
        // The burn must still exist on the source: a reattest of a record
        // whose burn a re-pin or rewind removed must never mint (I6 — stale
        // derivations are parked, never re-owned).
        let source = by_domain(&self.chains, record.source_domain)?;
        let burn: Option<Box<serde_json::value::RawValue>> = source
            .rpc
            .call("eth_getTransactionByHash", (record.burn_tx,))
            .await?;
        if burn.is_none() {
            self.park(
                record.nonce,
                "the source burn no longer exists (a re-pin or rewind); re-run the transfer".into(),
            )?;
            return Ok(false);
        }
        if self.used_nonce(chain, record.nonce).await? != U256::ZERO {
            self.mark_relayed(record.nonce, None)?;
            tracing::info!(nonce = %record.nonce, chain = chain.name, "cctp nonce already consumed on-chain; reconciled as relayed");
            return Ok(true);
        }
        if let Some(hash) = record.relay_tx {
            if let Some(receipt) = receipt(&chain.rpc, hash).await? {
                return self.settle(chain, record, &receipt).await;
            }
            if self.sender.in_pool(&chain.rpc, hash).await? {
                return Ok(false);
            }
            tracing::warn!(
                nonce = %record.nonce,
                tx = %hash,
                chain = chain.name,
                "cctp relay tx left the pool without a receipt (restart?); re-sending"
            );
        }
        let attestation = record.attestation.clone().ok_or_else(|| Error::BadRequest {
            reason: format!("message {} is signed without an attestation", record.nonce),
        })?;
        let calldata = if decoded.body.hook_data.is_empty() {
            receive_message_calldata(&record.message, &attestation)
        } else {
            crate::hook::relay_calldata(&record.message, &attestation)
        };
        match self
            .sender
            .send(&chain.rpc, target, &calldata, U256::ZERO)
            .await
        {
            Ok(hash) => {
                self.store.with(|state| {
                    if let Some(m) = state.messages.iter_mut().find(|m| m.nonce == record.nonce) {
                        m.relay_tx = Some(hash);
                        m.attempts += 1;
                    }
                    Ok(())
                })?;
                self.store.save()?;
                tracing::info!(nonce = %record.nonce, chain = chain.name, tx = %hash, "cctp receiveMessage submitted");
                match wait_for_receipt(&chain.rpc, hash, self.receipt_timeout).await? {
                    Some(receipt) => self.settle(chain, record, &receipt).await,
                    None => Ok(false),
                }
            }
            Err(e) => {
                let attempts = self.bump_attempts(record.nonce)?;
                if matches!(e, Error::Rpc(_)) && attempts >= MAX_SEND_ATTEMPTS {
                    self.park(record.nonce, format!("send refused {attempts} times: {e}"))?;
                } else {
                    tracing::warn!(nonce = %record.nonce, attempts, error = %e, "cctp receiveMessage submission failed; will retry");
                }
                Ok(false)
            }
        }
    }

    /// A mined receipt: success marks RELAYED; a revert is either the
    /// already-consumed case (reconcile) or a definitive park.
    async fn settle(
        &self,
        chain: &Chain,
        record: &MessageRecord,
        receipt: &TransactionReceipt,
    ) -> Result<bool> {
        if receipt.status() {
            self.mark_relayed(record.nonce, Some(receipt.transaction_hash))?;
            tracing::info!(
                nonce = %record.nonce,
                chain = chain.name,
                tx = %receipt.transaction_hash,
                "cctp mint confirmed (MessageReceived)"
            );
            return Ok(true);
        }
        if self.used_nonce(chain, record.nonce).await? != U256::ZERO {
            self.mark_relayed(record.nonce, Some(receipt.transaction_hash))?;
            tracing::info!(nonce = %record.nonce, chain = chain.name, "cctp relay reverted but the nonce is consumed; reconciled as relayed");
            return Ok(true);
        }
        self.park(
            record.nonce,
            format!("receiveMessage reverted (tx {})", receipt.transaction_hash),
        )?;
        Ok(false)
    }

    async fn used_nonce(&self, chain: &Chain, nonce: B256) -> Result<U256> {
        crate::transfer::eth_call(
            &chain.rpc,
            MESSAGE_TRANSMITTER_V2,
            &IMessageTransmitterV2::usedNoncesCall { nonce },
        )
        .await
    }

    fn mark_relayed(&self, nonce: B256, tx: Option<B256>) -> Result<()> {
        self.store.with(|state| {
            if let Some(m) = state.messages.iter_mut().find(|m| m.nonce == nonce) {
                m.state = RelayState::Relayed;
                m.relayed_ms = Some(crate::now_ms());
                if tx.is_some() {
                    m.relay_tx = tx;
                }
            }
            Ok(())
        })?;
        self.metrics.relays_confirmed.fetch_add(1, Ordering::Relaxed);
        self.store.save()
    }

    fn park(&self, nonce: B256, reason: String) -> Result<()> {
        tracing::error!(%nonce, reason, "cctp message PARKED (never dropped; /health shows it)");
        self.store.with(|state| {
            if let Some(m) = state.messages.iter_mut().find(|m| m.nonce == nonce) {
                m.state = RelayState::Parked;
                m.park_reason = Some(reason);
            }
            Ok(())
        })?;
        self.metrics.relays_parked.fetch_add(1, Ordering::Relaxed);
        self.store.save()
    }

    fn bump_attempts(&self, nonce: B256) -> Result<u32> {
        let attempts = self.store.with(|state| {
            let mut attempts = 0;
            if let Some(m) = state.messages.iter_mut().find(|m| m.nonce == nonce) {
                m.attempts += 1;
                attempts = m.attempts;
            }
            Ok(attempts)
        })?;
        self.store.save()?;
        Ok(attempts)
    }
}
