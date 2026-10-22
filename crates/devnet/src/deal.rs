//! `deal` — the unlimited ERC20 faucet (plan §4.4).
//!
//! Anvil's own `anvil_dealERC20` cannot discover balance slots on a forked,
//! no-mining chain ("no slot found", measured): its probe writes never reach a
//! mined state. This one discovers the slot with `eth_call` + a `stateDiff`
//! override — no mining involved — then writes it silently and marks it
//! devnet-owned so parent diffs can never overwrite it.
use crate::error::Result;
use crate::metrics::Metrics;
use crate::shadow::{save_registry, Shadow};
use crate::shared::Shared;
use alloy_primitives::{Address, Bytes, U256};
use mirror_anvil::Rpc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct DealRequest {
    pub token: Address,
    pub to: Address,
    /// Base units (e.g. 100 USDC = 100000000).
    pub amount: U256,
}

/// The deal's answer, shaped for the admin route: exactly one of the cases —
/// a discovered slot, a probe fault, no standard layout, or a failed write.
/// `None` fields are skipped, so each case serializes to its own JSON shape.
#[derive(Debug, Clone, Serialize)]
pub struct DealOutcome {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<Value>,
}

/// Discover the token's balance slot and write `amount` silently, marking the
/// write devnet-owned. A probe that could not run is a fault, not "this slot is
/// not the balance slot" (boundary-discipline): if no slot matches, the fault
/// is reported instead of a layout verdict.
pub async fn deal(
    target: &Rpc,
    shadow: &Shared<Shadow>,
    shadow_path: &Path,
    metrics: &Metrics,
    chunk_size: usize,
    req: &DealRequest,
) -> Result<DealOutcome> {
    let value = format!("0x{:064x}", req.amount);
    // Probe with a sentinel, not the target: a balance that already equals the
    // target makes *any* slot look like the balance slot (measured: a re-run
    // matched slot 0 because the previous deal had left the exact amount at
    // slot 3). The sentinel stays below 2^255 so packed layouts (USDC's
    // balanceAndBlacklistStates) return it unmasked.
    let sentinel: U256 = (U256::from(0x5eed_0000u64) << 192) | U256::from(0x1234_5678u64);
    let sentinel_hex = format!("0x{sentinel:064x}");
    // ABI-encode balanceOf(address): selector + the 32-byte left-padded address.
    // Built byte-wise: the old `format!`-then-decode path could silently yield
    // empty calldata (a bare `0x` compared against the sentinel) if it ever
    // failed to parse.
    let mut calldata = Vec::with_capacity(36);
    calldata.extend_from_slice(&[0x70, 0xa0, 0x82, 0x31]);
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(req.to.as_slice());
    let data = Bytes::from(calldata);
    // Candidate standard layouts: balanceOf is `mapping(address => uint256)` at
    // some low slot; packed layouts (USDC's balanceAndBlacklistStates) put the
    // balance in the low 255 bits of the same shape, so the probe finds both.
    let (found, probes, fault) =
        discover_balance_slot(target, req.token, req.to, &data, &sentinel_hex).await;
    if let Some((idx, idx_hex)) = found {
        let w = mirror_anvil::Write::Storage {
            address: req.token,
            slot: idx,
            value: req.amount,
        };
        let applied =
            match mirror_anvil::apply_writes(target, std::slice::from_ref(&w), chunk_size).await {
                Ok(applied) => applied,
                Err(e) => {
                    return Ok(DealOutcome {
                        ok: false,
                        slot: Some(idx_hex),
                        balance: None,
                        error: Some(format!("write failed: {e}")),
                        probes: Vec::new(),
                    })
                }
            };
        if applied != 1 {
            // A refused write (e.g. `-32602`) must not be marked: a mark on a
            // slot whose write never landed would freeze mainnet's value under
            // a devnet mark - the worst of both worlds.
            return Ok(DealOutcome {
                ok: false,
                slot: Some(idx_hex),
                balance: None,
                error: Some(format!("write refused ({applied} of 1 applied)")),
                probes: Vec::new(),
            });
        }
        // Read back *before* marking: only a verified write becomes
        // devnet-owned. A read fault leaves the slot unmarked (nothing was
        // claimed) and the balance absent, never an empty string that looks
        // like a value.
        let after = target
            .call_latest(req.token, &data)
            .await
            .map(|b| format!("0x{}", alloy_primitives::hex::encode(b)))
            .ok();
        let ok = after.as_deref() == Some(value.as_str());
        if ok {
            {
                let mut s = shadow.lock()?;
                s.mark(&w);
            }
            save_registry(shadow, shadow_path, metrics)?;
        }
        tracing::info!(
            token = %req.token,
            to = %req.to,
            slot = %idx_hex,
            ok,
            "devnet ERC20 deal (silent storage write, marked devnet-owned)"
        );
        return Ok(DealOutcome {
            ok,
            slot: Some(idx_hex),
            balance: after,
            error: None,
            probes: Vec::new(),
        });
    }
    if let Some(e) = fault {
        return Ok(DealOutcome {
            ok: false,
            slot: None,
            balance: None,
            error: Some(format!("balance-slot probe failed: {e}")),
            probes,
        });
    }
    Ok(DealOutcome {
        ok: false,
        slot: None,
        balance: None,
        error: Some(
            "no standard balance slot found in slots 0..16 (non-standard layout): \
             write the slot yourself with anvil_setStorageAt"
                .to_string(),
        ),
        probes,
    })
}

/// Probe slots 0..16 for the balance slot: write the sentinel into the mapping
/// slot with a `stateDiff` override and read `balanceOf` back - one `eth_call`
/// per probe, no mining, ~5 ms. 0..16 covers the standard layouts plus the
/// common packed ones (USDC's balanceAndBlacklistStates sits at slot 9).
///
/// Returns the found slot (with its hex index), the first four probe records
/// (the error shape carries them) and the first probe *fault*: a probe that
/// could not run is not "this is not the balance slot" (boundary-discipline).
async fn discover_balance_slot(
    target: &Rpc,
    token: Address,
    to: Address,
    data: &Bytes,
    sentinel_hex: &str,
) -> (
    Option<(U256, String)>,
    Vec<Value>,
    Option<mirror_anvil::Error>,
) {
    let mut probes: Vec<Value> = Vec::new();
    let mut fault: Option<mirror_anvil::Error> = None;
    for slot in 0u64..16 {
        let idx = mirror_anvil::mapping_slot(to, U256::from(slot));
        let idx_hex = format!("0x{idx:064x}");
        // Address keys must be plain `0x`-prefixed hex (`{:#x}` on an alloy
        // Address is not guaranteed to be) and override *values* must be full
        // 32-byte hex: anvil answers a quantized value with "invalid string
        // length" (measured).
        let over = json!({
            format!("0x{}", alloy_primitives::hex::encode(token)): { "stateDiff": { idx_hex.clone(): sentinel_hex } }
        });
        let probe = match target.call_with_override(token, data, &over).await {
            Ok(probe) => Some(probe),
            Err(e) => {
                tracing::warn!(slot = %idx_hex, error = %e, "deal probe failed");
                fault = Some(e);
                None
            }
        };
        if probes.len() < 4 {
            probes.push(json!({
                "slot": idx_hex,
                "result": probe.clone(),
                "want": sentinel_hex,
            }));
        }
        if probe.as_deref() == Some(sentinel_hex) {
            return (Some((idx, idx_hex)), probes, fault);
        }
    }
    (None, probes, fault)
}
