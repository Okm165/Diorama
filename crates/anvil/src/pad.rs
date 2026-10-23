//! The mapping-slot preimage: `keccak256(abi.encode(key, slot))`.
//!
//! The measured padding rules (anvil rejects odd-length hex with `-32602 odd
//! number of digits`) live with the wire types, not here: the write path sends
//! full 32-byte words (`B256`) and minimal even-length quantities
//! (`U256`/`U64`), so the official types' serde is that rule's single home
//! (`crates/anvil/src/writes.rs`).
use alloy_primitives::{Address, U256};

/// `keccak256(abi.encode(key, slot))` - the storage slot of a Solidity mapping.
pub fn mapping_slot(key: Address, slot: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(key.as_slice());
    buf[32..64].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(alloy_primitives::keccak256(buf).0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cast index address 0x00000000000000000000000000000000000F00d9 3`
    /// = 0x0931201d1ec24ebc0aa88d6c15c762444e04b39af5da0d3c3f33b3aff236ac24
    #[test]
    fn mapping_slot_matches_cast_index() {
        let key: Address = "0x00000000000000000000000000000000000F00d9".parse().unwrap();
        assert_eq!(
            format!("{:064x}", mapping_slot(key, U256::from(3u64))),
            "0931201d1ec24ebc0aa88d6c15c762444e04b39af5da0d3c3f33b3aff236ac24"
        );
    }
}
