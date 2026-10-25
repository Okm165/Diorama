//! Facts: what happened, as events.
//!
//! Commands are calls (HTTP POST → typed handler → `AdminSurface` method);
//! facts are events. The vocabulary is Fowler's *event notification*: a fact
//! says **what changed**, carries the minimal payload, and is not the state
//! itself — a consumer that needs details queries back (`/status`,
//! `shadow.json`, `cursor.json`). The durable record is the state files, not
//! the stream: this is deliberately **not** event sourcing, and no outbox is
//! needed because facts carry no authority (a dropped notification loses
//! nothing).
//!
//! Transport: in-process a tokio broadcast bus (all subscribers see every
//! fact; a lagged subscriber gets `Lagged` and resumes — fine by
//! construction); cross-process `GET /events` serves the same JSON over SSE.
//! SSE's automatic reconnect is the client's job; the server replays nothing
//! (notification semantics).
use alloy_primitives::{Address, B256};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;

/// One fact. Profile-agnostic on purpose: `chain` names the devnet chain
/// ("t" on the mainnet profile, "v" on the venue's), the rest is the spine's
/// own vocabulary. Facts that only one profile can produce (`TopUpDelivered`)
/// simply never occur on the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "fact", rename_all = "snake_case")]
pub enum Fact {
    /// One diff was composed and applied to the devnet chain.
    BlockApplied {
        chain: &'static str,
        block: u64,
        /// The composer's per-block truth (`bal` | `trace` | `poll` | `late` |
        /// `reapply`).
        source: &'static str,
        writes: usize,
        filtered: usize,
        skipped_shadowed: usize,
        sampled: usize,
        sampled_ok: usize,
        ms: u64,
    },
    /// The devnet cursor moved (late completion and re-apply included).
    CursorAdvanced { chain: &'static str, block: u64 },
    /// The chain was found reset under the engine (re-pin / wiped state).
    ResetDetected {
        chain: &'static str,
        witness: ResetWitness,
    },
    /// The epoch rule ran: a pin change (marks *and* values reconciled).
    EpochChanged {
        chain: &'static str,
        pin: Option<u64>,
    },
    /// A probe run finished (on-demand runs; the per-block counters are
    /// metrics, not facts — publishing per block would be noise).
    ProbesRun {
        chain: &'static str,
        at: u64,
        ok: usize,
        failed: usize,
        skipped: usize,
    },
    /// The shadow scan walked blocks (the COW marks' progress).
    ShadowScanned {
        chain: &'static str,
        blocks: u64,
        empty: u64,
        shadowed: u64,
    },
    /// The top-up loop delivered payloads to reth (mainnet-only).
    TopUpDelivered { delivered: usize },
    /// The faucet wrote a key and marked it devnet-owned.
    DealApplied { chain: &'static str, address: Address },
    /// The journal rewind undid applied blocks (`/admin/rewind`): the chain
    /// and the cursor moved backwards together.
    RewindApplied {
        chain: &'static str,
        blocks: u64,
        cursor: u64,
    },
}

/// A fact with its bus sequence number — the SSE `id:` and the consumer's
/// gap detector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    pub seq: u64,
    #[serde(flatten)]
    pub fact: Fact,
}

/// What proved the reset: the cursor's recorded identity vs what the chain
/// shows now. The witness names its kind, so a consumer never has to guess
/// what the values mean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "witness", rename_all = "snake_case")]
pub enum ResetWitness {
    /// The cursor sits below the chain's fork base (a re-pin moved the base).
    Position { base: Option<u64>, cursor: u64 },
    /// The cursor block no longer carries its recorded hash.
    Hash {
        block: u64,
        expected: B256,
        found: Option<B256>,
    },
    /// The venue canary no longer matches the fork pin.
    Pin {
        expected: Option<u64>,
        found: Option<u64>,
    },
}

/// The in-process fact bus. Cheap to publish into (no subscriber is fine),
/// cheap to subscribe to; the ring keeps the newest facts only — a slow
/// consumer lags and resumes, which is exactly the notification contract.
pub struct FactBus {
    tx: broadcast::Sender<Notice>,
    seq: AtomicU64,
}

impl FactBus {
    /// `capacity` is rounded up to a power of two by tokio; 256 is ample for
    /// the burstiest loops (catch-up applies) while keeping the ring tiny.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            seq: AtomicU64::new(0),
        }
    }

    /// Publish one fact; returns its sequence number. A closed bus (no
    /// receivers, senders dropped) is not an error: facts are notifications.
    pub fn publish(&self, fact: Fact) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.tx.send(Notice { seq, fact });
        seq
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        self.tx.subscribe()
    }

    /// The last published sequence number (0 before the first fact).
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }
}

impl Default for FactBus {
    fn default() -> Self {
        Self::new(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_are_sequenced_and_every_subscriber_sees_them() {
        let bus = FactBus::new(16);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        assert_eq!(bus.seq(), 0);

        let first = bus.publish(Fact::CursorAdvanced {
            chain: "t",
            block: 7,
        });
        let second = bus.publish(Fact::ShadowScanned {
            chain: "v",
            blocks: 3,
            empty: 1,
            shadowed: 12,
        });
        assert_eq!((first, second), (1, 2));
        assert_eq!(bus.seq(), 2);

        for rx in [&mut a, &mut b] {
            let n = rx.try_recv().expect("first fact");
            assert_eq!(n.seq, 1);
            assert!(matches!(n.fact, Fact::CursorAdvanced { block: 7, .. }));
            let n = rx.try_recv().expect("second fact");
            assert_eq!(n.seq, 2);
        }
    }

    #[test]
    fn a_notice_serializes_as_one_flat_json_object() {
        let notice = Notice {
            seq: 41,
            fact: Fact::BlockApplied {
                chain: "t",
                block: 26_264_454,
                source: "trace",
                writes: 3456,
                filtered: 388,
                skipped_shadowed: 393,
                sampled: 8,
                sampled_ok: 8,
                ms: 30194,
            },
        };
        let json = serde_json::to_value(&notice).unwrap();
        assert_eq!(json["seq"], 41);
        assert_eq!(json["fact"], "block_applied");
        assert_eq!(json["block"], 26_264_454);
        assert_eq!(json["sampled_ok"], 8);
    }
}
