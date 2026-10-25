//! The shared engine shell: the one home for the wiring both engines run —
//! the admin surface's construction, `/admin/epoch`'s re-pin, the boot
//! asserts, the status document's shared fields, the HTTP serve, and the
//! service's shutdown tail.
//!
//! A mirror implements [`Engine`] on its own context, in its own folder: the
//! required methods are exactly the facts that differ (its chain, its source
//! URL, its journal semantics, its probes, its scan counters, its status
//! document, its metric exposition); the provided methods and the free
//! functions here are the shell, so the two engines share one implementation
//! of everything else. [`ShellError`] is the error contract those helpers
//! need.
use crate::admin::{AdminSurface, EpochResponse};
use crate::plane::Plane;
use crate::probes::ProbeOutcome;
use crate::scan::ScanWalk;
use crate::spine::Mirror;
use axum::response::IntoResponse;
use futures_util::future::BoxFuture;
use mirror_anvil::Rpc;
use mirror_config::EngineSection;
use std::sync::Arc;
use std::time::Duration;

/// The shell's view of a mirror's engine context.
pub trait Engine: Send + Sync + Sized + 'static {
    /// The mirror's error taxonomy (the HTTP mapping classifies it).
    type Error: ShellError;
    /// The mirror's `/status` document: its own typed fields beside the
    /// shared ones (flatten a [`SharedStatus`]). The shell serves it without
    /// knowing the fields — the mirror's struct pins the names.
    type Status: serde::Serialize + Send + 'static;

    /// The shared plane: registry, journal, cursor state, admin gauges.
    fn plane(&self) -> &Plane;
    /// The divergence bundle (source, reset detector, policies, scope).
    fn mirror(&self) -> &Arc<dyn Mirror>;
    /// The devnet chain the shell drives.
    fn target(&self) -> &Arc<Rpc>;
    /// The source URL `/admin/epoch` re-pins from.
    fn source_url(&self) -> &str;
    /// What this deployment's journal holds (the rewind's semantics).
    fn journal_scope(&self) -> JournalScope;
    /// The engine's section: loops, HTTP, verifier.
    fn engine(&self) -> &EngineSection;
    /// Run the mirror's probes now (`/admin/probes`; the bodies differ —
    /// per-block parity vs at-pin).
    fn probes_now(&self) -> BoxFuture<'_, crate::Result<ProbeOutcome, Self::Error>>;
    /// Record one shadow-scan walk in the mirror's counters.
    fn record_scan(&self, walk: &ScanWalk);
    /// The mirror's own metric exposition (rendered before the shared admin
    /// gauges).
    fn profile_metrics(&self) -> String;
    /// The mirror's `/status` document.
    fn status(&self) -> BoxFuture<'_, crate::Result<Self::Status, Self::Error>>;

    /// The chain tag facts and the status carry ("t" | "v").
    fn label(&self) -> &'static str {
        self.mirror().label()
    }

    /// The devnet admin surface, wired to this engine's chain and re-sync
    /// source: one formula over the facts above.
    fn admin(&self) -> AdminSurface<'_> {
        self.plane().admin(
            self.target(),
            self.mirror().source().as_ref(),
            self.engine().loops.chunk_size,
            self.journal_scope(),
        )
    }

    /// Re-pin the chain to the source's latest block (`/admin/epoch`).
    fn epoch_repin(&self) -> BoxFuture<'_, crate::Result<EpochResponse, Self::Error>> {
        Box::pin(async move {
            Ok(AdminSurface::epoch_repin(self.target(), self.source_url()).await?)
        })
    }
}

/// What a deployment's journal holds — the rewind's semantics, per profile
/// (a named type, not a bool: the meaning is the trait's contract, and both
/// mirrors read it here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalScope {
    /// The engine's *own* writes (the venue: the deal faucet's silent storage
    /// writes), keyed by the devnet's coordinate — `/admin/rewind` undoes
    /// them.
    OwnWrites,
    /// The applier's parent before-values (the mainnet): a rewind restores
    /// the parent's pre-state and the applier re-derives; mixing the engine's
    /// own writes in would change what a rewind means.
    ParentBeforeValues,
}

/// The shell's error contract: the constructors the shared helpers need
/// (the listener's bind, the IO wrapper, the chain-identity refusal). The
/// rest of each taxonomy is the mirror's own.
pub trait ShellError:
    std::error::Error + IntoResponse + From<crate::Error> + From<mirror_anvil::Error> + Send + 'static
{
    fn bind(addr: String, source: std::io::Error) -> Self;
    fn io(what: &'static str, path: String, source: std::io::Error) -> Self;
    fn wrong_chain(anvil: &'static str, got: u64, want: u64) -> Self;
}

/// The shared status fields every engine reports: one gathering, so the
/// names the scripts read (`cursor`, `halted`, `shadowed*`) cannot drift
/// between the two surfaces. A mirror flattens this beside its own typed
/// fields.
#[derive(Debug, serde::Serialize)]
pub struct SharedStatus {
    /// The devnet chain's head.
    pub head: u64,
    /// The cursor: the last processed chain block.
    pub cursor: Option<u64>,
    /// Whether `/admin/pause` is holding the loops.
    pub halted: bool,
    pub shadowed_slots: usize,
    pub shadowed_accounts: usize,
    /// The shadowed account list, not just counts: the devnet's own writes
    /// are copy-on-write, so these accounts are expected to differ from the
    /// parent chain — parity checks must skip them.
    pub shadowed: Vec<String>,
    /// What the follow source declares it can serve (the audited capability
    /// matrix).
    pub capabilities: crate::source::Capabilities,
}

impl SharedStatus {
    /// Gather the shared fields: the head read degrades to 0 (a diagnostic
    /// must not fail), the registry read is typed.
    pub async fn gather(target: &Rpc, plane: &Plane, mirror: &dyn Mirror) -> crate::Result<Self> {
        let head = target.block_number().await.unwrap_or(0);
        let cursor = plane.state.cursor()?;
        let shadow = plane.shadow.lock()?;
        Ok(Self {
            head,
            cursor: cursor.as_ref().map(|c| c.block),
            halted: plane.is_halted(),
            shadowed_slots: shadow.slot_count(),
            shadowed_accounts: shadow.accounts.len(),
            shadowed: shadow.shadowed_addresses(),
            capabilities: mirror.follow_source().capabilities(),
        })
    }
}

/// Wait for the chain, assert its identity, and take block production: the
/// boot asserts both engines run, one home. A coordinated restart must not
/// become CrashLoopBackOff (the wait); a wrong chain would mirror the wrong
/// state silently — state, addresses and every measured fact follow from
/// the id; automine must be off because the engine is the block producer
/// (an anvil restart resets it to on, so a reset path re-asserts it through
/// this same call).
pub async fn assert_chain<E: Engine>(
    ctx: &E,
    anvil: &'static str,
    chain_id: u64,
    wait: Duration,
) -> crate::Result<(), E::Error> {
    ctx.target().wait_until_up(anvil, wait).await?;
    let got = ctx.target().chain_id().await?;
    if got != chain_id {
        return Err(E::Error::wrong_chain(anvil, got, chain_id));
    }
    ctx.target().anvil_set_automine(false).await?;
    Ok(())
}

/// Bind the configured address and serve the router until the process is
/// torn down.
pub async fn serve<E: Engine>(ctx: &E, router: axum::Router) -> crate::Result<(), E::Error> {
    let addr = format!("{}:{}", ctx.engine().http.addr, ctx.engine().http.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| E::Error::bind(addr.clone(), source))?;
    tracing::info!(addr, "engine http api listening");
    axum::serve(listener, router)
        .await
        .map_err(|source| E::Error::io("http api", addr, source))
}

/// Spawn the HTTP surface as a task: a serve fault is logged, never fatal —
/// the shutdown signal ends the process.
pub fn spawn_http<E: Engine>(ctx: &Arc<E>, router: axum::Router) -> tokio::task::JoinHandle<()> {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        if let Err(e) = serve(&*ctx, router).await {
            tracing::error!(error = %e, "http server exited");
        }
    })
}

/// The service's shutdown tail: wait for the signal, abort the loops. Every
/// mutation persists as it happens, so there is nothing to drain.
pub async fn shutdown(tasks: Vec<tokio::task::JoinHandle<()>>) {
    mirror_config::boot::shutdown_signal().await;
    tracing::info!("shutdown signal received");
    for t in tasks {
        t.abort();
    }
}
