//! The watcher: both chains' `MessageSent` events → the persisted attestation
//! store (§15.4).
//!
//! One pass: scan each chain from the configured start block to head
//! (`eth_getLogs`, chunked — the emitter is the pinned MessageTransmitterV2,
//! topic0 the measured `MessageSent(bytes)` hash), decode each log into a
//! [`MessageRecord`], patch the attester fields (`nonce = burn-tx hash`,
//! `finalityThresholdExecuted = 2000`, `feeExecuted = 0`,
//! `expirationBlock = configured`, §15.3), and sign every record whose
//! emulated PENDING window has elapsed.
//!
//! **Restart backfill** (§15.4): the first scan of every run starts at
//! `start_block` — a burn awaiting attestation across a restart is re-read
//! and re-signed — and later passes continue from the scanned head. Dedupe is
//! by `(sourceDomain, nonce)`; the nonce is the burn tx hash, so a re-scan is
//! idempotent by construction.
//!
//! Known boundary: a source-chain **re-fork** (T's epoch reset) invalidates
//! watched burns. The records survive; an operator clears the state dir (or
//! sets a fresh start block) when a reset should drop history. The nonce
//! policy is the plan's deterministic one (burn tx hash, §15.3), so a
//! transaction emitting **two** burns collides on the nonce: the second is
//! refused with a warning (the devnet lanes burn once per tx).
use crate::attester::Attester;
use crate::chains::{Chain, Domain, FINALITY_THRESHOLD_FINALIZED, MESSAGE_TRANSMITTER_V2};
use crate::error::{Error, Result};
use crate::metrics::CctpMetrics;
use crate::store::{MessageRecord, RelayState, Store};
use crate::transfer::{Message, IMessageTransmitterV2};
use alloy_primitives::{Bytes, B256, U256};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::SolEvent;
use std::sync::Arc;
use std::time::Duration;

/// One chain's watcher. The bin spawns one per served chain.
pub struct Watcher {
    pub chain: Chain,
    /// The only destination this chain's messages are served for (the peer
    /// mirror's domain): a burn to a third chain is not this bridge's to
    /// relay, and is skipped with a warning (never silently).
    pub destination_domain: Domain,
    /// The peer chain — the *destination* of this watcher's messages, whose
    /// head anchors the served `expirationBlock` (the field is checked
    /// against `block.number` on the destination, §15.1).
    pub destination: Chain,
    pub store: Arc<Store>,
    pub attester: Arc<Attester>,
    /// First block scanned every boot (the deploy block of the CCTP contracts
    /// on that chain; 0 = genesis, correct but slow on a deep fork).
    pub start_block: u64,
    /// Where the *next* scan resumes inside this run (0 = not scanned yet, so
    /// the first pass backfills from `start_block`; later passes continue from
    /// the last scanned head). Not persisted: every boot re-reads from
    /// `start_block` by design (§15.4's restart self-heal) — the dedupe by
    /// `(sourceDomain, nonce)` makes the re-read idempotent.
    pub scanned_upto: std::sync::atomic::AtomicU64,
    /// `eth_getLogs` range per call.
    pub chunk_blocks: u64,
    /// The emulated PENDING window: an attestation is produced this long after
    /// the burn is watched (0 = immediately, §15.5's PENDING emulation).
    pub finality_delay: Duration,
    /// The served `expirationBlock` window in blocks (0 = no expiry, the
    /// contract's own rule). Relative to the *destination's* head: the field
    /// is compared against `block.number` at receive time, so an absolute
    /// value would be meaningless on a real-height chain (the scratch chains
    /// sit at ~49M — measured).
    pub expiration_blocks: u64,
    /// The last fork base the below-pin warning named (0 = none yet): the
    /// scan runs every tick, but the clamp is a fact about the config, not
    /// per-pass news.
    pub clamp_warned: std::sync::atomic::AtomicU64,
    pub metrics: Arc<CctpMetrics>,
}

impl Watcher {
    /// The steady loop: one pass per interval; failures are logged and
    /// counted, never fatal (a chain that is down must not stop the other).
    pub async fn run(self, poll: Duration) {
        let mut ticker = tokio::time::interval(poll);
        loop {
            ticker.tick().await;
            if let Err(e) = self.tick().await {
                self.metrics.scan_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(chain = self.chain.name, error = %e, "cctp watcher pass failed; will retry");
            }
        }
    }

    /// One pass: absorb new logs, sign everything due, persist if anything
    /// moved.
    pub async fn tick(&self) -> Result<usize> {
        let seen = self.scan().await?;
        let signed = self.sign_due()?;
        if seen > 0 || signed > 0 {
            self.store.save()?;
        }
        Ok(seen)
    }

    /// Scan from the in-run cursor (boot: `start_block`) to head in chunks;
    /// returns the number of new records.
    ///
    /// The scan never reads below the fork base: pre-pin blocks are the
    /// *source's* history — anvil forwards such ranges upstream, which
    /// rejects them (measured: the public HyperEVM RPC caps at 1000 blocks,
    /// drpc refuses ranges over 10000) and their events are real source
    /// burns, not this devnet's. A fork base above the cursor (a re-pin) or a
    /// head below it (a snapshot revert) means the blocks the cursor points
    /// at are gone: their records are parked (stale derivations, I6 — never
    /// relayed) and the scan restarts from the base.
    pub async fn scan(&self) -> Result<usize> {
        let head = self.chain.rpc.block_number().await?;
        let pin = self.chain.rpc.fork_block().await?;
        let last = self.scanned_upto.load(std::sync::atomic::Ordering::Relaxed);
        let mut from = self.resume_from(last, head, pin).await?;
        let mut seen = 0usize;
        while from <= head {
            let to = from.saturating_add(self.chunk_blocks - 1).min(head);
            let logs = self.message_logs(from, to).await?;
            // One head read per chunk: the expiration window's anchor (the
            // destination's block coordinate, §15.1).
            let expiration = self.expiration_block().await?;
            for log in &logs {
                seen += self.absorb(log, expiration)?;
            }
            from = to + 1;
        }
        // The cursor is the highest scanned block of the *current* chain:
        // a re-pin or a rewind resets it to the new base (the rescan above),
        // so it never skips a block.
        self.scanned_upto
            .store(head, std::sync::atomic::Ordering::Relaxed);
        self.metrics
            .last_scan_block
            .store(head, std::sync::atomic::Ordering::Relaxed);
        Ok(seen)
    }

    /// Where this scan resumes: the in-run cursor + 1 (the configured start on
    /// boot), clamped at the fork base.
    ///
    /// The scan never reads below the fork base: pre-pin blocks are the
    /// *source's* history — anvil forwards such ranges upstream, which
    /// rejects them (measured: the public HyperEVM RPC caps at 1000 blocks,
    /// drpc refuses ranges over 10000) and their events are real source
    /// burns, not this devnet's. A fork base above the cursor (a re-pin) or a
    /// head below it (a snapshot revert) means the blocks the cursor points
    /// at are gone: their records are parked (stale derivations, I6 — never
    /// relayed) and the scan restarts from the base.
    async fn resume_from(&self, last: u64, head: u64, pin: Option<u64>) -> Result<u64> {
        let cursor_off_base = cursor_off_base(last, head, pin);
        if cursor_off_base {
            tracing::warn!(
                chain = self.chain.name,
                head,
                cursor = last,
                pin = ?pin,
                "scan cursor is above the chain's base (re-pin or rewind); rescanning and parking stale records"
            );
        }
        // Also on boot: a restart after a re-pin inherits records from a
        // world that no longer exists — reconcile them, don't trust them.
        if cursor_off_base || last == 0 {
            self.park_vanished().await?;
        }
        let base = if last > 0 && last > head {
            // A rewind: everything above the head is invalid, so the cursor
            // cannot resume — restart at the configured start (the pin clamp
            // below still applies).
            self.start_block
        } else {
            match last {
                0 => self.start_block,
                n => (n + 1).max(self.start_block),
            }
        };
        let floor = pin.map(|pin| pin.saturating_add(1));
        Ok(match floor {
            Some(floor) if self.start_block < floor => {
                if self.clamp_warned.swap(floor, std::sync::atomic::Ordering::Relaxed) != floor {
                    tracing::warn!(
                        chain = self.chain.name,
                        configured_start = self.start_block,
                        pin = floor - 1,
                        "configured start block is below the fork pin; scanning from the pin"
                    );
                }
                base.max(floor)
            }
            _ => base,
        })
    }

    /// The `MessageSent` logs of one block range.
    async fn message_logs(&self, from: u64, to: u64) -> Result<Vec<Log>> {
        let filter = Filter::new()
            .address(MESSAGE_TRANSMITTER_V2)
            .event_signature(IMessageTransmitterV2::MessageSent::SIGNATURE_HASH)
            .from_block(from)
            .to_block(to);
        let logs: Vec<Log> = self.chain.rpc.call("eth_getLogs", (filter,)).await?;
        Ok(logs)
    }

    /// Park the non-relayed records this chain's history no longer backs: a
    /// re-pin or a rewind leaves their burn transactions absent, so they are
    /// stale derivations (I6) — kept visible, never relayed.
    async fn park_vanished(&self) -> Result<usize> {
        let candidates: Vec<B256> = self.store.with(|state| {
            Ok(state
                .messages
                .iter()
                .filter(|m| {
                    m.source_domain == self.chain.domain
                        && matches!(m.state, RelayState::Pending | RelayState::Signed)
                })
                .map(|m| m.burn_tx)
                .collect::<Vec<_>>())
        })?;
        let mut parked = 0usize;
        for burn_tx in candidates {
            let exists: Option<alloy_rpc_types_eth::Transaction> = self
                .chain
                .rpc
                .call("eth_getTransactionByHash", (burn_tx,))
                .await?;
            if exists.is_some() {
                continue;
            }
            self.store.with(|state| {
                if let Some(record) = state.messages.iter_mut().find(|m| m.burn_tx == burn_tx) {
                    record.state = RelayState::Parked;
                    record.park_reason = Some(
                        "source chain re-pinned or rewound: the burn transaction no longer exists"
                            .into(),
                    );
                }
                Ok(())
            })?;
            parked += 1;
        }
        if parked > 0 {
            self.metrics
                .relays_parked
                .fetch_add(parked as u64, std::sync::atomic::Ordering::Relaxed);
            self.store.save()?;
            tracing::warn!(
                chain = self.chain.name,
                parked,
                "stale cctp records parked (their burn transactions vanished)"
            );
        }
        Ok(parked)
    }

    /// The served `expirationBlock`: 0 = no expiry; otherwise the
    /// destination's head plus the configured window — the reattest route
    /// re-anchors it the same way (iris.rs).
    async fn expiration_block(&self) -> Result<U256> {
        if self.expiration_blocks == 0 {
            return Ok(U256::ZERO);
        }
        let head = self.destination.rpc.block_number().await?;
        Ok(U256::from(head.saturating_add(self.expiration_blocks)))
    }

    /// Decode one `MessageSent` log into a record. The log must carry its
    /// transaction hash (it is the deterministic nonce) and its block; a log
    /// outside this bridge's lanes is skipped (`None`).
    fn decode_record(&self, log: &Log, expiration: U256) -> Result<Option<MessageRecord>> {
        let sent = decode_message_sent(log)?;
        let parsed = Message::decode(&sent.raw_message)?;
        if parsed.header.source_domain != self.chain.domain
            || parsed.header.destination_domain != self.destination_domain
        {
            tracing::warn!(
                chain = self.chain.name,
                tx = %sent.burn_tx,
                source = %parsed.header.source_domain,
                destination = %parsed.header.destination_domain,
                "MessageSent outside this bridge's lanes; skipped"
            );
            return Ok(None);
        }
        Ok(Some(self.record_of(sent, &parsed, expiration)))
    }

    /// The record one in-lane message makes: the deterministic nonce is the
    /// burn tx hash (§15.3); the raw event message carries `EMPTY_NONCE` and
    /// the served artifact fills it.
    fn record_of(&self, sent: MessageSent, parsed: &Message, expiration: U256) -> MessageRecord {
        let burn_tx = sent.burn_tx;
        let served = parsed.with_attester_fields(
            burn_tx,
            FINALITY_THRESHOLD_FINALIZED,
            U256::ZERO,
            expiration,
        );
        let now = crate::now_ms();
        MessageRecord {
            source_domain: parsed.header.source_domain,
            destination_domain: parsed.header.destination_domain,
            nonce: burn_tx,
            burn_tx,
            burn_block: sent.burn_block,
            raw_message: sent.raw_message,
            message_hash: served.keccak(),
            message: served.encode(),
            attestation: None,
            state: RelayState::Pending,
            created_ms: now,
            attest_after_ms: now + self.finality_delay.as_millis() as u64,
            relay_tx: None,
            relayed_ms: None,
            attempts: 0,
            park_reason: None,
        }
    }

    /// Insert one record (or reconcile a re-scan of it) and report it.
    fn insert(&self, record: MessageRecord) -> Result<usize> {
        let burn_tx = record.burn_tx;
        let burn_block = record.burn_block;
        let source_domain = record.source_domain;
        let destination_domain = record.destination_domain;
        // `Some(true)` = the same event re-scanned (the restart backfill,
        // normal); `Some(false)` = a different burn with the same nonce (two
        // burns in one tx — refused loudly).
        let duplicate = self.store.with(move |state| {
            if let Some(existing) = state
                .messages
                .iter()
                .find(|m| m.nonce == burn_tx && m.source_domain == source_domain)
            {
                return Ok(Some(existing.raw_message == record.raw_message));
            }
            state.messages.push(record);
            Ok(None)
        })?;
        match duplicate {
            None => {
                self.metrics
                    .messages_seen
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::info!(
                    chain = self.chain.name,
                    tx = %burn_tx,
                    block = burn_block,
                    source = %source_domain,
                    destination = %destination_domain,
                    "cctp burn watched"
                );
                Ok(1)
            }
            Some(true) => Ok(0),
            Some(false) => {
                tracing::warn!(
                    chain = self.chain.name,
                    tx = %burn_tx,
                    "duplicate nonce with a different message (a second burn in the same tx?); kept the first"
                );
                Ok(0)
            }
        }
    }

    /// Decode one `MessageSent` log into a record.
    fn absorb(&self, log: &Log, expiration: U256) -> Result<usize> {
        match self.decode_record(log, expiration)? {
            Some(record) => self.insert(record),
            None => Ok(0),
        }
    }

    /// Sign every PENDING record whose window elapsed (the PENDING → SIGNED
    /// transition); returns how many moved.
    fn sign_due(&self) -> Result<usize> {
        let now = crate::now_ms();
        let mut signed = 0usize;
        self.store.with(|state| {
            for record in state.messages.iter_mut() {
                if record.state != RelayState::Pending || now < record.attest_after_ms {
                    continue;
                }
                record.attestation = Some(self.attester.sign(&record.message)?);
                record.state = RelayState::Signed;
                signed += 1;
                tracing::info!(
                    nonce = %record.nonce,
                    destination = %record.destination_domain,
                    "cctp message attested (PENDING -> SIGNED)"
                );
            }
            Ok(())
        })?;
        if signed > 0 {
            self.metrics
                .messages_signed
                .fetch_add(signed as u64, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(signed)
    }
}

/// The `MessageSent` log's parts: its transaction hash (the deterministic
/// nonce), its block, and the raw message bytes. Both the hash and the block
/// are required — a log without them cannot be turned into a relayable
/// record, and skipping it silently would drop a burn.
fn decode_message_sent(log: &Log) -> Result<MessageSent> {
    let decoded =
        IMessageTransmitterV2::MessageSent::decode_log(&log.inner).map_err(|source| Error::Abi {
            what: "MessageSent event",
            source,
        })?;
    let burn_tx = log.transaction_hash.ok_or_else(|| Error::BadRequest {
        reason: "MessageSent log without a transaction hash".into(),
    })?;
    let burn_block = log.block_number.ok_or_else(|| Error::BadRequest {
        reason: "MessageSent log without a block number".into(),
    })?;
    Ok(MessageSent {
        burn_tx,
        burn_block,
        raw_message: decoded.data.message,
    })
}

/// The `MessageSent` log's decoded parts (named, not a tuple: the three travel
/// together into the record).
struct MessageSent {
    burn_tx: B256,
    burn_block: u64,
    raw_message: Bytes,
}

/// The scan cursor's validity window: `pin <= cursor <= head` (`0` = never
/// scanned; the boot path handles it). A cursor strictly below the base is a
/// forward re-pin, one above the head is a rewind — both park stale records.
/// `cursor == pin` is the legitimate rest state right after a re-pin: the new
/// base has no scanned blocks yet (the scan starts at `pin + 1`), so it must
/// not fire (measured: treating it as off-base warned and re-parked every
/// pass while the fresh chain sat at its base).
fn cursor_off_base(cursor: u64, head: u64, pin: Option<u64>) -> bool {
    cursor > 0 && (cursor > head || pin.is_some_and(|pin| cursor < pin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::tests_support::sample_raw_message;
    use alloy_primitives::B256;

    /// The cursor's validity window: a re-pin to a fresh base leaves the
    /// cursor *at* the pin until the first block mines; treating that as
    /// off-base warned and re-parked every pass (measured on the scratch
    /// stack after the sim's epoch drill).
    #[test]
    fn cursor_off_base_keeps_the_pin_as_the_rest_state() {
        assert!(!cursor_off_base(0, 100, Some(100)), "never scanned: the boot path handles it");
        assert!(!cursor_off_base(100, 100, Some(100)), "cursor at the pin: the rest state");
        assert!(!cursor_off_base(101, 101, Some(100)), "cursor past the pin");
        assert!(!cursor_off_base(150, 200, None), "a real chain has no pin");
        assert!(cursor_off_base(50, 100, Some(100)), "cursor below the base: a forward re-pin");
        assert!(cursor_off_base(150, 100, Some(100)), "cursor above the head: a rewind");
        assert!(cursor_off_base(150, 100, None), "a rewind on a real chain still parks");
    }

    fn watcher(dir: &std::path::Path, destination_domain: Domain, finality_delay: Duration) -> Watcher {
        Watcher {
            chain: Chain::ethereum("http://127.0.0.1:1", Duration::from_secs(1)).unwrap(),
            destination_domain,
            // Never read with `expiration_blocks = 0` (the tests' window): the
            // dummy URL would fault, which is the point of keeping it unused.
            destination: Chain::hyper_evm("http://127.0.0.1:1", Duration::from_secs(1)).unwrap(),
            store: Arc::new(Store::load(dir.join("cctp.json")).unwrap()),
            attester: Arc::new(Attester::from_seed("watcher-test").unwrap()),
            start_block: 0,
            scanned_upto: std::sync::atomic::AtomicU64::new(0),
            chunk_blocks: 2000,
            finality_delay,
            expiration_blocks: 0,
            clamp_warned: std::sync::atomic::AtomicU64::new(0),
            metrics: Arc::new(CctpMetrics::default()),
        }
    }

    /// A synthetic `MessageSent` log carrying the sample raw message with the
    /// given lane.
    fn log(tx: B256, source: u32, destination: u32) -> Log {
        let mut message = Message::decode(&sample_raw_message()).unwrap();
        message.header.source_domain = Domain(source);
        message.header.destination_domain = Domain(destination);
        let event = IMessageTransmitterV2::MessageSent {
            message: message.encode(),
        };
        let inner = alloy_primitives::Log {
            address: MESSAGE_TRANSMITTER_V2,
            data: event.encode_log_data(),
        };
        Log {
            inner,
            block_hash: None,
            block_number: Some(7),
            block_timestamp: None,
            transaction_hash: Some(tx),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        }
    }

    /// One absorb: the nonce is the burn tx hash, the served message fills the
    /// attester fields, the state starts PENDING; a re-scan (the restart
    /// backfill) is idempotent.
    #[test]
    fn absorb_patches_dedupes_and_signs() {
        let dir = crate::test_dir("watcher");
        let _ = std::fs::remove_dir_all(&dir);
        let watcher = watcher(&dir, Domain(19), Duration::ZERO);
        let tx = B256::repeat_byte(0xab);

        assert_eq!(
            watcher.absorb(&log(tx, 0, 19), U256::ZERO).unwrap(),
            1,
            "first absorb inserts"
        );
        assert_eq!(
            watcher.absorb(&log(tx, 0, 19), U256::ZERO).unwrap(),
            0,
            "re-scan is idempotent"
        );
        let record = watcher
            .store
            .with(|s| Ok(s.messages[0].clone()))
            .unwrap();
        assert_eq!(record.nonce, tx, "the nonce is the burn tx hash");
        assert_eq!(record.burn_block, 7);
        assert_eq!(record.state, RelayState::Pending);
        assert!(record.attestation.is_none());
        let served = record.decoded().unwrap();
        assert_eq!(served.header.nonce, tx);
        assert_eq!(served.header.finality_threshold_executed, 2000);
        assert_ne!(record.raw_message, record.message, "the raw event message is not the served one");

        assert_eq!(watcher.sign_due().unwrap(), 1);
        let record = watcher.store.with(|s| Ok(s.messages[0].clone())).unwrap();
        assert_eq!(record.state, RelayState::Signed);
        let attestation = record.attestation.unwrap();
        assert_eq!(attestation.len(), 65);
        let signature = alloy_primitives::Signature::from_raw(&attestation).unwrap();
        assert_eq!(
            signature
                .recover_address_from_prehash(&record.message_hash)
                .unwrap(),
            watcher.attester.address()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A burn to a third chain is not this bridge's lane; it is skipped (with
    /// a warning) and never stored.
    #[test]
    fn absorb_skips_foreign_lanes() {
        let dir = crate::test_dir("lanes");
        let _ = std::fs::remove_dir_all(&dir);
        let watcher = watcher(&dir, Domain(19), Duration::ZERO);
        assert_eq!(watcher.absorb(&log(B256::repeat_byte(1), 0, 3), U256::ZERO).unwrap(), 0);
        assert_eq!(watcher.absorb(&log(B256::repeat_byte(2), 7, 19), U256::ZERO).unwrap(), 0);
        assert_eq!(watcher.store.counts().unwrap().pending, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The emulated PENDING window holds the attestation back.
    #[test]
    fn sign_due_respects_the_pending_window() {
        let dir = crate::test_dir("pending");
        let _ = std::fs::remove_dir_all(&dir);
        let watcher = watcher(&dir, Domain(19), Duration::from_secs(3600));
        watcher.absorb(&log(B256::repeat_byte(3), 0, 19), U256::ZERO).unwrap();
        assert_eq!(watcher.sign_due().unwrap(), 0, "window still open");
        assert_eq!(
            watcher.store.with(|s| Ok(s.messages[0].state)).unwrap(),
            RelayState::Pending
        );
        // The window elapsing signs it.
        watcher
            .store
            .with(|s| {
                s.messages[0].attest_after_ms = 0;
                Ok(())
            })
            .unwrap();
        assert_eq!(watcher.sign_due().unwrap(), 1);
        assert_eq!(
            watcher.store.with(|s| Ok(s.messages[0].state)).unwrap(),
            RelayState::Signed
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

}
