//! `mirror-cctp` — the CCTP bridge (plan §15): real USDC between the Mainnet
//! Mirror (T, Ethereum, domain 0) and the Venue Mirror (V, HyperEVM, domain 19)
//! over the real CCTP V2 contracts already present in both forks.
//!
//! The only devnet substitutions are Circle's off-chain pieces: the
//! attestation authority (a deterministic devnet key installed through the
//! contracts' own `enableAttester`/`setSignatureThreshold` setters, §15.3) and
//! the Iris API (this crate's local, Iris-compatible service, §16.2). Every
//! on-chain check stays real: message format, domain, `destinationCaller`,
//! nonce replay, signature threshold/ordering, expiration, fee caps.
//!
//! The flow this crate implements, end to end:
//!
//! ```text
//! TokenMessengerV2.depositForBurn  → MessageSent event
//!   → watcher.rs  (decode, fill the attester fields, persist)
//!   → attester.rs (keccak256(message), 65-byte ECDSA, §15.3 encoding)
//!   → iris.rs     (GET /v2/messages/{domain}?transactionHash=…)
//!   → relayer.rs  (receiveMessage(message, attestation) on the destination)
//! ```
//!
//! The measured facts the modules cite (message layout, contract addresses,
//! the fee table, the receive-path proof) live in `chains.rs` and the module
//! docs; nothing here guesses a number the forks did not answer.

pub mod attester;
pub mod chains;
pub mod control;
pub mod error;
pub mod iris;
pub mod metrics;
pub mod relayer;
pub mod store;
pub mod transfer;
pub mod watcher;

pub use error::{Error, Result};
/// The alloy primitives the bridge's public surface exposes (addresses,
/// amounts, hashes): the bin reads them from here, never from a second alloy
/// dependency.
pub use alloy_primitives::{Address, Bytes, B256, U256};

/// Wall-clock milliseconds: the PENDING window (`attest_after_ms`), receipt
/// timestamps, and the Iris `lastUpdated` field all read one clock.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
