//! Atomic JSON state (the `shadow.json` pattern): a load has three answers —
//! absent (a fact: the caller starts empty), fault, or corrupt (dies at the
//! load point, path named) — and a save never leaves a torn file (write temp,
//! rename).
//!
//! The persisted unit is the **message record**: the watcher inserts one per
//! `MessageSent` log, the attester transitions it PENDING → SIGNED, the
//! relayer marks it RELAYED or PARKED. Parked records are kept, never
//! dropped: a failed mint is a fact an operator must see (§15.4).
use crate::chains::Domain;
use crate::error::{Error, Result};
use crate::transfer::Message;
use alloy_primitives::{Address, Bytes, B256};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

/// Where one message is in its lifecycle. The wire name is `snake_case`
/// (`/control/state` serves it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayState {
    /// Watched; the emulated PENDING window has not elapsed yet.
    Pending,
    /// Attestation produced; waiting for the relayer.
    Signed,
    /// `receiveMessage` landed (or the nonce was already consumed on-chain).
    Relayed,
    /// A definitive failure (a mined revert, or repeated send refusals); kept
    /// visible, never retried silently.
    Parked,
}

/// One watched burn and everything the service knows about it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageRecord {
    pub source_domain: Domain,
    pub destination_domain: Domain,
    /// The deterministic nonce: the burn transaction hash (§15.3 — the
    /// attester fills `nonce = burn-tx hash`, stable across re-attestation).
    pub nonce: B256,
    pub burn_tx: B256,
    pub burn_block: u64,
    /// The raw `MessageSent` payload: `EMPTY_NONCE` and
    /// `EMPTY_FINALITY_THRESHOLD_EXECUTED` (both 0), `feeExecuted` and
    /// `expirationBlock` 0 — unsubmittable by construction (the zero-nonce
    /// pre-block, §15.1).
    pub raw_message: Bytes,
    /// The served message: the raw one with the attester fields filled. This
    /// is what the signature covers and what `receiveMessage` is called with.
    pub message: Bytes,
    /// `keccak256(message)` — the signed digest.
    pub message_hash: B256,
    /// The 65-byte ECDSA signature, once signed.
    #[serde(default)]
    pub attestation: Option<Bytes>,
    pub state: RelayState,
    pub created_ms: u64,
    /// When the emulated PENDING window elapses (`finality_delay_secs`).
    pub attest_after_ms: u64,
    /// The last `receiveMessage` tx submitted for this message (checked
    /// before re-sending: a tx may still be in the pool across ticks).
    #[serde(default)]
    pub relay_tx: Option<B256>,
    #[serde(default)]
    pub relayed_ms: Option<u64>,
    /// Send attempts (transient failures); a definitive revert parks at once.
    #[serde(default)]
    pub attempts: u32,
    /// Why a parked message was parked, in the boundary's own words.
    #[serde(default)]
    pub park_reason: Option<String>,
}

impl MessageRecord {
    /// The served message parsed back into typed fields (the watcher's
    /// patched artifact); a decode fault here means the stored bytes are
    /// corrupt, which is a fault, not an empty answer.
    pub fn decoded(&self) -> Result<Message> {
        Message::decode(&self.message)
    }

    /// The Iris entry for this record: the served message always; the
    /// attestation only once it exists.
    pub fn message_hex(&self) -> String {
        format!("0x{}", alloy_primitives::hex::encode(&self.message))
    }

    pub fn attestation_hex(&self) -> Option<String> {
        self.attestation
            .as_ref()
            .map(|a| format!("0x{}", alloy_primitives::hex::encode(a)))
    }
}

/// The persisted file: every message this bridge has watched.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CctpState {
    #[serde(default)]
    pub messages: Vec<MessageRecord>,
    /// The deployed hook wrapper per domain (the boot step deploys one per
    /// served chain, §15.4): hook-carrying messages are consumed through it.
    /// `serde(default)` keeps pre-hook state files loadable.
    #[serde(default)]
    pub hooks: std::collections::BTreeMap<Domain, Address>,
}

/// Per-state counts (the `/health` and `/metrics` diagnostics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub pending: usize,
    pub signed: usize,
    pub relayed: usize,
    pub parked: usize,
}

impl Counts {
    pub fn of(state: &CctpState) -> Self {
        let mut counts = Self::default();
        for message in &state.messages {
            match message.state {
                RelayState::Pending => counts.pending += 1,
                RelayState::Signed => counts.signed += 1,
                RelayState::Relayed => counts.relayed += 1,
                RelayState::Parked => counts.parked += 1,
            }
        }
        counts
    }
}

/// The state file plus its in-memory truth. All access goes through
/// [`Store::with`]; persistence is explicit ([`Store::save`]) so a tick writes
/// once, not per message.
pub struct Store {
    path: PathBuf,
    state: Mutex<CctpState>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Load the state file. Absent is a fact (first boot starts empty); an IO
    /// fault is not emptiness; corrupt bytes die here, at the load point,
    /// with the path named.
    pub fn load(path: PathBuf) -> Result<Self> {
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| Error::Corrupt {
                what: "cctp state",
                path: path.clone(),
                source,
            })?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => CctpState::default(),
            Err(source) => {
                return Err(Error::Io {
                    what: "cctp state",
                    path,
                    source,
                })
            }
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    /// Run one critical section against the state. A poisoned lock is a typed
    /// refusal (a panic happened mid-update), never a silent recovery.
    pub fn with<R>(&self, f: impl FnOnce(&mut CctpState) -> Result<R>) -> Result<R> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| Error::Poisoned { what: "cctp state" })?;
        f(&mut guard)
    }

    /// The state counts (diagnostics).
    pub fn counts(&self) -> Result<Counts> {
        self.with(|state| Ok(Counts::of(state)))
    }

    /// Mutate the message record for `nonce` and persist, one atomic step.
    /// Absent is `None`, not an error: the watcher is the only creator, and
    /// every caller here refines a record that already exists.
    pub fn update_message<T>(
        &self,
        nonce: B256,
        f: impl FnOnce(&mut MessageRecord) -> T,
    ) -> Result<Option<T>> {
        let out = self.with(|state| {
            Ok(state
                .messages
                .iter_mut()
                .find(|m| m.nonce == nonce)
                .map(f))
        })?;
        self.save()?;
        Ok(out)
    }

    /// Save atomically (temp + rename); missing parent directories are
    /// created. The snapshot is taken under the lock, the write outside it.
    pub fn save(&self) -> Result<()> {
        let snapshot = self.with(|state| Ok(state.clone()))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                what: "cctp state directory",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let tmp = self.path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(&snapshot).map_err(|source| Error::Encode {
            what: "cctp state",
            source,
        })?;
        std::fs::write(&tmp, bytes).map_err(|source| Error::Io {
            what: "cctp state",
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|source| Error::Io {
            what: "cctp state",
            path: self.path.clone(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::Message;

    fn sample_message() -> Message {
        let raw = Message::decode(&crate::transfer::tests_support::sample_raw_message())
            .expect("sample decodes");
        raw.with_attester_fields(B256::repeat_byte(0xab), 2000, Default::default(), Default::default())
    }

    fn sample_record() -> MessageRecord {
        let message = sample_message();
        MessageRecord {
            source_domain: Domain(0),
            destination_domain: Domain(19),
            nonce: B256::repeat_byte(0xab),
            burn_tx: B256::repeat_byte(0xab),
            burn_block: 7,
            raw_message: crate::transfer::tests_support::sample_raw_message(),
            message: message.encode(),
            message_hash: message.keccak(),
            attestation: None,
            state: RelayState::Pending,
            created_ms: 1,
            attest_after_ms: 1,
            relay_tx: None,
            relayed_ms: None,
            attempts: 0,
            park_reason: None,
        }
    }

    /// Boundary-discipline: absent is a fact, corruption dies at the load
    /// point with the path named.
    #[test]
    fn absent_is_a_fact_corruption_dies_at_load() {
        let dir = crate::test_dir("store");
        let path = dir.join("cctp.json");
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::load(path.clone()).expect("absent loads empty");
        assert_eq!(store.counts().unwrap(), Counts::default());

        store
            .with(|state| {
                state.messages.push(sample_record());
                Ok(())
            })
            .unwrap();
        store.save().unwrap();
        let reloaded = Store::load(path.clone()).expect("saved state reloads");
        assert_eq!(reloaded.counts().unwrap().pending, 1);
        assert_eq!(
            reloaded.with(|s| Ok(s.messages.clone())).unwrap(),
            store.with(|s| Ok(s.messages.clone())).unwrap()
        );

        std::fs::write(&path, b"{not json").unwrap();
        let err = Store::load(path.clone()).unwrap_err();
        assert!(matches!(err, Error::Corrupt { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
