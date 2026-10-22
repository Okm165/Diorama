//! prestateTracer diff flattening (`debug_traceBlockByNumber(..., diffMode)`) —
//! the fallback when a BAL is unavailable. The wire shapes are alloy's
//! (`alloy_rpc_types_trace::geth`); the flattening is one small function per
//! account field, so a tracer shape change lands as a compile error, never as
//! a silently skipped write.
//!
//! Measured on anvil 1.8.6 (the mirror's pin): the frame lists only *changed*
//! accounts in `post` (created accounts absent from `pre`, selfdestructed
//! absent from `post`), `balance`/`code`/`storage` as hex strings, `nonce` as
//! a JSON number, and storage keys/values as **full-width** 32-byte hex —
//! exactly what alloy's `AccountState` models, so the parse is the official
//! type's, not ours.
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_trace::geth::{AccountState, GethTrace, PreStateFrame, TraceResult};

/// One state write a trace diff reports.
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

/// Flatten one transaction's `prestateTracer{diffMode}` frame into writes:
/// every account the frame lists in `post` contributes the fields that
/// differ from `pre`.
pub fn flatten_trace_diff(frame: &PreStateFrame) -> Vec<Write> {
    let Some(diff) = frame.as_diff() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (address, post) in &diff.post {
        let pre = diff.pre.get(address);
        push_balance(&mut out, *address, pre, post);
        push_nonce(&mut out, *address, pre, post);
        push_code(&mut out, *address, pre, post);
        push_storage(&mut out, *address, pre, post);
    }
    out
}

fn push_balance(out: &mut Vec<Write>, address: Address, pre: Option<&AccountState>, post: &AccountState) {
    if let Some(value) = post.balance {
        if pre.and_then(|p| p.balance) != Some(value) {
            out.push(Write::Balance { address, value });
        }
    }
}

fn push_nonce(out: &mut Vec<Write>, address: Address, pre: Option<&AccountState>, post: &AccountState) {
    if let Some(value) = post.nonce {
        if pre.and_then(|p| p.nonce) != Some(value) {
            out.push(Write::Nonce { address, value });
        }
    }
}

fn push_code(out: &mut Vec<Write>, address: Address, pre: Option<&AccountState>, post: &AccountState) {
    if let Some(value) = &post.code {
        if pre.and_then(|p| p.code.as_ref()) != Some(value) {
            out.push(Write::Code {
                address,
                value: value.clone(),
            });
        }
    }
}

fn push_storage(out: &mut Vec<Write>, address: Address, pre: Option<&AccountState>, post: &AccountState) {
    for (slot, value) in &post.storage {
        if pre.and_then(|p| p.storage.get(slot)) == Some(value) {
            continue;
        }
        out.push(Write::Storage {
            address,
            slot: U256::from_be_bytes(slot.0),
            value: U256::from_be_bytes(value.0),
        });
    }
}

/// Flatten one transaction's trace answer: the prestate frame's writes, or
/// none when the tracer answered with another frame (we always ask for the
/// prestate tracer, so another frame means nothing to mark).
pub fn flatten_trace(trace: &GethTrace) -> Vec<Write> {
    match trace {
        GethTrace::PreStateTracer(frame) => flatten_trace_diff(frame),
        _ => Vec::new(),
    }
}

/// Flatten a `debug_traceBlockByNumber` answer (`[{txHash, result}]`):
/// the prestate frames of every successfully traced transaction, in order.
/// A transaction the tracer could not process contributes no writes — the
/// caller's convergence repair is the backstop for a partial flattening.
pub fn flatten_trace_block(traces: &[TraceResult]) -> Vec<Write> {
    traces
        .iter()
        .filter_map(TraceResult::success)
        .flat_map(flatten_trace)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A genuine excerpt of anvil 1.8.6 `prestateTracer` diffMode output
    /// (measured 2026-10-22 on the dry-run fork): storage keys/values are
    /// full-width 32-byte hex; `nonce` is a JSON number; `post` carries only
    /// the fields that changed, `pre` the touched account's prior state.
    const MEASURED_FRAME: &str = r#"{
      "post": {
        "0x5875db54cd9ae2b2a875e09bb731772297ae9d92": {"balance": "0xbdc602038b410e5", "nonce": 172449},
        "0x0000000aa232009084bd71a5797d089aa4edfad4": {
          "storage": {
            "0x0000000000000000000000000000000000000000000000000000000000000003": "0x000000004be6a279be925c8183c6ab0b5b60554bc7adbacf0000000001908b7a",
            "0xc00849b9357c3b9274369e6c5b3b60d0742a7ddad7c3c48b10e9b9109914640a": "0x0000000000000000000000000000000000000000000000000000001cfc5bd1cb"
          }
        }
      },
      "pre": {
        "0x5875db54cd9ae2b2a875e09bb731772297ae9d92": {"balance": "0xbdc860812f71543", "nonce": 172448},
        "0x0000000aa232009084bd71a5797d089aa4edfad4": {
          "balance": "0x0",
          "nonce": 8,
          "storage": {
            "0x0000000000000000000000000000000000000000000000000000000000000003": "0x000000004be6a279be925c8183c6ab0b5b60554bc7adbacf0000000001908b79",
            "0xc00849b9357c3b9274369e6c5b3b60d0742a7ddad7c3c48b10e9b9109914640a": "0x0000000000000000000000000000000000000000000000000000001cf99154b7"
          }
        }
      }
    }"#;

    #[test]
    fn trace_diff_flattening() {
        let frame: PreStateFrame = serde_json::from_str(MEASURED_FRAME).expect("measured frame");
        let writes = flatten_trace_diff(&frame);
        assert_eq!(
            writes,
            vec![
                Write::Storage {
                    address: "0x0000000aa232009084bd71a5797d089aa4edfad4".parse().unwrap(),
                    slot: U256::from(3u64),
                    value: U256::from_str_radix(
                        "4be6a279be925c8183c6ab0b5b60554bc7adbacf0000000001908b7a",
                        16
                    )
                    .unwrap(),
                },
                Write::Storage {
                    address: "0x0000000aa232009084bd71a5797d089aa4edfad4".parse().unwrap(),
                    slot: U256::from_str_radix(
                        "c00849b9357c3b9274369e6c5b3b60d0742a7ddad7c3c48b10e9b9109914640a",
                        16
                    )
                    .unwrap(),
                    value: U256::from_str_radix("1cfc5bd1cb", 16).unwrap(),
                },
                Write::Balance {
                    address: "0x5875db54cd9ae2b2a875e09bb731772297ae9d92".parse().unwrap(),
                    value: U256::from(0x0bdc_6020_38b4_10e5_u64),
                },
                Write::Nonce {
                    address: "0x5875db54cd9ae2b2a875e09bb731772297ae9d92".parse().unwrap(),
                    value: 172_449,
                },
            ]
        );
    }

    #[test]
    fn unchanged_fields_are_not_writes() {
        // The same value in `pre` and `post` is not a write (the scanner must
        // not mark untouched state as devnet-owned).
        let frame: PreStateFrame = serde_json::from_str(
            r#"{"pre":{"0x0000000000000000000000000000000000000001":{"balance":"0x1"}},
                "post":{"0x0000000000000000000000000000000000000001":{"balance":"0x1"}}}"#,
        )
        .expect("frame");
        assert!(flatten_trace_diff(&frame).is_empty());
    }
}
