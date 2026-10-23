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
//! Both are parsed into alloy's official beacon types (`crate::blob`), and the
//! payload goes to reth through the official engine types — no hand-rewritten
//! JSON anywhere on this path.
//!
//! No P2P peers, no public RPC.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Mainnet genesis time (beacon chain); slot = (timestamp - genesis) / 12.
const MAINNET_GENESIS_TIME: u64 = 1_606_824_023;
const SECONDS_PER_SLOT: u64 = 12;

pub async fn source_sync_loop(ctx: Arc<Ctx>) {
    let Some(engine) = ctx.engine_api.clone() else {
        return;
    };
    let interval = Duration::from_millis(ctx.engine.source_sync.interval_ms.max(250));
    let mut last_report = Instant::now();
    // CL head execution block, cached for a minute (one beacon call per minute).
    let mut head_cache: Option<(Instant, u64)> = None;
    loop {
        match top_up(&ctx, &engine).await {
            Ok(0) => tokio::time::sleep(interval).await,
            Ok(n) => {
                ctx.metrics
                    .add(&ctx.eth_metrics.source_sync_delivered, n as u64);
                if last_report.elapsed() > Duration::from_secs(5) {
                    if let (Some(cl_head), Ok(reth_head)) = (
                        cl_head_el_block(&ctx, &mut head_cache).await,
                        ctx.reth.block_number().await,
                    ) {
                        ctx.metrics.set(
                            &ctx.eth_metrics.source_sync_gap_blocks,
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
    let n = block
        .data
        .message
        .body
        .payload()?
        .payload_inner
        .payload_inner
        .block_number;
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
    let budget = ctx.engine.source_sync.max_per_pass.max(1) as usize;
    let mut delivered = 0usize;
    while slot <= cl_head && delivered < budget {
        let Some(block) = ctx.beacon.block_by_slot(slot).await? else {
            slot += 1; // missed slot
            continue;
        };
        let body = &block.data.message.body;
        let Some(payload) = body.payload() else {
            slot += 1; // no payload in this slot
            continue;
        };
        let number = payload.payload_inner.payload_inner.block_number;
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
        let payload_hash = payload.payload_inner.payload_inner.block_hash;
        let versioned_hashes = body.versioned_hashes();
        let requests = body.requests();
        let status = engine
            .new_payload(
                payload,
                &versioned_hashes,
                block.data.message.parent_root,
                requests.as_ref(),
            )
            .await?;
        match &status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted => {}
            other => {
                return Err(Error::PayloadRejected {
                    number,
                    status: other.as_str().to_string(),
                    message: other.validation_error().unwrap_or_default().to_string(),
                })
            }
        }
        engine
            .forkchoice_updated(&ForkchoiceState {
                head_block_hash: payload_hash,
                safe_block_hash: B256::ZERO,
                finalized_block_hash: B256::ZERO,
            })
            .await?;
        tracing::debug!(block = number, slot, "top-up delivered");
        delivered += 1;
        slot += 1;
    }
    Ok(delivered)
}
