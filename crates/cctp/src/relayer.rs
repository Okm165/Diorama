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
    receipt, receive_message_calldata, wait_for_receipt, IMessageTransmitterV2, Message, TxSender,
};
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::TransactionReceipt;
use mirror_config::RelayerMode;
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
    /// `Auto`: the tick relays every SIGNED record. `Manual`: only
    /// [`Relayer::relay_now`] (the control trigger) relays — expiry and
    /// reattestation tests then decide the exact block a message lands.
    pub mode: RelayerMode,
    /// Gas-alert thresholds (daloc's defaults, wei).
    pub balance_low: U256,
    pub balance_critical: U256,
    /// How long one tick waits for a submitted tx's receipt before leaving it
    /// for the next tick.
    pub receipt_timeout: Duration,
    /// Last head seen per chain (parallel to `chains`): a head below the
    /// watermark is a rewind (a snapshot revert), a fork base above it a
    /// forward re-pin (the epoch model) — either un-minted relayed messages,
    /// so their records are parked, never re-relayed (a stale derivation,
    /// I6; `POST /v2/reattest/{nonce}` is the deliberate way back).
    pub heads: Vec<std::sync::atomic::AtomicU64>,
}

/// Whether the message pins a `destinationCaller` this bridge cannot serve
/// (zero = anyone): such a message is parked instead of paying for the real
/// revert — the Iris still serves it, it is just not relayable here.
fn caller_mismatch(caller: B256, consumer: Address) -> bool {
    caller != B256::ZERO && caller != consumer.into_word()
}

impl Relayer {
    /// The steady loop: one pass per interval; per-message failures are logged
    /// and counted, never fatal.
    pub async fn run(self: Arc<Self>, poll: Duration) {
        let mut ticker = tokio::time::interval(poll);
        let mut last_status = 0u64;
        loop {
            ticker.tick().await;
            if let Err(e) = self.tick().await {
                tracing::warn!(error = %e, "cctp relayer pass failed; will retry");
            }
            // The bridge's heartbeat (the mirrors' 60 s cadence): the store's
            // own counts, so the queue's state is visible in the logs, not
            // only through /health.
            let now = mirror_config::boot::now_ms();
            if now.saturating_sub(last_status) >= 60_000 {
                last_status = now;
                match self.store.counts() {
                    Ok(c) => tracing::info!(
                        pending = c.pending,
                        signed = c.signed,
                        relayed = c.relayed,
                        parked = c.parked,
                        "bridge status"
                    ),
                    Err(e) => tracing::warn!(error = %e, "bridge status counts failed"),
                }
            }
        }
    }

    /// One pass: gas alerts, destination-window reconciliation, then (in
    /// `auto` mode) every SIGNED record once. Manual mode keeps the alerts and
    /// the window check — both only *park*, never relay.
    pub async fn tick(&self) -> Result<usize> {
        // Self-fund before the alerts: a re-pin wipes V's state, and the
        // relay wallet's stipend with it (measured: the gas alert stayed
        // critical until the next boot) — funding again keeps the alerts
        // honest, and the funding itself is idempotent.
        if let Err(e) =
            crate::service::fund_relayer(&self.chains, self.sender.address(), self.balance_low).await
        {
            tracing::warn!(error = %e, "cctp relayer stipend check failed");
        }
        if let Err(e) = self.gas_alerts().await {
            tracing::warn!(error = %e, "cctp gas alert read failed");
        }
        if let Err(e) = self.reconcile_destinations().await {
            tracing::warn!(error = %e, "cctp destination window check failed");
        }
        if self.mode == RelayerMode::Manual {
            return Ok(0);
        }
        self.relay_now(None).await
    }

    /// Relay the SIGNED records now: one by `nonce`, every one when `None`
    /// (the auto tick and the manual trigger share this path). Returns the
    /// number confirmed in this pass.
    pub async fn relay_now(&self, nonce: Option<B256>) -> Result<usize> {
        let signed = self.store.with(|state| {
            Ok(state
                .messages
                .iter()
                .filter(|m| m.state == RelayState::Signed && nonce.is_none_or(|n| m.nonce == n))
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

    /// A destination whose history window moved under its watermark has lost
    /// mints: a head below the watermark is a rewind, a fork base above it a
    /// forward re-pin (the epoch model re-pins live — a restart's boot pass
    /// must not be the only way back). Any relayed message whose nonce is no
    /// longer consumed is parked (the destination's reset is a deliberate
    /// action; re-minting into it would fight it). The chain stays the dedup
    /// truth: a still-consumed nonce means the mint predates the move and
    /// stands.
    async fn reconcile_destinations(&self) -> Result<()> {
        for (index, chain) in self.chains.iter().enumerate() {
            let head = chain.rpc.block_number().await?;
            let pin = chain.rpc.fork_block().await?;
            let watermark = self.heads[index].load(Ordering::Relaxed);
            // The watcher's window predicate, shared: a watermark above the
            // head is a rewind, below the fork base a forward re-pin.
            let moved = crate::watcher::cursor_off_base(watermark, head, pin);
            if watermark > 0 && !moved {
                self.heads[index].store(head, Ordering::Relaxed);
                continue;
            }
            if moved {
                tracing::warn!(
                    chain = chain.name,
                    head,
                    watermark,
                    pin = ?pin,
                    "destination rewound or re-pinned; reconciling relayed messages"
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
        let decoded = record.decoded()?;
        let (target, consumer) = self.consumer(record, &decoded)?;
        if caller_mismatch(decoded.header.destination_caller, consumer) {
            return self.refuse(
                record.nonce,
                format!(
                    "destinationCaller {} is not this bridge's consumer ({consumer})",
                    decoded.header.destination_caller
                ),
            );
        }
        // A re-pin removes the wrapper's code (it was deployed on the old
        // base): a call to it would succeed vacuously and the mint would be
        // lost while the record read `relayed`. Refuse by name instead — the
        // boot redeploys on the next restart.
        if !decoded.body.hook_data.is_empty()
            && !crate::transfer::has_code(&chain.rpc, target).await?
        {
            return self.refuse(
                record.nonce,
                format!(
                    "the hook wrapper {target} has no code on {} (a re-pin?); restart the bridge to redeploy it",
                    chain.name
                ),
            );
        }
        // The burn must still exist on the source: a reattest of a record
        // whose burn a re-pin or rewind removed must never mint (I6 — stale
        // derivations are parked, never re-owned).
        if !self.burn_exists(record).await? {
            return self.refuse(
                record.nonce,
                "the source burn no longer exists (a re-pin or rewind); re-run the transfer".into(),
            );
        }
        if self.used_nonce(chain, record.nonce).await? != U256::ZERO {
            self.mark_relayed(record.nonce, None)?;
            tracing::info!(nonce = %record.nonce, chain = chain.name, "cctp nonce already consumed on-chain; reconciled as relayed");
            return Ok(true);
        }
        if let Some(settled) = self.resume_in_flight(chain, record).await? {
            return Ok(settled);
        }
        self.submit(chain, record, target, &decoded).await
    }

    /// The call target and the caller the message pins: the transmitter for a
    /// plain message, the hook wrapper for a hook-carrying one (§15.4 pins the
    /// wrapper as `destinationCaller`). A hook without a deployed wrapper is a
    /// named refusal — the boot step deploys one.
    fn consumer(&self, record: &MessageRecord, decoded: &Message) -> Result<(Address, Address)> {
        if decoded.body.hook_data.is_empty() {
            return Ok((MESSAGE_TRANSMITTER_V2, self.sender.address()));
        }
        let wrapper = self
            .store
            .with(|state| Ok(state.hooks.get(&record.destination_domain).copied()))?
            .ok_or_else(|| Error::bad_request(format!(
                    "message {} carries a hook but domain {} has no wrapper (the boot step deploys one)",
                    record.nonce, record.destination_domain
                ),))?;
        Ok((wrapper, wrapper))
    }

    /// Whether the source chain still carries the burn transaction.
    async fn burn_exists(&self, record: &MessageRecord) -> Result<bool> {
        let source = by_domain(&self.chains, record.source_domain)?;
        let burn: Option<alloy_rpc_types_eth::Transaction> = source
            .rpc
            .call(crate::TX_BY_HASH, (record.burn_tx,))
            .await?;
        Ok(burn.is_some())
    }

    /// A previous attempt's tx: a receipt settles it; still in the pool is
    /// "in flight"; left the pool without a receipt (a restart) means the
    /// caller re-sends. `None` = no previous attempt (or re-send).
    async fn resume_in_flight(&self, chain: &Chain, record: &MessageRecord) -> Result<Option<bool>> {
        let Some(hash) = record.relay_tx else {
            return Ok(None);
        };
        if let Some(receipt) = receipt(&chain.rpc, hash).await? {
            return Ok(Some(self.settle(chain, record, &receipt).await?));
        }
        if self.sender.in_pool(&chain.rpc, hash).await? {
            return Ok(Some(false));
        }
        tracing::warn!(
            nonce = %record.nonce,
            tx = %hash,
            chain = chain.name,
            "cctp relay tx left the pool without a receipt (restart?); re-sending"
        );
        Ok(None)
    }

    /// Submit `receiveMessage` (plain) or the wrapper's `relay` (hook), record
    /// the attempt, and settle it or keep it in flight. A send refused after
    /// `MAX_SEND_ATTEMPTS` transport faults is parked, never retried forever.
    async fn submit(
        &self,
        chain: &Chain,
        record: &MessageRecord,
        target: Address,
        decoded: &Message,
    ) -> Result<bool> {
        let attestation = record.attestation.clone().ok_or_else(|| Error::bad_request(format!("message {} is signed without an attestation", record.nonce),))?;
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
                self.store.update_message(
                    |m| m.nonce == record.nonce,
                    |m| {
                        m.relay_tx = Some(hash);
                        m.attempts += 1;
                    },
                )?;
                tracing::info!(
                    nonce = %record.nonce,
                    chain = chain.name,
                    tx = %hash,
                    consumer = if decoded.body.hook_data.is_empty() { "transmitter" } else { "hook wrapper" },
                    "cctp relay submitted"
                );
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
        // Settle by the *effect*, never the status: the nonce is the chain's
        // dedup truth, consumed exactly when the mint happened. A status alone
        // cannot prove a mint — the wrapper's `relay` is non-atomic by design
        // and a call to a code-less address (a wrapper a re-pin removed)
        // succeeds vacuously; trusting the status would mark a lost message
        // relayed (measured 2026-10-23).
        if self.used_nonce(chain, record.nonce).await? != U256::ZERO {
            self.mark_relayed(record.nonce, Some(receipt.transaction_hash))?;
            tracing::info!(
                nonce = %record.nonce,
                chain = chain.name,
                tx = %receipt.transaction_hash,
                "cctp mint confirmed (the destination nonce is consumed)"
            );
            return Ok(true);
        }
        if receipt.status() {
            self.park(
                record.nonce,
                format!(
                    "relay tx {} succeeded but the nonce is unconsumed — the consumer did not mint (a stale hook wrapper after a re-pin?)",
                    receipt.transaction_hash
                ),
            )?;
            return Ok(false);
        }
        self.park(
            record.nonce,
            format!("receiveMessage reverted (tx {})", receipt.transaction_hash),
        )?;
        Ok(false)
    }

    async fn used_nonce(&self, chain: &Chain, nonce: B256) -> Result<U256> {
        chain
            .rpc
            .call_decode(
                MESSAGE_TRANSMITTER_V2,
                &IMessageTransmitterV2::usedNoncesCall { nonce },
            )
            .await
            .map_err(Error::from)
    }

    /// Park a record and report the refusal (`false` = not relayed): the one
    /// shape every gate in `relay` uses — the reason names what was seen.
    fn refuse(&self, nonce: B256, reason: String) -> Result<bool> {
        self.park(nonce, reason)?;
        Ok(false)
    }

    fn mark_relayed(&self, nonce: B256, tx: Option<B256>) -> Result<()> {
        self.store.update_message(
            |m| m.nonce == nonce,
            |m| {
                m.state = RelayState::Relayed;
                m.relayed_ms = Some(crate::now_ms());
                if tx.is_some() {
                    m.relay_tx = tx;
                }
            },
        )?;
        // Counted only after the state is persisted: a confirm that failed to
        // persist must not be counted.
        self.metrics.relays_confirmed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn park(&self, nonce: B256, reason: String) -> Result<()> {
        tracing::error!(%nonce, reason, "cctp message PARKED (never dropped; /health shows it)");
        self.store.update_message(
            |m| m.nonce == nonce,
            |m| {
                m.state = RelayState::Parked;
                m.park_reason = Some(reason);
            },
        )?;
        self.metrics.relays_parked.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// One more send attempt on the record; the new count (0 when the record
    /// does not exist — nothing to count).
    fn bump_attempts(&self, nonce: B256) -> Result<u32> {
        Ok(self
            .store
            .update_message(
                |m| m.nonce == nonce,
                |m| {
                    m.attempts += 1;
                    m.attempts
                },
            )?
            .unwrap_or(0))
    }
}
