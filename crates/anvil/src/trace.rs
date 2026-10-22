//! prestateTracer diff flattening (`debug_traceBlockByNumber(..., diffMode)`) —
//! the fallback when a BAL is unavailable. Shapes measured on anvil 1.8.5.
use alloy_primitives::{Address, Bytes, U256};
use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    Storage {
        address: Address,
        slot: U256,
        value: U256,
    },
    Balance {
        address: Address,
        value: U256,
    },
    Nonce {
        address: Address,
        value: u64,
    },
    Code {
        address: Address,
        value: Bytes,
    },
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
            let pre_storage = pre_acct
                .and_then(|p| p.get("storage"))
                .and_then(Value::as_object);
            for (slot_str, value) in storage {
                let Some(v) = value.as_str() else { continue };
                let pre_v = pre_storage
                    .and_then(|s| s.get(slot_str))
                    .and_then(Value::as_str);
                if pre_v == Some(v) {
                    continue;
                }
                if let (Some(slot), Some(value)) = (parse_hex_u256(slot_str), parse_hex_u256(v)) {
                    out.push(Write::Storage {
                        address,
                        slot,
                        value,
                    });
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(writes
            .iter()
            .any(|w| matches!(w, Write::Nonce { value: 1, .. })));
        assert_eq!(
            writes
                .iter()
                .filter(|w| matches!(w, Write::Balance { .. }))
                .count(),
            3
        );
    }
}
