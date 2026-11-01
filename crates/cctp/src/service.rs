//! The bridge service's composition root (the `cctp-bridge` bin is the shell
//! over [`main_entry`] — the same shell as the engines').
//!
//! Boot order: load the instance, build both chains, assert the wiring (chain
//! ids, `localDomain`, message versions, the peer messenger — fail closed),
//! install the devnet attester through the contracts' own setters on both
//! chains, then spawn the two watchers, the relayer and the two listeners
//! (8095 Iris-compatible, 8096 control).
use crate::chains::{Chain, Domain};
use crate::transfer::{TxSender, Wallet};
use crate::{Address, B256, U256};
use clap::{Parser, Subcommand};
use diorama_config::boot;
use diorama_config::CctpSection;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The shared runtime every spawned plane reads.
struct Bridge {
    chains: Vec<Chain>,
    /// The directed lanes. One watcher per lane; the relayer serves all.
    lanes: Vec<Lane>,
    section: CctpSection,
    store: Arc<crate::store::Store>,
    attester: Arc<crate::attester::Attester>,
    sender: Arc<TxSender>,
    metrics: Arc<crate::metrics::CctpMetrics>,
    receipt_timeout: Duration,
    receipt_poll: Duration,
    poll: Duration,
}

/// One directed lane, resolved to domains at boot. The control and Iris
/// surfaces read it too: a bridge must never burn on a lane its watcher will
/// not serve, nor advertise a fee for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lane {
    pub(crate) source: Domain,
    pub(crate) destination: Domain,
    /// The lane's configured Iris fee table (empty = no table configured).
    pub(crate) fees: Vec<diorama_config::LaneFee>,
}

/// The bridge's CLI: `run` (the service) and `check`/`--dry-run` (topology
/// preflight without touching a chain). One process = one bridge; the config
/// is the instance, never a mirror's file.
#[derive(Parser)]
#[command(name = "diorama-cctp-bridge", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the bridge: watchers, relayer, Iris API and control API.
    Run {
        #[arg(long, default_value = diorama_config::boot::DEFAULT_INSTANCE_PATH)]
        config: PathBuf,
        /// Validate and print the topology, then exit without any RPC call.
        #[arg(long)]
        dry_run: bool,
    },
    /// Validate and print the topology without booting or touching a chain.
    Check {
        #[arg(long, default_value = diorama_config::boot::DEFAULT_INSTANCE_PATH)]
        config: PathBuf,
    },
}

pub async fn main_entry(args: Vec<String>) -> crate::Result<()> {
    boot::init_logging();
    let cli: Cli = boot::parse_cli("diorama-cctp-bridge", args);
    match cli.cmd {
        Cmd::Run { config, dry_run } => {
            let (section, policy, endpoint_specs) = load_wiring(&config)?;
            if dry_run {
                print_topology(&section, policy, &endpoint_specs)?;
                return Ok(());
            }
            let (bridge, min_fees) = boot_bridge(section, policy, endpoint_specs).await?;

            bridge.spawn_watchers()?;
            let relayer = bridge.spawn_relayer();
            let (iris, control) = bridge.spawn_apis(min_fees, relayer);

            boot::shutdown_signal().await;
            tracing::info!("shutdown signal received");
            iris.abort();
            control.abort();
            bridge.store.save()?;
            Ok(())
        }
        Cmd::Check { config } => {
            let (section, policy, endpoint_specs) = load_wiring(&config)?;
            print_topology(&section, policy, &endpoint_specs)
        }
    }
}

/// Print the resolved chains and lanes without any RPC call. This is the
/// preflight that lets a composed deployment (N chains, directed lanes) be
/// reviewed before it reaches a cluster.
fn print_topology(
    section: &CctpSection,
    policy: diorama_anvil::PoolPolicy,
    endpoint_specs: &BTreeMap<String, Vec<diorama_anvil::EndpointSpec>>,
) -> crate::Result<()> {
    let chains = build_chains(
        section,
        Duration::from_secs(section.rpc_timeout_secs),
        policy,
        endpoint_specs,
    )?;
    let lanes = build_lanes(section, &chains)?;
    println!(
        "bridge: {} ({} chain(s), {} lane(s))",
        section.state_dir.display(),
        chains.len(),
        lanes.len()
    );
    for chain in &chains {
        println!(
            "  chain {:<16} id={:<10} domain={:<4} rpc={}",
            chain.name,
            chain.chain_id,
            chain.domain,
            chain.rpc.urls().join(",")
        );
    }
    for lane in &lanes {
        println!("  lane  {} -> {}", lane.source, lane.destination);
    }
    Ok(())
}

/// Load the instance and require this pod's wiring: one instance file, many
/// readers — the bridge reads only `[services.cctp_bridge]`, so the venue engine's /
/// simulator's `${VAR}` wiring is not this pod's to provide (`inspect` reports
/// unresolved references instead of failing on them; the cctp section's own
/// wiring is then required here, by name).
fn load_wiring(
    config: &std::path::Path,
) -> crate::Result<(
    CctpSection,
    diorama_anvil::PoolPolicy,
    BTreeMap<String, Vec<diorama_anvil::EndpointSpec>>,
)> {
    let (mut cfg, unresolved) = diorama_config::Instance::inspect(config)?;
    // The bridge is a first-class instance: it reads the same shared endpoint
    // policy as both mirrors, not a crate default. Capture it before the
    // section is taken out of the instance.
    let policy = cfg.endpoint_policy();
    // Preserve each chain's endpoint attribution (roles, chain id, weight) for
    // its pool; the URL-only constructor would silently drop them.
    let endpoint_specs: BTreeMap<String, Vec<diorama_anvil::EndpointSpec>> = cfg
        .services
        .cctp_bridge
        .as_ref()
        .map(|section| {
            section
                .chains
                .iter()
                .map(|chain| {
                    (
                        chain.id.clone(),
                        cfg.endpoint_specs(chain.rpc_urls.as_slice()),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let section = cfg.take_cctp()?;
    let state_dir = section.state_dir.to_string_lossy().to_string();
    let mut values: Vec<&str> = vec![&state_dir, &section.relay_key, &section.attester_seed];
    if let Some(t_url) = &section.t_url {
        values.push(t_url);
    }
    if let Some(v_url) = &section.v_url {
        values.push(v_url);
    }
    for chain in &section.chains {
        values.extend(chain.rpc_urls.iter());
    }
    diorama_config::require_resolved_logged("cctp wiring", &values, &unresolved)?;
    Ok((section, policy, endpoint_specs))
}

/// Boot the bridge's boundary: wait (bounded) for each chain — the same boot
/// convention as both engines, so a coordinated restart (the anvil re-forking,
/// the node unwinding) does not turn the bridge into CrashLoopBackOff — verify
/// the wiring, install the devnet attester through the contracts' own setters,
/// load the relay wallet (with the plan's low-balance top-up: a failed top-up
/// is not fatal, the Iris API still serves), the store, and the hook wrappers.
/// Returns the bridge and the measured per-domain minimum fees the APIs serve.
async fn boot_bridge(
    section: CctpSection,
    policy: diorama_anvil::PoolPolicy,
    endpoint_specs: BTreeMap<String, Vec<diorama_anvil::EndpointSpec>>,
) -> crate::Result<(Bridge, BTreeMap<(Domain, Domain), Option<U256>>)> {
    let receipt_timeout = Duration::from_secs(section.receipt_timeout_secs);
    let receipt_poll = Duration::from_millis(section.receipt_poll_ms);
    let chains = build_chains(
        &section,
        Duration::from_secs(section.rpc_timeout_secs),
        policy,
        &endpoint_specs,
    )?;
    wait_for_chains(&chains, Duration::from_secs(section.boot_timeout_secs)).await?;
    let lanes = build_lanes(&section, &chains)?;
    let min_fees = verify_wiring(&chains, &lanes).await?;
    let attester = Arc::new(crate::attester::Attester::from_seed(
        &section.attester_seed,
    )?);
    install_attester(
        &chains,
        &attester,
        receipt_timeout,
        receipt_poll,
        section.balance_low_wei,
    )
    .await?;

    let wallet = Wallet::from_secret(&section.relay_key)?;
    tracing::info!(relayer = %wallet.address(), "cctp relay wallet loaded");
    if let Err(e) = fund_relayer(&chains, wallet.address(), section.balance_low_wei).await {
        tracing::error!(error = %e, "relay wallet top-up failed; the gas alerts stay on");
    }
    let sender = Arc::new(TxSender::new(wallet));
    let store = Arc::new(crate::store::Store::load(
        section.state_dir.join("cctp.json"),
    )?);
    install_hooks(&chains, &store, &sender, receipt_timeout, receipt_poll).await;
    let bridge = Bridge {
        chains,
        lanes,
        store,
        attester,
        sender,
        metrics: Arc::new(crate::metrics::CctpMetrics::default()),
        receipt_timeout,
        receipt_poll,
        poll: Duration::from_millis(section.poll_interval_ms),
        section,
    };
    Ok((bridge, min_fees))
}

/// Wait (bounded) for every chain to answer. A coordinated restart (the anvil
/// re-forking, the node unwinding) is the normal cause of a slow boot; the
/// budget is the deployment's.
async fn wait_for_chains(chains: &[Chain], timeout: Duration) -> crate::Result<()> {
    for chain in chains {
        chain.rpc.wait_until_up(chain.name, timeout).await?;
    }
    Ok(())
}

impl Bridge {
    /// One watcher per chain; each serves only its peer's domain. The boot
    /// wiring asserts both chains — a missing peer here is a refusal, never a
    /// panic (data paths refuse or degrade).
    fn spawn_watchers(&self) -> crate::Result<()> {
        for lane in &self.lanes {
            let Some(source) = self
                .chains
                .iter()
                .find(|c| c.domain == lane.source)
                .cloned()
            else {
                return Err(crate::Error::UnknownDomain {
                    domain: lane.source,
                });
            };
            let Some(destination) = self
                .chains
                .iter()
                .find(|c| c.domain == lane.destination)
                .cloned()
            else {
                return Err(crate::Error::UnknownDomain {
                    domain: lane.destination,
                });
            };
            let watcher = crate::watcher::Watcher {
                chain: source.clone(),
                destination_domain: lane.destination,
                destination,
                store: self.store.clone(),
                attester: self.attester.clone(),
                start_block: source.start_block,
                scanned_upto: std::sync::atomic::AtomicU64::new(0),
                chunk_blocks: self.section.scan_chunk_blocks,
                finality_delay: Duration::from_secs(self.section.finality_delay_secs),
                expiration_blocks: self.section.expiration_blocks,
                clamp_warned: std::sync::atomic::AtomicU64::new(0),
                metrics: self.metrics.clone(),
            };
            tokio::spawn(watcher.run(self.poll));
        }
        Ok(())
    }

    /// The one relayer, shared with the control API: manual mode's trigger
    /// and the auto tick drive the same instance.
    fn spawn_relayer(&self) -> Arc<crate::relayer::Relayer> {
        let relayer = Arc::new(crate::relayer::Relayer {
            chains: self.chains.clone(),
            store: self.store.clone(),
            sender: self.sender.clone(),
            metrics: self.metrics.clone(),
            mode: self.section.relayer,
            balance_low: self.section.balance_low_wei,
            balance_critical: self.section.balance_critical_wei,
            receipt_timeout: self.receipt_timeout,
            receipt_poll: self.receipt_poll,
            max_send_attempts: self.section.max_send_attempts,
            status_log_interval: Duration::from_millis(self.section.status_log_interval_ms),
            heads: self
                .chains
                .iter()
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
        });
        tokio::spawn(relayer.clone().run(self.poll));
        relayer
    }

    /// Two listeners: the Iris API daloc's relay reads (8095), the control
    /// API tests drive (8096).
    fn spawn_apis(
        &self,
        min_fees: BTreeMap<(Domain, Domain), Option<U256>>,
        relayer: Arc<crate::relayer::Relayer>,
    ) -> (
        tokio::task::JoinHandle<crate::Result<()>>,
        tokio::task::JoinHandle<crate::Result<()>>,
    ) {
        let iris_addr = format!("{}:{}", self.section.iris.addr, self.section.iris.port);
        let control_addr = format!(
            "{}:{}",
            self.section.control.addr, self.section.control.port
        );
        let iris_state = Arc::new(crate::iris::IrisState {
            store: self.store.clone(),
            attester: self.attester.clone(),
            chains: self.chains.clone(),
            lanes: self.lanes.clone(),
            expiration_blocks: self.section.expiration_blocks,
            fast_burn_allowance: self.section.fast_burn_allowance,
        });
        let control_state = Arc::new(crate::control::ControlState {
            store: self.store.clone(),
            chains: self.chains.clone(),
            lanes: self.lanes.clone(),
            sender: self.sender.clone(),
            metrics: self.metrics.clone(),
            relayer,
            min_fees,
            receipt_timeout: self.receipt_timeout,
            receipt_poll: self.receipt_poll,
        });
        let iris = tokio::spawn(crate::iris::serve(
            iris_addr.clone(),
            crate::iris::router(iris_state),
        ));
        let control = tokio::spawn(crate::iris::serve(
            control_addr.clone(),
            crate::control::router(control_state),
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

/// The bridge's chains: the composable list when present, else the legacy
/// T/V pair. A configured chain carries its own id/domain/contracts; nothing
/// here assumes Ethereum or HyperEVM.
fn build_chains(
    section: &CctpSection,
    timeout: Duration,
    policy: diorama_anvil::PoolPolicy,
    endpoint_specs: &BTreeMap<String, Vec<diorama_anvil::EndpointSpec>>,
) -> crate::Result<Vec<Chain>> {
    if !section.chains.is_empty() {
        return section
            .chains
            .iter()
            .map(|chain| {
                Chain::from_config(
                    chain,
                    timeout,
                    policy,
                    endpoint_specs.get(&chain.id).cloned().unwrap_or_default(),
                )
            })
            .collect();
    }
    let t_url = section
        .t_url
        .as_deref()
        .ok_or_else(|| crate::Error::Wiring {
            chain: "ethereum",
            what: "t_url",
            got: "absent".into(),
            want: "a legacy two-chain URL".into(),
        })?;
    let v_url = section
        .v_url
        .as_deref()
        .ok_or_else(|| crate::Error::Wiring {
            chain: "hyper_evm",
            what: "v_url",
            got: "absent".into(),
            want: "a legacy two-chain URL".into(),
        })?;
    let mut t = Chain::ethereum(t_url, timeout, policy)?;
    t.start_block = section.t_start_block.unwrap_or(0);
    let mut v = Chain::hyper_evm(v_url, timeout, policy)?;
    v.start_block = section.v_start_block.unwrap_or(0);
    Ok(vec![t, v])
}

/// The directed lanes: explicit when configured, otherwise the legacy
/// two-chain pair in both directions (the old per-chain watcher behaviour).
fn build_lanes(section: &CctpSection, chains: &[Chain]) -> crate::Result<Vec<Lane>> {
    if !section.lanes.is_empty() {
        return section
            .lanes
            .iter()
            .map(|lane| {
                let source = section
                    .chains
                    .iter()
                    .find(|chain| chain.id == lane.source)
                    .ok_or_else(|| crate::Error::UnknownChain {
                        id: lane.source.clone(),
                    })?;
                let destination = section
                    .chains
                    .iter()
                    .find(|chain| chain.id == lane.destination)
                    .ok_or_else(|| crate::Error::UnknownChain {
                        id: lane.destination.clone(),
                    })?;
                Ok(Lane {
                    source: Domain(source.domain),
                    destination: Domain(destination.domain),
                    fees: lane.fees.clone(),
                })
            })
            .collect();
    }
    if chains.len() == 2 {
        return Ok(vec![
            Lane {
                source: chains[0].domain,
                destination: chains[1].domain,
                fees: Vec::new(),
            },
            Lane {
                source: chains[1].domain,
                destination: chains[0].domain,
                fees: Vec::new(),
            },
        ]);
    }
    Err(crate::Error::Wiring {
        chain: "cctp",
        what: "lanes",
        got: "absent".into(),
        want: "at least one [[services.cctp_bridge.lanes]] entry for a multi-chain topology".into(),
    })
}

/// The wiring asserts + the per-domain min-fee probe (§15.2: absent on
/// Ethereum, `0` on HyperEVM). A mismatch is a boot refusal.
async fn verify_wiring(
    chains: &[Chain],
    lanes: &[Lane],
) -> crate::Result<BTreeMap<(Domain, Domain), Option<U256>>> {
    let mut min_fees = BTreeMap::new();
    for lane in lanes {
        let chain =
            chains
                .iter()
                .find(|c| c.domain == lane.source)
                .ok_or(crate::Error::UnknownDomain {
                    domain: lane.source,
                })?;
        let peer = chains.iter().find(|c| c.domain == lane.destination).ok_or(
            crate::Error::UnknownDomain {
                domain: lane.destination,
            },
        )?;
        let wiring = chain.verify_wiring(peer).await?;
        tracing::info!(
            chain = chain.name,
            domain = %chain.domain,
            chain_id = %wiring.chain_id,
            local_domain = %wiring.local_domain,
            version = wiring.version,
            body_version = wiring.body_version,
            transmitter = %wiring.local_transmitter,
            peer_messenger = %wiring.peer_messenger,
            min_fee = ?wiring.min_fee,
            "cctp wiring verified"
        );
        min_fees.insert((lane.source, lane.destination), wiring.min_fee);
    }
    Ok(min_fees)
}

/// The recorded wrapper, reused only when its code is present and its wiring
/// verifies — a re-pin drops deployments, and a stale record must not serve.
async fn existing_wrapper(
    chain: &Chain,
    store: &Arc<crate::store::Store>,
    sender: &TxSender,
) -> Option<Address> {
    let wrapper = store
        .with(|state| Ok(state.hooks.get(&chain.domain).copied()))
        .ok()
        .flatten()?;
    let present = match chain.rpc.block_number().await {
        Ok(head) => matches!(
            chain.rpc.code_at(wrapper, head).await,
            Ok(code) if !code.is_empty()
        ),
        Err(_) => false,
    };
    if !present {
        tracing::warn!(
            chain = chain.name,
            wrapper = %wrapper,
            "recorded hook wrapper is not deployed (a re-pin?); redeploying"
        );
        return None;
    }
    match crate::hook::verify(
        &chain.rpc,
        wrapper,
        sender.address(),
        chain.message_transmitter,
    )
    .await
    {
        Ok(()) => Some(wrapper),
        Err(e) => {
            tracing::warn!(
                chain = chain.name,
                wrapper = %wrapper,
                error = %e,
                "recorded hook wrapper failed verification; redeploying"
            );
            None
        }
    }
}

/// Deploy the wrapper (owner = the relay key, transmitter = the local
/// `MessageTransmitterV2`), verify it, and record it in the state.
async fn deploy_wrapper(
    chain: &Chain,
    store: &Arc<crate::store::Store>,
    sender: &TxSender,
    receipt_timeout: Duration,
    receipt_poll: Duration,
) {
    let hash = match crate::hook::deploy(&chain.write_rpc, sender, chain.message_transmitter).await
    {
        Ok(hash) => hash,
        Err(e) => {
            tracing::warn!(
                chain = chain.name,
                error = %e,
                "hook wrapper deploy submission failed; hook transfers will refuse until it lands"
            );
            return;
        }
    };
    handle_deploy_receipt(chain, store, sender, hash, receipt_timeout, receipt_poll).await;
}

/// The deploy's receipt: a mined success with a contract address is verified
/// and recorded; every other outcome is a named warn (the next boot retries).
#[expect(
    clippy::too_many_arguments,
    reason = "the receipt's inputs are the deployment's own facts; grouping them would hide \
              which caller supplied each"
)]
async fn handle_deploy_receipt(
    chain: &Chain,
    store: &Arc<crate::store::Store>,
    sender: &TxSender,
    hash: B256,
    receipt_timeout: Duration,
    receipt_poll: Duration,
) {
    match crate::transfer::wait_for_receipt(&chain.write_rpc, hash, receipt_timeout, receipt_poll)
        .await
    {
        Ok(Some(receipt)) if receipt.status() => {
            if let Some(wrapper) = receipt.contract_address {
                record_wrapper(chain, store, sender, wrapper).await;
            } else {
                tracing::warn!(
                    chain = chain.name,
                    tx = %hash,
                    "hook wrapper deploy mined without a contract address"
                );
            }
        }
        Ok(Some(_)) => {
            tracing::warn!(chain = chain.name, tx = %hash, "hook wrapper deploy reverted");
        }
        Ok(None) => tracing::warn!(
            chain = chain.name,
            tx = %hash,
            timeout = receipt_timeout.as_secs(),
            "hook wrapper deploy not mined within the window; the next boot retries"
        ),
        Err(e) => tracing::warn!(
            chain = chain.name,
            tx = %hash,
            error = %e,
            "hook wrapper deploy receipt read failed"
        ),
    }
}

/// Verify a freshly deployed wrapper and record it. A failed verification is
/// warned, never recorded: the recorded address must be one the credit path
/// can actually call.
async fn record_wrapper(
    chain: &Chain,
    store: &Arc<crate::store::Store>,
    sender: &TxSender,
    wrapper: Address,
) {
    if let Err(e) = crate::hook::verify(
        &chain.rpc,
        wrapper,
        sender.address(),
        chain.message_transmitter,
    )
    .await
    {
        tracing::warn!(chain = chain.name, wrapper = %wrapper, error = %e, "freshly deployed hook wrapper failed verification");
        return;
    }
    if let Err(e) = store.with(|state| {
        state.hooks.insert(chain.domain, wrapper);
        Ok(())
    }) {
        tracing::warn!(error = %e, "recording the hook wrapper failed");
        return;
    }
    if let Err(e) = store.save() {
        tracing::warn!(error = %e, "persisting the hook wrapper failed");
    }
    tracing::info!(chain = chain.name, wrapper = %wrapper, "cctp hook wrapper deployed");
}

/// Deploy the hook wrapper on every served chain that lacks one (§15.4's boot
/// step). A chain that fails is warned about, never fatal: the Iris API still
/// serves, and hook transfers refuse by name until a later boot lands the
/// deployment.
async fn install_hooks(
    chains: &[Chain],
    store: &Arc<crate::store::Store>,
    sender: &TxSender,
    receipt_timeout: Duration,
    receipt_poll: Duration,
) {
    for chain in chains {
        if let Some(wrapper) = existing_wrapper(chain, store, sender).await {
            tracing::info!(chain = chain.name, wrapper = %wrapper, "cctp hook wrapper present");
            continue;
        }
        deploy_wrapper(chain, store, sender, receipt_timeout, receipt_poll).await;
    }
}

/// The devnet attester, installed through the real setters (idempotent).
async fn install_attester(
    chains: &[Chain],
    attester: &crate::attester::Attester,
    receipt_timeout: Duration,
    receipt_poll: Duration,
    stipend: U256,
) -> crate::Result<()> {
    for chain in chains {
        let outcome = crate::attester::install(
            &chain.write_rpc,
            chain.name,
            chain.message_transmitter,
            attester.address(),
            receipt_timeout,
            receipt_poll,
            stipend,
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

/// Top the relay wallet up to the configured low threshold when it falls
/// below it (§15.4's boot funding; `anvil_setBalance` on both chains). The
/// threshold is the deployment's own value — the same one the gas alert
/// reads — so the top-up has no second, hardcoded amount to drift from.
pub(crate) async fn fund_relayer(
    chains: &[Chain],
    wallet: crate::Address,
    low: U256,
) -> crate::Result<()> {
    for chain in chains {
        let balance = chain.native_balance(wallet).await?;
        if balance < low {
            chain.fund_native(wallet, low).await?;
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
