//! Engine HTTP API: health, status, Prometheus metrics, and the small admin
//! surface (rewind / resume / shadow marking) - the Tenderly-style controls
//! that belong to the mirror itself rather than to Anvil.
use crate::ctx::Ctx;
use alloy_primitives::{Address, U256};
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
        .route("/admin/shadow", post(admin_shadow))
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
}

/// Mark devnet writes that bypassed T's block pipeline (direct cheatcode calls).
async fn admin_shadow(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<ShadowReq>,
) -> Json<serde_json::Value> {
    let mut resynced = 0usize;
    if req.clear {
        {
            let mut s = ctx.shadow.lock().unwrap();
            if req.account_fields {
                s.unmark_account_fields(req.address);
            }
            for slot in &req.slots {
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
        if req.account_fields {
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
        for slot in &req.slots {
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
