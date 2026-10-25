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
//! Clocks: `block` = the scanned chain's height (per lane: T or V);
//! `cursor` = that lane's scan cursor; `pin` = the chain's fork base.

/// The method the burn watchers share (the relayer, the sender's pool check,
/// the watcher's vanished-tx check): one home.
pub(crate) const TX_BY_HASH: &str = "eth_getTransactionByHash";
pub mod attester;
pub mod chains;
pub mod control;
pub mod error;
pub mod hook;
pub mod iris;
pub mod metrics;
pub mod relayer;
pub mod service;
pub mod store;
pub mod transfer;
pub mod watcher;

pub use error::{Error, Result};
pub use service::main_entry;
/// The alloy primitives the bridge's public surface exposes (addresses,
/// amounts, hashes): the bin reads them from here, never from a second alloy
/// dependency.
pub use alloy_primitives::{Address, Bytes, B256, U256};

/// Wall-clock milliseconds: the PENDING window (`attest_after_ms`), receipt
/// timestamps, and the Iris `lastUpdated` field all read one clock — the
/// shared definition lives in `mirror-config`'s boot module.
pub use mirror_config::boot::now_ms;

/// A scratch directory for one test (per test + process: the tests may run
/// concurrently). One definition, so no test forgets the process id and
/// collides with a parallel run.
#[cfg(test)]
pub(crate) fn test_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("mirror-cctp-{tag}-{}", std::process::id()))
}

#[cfg(test)]
mod design_assertions {
    /// The Rust-for-Rustaceans pin (plan §11.3 rule 11).
    fn is_normal<T: Send + Sync + Sized + Unpin>() {}

    #[test]
    fn public_types_are_normal() {
        is_normal::<crate::chains::Domain>();
        is_normal::<crate::chains::Chain>();
        is_normal::<crate::chains::Wiring>();
        is_normal::<crate::transfer::MessageHeader>();
        is_normal::<crate::transfer::BurnMessageBody>();
        is_normal::<crate::transfer::Message>();
        is_normal::<crate::transfer::Wallet>();
        is_normal::<crate::transfer::TxSender>();
        is_normal::<crate::attester::Attester>();
        is_normal::<crate::watcher::Watcher>();
        is_normal::<crate::relayer::Relayer>();
        is_normal::<crate::iris::IrisState>();
        is_normal::<crate::metrics::CctpMetrics>();
        is_normal::<crate::error::Error>();
    }
}
