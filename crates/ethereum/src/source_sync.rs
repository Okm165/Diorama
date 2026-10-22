//! Top-up sync: feed reth the execution payloads it is missing, using only the
//! local consensus client (the EIP-8379 "top-up" choreography over the Engine
//! API).
//!
//! Why this exists
//! ---------------
//! The consensus layer drives the execution layer over the Engine API. After a
//! checkpoint sync - or after any EL restart - the CL's head is far ahead of
//! the EL's. The CL does not re-deliver payloads it has already imported: the
//! EL is expected to fetch the missing ancestors from its own P2P network. A
//! self-contained mirror has no external EL peers, so the engine performs the
//! top-up itself: for every beacon block between the EL head and the CL head it
//! calls `engine_newPayloadV4` in ascending order and then
//! `engine_forkchoiceUpdatedV3`, which is exactly what EIP-8379 specifies
//! ("the consensus client feeds the execution client each block that
//! immediately follows the execution client head, until the execution client
//! reaches the consensus client head").
//!
//! Data comes from the local Lighthouse only (Lighthouse therefore runs with
//! `--prune-payloads=false`), plus reth's authrpc:
//!   - `GET /eth/v1/beacon/headers/head`   -> CL head slot
//!   - `GET /eth/v2/beacon/blocks/{slot}`  -> beacon block + execution payload
//!
//! No P2P peers, no public RPC.
use crate::ctx::Ctx;
use crate::error::{BeaconError, Error, Result};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Mainnet genesis time (beacon chain); slot = (timestamp - genesis) / 12.
const MAINNET_GENESIS_TIME: u64 = 1_606_824_023;
const SECONDS_PER_SLOT: u64 = 12;
const ZERO_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

/// Beacon-API (snake_case) -> Engine-API (camelCase) payload field map. Only
/// these fields are forwarded: the Engine API schema is strict and the beacon
/// block may carry extra fields.
const PAYLOAD_FIELDS: &[(&str, &str)] = &[
    ("parent_hash", "parentHash"),
    ("fee_recipient", "feeRecipient"),
    ("state_root", "stateRoot"),
    ("receipts_root", "receiptsRoot"),
    ("logs_bloom", "logsBloom"),
    ("prev_randao", "prevRandao"),
    ("block_number", "blockNumber"),
    ("gas_limit", "gasLimit"),
    ("gas_used", "gasUsed"),
    ("timestamp", "timestamp"),
    ("extra_data", "extraData"),
    ("base_fee_per_gas", "baseFeePerGas"),
    ("block_hash", "blockHash"),
    ("transactions", "transactions"),
    ("blob_gas_used", "blobGasUsed"),
    ("excess_blob_gas", "excessBlobGas"),
];

const WITHDRAWAL_FIELDS: &[(&str, &str)] = &[
    ("index", "index"),
    ("validator_index", "validatorIndex"),
    ("address", "address"),
    ("amount", "amount"),
];

pub async fn source_sync_loop(ctx: Arc<Ctx>) {
    let Some(engine) = ctx.engine.clone() else {
        return;
    };
    let interval = Duration::from_millis(ctx.cfg.services.engine.source_sync.interval_ms.max(250));
    let mut last_report = Instant::now();
    // CL head execution block, cached for a minute (one beacon call per minute).
    let mut head_cache: Option<(Instant, u64)> = None;
    loop {
        match top_up(&ctx, &engine).await {
            Ok(0) => tokio::time::sleep(interval).await,
            Ok(n) => {
                ctx.metrics
                    .add(&ctx.metrics.source_sync_delivered, n as u64);
                if last_report.elapsed() > Duration::from_secs(5) {
                    if let (Some(cl_head), Ok(reth_head)) = (
                        cl_head_el_block(&ctx, &mut head_cache).await,
                        ctx.reth.block_number().await,
                    ) {
                        ctx.metrics.set(
                            &ctx.metrics.source_sync_gap_blocks,
                            cl_head.saturating_sub(reth_head),
                        );
                    }
                    tracing::info!(payloads = n, "top-up: delivered payloads to reth");
                    last_report = Instant::now();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "top-up failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// The CL head's execution block number, cached for a minute. Metrics-only:
/// a beacon fault here degrades the gauge, not the top-up itself.
async fn cl_head_el_block(ctx: &Ctx, cache: &mut Option<(Instant, u64)>) -> Option<u64> {
    if let Some((at, n)) = cache {
        if at.elapsed() < Duration::from_secs(60) {
            return Some(*n);
        }
    }
    let slot = ctx.beacon.head_slot().await.ok()?;
    let block = ctx.beacon.block_by_slot(slot).await.ok()??;
    let n = quantity_u64(block.execution_payload()?.get("block_number")).ok()?;
    *cache = Some((Instant::now(), n));
    Some(n)
}

/// One pass: deliver as many contiguous payloads as the budget allows.
/// Returns how many were delivered.
async fn top_up(ctx: &Ctx, engine: &crate::engine_api::EngineClient) -> Result<usize> {
    if !ctx.beacon.enabled() {
        // No beacon configured: the top-up has no source. A fact, not a fault.
        return Ok(0);
    }
    let el_head = ctx.reth.block_number().await?;
    let el = ctx.reth.block_by_number(el_head, false).await?;
    let cl_head = ctx.beacon.head_slot().await?;
    // First slot whose payload can follow the EL head.
    let mut slot =
        (el.header.timestamp.saturating_sub(MAINNET_GENESIS_TIME)) / SECONDS_PER_SLOT + 1;
    if slot > cl_head {
        return Ok(0);
    }
    let budget = ctx.cfg.services.engine.source_sync.max_per_pass.max(1) as usize;
    let mut delivered = 0usize;
    while slot <= cl_head && delivered < budget {
        let Some(block) = ctx.beacon.block_by_slot(slot).await? else {
            slot += 1; // missed slot
            continue;
        };
        let Some(payload) = block.execution_payload() else {
            slot += 1; // no payload in this slot
            continue;
        };
        let number = quantity_u64(payload.get("block_number"))?;
        if number <= el_head {
            slot += 1; // already executed
            continue;
        }
        if number != el_head + delivered as u64 + 1 {
            return Err(Error::TopUpGap {
                el_head,
                number,
                slot,
            });
        }
        let payload_hash = payload
            .get("block_hash")
            .and_then(|v| v.as_str())
            .ok_or(BeaconError::Payload {
                what: "payload has no block_hash",
            })?
            .to_string();
        let versioned_hashes = block
            .blob_versioned_hashes()
            .iter()
            .map(|h| format!("0x{}", hex::encode(h)))
            .collect::<Vec<_>>();
        let parent_beacon_block_root = block.parent_root();
        let requests = block.execution_requests();
        let payload_json = to_engine_payload(payload)?;
        let (status, message) = engine
            .new_payload(payload_json, versioned_hashes, parent_beacon_block_root, requests)
            .await?;
        match status.as_str() {
            "VALID" | "ACCEPTED" => {}
            other => {
                return Err(Error::PayloadRejected {
                    number,
                    status: other.to_string(),
                    message,
                })
            }
        }
        engine.forkchoice_updated(&payload_hash, ZERO_HASH, ZERO_HASH).await?;
        tracing::debug!(block = number, slot, "top-up delivered");
        delivered += 1;
        slot += 1;
    }
    Ok(delivered)
}

/// The beacon API serves quantities as decimal strings; the Engine API wants
/// hex quantities. Accept both (single implementation lives in `blob`).
fn quantity_u64(v: Option<&Value>) -> Result<u64, BeaconError> {
    let s = v
        .and_then(|v| v.as_str())
        .ok_or(BeaconError::Payload {
            what: "missing quantity",
        })?;
    crate::blob::parse_quantity(s).ok_or(BeaconError::Payload {
        what: "bad quantity",
    })
}

/// Engine API quantity (hex).
fn hex_quantity(v: &Value) -> Result<Value, BeaconError> {
    Ok(Value::String(format!("0x{:x}", quantity_u64(Some(v))?)))
}

/// Rewrite a beacon-API execution payload into the Engine API shape.
fn to_engine_payload(src: &Value) -> Result<Value, BeaconError> {
    let obj = src.as_object().ok_or(BeaconError::Payload {
        what: "execution_payload is not an object",
    })?;
    let mut out = Map::new();
    for (snake, camel) in PAYLOAD_FIELDS {
        if let Some(v) = obj.get(*snake) {
            out.insert((*camel).to_string(), v.clone());
        }
    }
    if let Some(Value::Array(withdrawals)) = obj.get("withdrawals") {
        let mapped = withdrawals
            .iter()
            .map(|w| {
                let mut m = Map::new();
                for (snake, camel) in WITHDRAWAL_FIELDS {
                    if let Some(v) = w.get(*snake) {
                        m.insert((*camel).to_string(), v.clone());
                    }
                }
                Value::Object(m)
            })
            .collect();
        out.insert("withdrawals".to_string(), Value::Array(mapped));
    }
    // Beacon API quantities are decimal strings; the Engine API requires hex.
    for key in [
        "blockNumber",
        "gasLimit",
        "gasUsed",
        "timestamp",
        "baseFeePerGas",
        "blobGasUsed",
        "excessBlobGas",
    ] {
        if let Some(v) = out.get(key).cloned() {
            out.insert(key.to_string(), hex_quantity(&v)?);
        }
    }
    if let Some(Value::Array(withdrawals)) = out.get("withdrawals").cloned() {
        let mut mapped = Vec::with_capacity(withdrawals.len());
        for w in withdrawals {
            let mut m = w.as_object().cloned().unwrap_or_default();
            for key in ["index", "validatorIndex", "amount"] {
                if let Some(v) = m.get(key).cloned() {
                    m.insert(key.to_string(), hex_quantity(&v)?);
                }
            }
            mapped.push(Value::Object(m));
        }
        out.insert("withdrawals".to_string(), Value::Array(mapped));
    }
    if out.get("parentHash").is_none() || out.get("blockHash").is_none() {
        return Err(BeaconError::Payload {
            what: "missing required fields",
        });
    }
    Ok(Value::Object(out))
}
