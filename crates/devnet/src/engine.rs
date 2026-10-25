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
/// The context facts every shell reads through [`Engine`]: the plane, the
/// mirror bundle, the devnet chain, the source URL and the engine section —
/// borrowed from the mirror's own context, so the trait provides the
/// accessors once and each `impl Engine` supplies only this view plus its
/// genuinely profile-specific pieces (the scan counters, the exposition, the
/// probes and the status document).
pub struct EngineCore<'a> {
    pub plane: &'a Plane,
    pub mirror: &'a Arc<dyn Mirror>,
    pub target: &'a Arc<Rpc>,
    /// The source endpoint (the follow source's RPC): the boot audit and the
    /// probes read it.
    pub source: &'a Arc<Rpc>,
    pub source_url: &'a str,
    pub engine: &'a EngineSection,
}

pub trait Engine: Send + Sync + Sized + 'static {
    /// The mirror's error taxonomy (the HTTP mapping classifies it).
    type Error: ShellError;
    /// The mirror's `/status` document: its own typed fields beside the
    /// shared ones (flatten a [`SharedStatus`]). The shell serves it without
    /// knowing the fields — the mirror's struct pins the names.
    type Status: serde::Serialize + Send + 'static;

    /// The shared facts, borrowed from the mirror's own context (see
    /// [`EngineCore`]) — the one data hook the accessors below read.
    fn core(&self) -> EngineCore<'_>;

    /// The shared plane: registry, journal, cursor state, admin gauges.
    fn plane(&self) -> &Plane {
        self.core().plane
    }
    /// The divergence bundle (source, reset detector, policies, scope).
    fn mirror(&self) -> &Arc<dyn Mirror> {
        self.core().mirror
    }
    /// The devnet chain the shell drives.
    fn target(&self) -> &Arc<Rpc> {
        self.core().target
    }
    /// The source endpoint the boot audit and the probes read.
    fn source_rpc(&self) -> &Arc<Rpc> {
        self.core().source
    }
    /// Run the epoch rule against the target: the shared reconcile with this
    /// shell's plane, target and chunk size (the counters and the publish
    /// conditionals stay the mirror's).
    fn epoch_rule(
        &self,
        pin: Option<u64>,
    ) -> BoxFuture<'_, crate::Result<crate::epoch::Epoch, Self::Error>> {
        Box::pin(async move {
            Ok(crate::epoch::reconcile(
                self.plane(),
                self.target(),
                self.engine().loops.chunk_size,
                pin,
            )
            .await?)
        })
    }
    /// The source URL `/admin/epoch` re-pins from.
    fn source_url(&self) -> &str {
        self.core().source_url
    }
    /// The engine's section: loops, HTTP, verifier.
    fn engine(&self) -> &EngineSection {
        self.core().engine
    }
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
            self.mirror().journal_scope(),
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

/// The shared shell faults: the boundaries the spine's own helpers cross —
/// the listener's bind, the HTTP api's IO, the chain-identity refusal. The
/// *fields* live here once (the spine constructs them); each shell maps them
/// into its own taxonomy through `From`, so the message stays the mirror's
/// own ("anvil-t is not the mainnet chain", "anvil-v is not the venue
/// chain").
#[derive(Debug, thiserror::Error)]
pub enum ShellFault {
    #[error("binding the http api at {addr}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{what}: {path}")]
    Io {
        what: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{anvil} answered with chain id {got}, expected {want}")]
    WrongChain {
        anvil: &'static str,
        got: u64,
        want: u64,
    },
}

/// The conversion every shell's taxonomy needs from the shared faults: the
/// three variants (`Bind`, `Io`, `WrongChain`) are the contract, the enum is
/// the mirror's own (its `#[error]` wording stays at the definition). One
/// expansion, so a new [`ShellFault`] shape is a compile error in one place,
/// not two.
#[macro_export]
macro_rules! shell_fault_conversion {
    ($ty:ty) => {
        impl From<$crate::engine::ShellFault> for $ty {
            fn from(fault: $crate::engine::ShellFault) -> Self {
                match fault {
                    $crate::engine::ShellFault::Bind { addr, source } => {
                        Self::Bind { addr, source }
                    }
                    $crate::engine::ShellFault::Io {
                        what,
                        path,
                        source,
                    } => Self::Io {
                        what,
                        path: path.into(),
                        source,
                    },
                    $crate::engine::ShellFault::WrongChain { anvil, got, want } => {
                        Self::WrongChain { anvil, got, want }
                    }
                }
            }
        }
    };
}

/// The selftest's scratch targets: the source RPC and the devnet chain's
/// anvil endpoint (the CLI flags stay per-engine — `--anvil-t`/`--anvil-v` —
/// and map into this one struct).
pub struct ScratchTargets {
    pub rpc: String,
    pub anvil: String,
}

/// The selftest banner both mirrors print: the identity line, the endpoints,
/// the pin, then the profile's own unit line. One shape, so the two runs read
/// alike.
pub fn selftest_banner(name: &str, rpc: &str, anvil: &str, pin: u64, unit: &str) {
    println!("== {name} ==");
    println!("rpc:      {rpc}");
    println!("anvil:    {anvil}");
    println!("pin:      {pin}");
    println!("{unit}");
    println!();
}

/// The selftest verdict line (the exit code is [`mirror_config::boot::
/// exit_verdict`]'s; this is the human line, one spelling).
pub fn selftest_verdict(ok: bool) {
    println!("== selftest {} ==", if ok { "PASSED" } else { "FAILED" });
}

/// The boot prefix both engines share: wait for the chain, assert its
/// identity and take block production, then audit the follow source's declared
/// capabilities against the live endpoint (a declaration that disagrees is
/// named here, never discovered mid-window). Read-only: the profile's own
/// first write (the epoch anchor / the pin reconcile) follows.
pub async fn boot_prefix<E: Engine>(
    ctx: &E,
    anvil: &'static str,
    chain_id: u64,
    wait: std::time::Duration,
    source_what: &str,
) -> crate::Result<(), E::Error> {
    assert_chain(ctx, anvil, chain_id, wait).await?;
    crate::source::audit_capabilities(
        source_what,
        ctx.mirror().follow_source().capabilities(),
        ctx.source_rpc(),
    )
    .await?;
    Ok(())
}

/// The engine service's hooks: what each profile's `run` differs in. The
/// sequence — inspect the instance, resolve the endpoints, require the
/// wiring, assemble the context, boot, log, spawn the loops, shut down — is
/// one invariant in [`run_engine`]; these are the profile's own pieces.
pub trait EngineService: Engine {
    /// The wiring check, labelled with this engine's name (each profile adds
    /// its own boundaries' values beside the endpoints).
    fn require_wiring(
        cfg: &mirror_config::Instance,
        unresolved: &[String],
        source_url: &str,
        target_url: &str,
    ) -> Result<(), Self::Error>;
    /// The instance's endpoints for this engine (source, target).
    fn urls(cfg: &mirror_config::Instance) -> Result<(String, String), Self::Error>;
    /// The context assembly.
    fn context(
        cfg: mirror_config::Instance,
        source_url: &str,
        target_url: &str,
    ) -> Result<Self, Self::Error>;
    /// The profile's boot (the shared prefix + its own tail).
    fn boot(ctx: &Self) -> BoxFuture<'_, Result<(), Self::Error>>;
    /// The start log's fields.
    fn log_start(ctx: &Self);
    /// The profile's loops, the shared HTTP task included (the mainnet builds
    /// its feed bridge here too — a fault before any loop serves is fatal).
    fn spawn_loops(
        ctx: &Arc<Self>,
    ) -> BoxFuture<'_, Result<Vec<tokio::task::JoinHandle<()>>, Self::Error>>;
}

/// The engine service future, one sequence for both mirrors. One instance
/// file, many readers: `inspect` reports the other services' unresolved
/// references instead of failing on them; this engine's own sections are then
/// required by name (via the profile's [`EngineService::require_wiring`]).
pub async fn run_engine<S: EngineService>(config: std::path::PathBuf) -> Result<(), S::Error> {
    let (cfg, unresolved) = mirror_config::Instance::inspect(&config)?;
    let (source_url, target_url) = S::urls(&cfg)?;
    S::require_wiring(&cfg, &unresolved, &source_url, &target_url)?;
    let ctx = Arc::new(S::context(cfg, &source_url, &target_url)?);
    S::boot(&ctx).await?;
    S::log_start(&ctx);
    let tasks = S::spawn_loops(&ctx).await?;
    shutdown(tasks).await;
    Ok(())
}

/// The selftest command's skeleton both engines share: inspect the instance,
/// run the command's wiring check, hand the instance to the profile's
/// pipeline, and exit with its verdict (the k8s Jobs read the exit code).
pub async fn selftest_entry<E, C, F>(config: &std::path::Path, check: C, run: F) -> Result<(), E>
where
    E: From<mirror_config::ConfigError>,
    C: FnOnce(&mirror_config::Instance, &[String]) -> Result<(), E>,
    F: FnOnce(mirror_config::Instance) -> BoxFuture<'static, Result<bool, E>>,
{
    let (cfg, unresolved) = mirror_config::Instance::inspect(config)?;
    check(&cfg, &unresolved)?;
    let ok = run(cfg).await?;
    mirror_config::boot::exit_verdict(ok);
}

/// The boot wiring check both engines share: every value the instance
/// references must be resolvable — the profile's `endpoints`, the engine's own
/// state dir, and `extra` (the boundaries only that profile builds).
/// `Ok(())` when the instance has no engine section (nothing to check).
pub fn require_engine_wiring(
    cfg: &mirror_config::Instance,
    unresolved: &[String],
    what: &'static str,
    endpoints: &[&str],
    extra: &[&str],
) -> Result<(), mirror_config::ConfigError> {
    let Some(engine) = cfg.services.engine.as_ref() else {
        return Ok(());
    };
    let state_dir = engine.state_dir.to_string_lossy().into_owned();
    let mut values: Vec<&str> = endpoints.to_vec();
    values.push(&state_dir);
    values.extend_from_slice(extra);
    mirror_config::require_resolved_logged(what, &values, unresolved)
}

/// The shell's error contract: the conversions the shared helpers need (the
/// devnet's own errors, the anvil boundary, the shared faults above). The
/// rest of each taxonomy is the mirror's own.
pub trait ShellError:
    std::error::Error
    + IntoResponse
    + From<crate::Error>
    + From<mirror_anvil::Error>
    + From<ShellFault>
    + From<mirror_config::ConfigError>
    + Send
    + 'static
{
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
        return Err(E::Error::from(ShellFault::WrongChain {
            anvil,
            got,
            want: chain_id,
        }));
    }
    ctx.target().anvil_set_automine(false).await?;
    Ok(())
}

/// Bind the configured address and serve the router until the process is
/// torn down.
pub async fn serve<E: Engine>(ctx: &E, router: axum::Router) -> crate::Result<(), E::Error> {
    let addr = format!("{}:{}", ctx.engine().http.addr, ctx.engine().http.port);
    serve_router("engine http api", addr, router).await
}

/// Serve one router on one listener: the bind fault, the listening line and
/// the serve fault in one place — every listener (the engines' HTTP api, the
/// simulator's two ports, the bridge's) crosses the same boundary, and the
/// label names the service in the log and in the fault.
pub async fn serve_router<E>(what: &'static str, addr: String, router: axum::Router) -> Result<(), E>
where
    E: From<ShellFault>,
{
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| E::from(ShellFault::Bind { addr: addr.clone(), source }))?;
    tracing::info!(addr, "{} listening", what);
    axum::serve(listener, router)
        .await
        .map_err(|source| E::from(ShellFault::Io { what, path: addr, source }))
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

#[cfg(test)]
mod tests {
    use super::ShellFault;

    #[derive(Debug, thiserror::Error)]
    enum TestErr {
        #[error("{what}: {path}")]
        Io {
            what: &'static str,
            path: std::path::PathBuf,
            #[source]
            source: std::io::Error,
        },
        #[error("binding at {addr}")]
        Bind {
            addr: String,
            #[source]
            source: std::io::Error,
        },
        #[error("{anvil} is not the chain")]
        WrongChain {
            anvil: &'static str,
            got: u64,
            want: u64,
        },
    }

    crate::shell_fault_conversion!(TestErr);

    /// The conversion macro's full contract: every shared fault shape maps
    /// field-for-field into a taxonomy carrying the three variants (the
    /// mirrors' own tests pin `WrongChain` through their taxonomies; this pins
    /// the macro itself, all three arms).
    #[test]
    fn the_shell_fault_conversion_covers_every_variant() {
        let bind = TestErr::from(ShellFault::Bind {
            addr: "0.0.0.0:8080".into(),
            source: std::io::Error::other("in use"),
        });
        assert!(matches!(bind, TestErr::Bind { ref addr, .. } if addr == "0.0.0.0:8080"));
        let io = TestErr::from(ShellFault::Io {
            what: "state",
            path: "/tmp/x".into(),
            source: std::io::Error::other("missing"),
        });
        assert!(matches!(io, TestErr::Io { what: "state", .. }));
        let chain = TestErr::from(ShellFault::WrongChain {
            anvil: "anvil-t",
            got: 1,
            want: 2,
        });
        assert!(matches!(chain, TestErr::WrongChain { got: 1, want: 2, .. }));
    }
}
