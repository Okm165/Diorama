//! The CCTP bridge binary — the third element's service (§15.4).
//!
//! Boot order: load the instance, build both chains, assert the wiring (chain
//! ids, `localDomain`, message versions, the peer messenger — fail closed),
//! install the devnet attester through the contracts' own setters on both
//! chains, then spawn the two watchers, the relayer and the two listeners
//! (8095 Iris-compatible, 8096 control).
//!
//! Thin shell by design: the runtime is built here, the story lives in
//! `mirror_cctp`.
use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use mirror_cctp::chains::{Chain, DOMAIN_ETHEREUM, DOMAIN_HYPER_EVM};
use mirror_cctp::transfer::{TxSender, Wallet};
use mirror_cctp::U256;
use mirror_config::CctpSection;

/// Worker stack budget: the same measured convention as the other binaries.
const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_SIZE)
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            report(&e);
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        match run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                report(&e);
                ExitCode::FAILURE
            }
        }
    })
}

/// The shared runtime every spawned plane reads.
struct Bridge {
    chains: Vec<Chain>,
    section: CctpSection,
    store: Arc<mirror_cctp::store::Store>,
    attester: Arc<mirror_cctp::attester::Attester>,
    sender: Arc<TxSender>,
    metrics: Arc<mirror_cctp::metrics::CctpMetrics>,
    receipt_timeout: Duration,
    poll: Duration,
}

async fn run() -> mirror_cctp::Result<()> {
    init_tracing();
    let config = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/etc/mirror/instance.toml".to_string());
    // One instance file, many readers: the bridge reads only
    // `[services.cctp]`, so the venue engine's / simulator's `${VAR}` wiring
    // is not this pod's to provide. `inspect` reports unresolved references
    // instead of failing on them; the cctp section's own wiring is then
    // required here, by name.
    let (mut cfg, unresolved) = mirror_config::Instance::inspect(std::path::Path::new(&config))?;
    let section = cfg.take_cctp()?;
    require_resolved(&section).inspect_err(|e| {
        tracing::error!(unresolved = %unresolved.join(", "), error = %e, "cctp wiring is incomplete");
    })?;
    let receipt_timeout = Duration::from_secs(section.receipt_timeout_secs);

    let chains = build_chains(&section, Duration::from_secs(30))?;
    let min_fees = verify_wiring(&chains).await?;
    let attester = Arc::new(mirror_cctp::attester::Attester::from_seed(
        &section.attester_seed,
    )?);
    install_attester(&chains, &attester, receipt_timeout).await?;

    let wallet = Wallet::from_secret(&section.relay_key)?;
    tracing::info!(relayer = %wallet.address(), "cctp relay wallet loaded");
    // The plan's boot step: a devnet deployment tops the relay wallet up when
    // it starts below the alert threshold. A failed top-up is not fatal (the
    // Iris API still serves); the gas alerts keep it visible.
    if let Err(e) = fund_relayer(&chains, wallet.address(), section.balance_low_wei).await {
        tracing::error!(error = %e, "relay wallet top-up failed; the gas alerts stay on");
    }
    let bridge = Bridge {
        chains,
        store: Arc::new(mirror_cctp::store::Store::load(
            section.state_dir.join("cctp.json"),
        )?),
        attester,
        sender: Arc::new(TxSender::new(wallet)),
        metrics: Arc::new(mirror_cctp::metrics::CctpMetrics::default()),
        receipt_timeout,
        poll: Duration::from_millis(section.poll_interval_ms),
        section,
    };

    bridge.spawn_watchers();
    bridge.spawn_relayer();
    let (iris, control) = bridge.spawn_apis(min_fees);

    tokio::signal::ctrl_c().await.map_err(|source| {
        mirror_cctp::Error::Io {
            what: "signal handler",
            path: "ctrl_c".into(),
            source,
        }
    })?;
    iris.abort();
    control.abort();
    bridge.store.save()?;
    Ok(())
}

impl Bridge {
    /// One watcher per chain; each serves only its peer's domain.
    fn spawn_watchers(&self) {
        for chain in &self.chains {
            let (destination_domain, start_block) = if chain.domain == DOMAIN_ETHEREUM {
                (DOMAIN_HYPER_EVM, self.section.t_start_block.unwrap_or(0))
            } else {
                (DOMAIN_ETHEREUM, self.section.v_start_block.unwrap_or(0))
            };
            let watcher = mirror_cctp::watcher::Watcher {
                chain: chain.clone(),
                destination_domain,
                store: self.store.clone(),
                attester: self.attester.clone(),
                start_block,
                scanned_upto: std::sync::atomic::AtomicU64::new(0),
                chunk_blocks: self.section.scan_chunk_blocks,
                finality_delay: Duration::from_secs(self.section.finality_delay_secs),
                expiration_blocks: self.section.expiration_blocks,
                metrics: self.metrics.clone(),
            };
            tokio::spawn(watcher.run(self.poll));
        }
    }

    fn spawn_relayer(&self) {
        let relayer = mirror_cctp::relayer::Relayer {
            chains: self.chains.clone(),
            store: self.store.clone(),
            sender: self.sender.clone(),
            metrics: self.metrics.clone(),
            balance_low: self.section.balance_low_wei,
            balance_critical: self.section.balance_critical_wei,
            receipt_timeout: self.receipt_timeout,
        };
        tokio::spawn(relayer.run(self.poll));
    }

    /// Two listeners: the Iris API daloc's relay reads (8095), the control
    /// API tests drive (8096).
    fn spawn_apis(
        &self,
        min_fees: BTreeMap<u32, Option<U256>>,
    ) -> (
        tokio::task::JoinHandle<mirror_cctp::Result<()>>,
        tokio::task::JoinHandle<mirror_cctp::Result<()>>,
    ) {
        let iris_addr = format!("{}:{}", self.section.iris.addr, self.section.iris.port);
        let control_addr = format!(
            "{}:{}",
            self.section.control.addr, self.section.control.port
        );
        let iris_state = Arc::new(mirror_cctp::iris::IrisState {
            store: self.store.clone(),
            attester: self.attester.clone(),
        });
        let control_state = Arc::new(mirror_cctp::control::ControlState {
            store: self.store.clone(),
            chains: self.chains.clone(),
            sender: self.sender.clone(),
            metrics: self.metrics.clone(),
            min_fees,
            receipt_timeout: self.receipt_timeout,
        });
        let iris = tokio::spawn(mirror_cctp::iris::serve(
            iris_addr.clone(),
            mirror_cctp::iris::router(iris_state),
        ));
        let control = tokio::spawn(mirror_cctp::iris::serve(
            control_addr.clone(),
            mirror_cctp::control::router(control_state),
        ));
        tracing::info!(
            iris = %iris_addr,
            control = %control_addr,
            state_dir = %self.section.state_dir.display(),
            "cctp bridge listening"
        );
        (iris, control)
    }
}

/// T (domain 0) and V (domain 19), from the instance's endpoints.
fn build_chains(section: &CctpSection, timeout: Duration) -> mirror_cctp::Result<Vec<Chain>> {
    Ok(vec![
        Chain::ethereum(&section.t_url, timeout)?,
        Chain::hyper_evm(&section.v_url, timeout)?,
    ])
}

/// Every `[services.cctp]` value the bridge reads must be fully resolved: an
/// unresolved `${VAR}` left in a URL/key/path is a named boot refusal, never
/// a literal used as an endpoint.
fn require_resolved(section: &CctpSection) -> mirror_cctp::Result<()> {
    let fields = [
        ("state_dir", section.state_dir.to_string_lossy().to_string()),
        ("t_url", section.t_url.clone()),
        ("v_url", section.v_url.clone()),
        ("relay_key", section.relay_key.clone()),
        ("attester_seed", section.attester_seed.clone()),
    ];
    for (key, value) in fields {
        if value.contains("${") {
            return Err(mirror_cctp::Error::BadRequest {
                reason: format!(
                    "services.cctp.{key} carries an unresolved ${{VAR}} reference; \
                     the bridge pod must carry that wiring"
                ),
            });
        }
    }
    Ok(())
}

/// The wiring asserts + the per-domain min-fee probe (§15.2: absent on
/// Ethereum, `0` on HyperEVM). A mismatch is a boot refusal.
async fn verify_wiring(chains: &[Chain]) -> mirror_cctp::Result<BTreeMap<u32, Option<U256>>> {
    let mut min_fees = BTreeMap::new();
    for chain in chains {
        let peer = chains
            .iter()
            .find(|c| c.domain != chain.domain)
            .ok_or(mirror_cctp::Error::UnknownDomain { domain: chain.domain })?;
        let wiring = chain.verify_wiring(peer).await?;
        tracing::info!(
            chain = chain.name,
            domain = chain.domain,
            chain_id = wiring.chain_id,
            local_domain = wiring.local_domain,
            version = wiring.version,
            body_version = wiring.body_version,
            transmitter = %wiring.local_transmitter,
            peer_messenger = %wiring.peer_messenger,
            min_fee = ?wiring.min_fee,
            "cctp wiring verified"
        );
        min_fees.insert(chain.domain, wiring.min_fee);
    }
    Ok(min_fees)
}

/// The devnet attester, installed through the real setters (idempotent).
async fn install_attester(
    chains: &[Chain],
    attester: &mirror_cctp::attester::Attester,
    receipt_timeout: Duration,
) -> mirror_cctp::Result<()> {
    for chain in chains {
        let outcome = mirror_cctp::attester::install(
            &chain.rpc,
            chain.name,
            attester.address(),
            receipt_timeout.max(Duration::from_secs(120)),
        )
        .await?;
        tracing::info!(
            chain = chain.name,
            attester = %attester.address(),
            outcome = ?outcome,
            "devnet attester ready"
        );
    }
    Ok(())
}

/// One ETH of devnet gas when the relay wallet starts below the low threshold
/// (§15.4's boot funding; `anvil_setBalance` on both chains).
const RELAY_GAS_STIPEND: u64 = 1_000_000_000_000_000_000;

async fn fund_relayer(
    chains: &[Chain],
    wallet: mirror_cctp::Address,
    low: U256,
) -> mirror_cctp::Result<()> {
    for chain in chains {
        let balance = chain.native_balance(wallet).await?;
        if balance < low {
            chain.fund_native(wallet, U256::from(RELAY_GAS_STIPEND)).await?;
            tracing::warn!(
                chain = chain.name,
                relayer = %wallet,
                was = %balance,
                "relay wallet below the low threshold; devnet stipend set"
            );
        }
    }
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("MIRROR_LOG_JSON").map(|v| v != "0").unwrap_or(false);
    if json {
        fmt().with_env_filter(filter).json().init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}

/// Print the error and its full cause chain: the typed sources are the point.
fn report(e: &dyn std::error::Error) {
    eprintln!("Error: {e}");
    let mut source = e.source();
    while let Some(s) = source {
        eprintln!("  caused by: {s}");
        source = s.source();
    }
}
