//! Engine HTTP API: health, status, Prometheus metrics, and the small admin
//! surface (rewind / resume / shadow marking) - the Tenderly-style controls
//! that belong to the mirror itself rather than to Anvil.
use crate::ctx::Ctx;
use alloy_primitives::{Address, U256, Bytes};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub async fn serve(ctx: Arc<Ctx>) -> anyhow::Result<()> {
    let addr = format!("{}:{}", ctx.cfg.http.addr, ctx.cfg.http.port);
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/admin/rewind", post(admin_rewind))
        .route("/admin/resume", post(admin_resume))
        .route("/admin/pause", post(admin_pause))
        .route("/admin/time", post(admin_time))
        .route("/admin/shadow", post(admin_shadow))
        .route("/admin/deal", post(admin_deal))
        .with_state(ctx);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr, "engine http api listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz(State(ctx): State<Arc<Ctx>>) -> impl IntoResponse {
    if ctx.halted.load(Ordering::Relaxed) {
        (StatusCode::SERVICE_UNAVAILABLE, "halted")
    } else {
        (StatusCode::OK, "ok")
    }
}

async fn status(State(ctx): State<Arc<Ctx>>) -> Json<serde_json::Value> {
    let reth_head = ctx.reth.block_number().await.unwrap_or(0);
    let follow = ctx.follow_head().await.ok().flatten().unwrap_or(0);
    let m_head = ctx.anvil_m.block_number().await.unwrap_or(0);
    let t_head = ctx.anvil_t.block_number().await.unwrap_or(0);
    let cm = ctx.cursor_m();
    let ct = ctx.cursor_t();
    let shadow = ctx.shadow.lock().unwrap();
    // The shadowed account list (not just counts): the devnet's own writes are
    // copy-on-write, so these accounts are expected to differ from mainnet -
    // parity checks (scenario probes) must skip them instead of alarming.
    let mut shadowed: Vec<String> = shadow
        .slots
        .keys()
        .chain(shadow.accounts.iter())
        .map(|a| format!("{a:?}").to_lowercase())
        .collect();
    shadowed.sort();
    shadowed.dedup();
    Json(json!({
        "reth_head": reth_head,
        "follow_head": follow,
        "follow": ctx.follow_tag(),
        "m_head": m_head,
        "t_head": t_head,
        "cursor": cm.as_ref().map(|c| c.block),
        "cursor_t": ct.as_ref().map(|c| c.block),
        "lag": follow.saturating_sub(cm.as_ref().map(|c| c.block).unwrap_or(follow)),
        "frozen": ctx.frozen.load(Ordering::Relaxed),
        "halted": ctx.halted.load(Ordering::Relaxed),
        "watchlist": ctx.watchlist.len(),
        "shadowed_slots": shadow.slot_count(),
        "shadowed_accounts": shadow.accounts.len(),
        "shadowed": shadowed,
        "blob_form": ctx.blob_form().map(|f| f.as_str()),
    }))
}

async fn metrics(State(ctx): State<Arc<Ctx>>) -> String {
    ctx.metrics.render()
}

#[derive(Deserialize)]
struct RewindReq {
    blocks: u64,
}

/// Undo the last N applied parent blocks on T using the journal.
/// Returns (new cursor, restored value count).
pub async fn do_rewind(ctx: &Ctx, blocks: u64) -> anyhow::Result<(u64, usize)> {
    let Some(cursor) = ctx.cursor_t() else {
        anyhow::bail!("no T cursor");
    };
    if blocks == 0 || blocks > cursor.block {
        anyhow::bail!("invalid rewind size");
    }
    let from = cursor.block - blocks + 1;
    let mut restored = 0usize;
    for b in (from..=cursor.block).rev() {
        restored += ctx
            .journal
            .undo_block(b, &ctx.anvil_t, ctx.cfg.loops.chunk_size)
            .await?;
    }
    let target = from - 1;
    match ctx.anvil_t.block_by_number_opt(target, false).await {
        Ok(Some(b)) => {
            let first = ctx.cursor_t().map(|c| c.first_block).unwrap_or(target);
            ctx.set_cursor_t(crate::ctx::cursor_from(&b, first));
        }
        _ => {
            let mut st = ctx.state.lock().unwrap();
            st.data.t = None;
        }
    }
    ctx.save_state()?;
    // T's *head* is still where it was - only the values were rolled back - so
    // the main loop's "diffs already applied" late-completion path would skip
    // the re-application and leave the rewound range stale (measured: T's
    // probes diverged after a rewind drill). Mark the range for re-apply.
    {
        let mut ra = ctx.reapply_from.lock().unwrap();
        *ra = Some(match *ra {
            Some((f, u)) => (f.min(from), u.max(cursor.block)),
            None => (from, cursor.block),
        });
    }
    tracing::warn!(blocks, restored, "T rewound via journal");
    Ok((target, restored))
}

async fn admin_rewind(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<RewindReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match do_rewind(&ctx, req.blocks).await {
        Ok((cursor, restored)) => Ok(Json(json!({
            "rewound_blocks": req.blocks,
            "restored_values": restored,
            "cursor_t": cursor,
        }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

async fn admin_resume(State(ctx): State<Arc<Ctx>>) -> Json<serde_json::Value> {
    ctx.resume();
    Json(json!({"resumed": true}))
}

/// `POST /admin/pause` - plan §4.4 `tenderly_stopSync`: hold both loops where
/// they are. Devnet writes, cheatcodes and reads keep working; nothing new is
/// applied until `/admin/resume`. Useful for a long manipulation you do not
/// want a parent block to land in the middle of.
async fn admin_pause(State(ctx): State<Arc<Ctx>>) -> Json<serde_json::Value> {
    ctx.halted.store(true, std::sync::atomic::Ordering::Relaxed);
    Json(json!({"paused": true}))
}

#[derive(serde::Deserialize)]
struct TimeReq {
    /// Seconds the devnet's clock runs ahead of mainnet. A financial product
    /// needs this: interest accrual, option expiry, funding intervals. Unlike a
    /// static fork's `evm_increaseTime`, the offset survives the next parent
    /// block (the applier aligns to `parent + offset`) and mainnet's data keeps
    /// flowing. Lowering it works, but timestamps are monotonic: the clamp lets
    /// the clock converge back rather than jumping backwards.
    offset_seconds: u64,
}

/// `POST /admin/time` - the devnet's clock, in seconds ahead of mainnet.
async fn admin_time(State(ctx): State<Arc<Ctx>>, Json(req): Json<TimeReq>) -> Json<serde_json::Value> {
    ctx.time_offset
        .store(req.offset_seconds, std::sync::atomic::Ordering::Relaxed);
    let head = ctx.anvil_t.block_number().await.unwrap_or(0);
    tracing::warn!(
        offset_seconds = req.offset_seconds,
        "devnet clock offset set (T diverges from mainnet's clock by design)"
    );
    Json(json!({
        "offset_seconds": req.offset_seconds,
        "t_head": head,
        "note": "T's timestamps are monotonic; lowering the offset lets the clock converge, it cannot jump backwards"
    }))
}

#[derive(Deserialize)]
struct ShadowReq {
    address: Address,
    #[serde(default)]
    slots: Vec<U256>,
    #[serde(default)]
    account_fields: bool,
    /// Hand these keys back to mainnet sync (and re-sync their values now).
    #[serde(default)]
    clear: bool,
    /// With `clear`: hand back *every* shadowed key of this account - the
    /// "reset this venue to mainnet truth" operation after a venue sim.
    #[serde(default)]
    all: bool,
}

/// Mark devnet writes that bypassed T's block pipeline (direct cheatcode calls).
async fn admin_shadow(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<ShadowReq>,
) -> Json<serde_json::Value> {
    let mut resynced = 0usize;
    if req.clear {
        // `all` hands every shadowed key of the account back to mainnet sync.
        let mut slots = req.slots.clone();
        if req.all {
            let s = ctx.shadow.lock().unwrap();
            if let Some(set) = s.slots.get(&req.address) {
                for slot in set {
                    if !slots.contains(slot) {
                        slots.push(*slot);
                    }
                }
            }
        }
        let account_fields = req.account_fields || req.all;
        {
            let mut s = ctx.shadow.lock().unwrap();
            if account_fields {
                s.unmark_account_fields(req.address);
            }
            for slot in &slots {
                s.unmark_slot(req.address, *slot);
            }
        }
        // A key without a shadow but with a stale devnet value is the most
        // confusing state there is: push mainnet's current values to T now.
        let mut calls: Vec<(String, serde_json::Value)> = Vec::new();
        // T's state reflects the last applied parent block, so re-sync from
        // *that* block - reading head would leave T holding a future value.
        let head = ctx
            .cursor_t()
            .map(|c| c.block)
            .unwrap_or_else(|| 0);
        let head = if head == 0 { ctx.reth.block_number().await.unwrap_or(0) } else { head };
        if account_fields {
            if let Ok(bal) = ctx.reth.balance_at(req.address, head).await {
                calls.push((
                    "anvil_setBalance".to_string(),
                    serde_json::json!([req.address, crate::bal::quant_hex_u256(bal)]),
                ));
            }
            if let Ok(nonce) = ctx.reth.nonce_at(req.address, head).await {
                calls.push((
                    "anvil_setNonce".to_string(),
                    serde_json::json!([req.address, format!("0x{nonce:x}")]),
                ));
            }
        }
        for slot in &slots {
            if let Ok(v) = ctx.reth.storage_at(req.address, *slot, head).await {
                calls.push((
                    "anvil_setStorageAt".to_string(),
                    serde_json::json!([
                        req.address,
                        crate::bal::quant_hex_u256(*slot),
                        format!("0x{}", hex::encode(v))
                    ]),
                ));
            }
        }
        if let Ok(results) = ctx.anvil_t.batch_raw(&calls).await {
            for r in results {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "unshadow re-sync write failed");
                } else {
                    resynced += 1;
                }
            }
        }
        let _ = ctx.save_shadow();
    } else {
        let mut s = ctx.shadow.lock().unwrap();
        if req.account_fields {
            s.mark_account_fields(req.address);
        }
        for slot in &req.slots {
            s.mark_slot(req.address, *slot);
        }
        drop(s);
        let _ = ctx.save_shadow();
    }
    let shadow = ctx.shadow.lock().unwrap();
    Json(json!({
        "address": req.address,
        "cleared": req.clear,
        "resynced_keys": resynced,
        "shadowed_slots": shadow.slot_count(),
        "shadowed_accounts": shadow.accounts.len(),
    }))
}

/// `POST /admin/deal` - the unlimited ERC20 faucet (plan §4.4:
/// `tenderly_setErc20Balance`). Anvil's own `anvil_dealERC20` cannot discover
/// balance slots on a forked, no-mining chain ("no slot found", measured): its
/// probe writes never reach a mined state. This one discovers the slot with
/// `eth_call` + a `stateDiff` override - no mining involved - then writes it
/// silently and marks it devnet-owned so mainnet diffs can never overwrite it.
#[derive(serde::Deserialize)]
struct DealReq {
    token: Address,
    to: Address,
    /// Base units (e.g. 100 USDC = 100000000).
    amount: U256,
}

async fn admin_deal(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<DealReq>,
) -> Json<serde_json::Value> {
    let value = format!("0x{:064x}", req.amount);
    // Probe with a sentinel, not the target: a balance that already equals the
    // target makes *any* slot look like the balance slot (measured: a re-run
    // matched slot 0 because the previous deal had left the exact amount at
    // slot 3). The sentinel stays below 2^255 so packed layouts (USDC's
    // balanceAndBlacklistStates) return it unmasked.
    let sentinel: U256 = (U256::from(0x5eed_0000u64) << 192) | U256::from(0x1234_5678u64);
    let sentinel_hex = format!("0x{:064x}", sentinel);
    // ABI-encode balanceOf(address): selector + the 32-byte left-padded address.
    // `format!("{:064x}", address)` does NOT pad an alloy Address (measured: it
    // produced 3 bytes and the probe silently compared against a bare `0x`).
    let data = Bytes::from(
        hex::decode(format!("70a08231{:0>64}", hex::encode(req.to))).unwrap_or_default(),
    );
    // Candidate standard layouts: balanceOf is `mapping(address => uint256)` at
    // some low slot; packed layouts (USDC's balanceAndBlacklistStates) put the
    // balance in the low 255 bits of the same shape, so the probe finds both.
    let mut probes: Vec<serde_json::Value> = Vec::new();
    // 0..16 covers the standard layouts plus the common packed ones (USDC's
    // balanceAndBlacklistStates sits at slot 9). Each probe is a single
    // eth_call with a stateDiff override - no mining, ~5 ms.
    for slot in 0u64..16 {
        let idx = crate::bal::mapping_slot(req.to, U256::from(slot));
        let idx_hex = format!("0x{:064x}", idx);
        // Address keys must be plain `0x`-prefixed hex (`{:#x}` on an alloy
        // Address is not guaranteed to be) and override *values* must be full
        // 32-byte hex: anvil answers a quantized value with "invalid string
        // length" (measured).
        let over = json!({
            format!("0x{}", hex::encode(req.token)): { "stateDiff": { idx_hex.clone(): sentinel_hex.clone() } }
        });
        let probe = ctx
            .anvil_t
            .call_with_override(req.token, &data, &over)
            .await;
        if probes.len() < 4 {
            probes.push(json!({
                "slot": idx_hex,
                "result": probe.clone(),
                "want": sentinel_hex.clone(),
            }));
        }
        if probe.as_deref() == Some(sentinel_hex.as_str()) {
            let w = crate::bal::Write::Storage {
                address: req.token,
                slot: idx,
                value: U256::from_be_bytes(
                    <[u8; 32]>::try_from(&hex::decode(&value[2..]).unwrap_or_default()[..]).unwrap_or([0u8; 32]),
                ),
            };
            if let Err(e) = crate::diff::apply_writes(&ctx, &ctx.anvil_t, std::slice::from_ref(&w)).await {
                return Json(json!({ "ok": false, "error": format!("write failed: {e}") }));
            }
            // Mark it devnet-owned: a parent diff must never take it back.
            {
                let mut s = ctx.shadow.lock().unwrap();
                s.mark(&w);
            }
            let _ = ctx.save_shadow();
            let after = ctx
                .anvil_t
                .call_latest(req.token, &data)
                .await
                .map(|b| format!("0x{}", hex::encode(b)))
                .ok();
            let ok = after.as_deref() == Some(value.as_str());
            tracing::info!(
                token = %req.token,
                to = %req.to,
                slot = %idx_hex,
                ok,
                "devnet ERC20 deal (silent storage write, marked devnet-owned)"
            );
            return Json(json!({
                "ok": ok,
                "slot": idx_hex,
                "balance": after.unwrap_or_default(),
            }));
        }
    }
    Json(json!({
        "ok": false,
        "error": "no standard balance slot found in slots 0..16 (non-standard layout): write the slot yourself with anvil_setStorageAt",
        "probes": probes,
    }))
}
