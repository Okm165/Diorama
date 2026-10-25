//! The bridge service's composition root (the `cctp-bridge` bin is the shell
//! over [`main_entry`] — the same shell as the engines').
//!
//! Boot order: load the instance, build both chains, assert the wiring (chain
//! ids, `localDomain`, message versions, the peer messenger — fail closed),
//! install the devnet attester through the contracts' own setters on both
//! chains, then spawn the two watchers, the relayer and the two listeners
//! (8095 Iris-compatible, 8096 control).
use crate::chains::{Chain, Domain, DOMAIN_ETHEREUM, DOMAIN_HYPER_EVM};
use crate::transfer::{TxSender, Wallet};
use crate::{Address, B256, U256};
use mirror_config::boot;
use mirror_config::CctpSection;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// The shared runtime every spawned plane reads.
struct Bridge {
    chains: Vec<Chain>,
    section: CctpSection,
    store: Arc<crate::store::Store>,
    attester: Arc<crate::attester::Attester>,
    sender: Arc<TxSender>,
    metrics: Arc<crate::metrics::CctpMetrics>,
    receipt_timeout: Duration,
    poll: Duration,
}

pub async fn main_entry(args: Vec<String>) -> crate::Result<()> {
    boot::init_logging();
    let config = boot::service_config_or_exit(&args);
    let section = load_wiring(&config)?;
    let (bridge, min_fees) = boot_bridge(section).await?;

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

/// Load the instance and require this pod's wiring: one instance file, many
/// readers — the bridge reads only `[services.cctp_bridge]`, so the venue engine's /
/// simulator's `${VAR}` wiring is not this pod's to provide (`inspect` reports
/// unresolved references instead of failing on them; the cctp section's own
/// wiring is then required here, by name).
fn load_wiring(config: &std::path::Path) -> crate::Result<CctpSection> {
    let (mut cfg, unresolved) = mirror_config::Instance::inspect(config)?;
    let section = cfg.take_cctp()?;
    let state_dir = section.state_dir.to_string_lossy().to_string();
    mirror_config::require_resolved_logged(
        "cctp wiring",
        &[
            &state_dir,
            &section.t_url,
            &section.v_url,
            &section.relay_key,
            &section.attester_seed,
        ],
        &unresolved,
    )?;
    Ok(section)
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
) -> crate::Result<(Bridge, BTreeMap<Domain, Option<U256>>)> {
    let receipt_timeout = Duration::from_secs(section.receipt_timeout_secs);
    let chains = build_chains(&section, Duration::from_secs(30))?;
    for chain in &chains {
        chain
            .rpc
            .wait_until_up(chain.name, Duration::from_secs(300))
            .await?;
    }
    let min_fees = verify_wiring(&chains).await?;
    let attester = Arc::new(crate::attester::Attester::from_seed(
        &section.attester_seed,
    )?);
    install_attester(&chains, &attester, receipt_timeout).await?;

    let wallet = Wallet::from_secret(&section.relay_key)?;
    tracing::info!(relayer = %wallet.address(), "cctp relay wallet loaded");
    if let Err(e) = fund_relayer(&chains, wallet.address(), section.balance_low_wei).await {
        tracing::error!(error = %e, "relay wallet top-up failed; the gas alerts stay on");
    }
    let sender = Arc::new(TxSender::new(wallet));
    let store = Arc::new(crate::store::Store::load(
        section.state_dir.join("cctp.json"),
    )?);
    install_hooks(&chains, &store, &sender, receipt_timeout).await;
    let bridge = Bridge {
        chains,
        store,
        attester,
        sender,
        metrics: Arc::new(crate::metrics::CctpMetrics::default()),
        receipt_timeout,
        poll: Duration::from_millis(section.poll_interval_ms),
        section,
    };
    Ok((bridge, min_fees))
}

impl Bridge {
    /// One watcher per chain; each serves only its peer's domain. The boot
    /// wiring asserts both chains — a missing peer here is a refusal, never a
    /// panic (the no-panics rule: data paths refuse or degrade).
    fn spawn_watchers(&self) -> crate::Result<()> {
        for chain in &self.chains {
            let (destination_domain, start_block) = if chain.domain == DOMAIN_ETHEREUM {
                (DOMAIN_HYPER_EVM, self.section.t_start_block.unwrap_or(0))
            } else {
                (DOMAIN_ETHEREUM, self.section.v_start_block.unwrap_or(0))
            };
            let Some(destination) = self
                .chains
                .iter()
                .find(|c| c.domain == destination_domain)
                .cloned()
            else {
                return Err(crate::Error::UnknownDomain {
                    domain: destination_domain,
                });
            };
            let watcher = crate::watcher::Watcher {
                chain: chain.clone(),
                destination_domain,
                destination,
                store: self.store.clone(),
                attester: self.attester.clone(),
                start_block,
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
        min_fees: BTreeMap<Domain, Option<U256>>,
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
            expiration_blocks: self.section.expiration_blocks,
        });
        let control_state = Arc::new(crate::control::ControlState {
            store: self.store.clone(),
            chains: self.chains.clone(),
            sender: self.sender.clone(),
            metrics: self.metrics.clone(),
            relayer,
            min_fees,
            receipt_timeout: self.receipt_timeout,
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

/// T (domain 0) and V (domain 19), from the instance's endpoints.
fn build_chains(section: &CctpSection, timeout: Duration) -> crate::Result<Vec<Chain>> {
    Ok(vec![
        Chain::ethereum(&section.t_url, timeout)?,
        Chain::hyper_evm(&section.v_url, timeout)?,
    ])
}

/// The wiring asserts + the per-domain min-fee probe (§15.2: absent on
/// Ethereum, `0` on HyperEVM). A mismatch is a boot refusal.
async fn verify_wiring(chains: &[Chain]) -> crate::Result<BTreeMap<Domain, Option<U256>>> {
    let mut min_fees = BTreeMap::new();
    for chain in chains {
        let peer = chains
            .iter()
            .find(|c| c.domain != chain.domain)
            .ok_or(crate::Error::UnknownDomain { domain: chain.domain })?;
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
        min_fees.insert(chain.domain, wiring.min_fee);
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
        crate::chains::MESSAGE_TRANSMITTER_V2,
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
) {
    let hash = match crate::hook::deploy(
        &chain.rpc,
        sender,
        crate::chains::MESSAGE_TRANSMITTER_V2,
    )
    .await
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
    handle_deploy_receipt(chain, store, sender, hash, receipt_timeout).await;
}

/// The deploy's receipt: a mined success with a contract address is verified
/// and recorded; every other outcome is a named warn (the next boot retries).
async fn handle_deploy_receipt(
    chain: &Chain,
    store: &Arc<crate::store::Store>,
    sender: &TxSender,
    hash: B256,
    receipt_timeout: Duration,
) {
    match crate::transfer::wait_for_receipt(&chain.rpc, hash, receipt_timeout).await {
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
        Ok(Some(_)) => tracing::warn!(chain = chain.name, tx = %hash, "hook wrapper deploy reverted"),
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
        crate::chains::MESSAGE_TRANSMITTER_V2,
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
) {
    for chain in chains {
        if let Some(wrapper) = existing_wrapper(chain, store, sender).await {
            tracing::info!(chain = chain.name, wrapper = %wrapper, "cctp hook wrapper present");
            continue;
        }
        deploy_wrapper(chain, store, sender, receipt_timeout).await;
    }
}

/// The devnet attester, installed through the real setters (idempotent).
async fn install_attester(
    chains: &[Chain],
    attester: &crate::attester::Attester,
    receipt_timeout: Duration,
) -> crate::Result<()> {
    for chain in chains {
        let outcome = crate::attester::install(
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

pub(crate) async fn fund_relayer(
    chains: &[Chain],
    wallet: crate::Address,
    low: U256,
) -> crate::Result<()> {
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


