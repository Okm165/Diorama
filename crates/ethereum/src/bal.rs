//! EIP-7928 BAL flattening + the measured padding rules + the prestateTracer
//! diff fallback.
//!
//! Measured rules (do not rediscover):
//!   - `anvil_setStorageAt` rejects odd-length hex (`-32602 odd number of
//!     digits`); 58% of raw BAL values are odd-length minimal hex. Storage
//!     values are padded to 32 bytes; quantities (balance/nonce) to even length.
//!   - "last change per key = final value" is guaranteed by the spec ordering;
//!     `AccountChanges::storage_post_states()` implements exactly that.
use alloy_eips::eip7928::BlockAccessList;
use alloy_primitives::{Address, Bytes, U256};
use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    Storage { address: Address, slot: U256, value: U256 },
    Balance { address: Address, value: U256 },
    Nonce { address: Address, value: u64 },
    Code { address: Address, value: Bytes },
}

impl Write {
    pub fn address(&self) -> Address {
        match self {
            Write::Storage { address, .. }
            | Write::Balance { address, .. }
            | Write::Nonce { address, .. }
            | Write::Code { address, .. } => *address,
        }
    }

}

/// Flatten a BAL into post-state writes (last change per key).
pub fn flatten_bal(bal: &BlockAccessList) -> Vec<Write> {
    let mut out = Vec::new();
    for acct in bal.iter() {
        if !acct.has_changes() {
            continue;
        }
        for (slot, value) in acct.storage_post_states() {
            out.push(Write::Storage {
                address: acct.address,
                slot,
                value,
            });
        }
        if let Some(value) = acct.balance_post_state() {
            out.push(Write::Balance {
                address: acct.address,
                value,
            });
        }
        if let Some(value) = acct.nonce_post_state() {
            out.push(Write::Nonce {
                address: acct.address,
                value,
            });
        }
        if let Some(value) = acct.code_post_state() {
            out.push(Write::Code {
                address: acct.address,
                value: value.clone(),
            });
        }
    }
    out
}

/// Flatten one `prestateTracer{diffMode}` result object (`{pre, post}`).
pub fn flatten_trace_diff(result: &Value) -> Vec<Write> {
    let mut out = Vec::new();
    let pre = result.get("pre").and_then(Value::as_object);
    let Some(post) = result.get("post").and_then(Value::as_object) else {
        return out;
    };
    for (addr_str, post_acct) in post {
        let Ok(address) = addr_str.parse::<Address>() else {
            continue;
        };
        let pre_acct = pre.and_then(|p| p.get(addr_str));
        // Balance
        let post_balance = post_acct.get("balance").and_then(Value::as_str);
        let pre_balance = pre_acct
            .and_then(|p| p.get("balance"))
            .and_then(Value::as_str);
        if let Some(pb) = post_balance {
            if pre_balance != Some(pb) {
                if let Some(v) = parse_hex_u256(pb) {
                    out.push(Write::Balance { address, value: v });
                }
            }
        }
        // Nonce (tracer emits a JSON number)
        let post_nonce = post_acct.get("nonce").and_then(Value::as_u64);
        let pre_nonce = pre_acct.and_then(|p| p.get("nonce")).and_then(Value::as_u64);
        if let Some(n) = post_nonce {
            if pre_nonce != Some(n) {
                out.push(Write::Nonce { address, value: n });
            }
        }
        // Code
        let post_code = post_acct.get("code").and_then(Value::as_str);
        let pre_code = pre_acct.and_then(|p| p.get("code")).and_then(Value::as_str);
        if let Some(c) = post_code {
            if pre_code != Some(c) {
                if let Ok(bytes) = c.parse::<Bytes>() {
                    out.push(Write::Code { address, value: bytes });
                }
            }
        }
        // Storage
        if let Some(storage) = post_acct.get("storage").and_then(Value::as_object) {
            let pre_storage = pre_acct.and_then(|p| p.get("storage")).and_then(Value::as_object);
            for (slot_str, value) in storage {
                let Some(v) = value.as_str() else { continue };
                let pre_v = pre_storage.and_then(|s| s.get(slot_str)).and_then(Value::as_str);
                if pre_v == Some(v) {
                    continue;
                }
                if let (Some(slot), Some(value)) = (parse_hex_u256(slot_str), parse_hex_u256(v)) {
                    out.push(Write::Storage { address, slot, value });
                }
            }
        }
    }
    out
}

/// `debug_traceBlockByNumber(..., diffMode)` returns one entry per tx:
/// `[{txHash, result: {pre, post}}, ...]`. Flatten all of them.
pub fn flatten_trace_block(traces: &Value) -> Result<Vec<Write>> {
    let arr = traces.as_array().context("trace block: expected array")?;
    let mut out = Vec::new();
    for entry in arr {
        if let Some(result) = entry.get("result") {
            out.extend(flatten_trace_diff(result));
        }
    }
    Ok(out)
}

pub fn parse_hex_u256(s: &str) -> Option<U256> {
    let t = s.trim_start_matches("0x");
    if t.is_empty() {
        return Some(U256::ZERO);
    }
    U256::from_str_radix(t, 16).ok()
}

// ---- measured padding rules ---------------------------------------------

/// 32-byte data hex (storage values, code).
/// `keccak256(abi.encode(key, slot))` - the storage slot of a Solidity mapping.
pub fn mapping_slot(key: Address, slot: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(key.as_slice());
    buf[32..64].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(alloy_primitives::keccak256(buf).0)
}

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
        assert_eq!(pad_data32(v), "0x0000000000000000000000000000000000000000000000000000000000000123");
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

    #[test]
    fn trace_diff_flattening() {
        // Shape measured on anvil 1.8.5 (prestateTracer diffMode).
        let trace: Value = serde_json::from_str(
            r#"{
              "post": {
                "0x0000000000000000000000000000000000000000": {"balance":"0x1319718a5000"},
                "0x70997970c51812dc3a010c7d01b50e0d17dc79c8": {"balance":"0xde0b6b3a7640001"},
                "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266": {"balance":"0x21e19e0a387cf2b5fff","nonce":1},
                "0x00000000000000000000000000000000000000aa": {"storage":{"0x1":"0x2a"}}
              },
              "pre": {
                "0x0000000000000000000000000000000000000000": {"balance":"0x0"},
                "0x70997970c51812dc3a010c7d01b50e0d17dc79c8": {"balance":"0xde0b6b3a7640000"},
                "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266": {"balance":"0x21e19e0a387cf2b5ffe","nonce":0}
              }
            }"#,
        )
        .unwrap();
        let writes = flatten_trace_diff(&trace);
        // 3 balances + 1 nonce + 1 storage; coinbase excluded only if unchanged (it changed)
        assert!(writes.iter().any(|w| matches!(w, Write::Storage { .. })));
        assert!(writes.iter().any(|w| matches!(w, Write::Nonce { value: 1, .. })));
        assert_eq!(
            writes
                .iter()
                .filter(|w| matches!(w, Write::Balance { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn bal_flatten_uses_post_states() {
        // Two writes to the same slot: only the last must survive.
        let bal: BlockAccessList = serde_json::from_str(
            // Canonical eth_getBlockAccessList shape ({index, value}); alloy's
            // serde renames also accept blockAccessIndex/newValue aliases.
            r#"[{
              "address":"0x00000000000000000000000000000000000000aa",
              "storageChanges":[
                {"slot":"0x1","changes":[{"index":1,"value":"0x2a"},{"index":2,"value":"0x2b"}]}
              ],
              "storageReads":["0x9"],
              "balanceChanges":[{"index":1,"value":"0x64"}],
              "nonceChanges":[],
              "codeChanges":[]
            }]"#,
        )
        .unwrap();
        let writes = flatten_bal(&bal);
        assert_eq!(writes.len(), 2);
        let storage = writes
            .iter()
            .find(|w| matches!(w, Write::Storage { .. }))
            .unwrap();
        match storage {
            Write::Storage { value, .. } => assert_eq!(*value, U256::from(0x2bu64)),
            _ => unreachable!(),
        }
        let balance = writes
            .iter()
            .find(|w| matches!(w, Write::Balance { .. }))
            .unwrap();
        match balance {
            Write::Balance { value, .. } => assert_eq!(*value, U256::from(100u64)),
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod mapping_slot_tests {
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
