//! `deal` — the unlimited ERC20 faucet (plan §4.4).
//!
//! Anvil's own `anvil_dealERC20` cannot discover balance slots on a forked,
//! no-mining chain ("no slot found", measured): its probe writes never reach a
//! mined state. This one discovers the slot with `eth_call` + a `stateDiff`
//! override — no mining involved — then writes it silently and marks it
//! devnet-owned so parent diffs can never overwrite it.
use crate::error::Result;
use crate::journal::{Journal, JournalEntry};
use crate::metrics::Metrics;
use crate::shadow::{save_registry, Shadow};
use crate::shared::Shared;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_eth::state::{AccountOverride, StateOverride};
use mirror_anvil::Rpc;
use serde::{Deserialize, Serialize};
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
    pub balance: Option<Bytes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<ProbeRecord>,
}

/// One slot-probe record (diagnostics carried by the error shape): the slot
/// tried, the node's answer (absent when the probe faulted), and the sentinel
/// it was looking for.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeRecord {
    pub slot: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Bytes>,
    pub want: String,
}

/// Discover the token's balance slot and write `amount` silently, marking the
/// write devnet-owned. A probe that could not run is a fault, not "this slot is
/// not the balance slot" (boundary-discipline): if no slot matches, the fault
/// is reported instead of a layout verdict.
///
/// `journal` records the write's before-value so `/admin/rewind` can undo it:
/// `Some((journal, block))` on a deployment whose journal is its *own* writes
/// (the venue — it has no parent stream to journal), `None` on the mainnet
/// (its journal is the applier's parent before-values; mixing engine writes in
/// would change what a rewind means).
pub async fn deal(
    target: &Rpc,
    shadow: &Shared<Shadow>,
    shadow_path: &Path,
    metrics: &Metrics,
    chunk_size: usize,
    journal: Option<(&Journal, u64)>,
    req: &DealRequest,
) -> Result<DealOutcome> {
    let data = balance_of_calldata(req.to);
    let (found, probes, fault) =
        discover_balance_slot(target, req.token, req.to, &data, deal_sentinel()).await;
    let Some((slot, slot_hex)) = found else {
        let error = match fault {
            Some(e) => format!("balance-slot probe failed: {e}"),
            None => "no standard balance slot found in slots 0..16 (non-standard layout): \
                     write the slot yourself with anvil_setStorageAt"
                .to_string(),
        };
        return Ok(DealOutcome {
            ok: false,
            slot: None,
            balance: None,
            error: Some(error),
            probes,
        });
    };
    let write = mirror_anvil::Write::Storage {
        address: req.token,
        slot,
        value: req.amount,
    };
    // The undo entry's before-value must be read *before* the write; a fault
    // here refuses the deal with nothing written (fail-closed: an unjournaled
    // write on a journaling deployment is an undo that later writes a wrong
    // value). The mainnet does not journal its deals (`None`) — frozen.
    let before = match journal {
        Some(_) => Some(target.storage_at_latest(req.token, slot).await?),
        None => None,
    };
    match mirror_anvil::apply_writes(target, std::slice::from_ref(&write), chunk_size).await {
        Ok(1) => {}
        // A refused write (e.g. `-32602`) must not be marked: a mark on a
        // slot whose write never landed would freeze mainnet's value under a
        // devnet mark — the worst of both worlds.
        Ok(applied) => {
            return Ok(write_refused(
                slot_hex,
                format!("write refused ({applied} of 1 applied)"),
            ))
        }
        Err(e) => return Ok(write_refused(slot_hex, format!("write failed: {e}"))),
    }
    // Read back *before* marking: only a verified write becomes devnet-owned.
    // A read fault leaves the slot unmarked (nothing was claimed) and the
    // balance absent, never an empty string that looks like a value.
    let after = target.call_latest(req.token, &data).await.ok();
    let ok = after.as_ref() == Some(&req.amount.to_be_bytes::<32>().into());
    if ok {
        mark_owned(shadow, shadow_path, metrics, &write)?;
        if let (Some((journal, block)), Some(before)) = (journal, before) {
            journal.record(
                block,
                &[JournalEntry::Storage {
                    address: req.token,
                    slot,
                    before: U256::from_be_bytes(before.0),
                }],
            )?;
        }
    }
    tracing::info!(
        token = %req.token,
        to = %req.to,
        slot = %slot_hex,
        ok,
        "devnet ERC20 deal (silent storage write, marked devnet-owned)"
    );
    Ok(DealOutcome {
        ok,
        slot: Some(slot_hex),
        balance: after,
        error: None,
        probes: Vec::new(),
    })
}

/// The outcome for a discovered slot whose write did not land: nothing is
/// marked, and the reason is named.
fn write_refused(slot: String, error: String) -> DealOutcome {
    DealOutcome {
        ok: false,
        slot: Some(slot),
        balance: None,
        error: Some(error),
        probes: Vec::new(),
    }
}

/// Mark the verified write devnet-owned and persist the registry.
fn mark_owned(
    shadow: &Shared<Shadow>,
    shadow_path: &Path,
    metrics: &Metrics,
    write: &mirror_anvil::Write,
) -> Result<()> {
    shadow.lock()?.mark(write);
    save_registry(shadow, shadow_path, metrics)
}

/// ABI `balanceOf(address)` calldata: selector + the 32-byte left-padded
/// address. Built byte-wise: the old `format!`-then-decode path could
/// silently yield empty calldata (a bare `0x` compared against the sentinel)
/// if it ever failed to parse.
fn balance_of_calldata(to: Address) -> Bytes {
    let mut calldata = Vec::with_capacity(36);
    calldata.extend_from_slice(&[0x70, 0xa0, 0x82, 0x31]);
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(to.as_slice());
    Bytes::from(calldata)
}

/// The probe sentinel: deliberately not the target amount — a balance that
/// already equals the target makes *any* slot look like the balance slot
/// (measured: a re-run matched slot 0 because the previous deal had left the
/// exact amount at slot 3). Kept below 2^255 so packed layouts (USDC's
/// `balanceAndBlacklistStates`) return it unmasked.
fn deal_sentinel() -> U256 {
    (U256::from(0x5eed_0000u64) << 192) | U256::from(0x1234_5678u64)
}

/// Probe slots 0..16 for the balance slot: write the sentinel into the mapping
/// slot with a `stateDiff` override and read `balanceOf` back — one `eth_call`
/// per probe, no mining, ~5 ms. 0..16 covers the standard layouts plus the
/// common packed ones (USDC's `balanceAndBlacklistStates` sits at slot 9).
///
/// Returns the found slot (with its hex index), the first four probe records
/// (the error shape carries them) and the first probe *fault*: a probe that
/// could not run is not "this is not the balance slot" (boundary-discipline).
async fn discover_balance_slot(
    target: &Rpc,
    token: Address,
    to: Address,
    data: &Bytes,
    sentinel: U256,
) -> (
    Option<(U256, String)>,
    Vec<ProbeRecord>,
    Option<mirror_anvil::Error>,
) {
    let sentinel_hex = format!("0x{sentinel:064x}");
    let sentinel_bytes = sentinel.to_be_bytes::<32>();
    let mut probes: Vec<ProbeRecord> = Vec::new();
    let mut fault: Option<mirror_anvil::Error> = None;
    for slot in 0u64..16 {
        let idx = mirror_anvil::mapping_slot(to, U256::from(slot));
        let idx_hex = format!("0x{idx:064x}");
        // The override is alloy's own state-override model: a `stateDiff`
        // entry keyed by the full-width 32-byte slot, valued by the full-width
        // sentinel word — anvil rejects quantized (short) hex here (measured).
        let over = StateOverride::from_iter([(
            token,
            AccountOverride {
                state_diff: Some([(B256::from(idx), B256::from(sentinel))].into_iter().collect()),
                ..Default::default()
            },
        )]);
        let probe = match target.call_with_override(token, data, &over).await {
            Ok(probe) => Some(probe),
            Err(e) => {
                tracing::warn!(slot = %idx_hex, error = %e, "deal probe failed");
                fault = Some(e);
                None
            }
        };
        if probes.len() < 4 {
            probes.push(ProbeRecord {
                slot: idx_hex.clone(),
                result: probe.clone(),
                want: sentinel_hex.clone(),
            });
        }
        if probe
            .as_ref()
            .is_some_and(|p| p.as_ref() == sentinel_bytes.as_slice())
        {
            return (Some((idx, idx_hex)), probes, fault);
        }
    }
    (None, probes, fault)
}
