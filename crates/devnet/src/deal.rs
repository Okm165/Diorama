//! `deal` — the unlimited ERC20 faucet (plan §4.4).
//!
//! Anvil's own `anvil_dealERC20` cannot discover balance slots on a forked,
//! no-mining chain ("no slot found", measured): its probe writes never reach a
//! mined state. This one discovers the slot with `eth_call` + a `stateDiff`
//! override — no mining involved — then writes it silently and marks it
//! devnet-owned so parent diffs can never overwrite it.
use crate::admin::AdminSurface;
use crate::error::Result;
use crate::journal::{JournalEntry, JournalSink};
use crate::plane::Plane;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_eth::state::{AccountOverride, StateOverride};
use mirror_anvil::Rpc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
    pub(crate) probes: Vec<ProbeRecord>,
}

/// One slot-probe record (diagnostics carried by the error shape): the slot
/// tried, the node's answer (absent when the probe faulted), and the sentinel
/// it was looking for.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProbeRecord {
    pub slot: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Bytes>,
    pub want: String,
}

/// Discover the token's balance slot, write `amount` silently and verify it,
/// marking the write devnet-owned. A probe that could not run is a fault, not
/// "this slot is not the balance slot" (boundary-discipline): if no slot
/// matches, the fault is reported instead of a layout verdict.
///
/// `journal` records the write's before-value so `/admin/rewind` can undo it:
/// `Some(JournalSink)` on a deployment whose journal is its *own* writes
/// (the venue — its follow windows are source state, not journaled devnet
/// writes), `None` on the mainnet
/// (its journal is the applier's parent before-values; mixing engine writes in
/// would change what a rewind means).
pub async fn deal(
    surface: &AdminSurface<'_>,
    journal: Option<JournalSink<'_>>,
    req: &DealRequest,
) -> Result<DealOutcome> {
    let data = balance_of_calldata(req.to);
    let discovery = discover_balance_slot(surface.target, req, &data, deal_sentinel()).await;
    let Some(slot) = discovery.found else {
        return Ok(no_slot(discovery.fault, discovery.probes));
    };
    let write = mirror_anvil::Write::Storage {
        address: req.token,
        slot: slot.idx,
        value: req.amount,
    };
    // The undo entry's before-value must be read *before* the write; a fault
    // here refuses the deal with nothing written (fail-closed: an unjournaled
    // write on a journaling deployment is an undo that later writes a wrong
    // value).
    let before = read_before(
        surface.target,
        Journaling::of(journal.is_some()),
        req.token,
        slot.idx,
    )
        .await?;
    if let Err(refused) = write_slot(surface.target, surface.chunk_size, &write, &slot.hex).await {
        return Ok(refused);
    }
    // Read back *before* marking: only a verified write becomes devnet-owned.
    // A read fault leaves the slot unmarked (nothing was claimed) and the
    // balance absent, never an empty string that looks like a value.
    let after = surface.target.call_latest(req.token, &data).await.ok();
    let ok = after.as_ref() == Some(&req.amount.to_be_bytes::<32>().into());
    if ok {
        mark_owned(surface.plane, &write)?;
        record_undo(journal, req.token, slot.idx, before)?;
    }
    tracing::info!(
        token = %req.token,
        to = %req.to,
        slot = %slot.hex,
        ok,
        "devnet ERC20 deal (silent storage write, marked devnet-owned)"
    );
    Ok(DealOutcome {
        ok,
        slot: Some(slot.hex),
        balance: after,
        error: None,
        probes: Vec::new(),
    })
}

/// The no-slot outcome: the probe fault when one occurred, else the layout
/// verdict — a probe that could not run is never reported as a verdict.
fn no_slot(fault: Option<mirror_anvil::Error>, probes: Vec<ProbeRecord>) -> DealOutcome {
    let error = match fault {
        Some(e) => format!("balance-slot probe failed: {e}"),
        None => "no standard balance slot found in slots 0..16 (non-standard layout): \
                 write the slot yourself with anvil_setStorageAt"
            .to_string(),
    };
    DealOutcome {
        ok: false,
        slot: None,
        balance: None,
        error: Some(error),
        probes,
    }
}

/// The slot's before-value for the undo entry: recorded when the deployment
/// journals its own writes, frozen on a deployment whose deals have no undo
/// (the mainnet's).
enum Before {
    Recorded(U256),
    Frozen,
}

/// Read the slot's before-value when the deployment journals its own writes
/// (the rewind undo entry) — the mainnet does not journal its deals (frozen
/// behavior).
async fn read_before(
    target: &Rpc,
    journaling: Journaling,
    token: Address,
    slot: U256,
) -> Result<Before> {
    match journaling {
        Journaling::Journaled => Ok(Before::Recorded(U256::from_be_bytes(
            target.storage_at_latest(token, slot).await?.0,
        ))),
        Journaling::Frozen => Ok(Before::Frozen),
    }
}

/// Whether the deployment journals its own writes (the rewind undo entry) or
/// leaves them frozen (the mainnet's deals).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Journaling {
    /// Writes get an undo entry (`/admin/rewind` restores them).
    Journaled,
    /// The mainnet's frozen deals: no undo.
    Frozen,
}

impl Journaling {
    /// The deployment's own answer: a journal is configured or not.
    fn of(configured: bool) -> Self {
        if configured {
            Journaling::Journaled
        } else {
            Journaling::Frozen
        }
    }
}

/// Apply the write. `Err(outcome)` is the shaped refusal for a slot whose
/// write did not land: a refused write (e.g. `-32602`) must not be marked — a
/// mark on a slot whose write never landed would freeze the source's value
/// under a devnet mark, the worst of both worlds.
async fn write_slot(
    target: &Rpc,
    chunk_size: usize,
    write: &mirror_anvil::Write,
    slot_hex: &str,
) -> std::result::Result<(), DealOutcome> {
    match mirror_anvil::apply_writes(target, std::slice::from_ref(write), chunk_size).await {
        Ok(1) => Ok(()),
        Ok(applied) => Err(write_refused(
            slot_hex.to_string(),
            format!("write refused ({applied} of 1 applied)"),
        )),
        Err(e) => Err(write_refused(
            slot_hex.to_string(),
            format!("write failed: {e}"),
        )),
    }
}

/// Journal the undo entry (the before-value) when the deployment journals its
/// own writes.
fn record_undo(
    journal: Option<JournalSink<'_>>,
    token: Address,
    slot: U256,
    before: Before,
) -> Result<()> {
    if let (Some(sink), Before::Recorded(value)) = (journal, before) {
        sink.journal.record(
            sink.at,
            &[JournalEntry::Storage {
                address: token,
                slot,
                before: value,
            }],
        )?;
    }
    Ok(())
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
fn mark_owned(plane: &Plane, write: &mirror_anvil::Write) -> Result<()> {
    plane.shadow.lock()?.mark(write);
    plane.save_shadow()
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
/// One candidate probe's answer: the raw word, or the read fault.
enum ProbeAnswer {
    Answered(Bytes),
    Faulted(mirror_anvil::Error),
}

/// One candidate slot: override `token`'s slot `idx` with the full-width
/// sentinel word and re-run `balanceOf`. The override is alloy's own
/// state-override model — anvil rejects quantized (short) hex here (measured).
async fn probe_candidate(
    target: &Rpc,
    token: Address,
    data: &Bytes,
    idx: B256,
    sentinel: U256,
) -> ProbeAnswer {
    let over = StateOverride::from_iter([(
        token,
        AccountOverride {
            state_diff: Some([(idx, B256::from(sentinel))].into_iter().collect()),
            ..Default::default()
        },
    )]);
    match target.call_with_override(token, data, &over).await {
        Ok(probe) => ProbeAnswer::Answered(probe),
        Err(e) => ProbeAnswer::Faulted(e),
    }
}

/// A discovered ERC-20 balance slot: its mapping index and its hex form.
struct DiscoveredSlot {
    idx: U256,
    hex: String,
}

/// The deal's slot discovery: the found slot (when one matched), the probes
/// the caller reports, and the first fault seen (a fault keeps the probes;
/// the slot stays `None`).
struct SlotDiscovery {
    found: Option<DiscoveredSlot>,
    probes: Vec<ProbeRecord>,
    fault: Option<mirror_anvil::Error>,
}

async fn discover_balance_slot(
    target: &Rpc,
    req: &DealRequest,
    data: &Bytes,
    sentinel: U256,
) -> SlotDiscovery {
    let (token, to) = (req.token, req.to);
    let sentinel_hex = format!("0x{sentinel:064x}");
    let sentinel_bytes = sentinel.to_be_bytes::<32>();
    let mut probes: Vec<ProbeRecord> = Vec::new();
    let mut fault: Option<mirror_anvil::Error> = None;
    for slot in 0u64..16 {
        let idx = mirror_anvil::mapping_slot(to, U256::from(slot));
        let idx_hex = format!("0x{idx:064x}");
        let probe = match probe_candidate(target, token, data, B256::from(idx), sentinel).await {
            ProbeAnswer::Answered(probe) => Some(probe),
            ProbeAnswer::Faulted(e) => {
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
            return SlotDiscovery {
                found: Some(DiscoveredSlot { idx, hex: idx_hex }),
                probes,
                fault,
            };
        }
    }
    SlotDiscovery {
        found: None,
        probes,
        fault,
    }
}
