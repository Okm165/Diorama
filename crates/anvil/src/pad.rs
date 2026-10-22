//! The measured padding rules (do not rediscover): `anvil_setStorageAt` rejects
//! odd-length hex (`-32602 odd number of digits`); 58% of raw BAL values are
//! odd-length minimal hex. Storage values -> 32 bytes; quantities -> even length.
use alloy_primitives::{Address, Bytes, U256};

/// `keccak256(abi.encode(key, slot))` - the storage slot of a Solidity mapping.
pub fn mapping_slot(key: Address, slot: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(key.as_slice());
    buf[32..64].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(alloy_primitives::keccak256(buf).0)
}

/// 32-byte data hex (storage values, code).
pub fn pad_data32(v: U256) -> String {
    format!("0x{:0>64}", format!("{v:x}"))
}

/// Minimal even-length hex quantity (balances, nonces).
pub fn quant_hex_u256(v: U256) -> String {
    let s = format!("{v:x}");
    if s.len() % 2 == 1 {
        format!("0x0{s}")
    } else {
        format!("0x{s}")
    }
}

pub fn quant_hex_u64(v: u64) -> String {
    let s = format!("{v:x}");
    if s.len() % 2 == 1 {
        format!("0x0{s}")
    } else {
        format!("0x{s}")
    }
}

pub fn bytes_hex(b: &Bytes) -> String {
    format!("0x{}", hex::encode(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_values_are_padded_to_64() {
        assert_eq!(pad_data32(U256::from(0x2au64)), format!("0x{:0>64}", "2a"));
        assert_eq!(pad_data32(U256::ZERO).len(), 66);
        assert_eq!(pad_data32(U256::MAX).len(), 66);
        // odd-length minimal value (the measured failure mode) becomes even
        let v = U256::from(0x123u64);
        assert_eq!(
            pad_data32(v),
            "0x0000000000000000000000000000000000000000000000000000000000000123"
        );
    }

    #[test]
    fn quantities_are_minimal_but_even_length() {
        assert_eq!(quant_hex_u256(U256::ZERO), "0x00");
        assert_eq!(quant_hex_u256(U256::from(1u64)), "0x01");
        assert_eq!(quant_hex_u256(U256::from(0x123u64)), "0x0123");
        assert_eq!(quant_hex_u256(U256::from(0x1234u64)), "0x1234");
        assert_eq!(quant_hex_u64(0), "0x00");
        assert_eq!(quant_hex_u64(0xabc), "0x0abc");
    }

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
