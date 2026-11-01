//! The watcher: both chains' `MessageSent` events → the persisted attestation
//! store.
//!
//! **Two lanes, two watermarks** (the DALOC `log_scanner` contract):
//!
//! - **WSS is primary.** `eth_subscribe("logs")` delivers a burn sub-second
//!   after inclusion; [`crate::ws_logs`] decodes it into the same
//!   [`Log`] the HTTP lane returns. WSS advances only the *health* watermark.
//! - **HTTP is reconciliation.** `eth_getLogs` runs at the configured cadence
//!   from the cursor to head, chunked (the emitter is the pinned
//!   MessageTransmitterV2, topic0 the measured `MessageSent(bytes)` hash).
//!   This is the only lane that advances `scanned_upto`, the cursor a restart
//!   or the next reconciliation resumes from. A WSS batch must never move it
//!   past blocks the HTTP arm never scanned, or a log WSS dropped inside that
//!   span would be lost forever.
//!
//! Every log is deduplicated by `(chain_id, tx_hash, log_index)` before it is
//! decoded, so a burn delivered by both lanes is processed once. Each log is
//! decoded into a [`MessageRecord`], the attester fields are patched (`nonce =
//! keccak256(burn-tx ‖ log-index)`, `finalityThresholdExecuted = 2000`,
//! `feeExecuted = 0`, `expirationBlock = configured`, §15.3), and every record
//! whose emulated PENDING window has elapsed is signed.
//!
//! **Restart backfill**: the first HTTP scan of every run starts at
//! `start_block` — a burn awaiting attestation across a restart is re-read
//! and re-signed — and later passes continue from the scanned head. Dedupe is
//! by `(sourceDomain, nonce)`, and the nonce is unique per `(tx, log index)`,
//! so a re-scan is idempotent and a transaction that emits two burns keeps
//! both records.
//!
//! **Health**: WSS disconnected while HTTP is fresh is `degraded`; only when
//! both lanes are stale is the watcher `unhealthy`.
//!
//! Known boundary: a source-chain **re-fork** (T's epoch reset) invalidates
//! watched burns. The records survive; an operator clears the state dir (or
//! sets a fresh start block) when a reset should drop history.
use crate::attester::Attester;
use crate::chains::{Chain, Domain, FINALITY_THRESHOLD_FINALIZED};
use crate::error::{Error, Result};
use crate::metrics::CctpMetrics;
use crate::store::{MessageRecord, RelayState, Store};
use crate::transfer::{IMessageTransmitterV2, Message};
use crate::ws_logs::{WsHealth, WsLogs, WsLogsConfig};
use alloy_primitives::{Bytes, B256, U256};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::SolEvent;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
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
    /// Where the *next* HTTP reconciliation resumes inside this run (0 = not
    /// scanned yet, so the first pass backfills from `start_block`; later
    /// passes continue from the last scanned head). **Only the HTTP arm writes
    /// it**; a WSS batch advances its own health watermark instead. Not
    /// persisted: every boot re-reads from `start_block` by design (§15.4's
    /// restart self-heal) — the dedupe by `(sourceDomain, nonce)` makes the
    /// re-read idempotent.
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

/// Why a stale record is parked (invariant I6): its burn transaction no
/// longer exists on this chain.
const PARK_REASON: &str =
    "source chain re-pinned or rewound: the burn transaction no longer exists";

/// The park's outcome: the record was marked with the reason, or it is gone
/// from the store (a stale derivation whose record no longer exists).
enum Park {
    Marked,
    Gone,
}

impl Watcher {
    /// The steady loop: WSS events are processed as they arrive; the HTTP
    /// reconciliation runs on the configured cadence. Failures are logged and
    /// counted, never fatal (a chain that is down must not stop the other).
    ///
    /// The two watermarks are deliberately separate: `scanned_upto` moves only
    /// after a successful HTTP pass, while the WSS batch advances only the
    /// health watermark. A WSS batch therefore can never make the next HTTP
    /// reconciliation skip a block WSS did not actually deliver.
    #[allow(clippy::too_many_lines)] // one select loop: the two lanes and their shared dedupe
    pub async fn run(self, poll: Duration) {
        let ws_url = self.ws_url();
        let ws_health = ws_url.as_ref().map(|_| Arc::new(WsHealth::default()));
        let health = Arc::new(WatcherHealth::new(self.chain.name, poll, ws_health.clone()));
        register_watcher_health(health.clone());

        let (ws_tx, mut ws_rx) = tokio::sync::mpsc::channel::<Log>(512);
        // Without a WSS URL the sender is kept alive so the select branch
        // stays pending instead of spinning on a closed channel.
        let _ws_tx_guard = match (ws_url, ws_health) {
            (Some(url), Some(ws_health)) => {
                let client = WsLogs::new(
                    WsLogsConfig {
                        url,
                        address: self.chain.message_transmitter,
                        topics: vec![IMessageTransmitterV2::MessageSent::SIGNATURE_HASH],
                        chain: self.chain.name,
                    },
                    ws_tx,
                    ws_health,
                );
                tokio::spawn(client.run());
                None
            }
            _ => Some(ws_tx),
        };

        let mut ticker = tokio::time::interval(poll);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut dedup = LogDedup::new();
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = self.reconcile(&mut dedup, &health).await {
                        self.metrics.scan_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(chain = self.chain.name, error = %e, "cctp HTTP reconciliation failed; will retry");
                    }
                }
                Some(log) = ws_rx.recv() => {
                    let mut logs = vec![log];
                    while let Ok(log) = ws_rx.try_recv() {
                        logs.push(log);
                    }
                    if let Err(e) = self.apply_wss(&logs, &mut dedup, &health).await {
                        self.metrics.scan_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(chain = self.chain.name, error = %e, "cctp WSS batch failed; the HTTP reconciliation will re-read it");
                    }
                }
            }
        }
    }

    /// One HTTP reconciliation pass: scan, move the HTTP watermark, sign what
    /// is due, persist if anything moved.
    async fn reconcile(&self, dedup: &mut LogDedup, health: &WatcherHealth) -> Result<usize> {
        let seen = self.scan_with(dedup).await?;
        health.record_http_scan(self.scanned_upto.load(Ordering::Relaxed), crate::now_ms());
        let signed = self.sign_due()?;
        if seen > 0 || signed > 0 {
            self.store.save()?;
        }
        Ok(seen)
    }

    /// One WSS batch: decode/insert promptly, advance only the WSS health
    /// watermark, sign what is due, persist if anything moved.
    ///
    /// `scanned_upto` is deliberately untouched — it is the HTTP
    /// reconciliation's cursor, and this arm has not proven those blocks were
    /// scanned by the arm a restart resumes from.
    async fn apply_wss(
        &self,
        logs: &[Log],
        dedup: &mut LogDedup,
        health: &WatcherHealth,
    ) -> Result<usize> {
        let expiration = self.expiration_block().await?;
        let seen = self.absorb_wss_logs(logs, expiration, dedup)?;
        if let Some(max_block) = logs.iter().filter_map(|log| log.block_number).max() {
            health.observe_wss(max_block);
        }
        let signed = self.sign_due()?;
        if seen > 0 || signed > 0 {
            self.store.save()?;
        }
        Ok(seen)
    }

    /// The WSS URL: anvil serves WebSocket on its HTTP port, and a configured
    /// `ws(s)://` endpoint is used verbatim. The first endpoint is the primary,
    /// matching the read pool's preference order.
    fn ws_url(&self) -> Option<String> {
        self.chain
            .rpc
            .urls()
            .into_iter()
            .find_map(|url| ws_url_of(&url))
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
    ///
    /// A standalone pass owns a fresh dedupe set (the store's
    /// `(sourceDomain, nonce)` key is the durable idempotence); the event loop
    /// uses [`Self::scan_with`] so the WSS and HTTP lanes share one set.
    pub async fn scan(&self) -> Result<usize> {
        let mut dedup = LogDedup::new();
        self.scan_with(&mut dedup).await
    }

    /// The HTTP reconciliation scan, sharing the caller's cross-lane dedupe
    /// set. This is the only arm that advances `scanned_upto`.
    async fn scan_with(&self, dedup: &mut LogDedup) -> Result<usize> {
        let head = self.chain.rpc.block_number().await?;
        let pin = self.chain.rpc.fork_block().await?;
        let last = self.scanned_upto.load(Ordering::Relaxed);
        let mut from = self.resume_from(last, head, pin).await?;
        let mut seen = 0usize;
        while from <= head {
            let to = from.saturating_add(self.chunk_blocks - 1).min(head);
            let logs = self.message_logs(from, to).await?;
            // One head read per chunk: the expiration window's anchor (the
            // destination's block coordinate, §15.1).
            let expiration = self.expiration_block().await?;
            for log in &logs {
                seen += self.absorb_once(log, expiration, dedup)?;
            }
            from = to + 1;
        }
        // The cursor is the highest scanned block of the *current* chain:
        // a re-pin or a rewind resets it to the new base (the rescan above),
        // so it never skips a block. Only this HTTP arm writes it.
        self.scanned_upto.store(head, Ordering::Relaxed);
        self.metrics.last_scan_block.store(head, Ordering::Relaxed);
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
                if self
                    .clamp_warned
                    .swap(floor, std::sync::atomic::Ordering::Relaxed)
                    != floor
                {
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
            .address(self.chain.message_transmitter)
            .event_signature(IMessageTransmitterV2::MessageSent::SIGNATURE_HASH)
            .from_block(from)
            .to_block(to);
        // The watcher's configured chunk may be wider than the endpoint's
        // declared cap (the public HyperEVM source is 900); the client splits
        // it again rather than forwarding an over-cap range.
        Ok(self.chain.rpc.logs_range(&filter, from, to).await?)
    }

    /// Park the non-relayed records this chain's history no longer backs: a
    /// re-pin or a rewind leaves their burn transactions absent, so they are
    /// stale derivations (I6) — kept visible, never relayed.
    async fn park_vanished(&self) -> Result<usize> {
        let candidates = self.stale_candidates()?;
        let mut parked = 0usize;
        for burn_tx in candidates {
            let exists: Option<alloy_rpc_types_eth::Transaction> =
                self.chain.rpc.call(crate::TX_BY_HASH, (burn_tx,)).await?;
            if exists.is_some() {
                continue;
            }
            if matches!(self.park(burn_tx)?, Park::Marked) {
                parked += 1;
            }
        }
        self.report_parked(parked)?;
        Ok(parked)
    }

    /// The records this chain's history could back: its own domain's
    /// non-relayed ones (pending or signed).
    fn stale_candidates(&self) -> Result<Vec<B256>> {
        self.store.with(|state| {
            Ok(state
                .messages
                .iter()
                .filter(|m| {
                    m.source_domain == self.chain.domain
                        && matches!(m.state, RelayState::Pending | RelayState::Signed)
                })
                .map(|m| m.burn_tx)
                .collect())
        })
    }

    /// Park one record (mark it with the reason); [`Park::Gone`] when it is
    /// gone.
    fn park(&self, burn_tx: B256) -> Result<Park> {
        let marked = self
            .store
            .update_message(
                |m| m.burn_tx == burn_tx,
                |record| {
                    record.state = RelayState::Parked;
                    record.park_reason = Some(PARK_REASON.into());
                },
            )?
            .is_some();
        Ok(if marked { Park::Marked } else { Park::Gone })
    }

    /// The parked count's report: the counter, the saved store, and the warn
    /// (nothing when nothing was parked).
    fn report_parked(&self, parked: usize) -> Result<()> {
        if parked == 0 {
            return Ok(());
        }
        self.metrics
            .relays_parked
            .fetch_add(parked as u64, std::sync::atomic::Ordering::Relaxed);
        self.store.save()?;
        tracing::warn!(
            chain = self.chain.name,
            parked,
            "stale cctp records parked (their burn transactions vanished)"
        );
        Ok(())
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

    /// The record one in-lane message makes: the deterministic nonce is
    /// `keccak256(burn_tx ‖ log_index)`, unique per burn even when one
    /// transaction emits several messages; the raw event message carries
    /// `EMPTY_NONCE` and the served artifact fills it.
    fn record_of(&self, sent: MessageSent, parsed: &Message, expiration: U256) -> MessageRecord {
        let burn_tx = sent.burn_tx;
        let nonce = burn_nonce(burn_tx, sent.log_index);
        let served = parsed.with_attester_fields(
            nonce,
            FINALITY_THRESHOLD_FINALIZED,
            U256::ZERO,
            expiration,
        );
        let now = crate::now_ms();
        MessageRecord {
            source_domain: parsed.header.source_domain,
            destination_domain: parsed.header.destination_domain,
            nonce,
            burn_tx,
            burn_log_index: sent.log_index,
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
        let (burn_tx, burn_block, source_domain, destination_domain) = (
            record.burn_tx,
            record.burn_block,
            record.source_domain,
            record.destination_domain,
        );
        match self.store.insert_message(record)? {
            crate::store::Inserted::New => {
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
            crate::store::Inserted::Rescanned => Ok(0),
            crate::store::Inserted::NonceCollision => {
                tracing::warn!(
                    chain = self.chain.name,
                    tx = %burn_tx,
                    "duplicate nonce with a different message (a second burn in the same tx?); kept the first"
                );
                Ok(0)
            }
        }
    }

    /// Decode one `MessageSent` log into a record. A malformed log is a
    /// permissionless event, not a chain fault: skip it with a named warning.
    /// Propagating the error would stop the scan at that block and the loop
    /// would retry the same block forever (a one-event DoS on the watcher).
    fn absorb(&self, log: &Log, expiration: U256) -> Result<usize> {
        match self.decode_record(log, expiration) {
            Ok(Some(record)) => self.insert(record),
            Ok(None) => Ok(0),
            Err(error) => {
                tracing::warn!(
                    chain = self.chain.name,
                    tx = ?log.transaction_hash,
                    log_index = ?log.log_index,
                    %error,
                    "malformed MessageSent log skipped; the scan continues"
                );
                self.metrics
                    .scan_errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(0)
            }
        }
    }

    /// Absorb one log unless the same `(chain_id, tx_hash, log_index)` was
    /// already processed by either lane. The key is remembered only after the
    /// absorb succeeds, so a transient store fault is retried instead of
    /// silently dropping the burn.
    fn absorb_once(&self, log: &Log, expiration: U256, dedup: &mut LogDedup) -> Result<usize> {
        let Some(key) = LogKey::of(self.chain.chain_id.id(), log) else {
            // A log without a tx hash/index cannot be deduplicated; `absorb`
            // names it and skips it.
            return self.absorb(log, expiration);
        };
        if dedup.contains(key) {
            return Ok(0);
        }
        let seen = self.absorb(log, expiration)?;
        dedup.remember(key);
        Ok(seen)
    }

    /// Absorb one WSS batch. Removed (reorged-out) logs are skipped: their
    /// effect was never canonical, and the HTTP arm will re-read the block if
    /// a replacement log lands there.
    fn absorb_wss_logs(
        &self,
        logs: &[Log],
        expiration: U256,
        dedup: &mut LogDedup,
    ) -> Result<usize> {
        let mut seen = 0usize;
        for log in logs {
            if log.removed {
                continue;
            }
            seen += self.absorb_once(log, expiration, dedup)?;
        }
        Ok(seen)
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

/// The WSS URL for an HTTP RPC endpoint (anvil serves WebSocket on its HTTP
/// port). A `ws(s)://` URL is used verbatim; an unsupported scheme has no WSS
/// lane and the watcher runs HTTP-only.
fn ws_url_of(url: &str) -> Option<String> {
    if url.starts_with("ws://") || url.starts_with("wss://") {
        return Some(url.to_string());
    }
    if let Some(rest) = url.strip_prefix("https://") {
        return Some(format!("wss://{rest}"));
    }
    if let Some(rest) = url.strip_prefix("http://") {
        return Some(format!("ws://{rest}"));
    }
    None
}

/// One log's cross-lane identity: the same burn delivered by WSS and HTTP is
/// the same `(chain, tx, log index)` even though the two lanes build separate
/// `Log` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LogKey {
    chain_id: u64,
    tx_hash: B256,
    log_index: u64,
}

impl LogKey {
    fn of(chain_id: u64, log: &Log) -> Option<Self> {
        Some(Self {
            chain_id,
            tx_hash: log.transaction_hash?,
            log_index: log.log_index?,
        })
    }
}

/// The in-run dedupe set shared by the WSS and HTTP lanes. Bounded FIFO: the
/// store's `(sourceDomain, nonce)` key is the durable idempotence, so the
/// in-memory set only has to cover the overlap window between the lanes (and
/// a replay burst), never the bridge's whole history.
const DEDUP_CAPACITY: usize = 65_536;

struct LogDedup {
    seen: HashSet<LogKey>,
    order: VecDeque<LogKey>,
    capacity: usize,
}

impl LogDedup {
    fn new() -> Self {
        Self::with_capacity(DEDUP_CAPACITY)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            seen: HashSet::with_capacity(capacity.min(4_096)),
            order: VecDeque::with_capacity(capacity.min(4_096)),
            capacity,
        }
    }

    fn contains(&self, key: LogKey) -> bool {
        self.seen.contains(&key)
    }

    fn remember(&mut self, key: LogKey) {
        if !self.seen.insert(key) {
            return;
        }
        self.order.push_back(key);
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
    }
}

/// The watcher's health verdict. `degraded` is the honest middle: one lane is
/// stale, the other is still serving (or reconciling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum HealthStatus {
    Ok,
    Degraded,
    Unhealthy,
}

impl HealthStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }

    pub(crate) fn worst(self, other: Self) -> Self {
        self.max(other)
    }

    /// The required truth table: a WSS disconnect while HTTP is fresh is
    /// `degraded`; only both lanes stale is `unhealthy`. Without a WSS lane
    /// the verdict is HTTP's alone.
    fn verdict(http_fresh: bool, ws_fresh: bool, ws_enabled: bool) -> Self {
        if !ws_enabled {
            return if http_fresh {
                Self::Ok
            } else {
                Self::Unhealthy
            };
        }
        match (http_fresh, ws_fresh) {
            (true, true) => Self::Ok,
            (false, false) => Self::Unhealthy,
            _ => Self::Degraded,
        }
    }
}

/// How many reconciliation intervals may pass without a successful HTTP pass
/// before the HTTP arm counts as stale (the DALOC scanner's five-interval
/// grace).
const STALE_SCAN_INTERVALS: u64 = 5;

/// The two-watermark health state. `http_watermark` is the only value a
/// restart may resume from; `wss_watermark` is an observation, never a cursor.
pub(crate) struct WatcherHealth {
    chain: &'static str,
    poll: Duration,
    http_watermark: std::sync::atomic::AtomicU64,
    wss_watermark: std::sync::atomic::AtomicU64,
    http_last_ok_ms: std::sync::atomic::AtomicU64,
    ws: Option<Arc<WsHealth>>,
}

impl WatcherHealth {
    pub(crate) fn new(chain: &'static str, poll: Duration, ws: Option<Arc<WsHealth>>) -> Self {
        Self {
            chain,
            poll,
            http_watermark: std::sync::atomic::AtomicU64::new(0),
            wss_watermark: std::sync::atomic::AtomicU64::new(0),
            // A watcher that just registered is not stale yet: it gets the
            // same grace as a successful pass before its first HTTP scan.
            http_last_ok_ms: std::sync::atomic::AtomicU64::new(crate::now_ms()),
            ws,
        }
    }

    pub(crate) fn snapshot(&self) -> WatcherHealthSnapshot {
        self.snapshot_at(crate::now_ms())
    }

    fn snapshot_at(&self, now_ms: u64) -> WatcherHealthSnapshot {
        let http_fresh = self.http_fresh_at(now_ms);
        let ws = self.ws.as_ref().map(|health| health.snapshot_at(now_ms));
        let ws_fresh = ws.as_ref().is_some_and(|health| health.connected);
        let status = HealthStatus::verdict(http_fresh, ws_fresh, self.ws.is_some());
        WatcherHealthSnapshot {
            chain: self.chain,
            status,
            http_fresh,
            http_watermark: self.persistable_watermark(),
            wss_watermark: self.wss_watermark.load(Ordering::Relaxed),
            ws,
        }
    }

    fn http_fresh_at(&self, now_ms: u64) -> bool {
        let last = self.http_last_ok_ms.load(Ordering::Relaxed);
        let grace_ms = (self.poll.as_millis() as u64).saturating_mul(STALE_SCAN_INTERVALS);
        last != 0 && now_ms.saturating_sub(last) < grace_ms
    }

    /// The HTTP arm's successful pass: the only watermark that may be
    /// persisted (the watcher's `scanned_upto` is this value).
    pub(crate) fn record_http_scan(&self, watermark: u64, now_ms: u64) {
        self.http_watermark.store(watermark, Ordering::Relaxed);
        self.http_last_ok_ms.store(now_ms, Ordering::Relaxed);
    }

    /// The WSS arm's observation. Deliberately does not touch
    /// `http_watermark` or the watcher's cursor.
    pub(crate) fn observe_wss(&self, watermark: u64) {
        self.wss_watermark.fetch_max(watermark, Ordering::Relaxed);
    }

    /// The watermark a restart/next reconciliation may resume from.
    pub(crate) fn persistable_watermark(&self) -> u64 {
        self.http_watermark.load(Ordering::Relaxed)
    }
}

/// The health fields `/health` serves for one watcher.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct WatcherHealthSnapshot {
    pub chain: &'static str,
    pub status: HealthStatus,
    pub http_fresh: bool,
    /// The last block the HTTP reconciliation completed (the persistable one).
    pub http_watermark: u64,
    /// The last block a WSS batch observed (health only, never a cursor).
    pub wss_watermark: u64,
    pub ws: Option<crate::ws_logs::WsHealthSnapshot>,
}

type HealthRegistry = Mutex<BTreeMap<&'static str, Arc<WatcherHealth>>>;

static WATCHER_HEALTH: OnceLock<HealthRegistry> = OnceLock::new();

fn watcher_health_registry() -> &'static HealthRegistry {
    WATCHER_HEALTH.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn register_watcher_health(health: Arc<WatcherHealth>) {
    watcher_health_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(health.chain, health);
}

/// Every running watcher's snapshot, for `/health`. An empty list means no
/// watcher has registered yet (the HTTP status then falls back to the store's
/// parked count).
pub(crate) fn watcher_health_snapshots() -> Vec<WatcherHealthSnapshot> {
    watcher_health_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .map(|health| health.snapshot())
        .collect()
}

/// The `MessageSent` log's parts: its transaction hash, log index, block and
/// the raw message bytes. The hash, index and block are required — a log
/// without them cannot be turned into a uniquely relayable record, and
/// skipping it silently would drop a burn.
fn decode_message_sent(log: &Log) -> Result<MessageSent> {
    let decoded = IMessageTransmitterV2::MessageSent::decode_log(&log.inner).map_err(|source| {
        Error::Abi {
            what: "MessageSent event",
            source,
        }
    })?;
    let burn_tx = log
        .transaction_hash
        .ok_or_else(|| Error::bad_request("MessageSent log without a transaction hash"))?;
    let log_index = log
        .log_index
        .ok_or_else(|| Error::bad_request("MessageSent log without a log index"))?;
    let burn_block = log
        .block_number
        .ok_or_else(|| Error::bad_request("MessageSent log without a block number"))?;
    Ok(MessageSent {
        burn_tx,
        log_index,
        burn_block,
        raw_message: decoded.data.message,
    })
}

/// The unique attester nonce for one burn. A transaction may emit several
/// `MessageSent` logs; the transaction hash alone is not unique, so the log
/// index is folded in. The destination's `usedNonces` table is keyed by this
/// value, which is what makes each burn independently relayable.
fn burn_nonce(burn_tx: B256, log_index: u64) -> B256 {
    let mut preimage = [0u8; 40];
    preimage[..32].copy_from_slice(burn_tx.as_slice());
    preimage[32..].copy_from_slice(&log_index.to_be_bytes());
    alloy_primitives::keccak256(preimage)
}

/// The `MessageSent` log's decoded parts (named, not a tuple: the four travel
/// together into the record).
struct MessageSent {
    burn_tx: B256,
    log_index: u64,
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
///
/// Shared with the relayer, which applies it to each *destination's*
/// watermark (its last-seen head): the same window rule decides when relayed
/// mints may have been un-minted, so a forward re-pin reconciles live.
pub(crate) fn cursor_off_base(cursor: u64, head: u64, pin: Option<u64>) -> bool {
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
        assert!(
            !cursor_off_base(0, 100, Some(100)),
            "never scanned: the boot path handles it"
        );
        assert!(
            !cursor_off_base(100, 100, Some(100)),
            "cursor at the pin: the rest state"
        );
        assert!(!cursor_off_base(101, 101, Some(100)), "cursor past the pin");
        assert!(!cursor_off_base(150, 200, None), "a real chain has no pin");
        assert!(
            cursor_off_base(50, 100, Some(100)),
            "cursor below the base: a forward re-pin"
        );
        assert!(
            cursor_off_base(150, 100, Some(100)),
            "cursor above the head: a rewind"
        );
        assert!(
            cursor_off_base(150, 100, None),
            "a rewind on a real chain still parks"
        );
    }

    fn watcher(
        dir: &std::path::Path,
        destination_domain: Domain,
        finality_delay: Duration,
    ) -> Watcher {
        Watcher {
            chain: Chain::ethereum(
                "http://127.0.0.1:1",
                Duration::from_secs(1),
                diorama_anvil::PoolPolicy::default(),
            )
            .unwrap(),
            destination_domain,
            // Never read with `expiration_blocks = 0` (the tests' window): the
            // dummy URL would fault, which is the point of keeping it unused.
            destination: Chain::hyper_evm(
                "http://127.0.0.1:1",
                Duration::from_secs(1),
                diorama_anvil::PoolPolicy::default(),
            )
            .unwrap(),
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
    /// given lane and log index.
    fn log_at(tx: B256, log_index: u64, source: u32, destination: u32) -> Log {
        let mut message = Message::decode(&sample_raw_message()).unwrap();
        message.header.source_domain = Domain(source);
        message.header.destination_domain = Domain(destination);
        let event = IMessageTransmitterV2::MessageSent {
            message: message.encode(),
        };
        let inner = alloy_primitives::Log {
            address: crate::chains::MESSAGE_TRANSMITTER_V2,
            data: event.encode_log_data(),
        };
        Log {
            inner,
            block_hash: None,
            block_number: Some(7),
            block_timestamp: None,
            transaction_hash: Some(tx),
            transaction_index: Some(0),
            log_index: Some(log_index),
            removed: false,
        }
    }

    /// The common one-log case: index 0.
    fn log(tx: B256, source: u32, destination: u32) -> Log {
        log_at(tx, 0, source, destination)
    }

    /// Anvil serves WS on its HTTP port; a configured WS URL is used
    /// verbatim, and an unsupported scheme has no WSS lane.
    #[test]
    fn ws_urls_follow_the_http_endpoint() {
        assert_eq!(
            ws_url_of("http://127.0.0.1:8545"),
            Some("ws://127.0.0.1:8545".into())
        );
        assert_eq!(
            ws_url_of("https://rpc.example/path"),
            Some("wss://rpc.example/path".into())
        );
        assert_eq!(ws_url_of("ws://a/ws"), Some("ws://a/ws".into()));
        assert_eq!(ws_url_of("wss://a/ws"), Some("wss://a/ws".into()));
        assert_eq!(ws_url_of("ftp://a"), None);
    }

    /// Dedupe is the full log identity: the same transaction/log index on a
    /// different chain is a different message, and a second log in the same
    /// transaction is too.
    #[test]
    fn dedup_is_keyed_by_chain_tx_and_log_index() {
        let tx = B256::repeat_byte(0xaa);
        let log = log_at(tx, 0, 0, 19);
        let key = LogKey::of(1, &log).unwrap();
        let mut dedup = LogDedup::with_capacity(8);
        assert!(!dedup.contains(key));
        dedup.remember(key);
        assert!(dedup.contains(key));
        assert_ne!(
            key,
            LogKey::of(2, &log).unwrap(),
            "chain id is part of the key"
        );
        assert_ne!(
            key,
            LogKey::of(1, &log_at(tx, 1, 0, 19)).unwrap(),
            "log index is part of the key"
        );

        // The set is bounded: the oldest key is evicted first (the durable
        // store key is still the final idempotence).
        let mut small = LogDedup::with_capacity(1);
        small.remember(key);
        let next = LogKey::of(1, &log_at(tx, 1, 0, 19)).unwrap();
        small.remember(next);
        assert!(!small.contains(key));
        assert!(small.contains(next));
    }

    /// A log delivered by both lanes is processed once: the second absorb is
    /// a dedupe hit, not a second record.
    #[test]
    fn dedup_prevents_a_second_lane_from_reprocessing() {
        let dir = crate::test_dir("dedup");
        let _ = std::fs::remove_dir_all(&dir);
        let watcher = watcher(&dir, Domain(19), Duration::ZERO);
        let mut dedup = LogDedup::new();
        let log = log_at(B256::repeat_byte(0xbb), 0, 0, 19);

        assert_eq!(
            watcher.absorb_once(&log, U256::ZERO, &mut dedup).unwrap(),
            1
        );
        assert_eq!(
            watcher.absorb_once(&log, U256::ZERO, &mut dedup).unwrap(),
            0,
            "the HTTP lane's re-read of the WSS log is a dedupe hit"
        );
        assert_eq!(watcher.store.counts().unwrap().pending, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two-watermark rule: a WSS batch at block 200 never moves the HTTP
    /// cursor (still 100); only the next successful HTTP pass may advance the
    /// persistable watermark.
    #[tokio::test]
    async fn wss_never_moves_the_http_cursor() {
        let dir = crate::test_dir("watermarks");
        let _ = std::fs::remove_dir_all(&dir);
        let watcher = watcher(&dir, Domain(19), Duration::ZERO);
        watcher.scanned_upto.store(100, Ordering::Relaxed);

        let health = WatcherHealth::new("ethereum", Duration::from_secs(1), None);
        health.record_http_scan(100, 1_000);

        let mut log = log_at(B256::repeat_byte(0xcc), 0, 0, 19);
        log.block_number = Some(200);
        let mut dedup = LogDedup::new();
        assert_eq!(
            watcher
                .apply_wss(&[log], &mut dedup, &health)
                .await
                .unwrap(),
            1
        );

        assert_eq!(
            watcher.scanned_upto.load(Ordering::Relaxed),
            100,
            "WSS must not advance the HTTP reconciliation cursor"
        );
        assert_eq!(
            health.persistable_watermark(),
            100,
            "the persisted watermark is the HTTP arm's, never WSS's"
        );
        let snapshot = health.snapshot_at(1_000);
        assert_eq!(snapshot.http_watermark, 100);
        assert_eq!(snapshot.wss_watermark, 200);

        health.record_http_scan(150, 2_000);
        assert_eq!(
            health.persistable_watermark(),
            150,
            "only the HTTP reconciliation advances the persistable watermark"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The required verdict: WSS down while HTTP is fresh is `degraded`;
    /// only both stale is `unhealthy`; no WSS lane is HTTP's verdict alone.
    #[test]
    fn health_verdict_is_degraded_only_when_one_lane_is_stale() {
        assert_eq!(HealthStatus::verdict(true, true, true), HealthStatus::Ok);
        assert_eq!(
            HealthStatus::verdict(true, false, true),
            HealthStatus::Degraded,
            "WSS disconnected while HTTP is fresh"
        );
        assert_eq!(
            HealthStatus::verdict(false, true, true),
            HealthStatus::Degraded,
            "HTTP stale while WSS is live"
        );
        assert_eq!(
            HealthStatus::verdict(false, false, true),
            HealthStatus::Unhealthy,
            "both lanes stale"
        );
        assert_eq!(
            HealthStatus::verdict(true, false, false),
            HealthStatus::Ok,
            "HTTP-only: the absent WSS lane is not a failure"
        );
        assert_eq!(
            HealthStatus::verdict(false, false, false),
            HealthStatus::Unhealthy
        );
        assert_eq!(
            HealthStatus::Ok.worst(HealthStatus::Degraded),
            HealthStatus::Degraded
        );
        assert_eq!(
            HealthStatus::Degraded.worst(HealthStatus::Unhealthy),
            HealthStatus::Unhealthy
        );
    }

    /// The snapshot tracks the same verdict as the lanes move: fresh HTTP +
    /// connected WSS is ok; WSS down is degraded; both stale is unhealthy.
    #[test]
    fn health_snapshot_tracks_the_lanes() {
        let ws = Arc::new(WsHealth::default());
        let health = WatcherHealth::new("ethereum", Duration::from_secs(1), Some(ws.clone()));
        health.record_http_scan(100, 1_000);

        assert_eq!(
            health.snapshot_at(1_000).status,
            HealthStatus::Degraded,
            "WSS has not connected yet"
        );
        ws.mark_connected();
        assert_eq!(health.snapshot_at(1_000).status, HealthStatus::Ok);
        assert_eq!(
            health.snapshot_at(7_000).status,
            HealthStatus::Degraded,
            "HTTP is stale but WSS is still live"
        );
        ws.mark_disconnected();
        assert_eq!(health.snapshot_at(7_000).status, HealthStatus::Unhealthy);
    }

    /// One absorb: the nonce is `keccak256(tx ‖ log index)`, the served
    /// message fills the attester fields, the state starts PENDING; a re-scan
    /// (the restart backfill) is idempotent.
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
        let record = watcher.store.with(|s| Ok(s.messages[0].clone())).unwrap();
        assert_eq!(
            record.nonce,
            burn_nonce(tx, 0),
            "the nonce is derived from the burn tx and log index"
        );
        assert_eq!(record.burn_log_index, 0);
        assert_eq!(record.burn_block, 7);
        assert_eq!(record.state, RelayState::Pending);
        assert!(record.attestation.is_none());
        let served = record.decoded().unwrap();
        assert_eq!(served.header.nonce, record.nonce);
        assert_eq!(served.header.finality_threshold_executed, 2000);
        assert_ne!(
            record.raw_message, record.message,
            "the raw event message is not the served one"
        );

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

    /// Two burns in one transaction are two records with distinct nonces.
    /// Keying the store on the transaction hash silently dropped the second
    /// burn and left its USDC unminted.
    #[test]
    fn two_burns_in_one_transaction_are_both_watched() {
        let dir = crate::test_dir("multi-burn");
        let _ = std::fs::remove_dir_all(&dir);
        let watcher = watcher(&dir, Domain(19), Duration::ZERO);
        let tx = B256::repeat_byte(0xcd);

        assert_eq!(
            watcher.absorb(&log_at(tx, 0, 0, 19), U256::ZERO).unwrap(),
            1
        );
        assert_eq!(
            watcher.absorb(&log_at(tx, 1, 0, 19), U256::ZERO).unwrap(),
            1,
            "the second burn in the same transaction must not collide"
        );
        let records = watcher.store.with(|s| Ok(s.messages.clone())).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].burn_tx, tx);
        assert_eq!(records[1].burn_tx, tx);
        assert_ne!(records[0].nonce, records[1].nonce);
        assert_eq!(records[0].burn_log_index, 0);
        assert_eq!(records[1].burn_log_index, 1);

        // A re-scan of both logs is still idempotent.
        assert_eq!(
            watcher.absorb(&log_at(tx, 0, 0, 19), U256::ZERO).unwrap(),
            0
        );
        assert_eq!(
            watcher.absorb(&log_at(tx, 1, 0, 19), U256::ZERO).unwrap(),
            0
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
        assert_eq!(
            watcher
                .absorb(&log(B256::repeat_byte(1), 0, 3), U256::ZERO)
                .unwrap(),
            0
        );
        assert_eq!(
            watcher
                .absorb(&log(B256::repeat_byte(2), 7, 19), U256::ZERO)
                .unwrap(),
            0
        );
        assert_eq!(watcher.store.counts().unwrap().pending, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The emulated PENDING window holds the attestation back.
    #[test]
    fn sign_due_respects_the_pending_window() {
        let dir = crate::test_dir("pending");
        let _ = std::fs::remove_dir_all(&dir);
        let watcher = watcher(&dir, Domain(19), Duration::from_secs(3600));
        watcher
            .absorb(&log(B256::repeat_byte(3), 0, 19), U256::ZERO)
            .unwrap();
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
