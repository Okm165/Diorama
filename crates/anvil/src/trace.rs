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

    /// The write's key half: what it writes, without the value. This is the
    /// vocabulary of the watch set, the shadow registry, the journal and the
    /// venue's declared closures — one key space for both mirrors.
    pub fn key(&self) -> Key {
        match self {
            Write::Storage { address, slot, .. } => Key::Storage {
                address: *address,
                slot: *slot,
            },
            Write::Balance { address, .. } => Key::Balance(*address),
            Write::Nonce { address, .. } => Key::Nonce(*address),
            Write::Code { address, .. } => Key::Code(*address),
        }
    }
}

/// A state key: `Write` without its value. `Key` is what a read asks for
/// (the venue's poll composer reads keys) and what a mark names (the shadow
/// registry's marks are keyed by it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Storage { address: Address, slot: U256 },
    Balance(Address),
    Nonce(Address),
    Code(Address),
}

impl Key {
    pub fn address(&self) -> Address {
        match self {
            Key::Storage { address, .. }
            | Key::Balance(address)
            | Key::Nonce(address)
            | Key::Code(address) => *address,
        }
    }
}

/// A state key's value, as a read returns it. `Write` is key + value; `Value`
/// is what the read seam answers, so the venue's poll composer can read a
/// plan of keys without matching on four separate methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Storage(U256),
    Balance(U256),
    Nonce(u64),
    Code(Bytes),
}

impl Write {
    /// Pair a key with its value. `None` when they do not match (a read plan
    /// bug): the caller keeps the key and treats the value as unusable, never
    /// writes a mistyped word.
    pub fn new(key: Key, value: Value) -> Option<Write> {
        match (key, value) {
            (Key::Storage { address, slot }, Value::Storage(value)) => {
                Some(Write::Storage {
                    address,
                    slot,
                    value,
                })
            }
            (Key::Balance(address), Value::Balance(value)) => {
                Some(Write::Balance { address, value })
            }
            (Key::Nonce(address), Value::Nonce(value)) => Some(Write::Nonce { address, value }),
            (Key::Code(address), Value::Code(value)) => Some(Write::Code { address, value }),
            _ => None,
        }
    }
}

/// Flatten one transaction's `prestateTracer{diffMode}` frame into writes:
/// the `pre` side lists the *changed* fields (measured: a read-only or no-op
/// touch appears nowhere), the `post` side omits fields that changed to zero
/// (measured on anvil 1.8.6: a drained storage slot is present in `pre` with
/// its old value and absent from `post`). A slot in `pre` but not in `post`
/// is therefore a write to zero — without this the shadow registry records a
/// stale non-zero value, and a same-pin restore resurrects it (measured: a
/// drained balance came back after a reset).
pub(crate) fn flatten_trace_diff(frame: &PreStateFrame) -> Vec<Write> {
    let Some(diff) = frame.as_diff() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (address, pre) in &diff.pre {
        let post = diff.post.get(address);
        push_storage(&mut out, *address, pre, post);
    }
    for (address, post) in &diff.post {
        let pre = diff.pre.get(address);
        push_balance(&mut out, *address, pre, post);
        push_nonce(&mut out, *address, pre, post);
        push_code(&mut out, *address, pre, post);
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

/// Storage writes, driven by `pre` (measured: `pre.storage` lists exactly the
/// changed slots — read-only and no-op touches appear nowhere — while `post`
/// omits slots that changed to zero; a drained slot is therefore
/// `pre`-present and `post`-absent, and must be recorded as a write to ZERO).
fn push_storage(out: &mut Vec<Write>, address: Address, pre: &AccountState, post: Option<&AccountState>) {
    for (slot, pre_value) in &pre.storage {
        let value = if let Some(post_value) = post.and_then(|p| p.storage.get(slot)) {
            if post_value == pre_value {
                continue;
            }
            U256::from_be_bytes(post_value.0)
        } else {
            if pre_value.is_zero() {
                continue;
            }
            U256::ZERO
        };
        out.push(Write::Storage {
            address,
            slot: U256::from_be_bytes(slot.0),
            value,
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

    /// A genuine excerpt of a zeroing transfer (measured 2026-10-22 on the
    /// scratch V): the drained slot sits in `pre` with its old value and is
    /// ABSENT from `post.storage` — anvil omits zeroed slots. Missing this
    /// made the shadow registry keep the stale non-zero value, and a same-pin
    /// restore resurrected a drained balance.
    #[test]
    fn a_slot_changed_to_zero_is_a_write() {
        let frame: PreStateFrame = serde_json::from_str(
            r#"{
              "pre": {
                "0x9b498c3c8a0b8cd8ba1d9851d40d186f1872b44e": {
                  "storage": {
                    "0x986d56fe2954bacde776e5a83b13aa605cf4db18d945bbea664eb3dc8135d4c1": "0x0000000000000000000000000000000000000000000000000000000000000064",
                    "0x4c9e99a9b0f9f4f7d2b6cf12a3b7e6c1a1e5b7d9e3f2a4c6b8d0e2f4a6c8e0f2": "0x0000000000000000000000000000000000000000000000000000000000000009"
                  }
                }
              },
              "post": {
                "0x9b498c3c8a0b8cd8ba1d9851d40d186f1872b44e": {
                  "storage": {
                    "0x4c9e99a9b0f9f4f7d2b6cf12a3b7e6c1a1e5b7d9e3f2a4c6b8d0e2f4a6c8e0f2": "0x000000000000000000000000000000000000000000000000000000000000006d"
                  }
                }
              }
            }"#,
        )
        .expect("measured frame");
        let writes = flatten_trace_diff(&frame);
        assert_eq!(writes.len(), 2, "{writes:?}");
        // BTreeMap order: the changed slot (4c9e…) precedes the zeroed one.
        // The changed slot keeps its post value.
        assert_eq!(
            writes[0],
            Write::Storage {
                address: "0x9b498c3c8a0b8cd8ba1d9851d40d186f1872b44e".parse().unwrap(),
                slot: U256::from_str_radix(
                    "4c9e99a9b0f9f4f7d2b6cf12a3b7e6c1a1e5b7d9e3f2a4c6b8d0e2f4a6c8e0f2",
                    16
                )
                .unwrap(),
                value: U256::from(0x6d_u64),
            }
        );
        // The zeroed slot (pre-present, post-absent) is recorded as ZERO.
        assert_eq!(
            writes[1],
            Write::Storage {
                address: "0x9b498c3c8a0b8cd8ba1d9851d40d186f1872b44e".parse().unwrap(),
                slot: U256::from_str_radix(
                    "986d56fe2954bacde776e5a83b13aa605cf4db18d945bbea664eb3dc8135d4c1",
                    16
                )
                .unwrap(),
                value: U256::ZERO,
            }
        );
    }
}
