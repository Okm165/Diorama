//! The instance file's shape + cross-validation (`check`, §19.5).
use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

use crate::endpoints::{EndpointGroup, Endpoints, WsEndpoints, HTTPS_SCHEME, HTTP_SCHEME};
use crate::sections::{
    CctpSection, EngineAnvil, EngineSection, FeedBridgeSection, Services, VenueSimSection,
};
use crate::ConfigError;

/// The section requirements' one spelling each: a message naming a missing
/// section must read the same wherever it is raised.
const VENUE_CHAIN_REQUIRED: &str = "profile \"venue\" requires [chains.venue]";
const VENUE_ENGINE_REQUIRED: &str = "profile \"venue\" requires [services.engine]";
const MAINNET_CHAIN_REQUIRED: &str = "profile \"mainnet\" requires [chains.l1]";
const MAINNET_ENGINE_REQUIRED: &str = "profile \"mainnet\" requires [services.engine]";

/// One mirror deployment: identity, chains, watchlist, service sections.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub instance: InstanceMeta,
    pub chains: Chains,
    /// The deployment's named endpoint groups: the single home of every URL
    /// list. A field writes `@name`; the loader expands it before validation.
    #[serde(default)]
    pub endpoints: BTreeMap<String, EndpointGroup>,
    /// The shared endpoint-pool policy (rounds, cooldown, attempt budget).
    /// Every lane — JSON-RPC, REST, WS, beacon — reads this one table.
    #[serde(default)]
    pub endpoint_defaults: diorama_anvil::PoolPolicy,
    /// The watched addresses: the parent-diff scope on the mainnet profile,
    /// and the account-field scope of the venue's poll follow (both mirrors
    /// follow their parent; the mechanisms differ — see
    /// `docs/ARCHITECTURE.md`, "One product, two chains").
    #[serde(default)]
    pub watchlist: Vec<WatchEntry>,
    pub services: Services,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstanceMeta {
    pub name: String,
    pub namespace: String,
    pub profile: Profile,
    /// The optional project specialization file (a path, or a name resolved
    /// beside the instance as `project.toml`). Generic mirrors leave it
    /// unset; a product deployment points at its ignored overlay.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Mainnet,
    Venue,
    /// A standalone CCTP bridge instance: no chain, engine or simulator
    /// sections, only `[services.cctp_bridge]`. One process = one bridge;
    /// several bridges compose as several instances.
    Cctp,
}

/// `[chains]`: one entry per chain the mirror serves. Exactly one is present
/// (the profile pins which).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Chains {
    pub l1: Option<ChainL1>,
    pub venue: Option<ChainVenue>,
}

/// `[chains.l1]`: the Ethereum source. `kind` only - the chain id, CCTP
/// domain and canonical addresses are code facts.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChainL1 {
    pub kind: ChainKind,
    /// The Ethereum source's read endpoints, in preference order. A single
    /// string (`source_url = "${RETH_URL}"`) and a list are both accepted;
    /// the primary (first) is the one anvil forks from and `/admin/epoch`
    /// re-pins against.
    #[serde(alias = "source_url")]
    pub source_urls: Endpoints,
    pub follow: FollowHead,
    pub follow_margin_blocks: u64,
    /// Blocks between the follow head and the fork base; the single home of
    /// the offset (anvil init containers read it via `diorama-config get`).
    pub fork_offset: u64,
}

/// `[chains.venue]`: the venue's fork source and public API. `kind` only -
/// chain id 999, CCTP domain 19 and the canonical HyperEVM addresses are code
/// facts. V pins at the source's `latest` (or an explicit block chosen at
/// deploy time by the anvil init container), so there is no follow/offset
/// policy here: the source is archive-deep (measured: ≥5M blocks).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChainVenue {
    pub kind: ChainKind,
    /// The HyperEVM read endpoints, in preference order. The engine's source
    /// client fans out over all of them (random start, next-on-failure); the
    /// primary (first) is the one anvil V forks from and `/admin/epoch`
    /// re-pins against.
    #[serde(alias = "source_url")]
    pub source_urls: Endpoints,
    /// The info REST API pool the simulator's market plane reads. A single
    /// `api_url` is the one-endpoint form.
    #[serde(alias = "api_url")]
    pub api_urls: Endpoints,
    /// The websocket pool the simulator's market plane subscribes to. A
    /// single `ws_url` is the one-endpoint form.
    #[serde(alias = "ws_url")]
    pub ws_urls: WsEndpoints,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChainKind {
    Ethereum,
    Hyperliquid,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FollowHead {
    /// Justified checkpoint block: ~1 epoch behind head, reorg-immune in practice.
    #[default]
    Safe,
    Latest,
    Finalized,
}

impl FollowHead {
    pub fn tag(self) -> &'static str {
        match self {
            FollowHead::Safe => "safe",
            FollowHead::Latest => "latest",
            FollowHead::Finalized => "finalized",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WatchEntry {
    pub address: Address,
    #[serde(default)]
    pub note: Option<String>,
}

/// reth's state window: a pin deeper than this cannot be replayed (measured:
/// the BAL/state serve window is 10,064 blocks). Public because the applier's
/// rebased-cursor gap bound is the same measured window, not a second copy.
pub const RETH_STATE_WINDOW_BLOCKS: u64 = 10_064;
/// The safe-head lag the pin must clear (~32-64 blocks, measured).
const MIN_FORK_OFFSET_BLOCKS: u64 = 64;

impl Instance {
    /// The watched addresses, as the engine's COW scope.
    pub fn watch_set(&self) -> HashSet<Address> {
        self.watchlist.iter().map(|w| w.address).collect()
    }

    /// Expand every `@name` endpoint reference against `[endpoints]`. The
    /// loader calls this once, after `${VAR}` expansion and before `check`,
    /// so every consumer sees concrete URL lists and the group table stays
    /// the single home of the deployment's endpoints.
    pub fn resolve_endpoints(&mut self) -> Result<(), ConfigError> {
        let groups = self.endpoints.clone();
        let mut issues = Vec::new();
        self.resolve_chain_endpoints(&groups, &mut issues);
        self.resolve_service_endpoints(&groups, &mut issues);
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid { issues })
        }
    }

    /// The pool policy every lane shares.
    pub fn endpoint_policy(&self) -> diorama_anvil::PoolPolicy {
        self.endpoint_defaults
    }

    /// Recover the attributes of `urls` from the named groups. Expansion
    /// deliberately keeps the URL list as the single value; this map restores
    /// the roles/chain id/weight for the consumer that builds the pool. A URL
    /// not named by any group is an unqualified endpoint (no declared roles,
    /// no chain assert).
    pub fn endpoint_specs(&self, urls: &[String]) -> Vec<diorama_anvil::EndpointSpec> {
        urls.iter()
            .map(|url| {
                let group = self.endpoints.values().find(|group| {
                    group
                        .http
                        .iter()
                        .chain(group.ws.iter())
                        .flat_map(|value| value.split(','))
                        .any(|value| value.trim().trim_end_matches('/') == url)
                });
                match group {
                    Some(group) => diorama_anvil::EndpointSpec {
                        url: url.clone(),
                        roles: group.roles.iter().cloned().collect(),
                        chain_id: group.chain_id,
                        weight: group.weight,
                    },
                    None => diorama_anvil::EndpointSpec::url(url.clone()),
                }
            })
            .collect()
    }

    /// The chain sections' endpoint references.
    fn resolve_chain_endpoints(
        &mut self,
        groups: &BTreeMap<String, EndpointGroup>,
        issues: &mut Vec<String>,
    ) {
        if let Some(l1) = &mut self.chains.l1 {
            if let Err(error) = l1.source_urls.expand(groups) {
                issues.push(format!("chains.l1.source_urls: {error}"));
            }
        }
        if let Some(venue) = &mut self.chains.venue {
            if let Err(error) = venue.source_urls.expand(groups) {
                issues.push(format!("chains.venue.source_urls: {error}"));
            }
            if let Err(error) = venue.api_urls.expand(groups) {
                issues.push(format!("chains.venue.api_urls: {error}"));
            }
            if let Err(error) = venue.ws_urls.expand(groups) {
                issues.push(format!("chains.venue.ws_urls: {error}"));
            }
        }
    }

    /// The service sections' endpoint references.
    fn resolve_service_endpoints(
        &mut self,
        groups: &BTreeMap<String, EndpointGroup>,
        issues: &mut Vec<String>,
    ) {
        if let Some(engine) = &mut self.services.engine {
            expand_optional_http(
                "services.engine.anvil.target_url",
                &mut engine.anvil.target_url,
                groups,
                issues,
            );
            expand_http_list(
                "services.engine.beacon.urls",
                &mut engine.beacon.urls,
                groups,
                issues,
            );
            expand_optional_http(
                "services.engine.source_sync.engine_url",
                &mut engine.source_sync.engine_url,
                groups,
                issues,
            );
        }
        if let Some(sim) = &mut self.services.venue_sim {
            expand_optional_http(
                "services.venue_sim.engine_admin_url",
                &mut sim.engine_admin_url,
                groups,
                issues,
            );
            expand_optional_http(
                "services.venue_sim.anvil_url",
                &mut sim.anvil_url,
                groups,
                issues,
            );
        }
        if let Some(cctp) = &mut self.services.cctp_bridge {
            if let Some(t_url) = &mut cctp.t_url {
                expand_http("services.cctp_bridge.t_url", t_url, groups, issues);
            }
            if let Some(v_url) = &mut cctp.v_url {
                expand_http("services.cctp_bridge.v_url", v_url, groups, issues);
            }
            for (index, chain) in cctp.chains.iter_mut().enumerate() {
                if let Err(error) = chain.rpc_urls.expand(groups) {
                    issues.push(format!(
                        "services.cctp_bridge.chains[{index}].rpc_urls: {error}"
                    ));
                }
            }
        }
    }

    /// Cross-validation. Every failure names the key and the reason.
    pub fn check(&self) -> Result<(), ConfigError> {
        let mut issues = Vec::new();
        self.check_profile_coherence(&mut issues);
        self.check_chains(&mut issues);
        self.check_endpoint_policy(&mut issues);
        self.check_services(&mut issues);
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid { issues })
        }
    }

    /// Exactly one profile's sections are present. A mirror process carries
    /// one chain + one engine; a bridge process carries only its CCTP
    /// section. Composition happens across instances (two venues, four
    /// mainnets, several bridges), never inside one file.
    fn check_profile_coherence(&self, issues: &mut Vec<String>) {
        let (name, needs, forbids): (&str, &[&str], &[&str]) = match self.instance.profile {
            Profile::Mainnet => (
                "mainnet",
                &["[chains.l1]", "[services.engine]"],
                &[
                    "[chains.venue]",
                    "[services.venue_sim]",
                    "[services.cctp_bridge]",
                ],
            ),
            Profile::Venue => (
                "venue",
                &["[chains.venue]", "[services.engine]"],
                &["[chains.l1]", "[services.cctp_bridge]"],
            ),
            Profile::Cctp => (
                "cctp",
                &["[services.cctp_bridge]"],
                &[
                    "[chains.l1]",
                    "[chains.venue]",
                    "[services.engine]",
                    "[services.venue_sim]",
                ],
            ),
        };
        for section in needs {
            if !self.section_present(section) {
                issues.push(format!("profile \"{name}\" requires {section}"));
            }
        }
        for section in forbids {
            if self.section_present(section) {
                issues.push(format!("profile \"{name}\" must not carry {section}"));
            }
        }
    }

    /// Whether a named section is present. One mapping for the profile
    /// coherence rules, so a new section is added in one place.
    fn section_present(&self, section: &str) -> bool {
        match section {
            "[chains.l1]" => self.chains.l1.is_some(),
            "[chains.venue]" => self.chains.venue.is_some(),
            "[services.engine]" => self.services.engine.is_some(),
            "[services.venue_sim]" => self.services.venue_sim.is_some(),
            "[services.cctp_bridge]" => self.services.cctp_bridge.is_some(),
            _ => false,
        }
    }

    fn check_chains(&self, issues: &mut Vec<String>) {
        if let Some(l1) = &self.chains.l1 {
            if l1.kind != ChainKind::Ethereum {
                issues.push("chains.l1.kind must be \"ethereum\"".into());
            }
            if l1.fork_offset < MIN_FORK_OFFSET_BLOCKS {
                issues.push(format!(
                    "chains.l1.fork_offset {} is below the safe-head lag (~{} blocks): \
                     the pin would sit at or behind the follow head",
                    l1.fork_offset, MIN_FORK_OFFSET_BLOCKS
                ));
            }
            if l1.fork_offset >= RETH_STATE_WINDOW_BLOCKS {
                issues.push(format!(
                    "chains.l1.fork_offset {} is outside reth's {}-block state window",
                    l1.fork_offset, RETH_STATE_WINDOW_BLOCKS
                ));
            }
            check_http_urls("chains.l1.source_urls", &l1.source_urls, issues);
        }
        if let Some(venue) = &self.chains.venue {
            if venue.kind != ChainKind::Hyperliquid {
                issues.push("chains.venue.kind must be \"hyperliquid\"".into());
            }
            check_http_urls("chains.venue.source_urls", &venue.source_urls, issues);
            check_http_urls("chains.venue.api_urls", &venue.api_urls, issues);
            check_ws_urls("chains.venue.ws_urls", &venue.ws_urls, issues);
        }
    }

    /// The shared pool policy is a config table, so a value that would make a
    /// call return without an attempt or spin hot is a config error here, not
    /// a runtime surprise in a lane.
    fn check_endpoint_policy(&self, issues: &mut Vec<String>) {
        let policy = &self.endpoint_defaults;
        if policy.rounds == 0 {
            issues.push("endpoint_defaults.rounds must be >= 1".into());
        }
        if policy.max_attempts == 0 {
            issues.push("endpoint_defaults.max_attempts must be >= 1".into());
        }
        if policy.cooldown_base_ms == 0 {
            issues.push("endpoint_defaults.cooldown_base_ms must be > 0".into());
        }
        if policy.cooldown_max_ms < policy.cooldown_base_ms {
            issues.push("endpoint_defaults.cooldown_max_ms must be >= cooldown_base_ms".into());
        }
        if policy.emergency_probe_ms == 0 {
            issues.push("endpoint_defaults.emergency_probe_ms must be > 0".into());
        }
    }

    fn check_services(&self, issues: &mut Vec<String>) {
        if let Some(eng) = &self.services.engine {
            check_engine(eng, issues);
        }
        if let Some(sim) = &self.services.venue_sim {
            check_venue_sim(sim, issues);
        }
        if let Some(cctp) = &self.services.cctp_bridge {
            check_cctp(cctp, issues);
        }
    }

    /// The mainnet profile's sections, moved out for the run path (the check
    /// already validated presence; a missing one is still a named error, never
    /// a silent default).
    pub fn take_mainnet(&mut self) -> Result<(ChainL1, EngineSection), ConfigError> {
        take_pair(
            self.chains.l1.take(),
            MAINNET_CHAIN_REQUIRED,
            self.services.engine.take(),
            MAINNET_ENGINE_REQUIRED,
        )
    }

    /// The venue profile's sections, moved out for the run path.
    pub fn take_venue(&mut self) -> Result<(ChainVenue, EngineSection), ConfigError> {
        take_pair(
            self.chains.venue.take(),
            VENUE_CHAIN_REQUIRED,
            self.services.engine.take(),
            VENUE_ENGINE_REQUIRED,
        )
    }

    /// The simulator's sections, moved out for the run path.
    pub fn take_venue_sim(&mut self) -> Result<(ChainVenue, VenueSimSection), ConfigError> {
        take_pair(
            self.chains.venue.take(),
            VENUE_CHAIN_REQUIRED,
            self.services.venue_sim.take(),
            "the simulator requires [services.venue_sim]",
        )
    }

    /// The CCTP bridge's section, moved out for the run path. Cross-cutting:
    /// it needs no chain section (its own `t_url`/`v_url` are the endpoints).
    pub fn take_cctp(&mut self) -> Result<CctpSection, ConfigError> {
        required(
            self.services.cctp_bridge.take(),
            "the CCTP bridge requires [services.cctp_bridge]",
        )
    }

    /// The mainnet profile's endpoints: the pre-context reads the run path
    /// needs (`Ctx::new` takes the sections itself).
    pub fn mainnet_urls(&self) -> Result<(Endpoints, String), ConfigError> {
        let (l1, engine) = take_pair(
            self.chains.l1.as_ref(),
            MAINNET_CHAIN_REQUIRED,
            self.services.engine.as_ref(),
            MAINNET_ENGINE_REQUIRED,
        )?;
        Ok((
            l1.source_urls.clone(),
            required(
                engine.anvil.target_url.clone(),
                "profile \"mainnet\" requires [services.engine.anvil].target_url",
            )?,
        ))
    }

    /// The venue profile's endpoints (the fork source and anvil V).
    pub fn venue_urls(&self) -> Result<(Endpoints, String), ConfigError> {
        let (chain, engine) = take_pair(
            self.chains.venue.as_ref(),
            VENUE_CHAIN_REQUIRED,
            self.services.engine.as_ref(),
            VENUE_ENGINE_REQUIRED,
        )?;
        let target_url = required(
            engine.anvil.target_url.clone(),
            "profile \"venue\" requires [services.engine.anvil].target_url",
        )?;
        Ok((chain.source_urls.clone(), target_url))
    }

    /// The instance as a TOML value (the shape the loader produced).
    pub fn to_value(&self) -> Result<toml::Value, ConfigError> {
        toml::Value::try_from(self).map_err(|e| ConfigError::Invalid {
            issues: vec![format!("serializing the instance: {e}")],
        })
    }

    /// One value by dotted path (`chains.l1.fork_offset`), for scripts.
    pub fn get(&self, path: &str) -> Result<toml::Value, ConfigError> {
        let value = self.to_value()?;
        let mut cursor = &value;
        for segment in path.split('.') {
            cursor = cursor.get(segment).ok_or_else(|| ConfigError::Invalid {
                issues: vec![format!("no such key `{path}`")],
            })?;
        }
        Ok(cursor.clone())
    }

    /// Shell-ready `KEY=VALUE` lines for one section. The anvil pods' init
    /// containers resolve their pin with `FORK_OFFSET` from here.
    pub fn export_env(&self, section: &str) -> Result<Vec<String>, ConfigError> {
        match section {
            "anvil" => {
                let l1 = required(
                    self.chains.l1.as_ref(),
                    "export-env anvil requires [chains.l1]",
                )?;
                Ok(vec![format!("FORK_OFFSET={}", l1.fork_offset)])
            }
            "client" | "daloc" => self.client_env(),
            other => Err(ConfigError::Invalid {
                issues: vec![format!("unknown section `{other}` (known: anvil, client)")],
            }),
        }
    }

    /// The client-facing endpoint block: our values under
    /// their names — we provide values, never rename their interface. The
    /// keys are **exactly** the `test-endpoints` ConfigMaps' spellings
    /// (`k8s/base/nodeports.yaml` for the first chain,
    /// `k8s/overlays/diorama-venue/nodeports.yaml` for the venue's
    /// `CHAIN_1_`-prefixed block): one fact, one spelling, whichever surface
    /// renders it. The chain URL is the deployment wiring (the same env the
    /// pods read); the service URLs are in-cluster DNS built from the
    /// instance's own ports, under the manifests' Service names (`engine`,
    /// `venue-sim`, `cctp-bridge` — `k8s/base/engine.yaml`, `base/venue/`).
    fn client_env(&self) -> Result<Vec<String>, ConfigError> {
        match self.instance.profile {
            Profile::Mainnet => self.client_env_mainnet(),
            Profile::Venue => self.client_env_venue(),
            Profile::Cctp => self.client_env_cctp(),
        }
    }

    /// The bridge's client block: the Iris base the DALOC relay points at
    /// (the relay appends `/v2/messages/...` itself). The export is consumed
    /// from the *client's* namespace, so the short service DNS name is
    /// qualified with this instance's namespace.
    fn client_env_cctp(&self) -> Result<Vec<String>, ConfigError> {
        let cctp = self.services.cctp_bridge.as_ref().ok_or_else(|| {
            mismatch("export-env client (cctp profile) requires [services.cctp_bridge]")
        })?;
        Ok(vec![format!(
            "IRIS_API_URL={}",
            cluster_url(
                &format!("http://cctp-bridge:{}", cctp.iris.port),
                &self.instance.namespace
            )
        )])
    }

    /// The mainnet profile's client block: the first chain's keys.
    fn client_env_mainnet(&self) -> Result<Vec<String>, ConfigError> {
        let engine = self.services.engine.as_ref().ok_or_else(|| {
            mismatch("export-env client (mainnet profile) requires [services.engine]")
        })?;
        let t_url = engine.anvil.target_url.as_deref().ok_or_else(|| {
            mismatch(
                "export-env client (mainnet profile) requires [services.engine.anvil].target_url",
            )
        })?;
        let t = resolved("ANVIL_T_URL", t_url)?;
        let t = cluster_url(&t, &self.instance.namespace);
        let admin = cluster_url(
            &format!("http://engine:{}", engine.http.port),
            &self.instance.namespace,
        );
        Ok(vec![
            format!("CHAIN_RPC_URL={t}"),
            format!("CHAIN_WS_URL={}", ws_of(&t)),
            format!("DIORAMA_ADMIN_URL={admin}"),
        ])
    }

    /// The venue profile's client block: the `CHAIN_1_`-prefixed second-chain
    /// keys and the simulator's URLs.
    fn client_env_venue(&self) -> Result<Vec<String>, ConfigError> {
        let engine = self.services.engine.as_ref().ok_or_else(|| {
            mismatch("export-env client (venue profile) requires [services.engine]")
        })?;
        let v_url = engine.anvil.target_url.as_deref().ok_or_else(|| {
            mismatch(
                "export-env client (venue profile) requires [services.engine.anvil].target_url",
            )
        })?;
        let v = resolved("ANVIL_V_URL", v_url)?;
        let v = cluster_url(&v, &self.instance.namespace);
        let sim = self.services.venue_sim.as_ref().ok_or_else(|| {
            mismatch("export-env client (venue profile) requires [services.venue_sim]")
        })?;
        let info = cluster_url(
            &format!("http://venue-sim:{}", sim.info.port),
            &self.instance.namespace,
        );
        let control = cluster_url(
            &format!("http://venue-sim:{}", sim.control.port),
            &self.instance.namespace,
        );
        let admin = cluster_url(
            &format!("http://engine:{}", engine.http.port),
            &self.instance.namespace,
        );
        Ok(vec![
            format!("CHAIN_1_CHAIN_RPC_URL={v}"),
            format!("CHAIN_1_CHAIN_WS_URL={}", ws_of(&v)),
            format!("HL_INFO_API_URL={info}"),
            format!("VENUE_CONTROL_URL={control}"),
            // DALOC's `VENUE_ACCOUNT_FEEDS` / `VENUE_RATE_FEEDS` are JSON
            // arrays, not URLs. Emitting URLs under those names poisoned a
            // consumer that sourced this block; the base URL has its own key.
            format!("VENUE_ADMIN_URL={admin}"),
        ])
    }
}

/// A value that must be concrete for the export: a lingering `${VAR}`
/// reference means the deployment wiring was not set, and the export's whole
/// purpose is values, never references.
fn resolved(name: &str, value: &str) -> Result<String, ConfigError> {
    if value.contains("${") {
        return Err(ConfigError::Invalid {
            issues: vec![format!(
                "export-env client: {name} is not set (deployment wiring)"
            )],
        });
    }
    Ok(value.to_string())
}

/// Qualify a short Kubernetes service DNS name with the instance's namespace:
/// `http://engine:8080` becomes
/// `http://engine.diorama-venue.svc.cluster.local:8080` for a consumer in
/// another namespace. A host that already carries a dot (a FQDN, an IP, a
/// public endpoint) is left alone, so the export never rewrites a real URL.
fn cluster_url(url: &str, namespace: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let host_end = rest.find(['/', ':', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() || host.contains('.') || host.parse::<std::net::IpAddr>().is_ok() {
        return url.to_string();
    }
    format!(
        "{scheme}://{host}.{namespace}.svc.cluster.local{}",
        &rest[host_end..]
    )
}

/// The websocket form of an http URL — anvil serves WS on its HTTP port.
fn ws_of(url: &str) -> String {
    match url.strip_prefix("http") {
        Some(rest) => format!("ws{rest}"),
        None => url.to_string(),
    }
}

/// Take two required sections at once (the profiles' pairs), each named when
/// absent: the same rule as [`required`], named for the call-site shape.
fn take_pair<A, B>(
    a: Option<A>,
    a_msg: &str,
    b: Option<B>,
    b_msg: &str,
) -> Result<(A, B), ConfigError> {
    Ok((required(a, a_msg)?, required(b, b_msg)?))
}

/// Take a required section, naming the need when it is absent: the one shape
/// every `take_*`/`*_urls` read uses.
fn required<T>(section: Option<T>, what: &str) -> Result<T, ConfigError> {
    section.ok_or_else(|| mismatch(what))
}

fn mismatch(what: &str) -> ConfigError {
    ConfigError::Invalid {
        issues: vec![what.to_string()],
    }
}

/// The composable CCTP topology: unique chain ids/domains, concrete
/// addresses, and lanes that name declared chains. A two-chain topology may
/// omit lanes (the legacy pair is the single lane); three or more chains must
/// state their lanes so the direction is never guessed.
fn check_cctp_chains(cctp: &CctpSection, issues: &mut Vec<String>) {
    let ids = check_cctp_chain_list(cctp, issues);
    // A bridge needs two ends. A one-chain topology parses and passes a naive
    // check but cannot boot (`build_lanes` refuses it); reject it here, where
    // the operator reads the error.
    if !cctp.chains.is_empty() && cctp.chains.len() < 2 {
        issues.push(
            "services.cctp_bridge.chains must declare at least two chains (a bridge \
             needs a source and a destination)"
                .into(),
        );
    }
    if cctp.chains.len() > 2 && cctp.lanes.is_empty() {
        issues.push(
            "services.cctp_bridge.lanes must state the directed lanes when more than \
             two chains are configured"
                .into(),
        );
    }
    check_cctp_lanes(cctp, &ids, issues);
}

/// The chain list: unique ids/domains/chain ids, a non-empty RPC pool and
/// concrete contract addresses. Returns the declared ids for the lane check.
fn check_cctp_chain_list(
    cctp: &CctpSection,
    issues: &mut Vec<String>,
) -> std::collections::HashSet<String> {
    use std::collections::HashSet;

    let mut ids = HashSet::new();
    let mut domains = HashSet::new();
    let mut chain_ids = HashSet::new();
    for (index, chain) in cctp.chains.iter().enumerate() {
        let key = format!("services.cctp_bridge.chains[{index}]");
        if chain.id.trim().is_empty() {
            issues.push(format!("{key}.id must not be empty"));
        }
        if !ids.insert(chain.id.clone()) {
            issues.push(format!("{key}.id `{}` is duplicated", chain.id));
        }
        if !domains.insert(chain.domain) {
            issues.push(format!(
                "{key}.domain {} is duplicated (a CCTP domain is unique)",
                chain.domain
            ));
        }
        if !chain_ids.insert(chain.chain_id) {
            issues.push(format!("{key}.chain_id {} is duplicated", chain.chain_id));
        }
        if chain.rpc_urls.is_empty() {
            issues.push(format!("{key}.rpc_urls must not be empty"));
        }
        check_http_urls(&format!("{key}.rpc_urls"), &chain.rpc_urls, issues);
        for (field, address) in [
            ("usdc", chain.usdc),
            ("token_messenger", chain.token_messenger),
            ("message_transmitter", chain.message_transmitter),
        ] {
            if address == Address::ZERO {
                issues.push(format!("{key}.{field} must not be the zero address"));
            }
        }
        if chain.token_minter == Some(Address::ZERO) {
            issues.push(format!(
                "{key}.token_minter must not be the zero address when present"
            ));
        }
    }
    ids
}

/// The directed lanes: both endpoints declared, no self-lane, no duplicate.
fn check_cctp_lanes(
    cctp: &CctpSection,
    ids: &std::collections::HashSet<String>,
    issues: &mut Vec<String>,
) {
    let mut lanes = std::collections::HashSet::new();
    for (index, lane) in cctp.lanes.iter().enumerate() {
        let key = format!("services.cctp_bridge.lanes[{index}]");
        if !ids.contains(&lane.source) {
            issues.push(format!(
                "{key}.source `{}` is not a declared chain",
                lane.source
            ));
        }
        if !ids.contains(&lane.destination) {
            issues.push(format!(
                "{key}.destination `{}` is not a declared chain",
                lane.destination
            ));
        }
        if lane.source == lane.destination {
            issues.push(format!("{key} must not burn and mint on the same chain"));
        }
        if !lanes.insert((lane.source.clone(), lane.destination.clone())) {
            issues.push(format!(
                "{key} duplicates the lane {} -> {}",
                lane.source, lane.destination
            ));
        }
        let mut thresholds = std::collections::HashSet::new();
        for (fee_index, fee) in lane.fees.iter().enumerate() {
            if fee.finality_threshold == 0 {
                issues.push(format!(
                    "{key}.fees[{fee_index}].finalityThreshold must be > 0"
                ));
            }
            if !thresholds.insert(fee.finality_threshold) {
                issues.push(format!(
                    "{key}.fees[{fee_index}] duplicates finalityThreshold {}",
                    fee.finality_threshold
                ));
            }
        }
    }
}

/// The CCTP bridge's coherence: endpoints, the secret-shaped relay key (never
/// a path), the deterministic attester seed, and ordered gas thresholds.
fn check_cctp(cctp: &CctpSection, issues: &mut Vec<String>) {
    require_path(&cctp.state_dir, "services.cctp_bridge.state_dir", issues);
    match (&cctp.t_url, &cctp.v_url, cctp.chains.is_empty()) {
        (Some(t_url), Some(v_url), true) => {
            check_http_url("services.cctp_bridge.t_url", t_url, issues);
            check_http_url("services.cctp_bridge.v_url", v_url, issues);
            if !cctp.lanes.is_empty() {
                issues.push(
                    "services.cctp_bridge.lanes require [[services.cctp_bridge.chains]]; \
                     the legacy t_url/v_url pair already names the single lane"
                        .into(),
                );
            }
        }
        (None, None, false) => {
            check_cctp_chains(cctp, issues);
        }
        (None, None, true) => issues.push(
            "services.cctp_bridge requires either t_url + v_url or at least one \
             [[services.cctp_bridge.chains]] entry"
                .into(),
        ),
        _ => issues.push(
            "services.cctp_bridge must use either the legacy t_url/v_url pair or \
             [[services.cctp_bridge.chains]], never both"
                .into(),
        ),
    }
    // After `${VAR}` expansion the value must be the 32-byte hex key; a raw
    // reference is checked once the deployment expands it.
    if !cctp.relay_key.contains("${") && !looks_like_secret(&cctp.relay_key) {
        issues.push(
            "services.cctp_bridge.relay_key must be a 32-byte hex key (or a ${VAR} reference), \
             never a path"
                .into(),
        );
    }
    if cctp.attester_seed.trim().is_empty() {
        issues.push("services.cctp_bridge.attester_seed must not be empty".into());
    }
    if cctp.scan_chunk_blocks == 0 {
        issues.push("services.cctp_bridge.scan_chunk_blocks must be > 0".into());
    }
    check_cctp_timing(cctp, issues);
    if cctp.balance_critical_wei >= cctp.balance_low_wei {
        issues
            .push("services.cctp_bridge.balance_critical_wei must be below balance_low_wei".into());
    }
}

/// The bridge's receipt/retry cadence: every value positive, and the poll no
/// longer than the timeout it sits inside.
fn check_cctp_timing(cctp: &CctpSection, issues: &mut Vec<String>) {
    if cctp.poll_interval_ms == 0 {
        issues.push("services.cctp_bridge.poll_interval_ms must be > 0".into());
    }
    if cctp.rpc_timeout_secs == 0 {
        issues.push("services.cctp_bridge.rpc_timeout_secs must be > 0".into());
    }
    if cctp.boot_timeout_secs == 0 {
        issues.push("services.cctp_bridge.boot_timeout_secs must be > 0".into());
    }
    if cctp.receipt_timeout_secs == 0 {
        issues.push("services.cctp_bridge.receipt_timeout_secs must be > 0".into());
    }
    if cctp.receipt_poll_ms == 0 {
        issues.push("services.cctp_bridge.receipt_poll_ms must be > 0".into());
    }
    if cctp.receipt_poll_ms > cctp.receipt_timeout_secs.saturating_mul(1_000) {
        issues.push(
            "services.cctp_bridge.receipt_poll_ms must not exceed receipt_timeout_secs".into(),
        );
    }
    if cctp.max_send_attempts == 0 {
        issues.push("services.cctp_bridge.max_send_attempts must be > 0".into());
    }
    if cctp.status_log_interval_ms == 0 {
        issues.push("services.cctp_bridge.status_log_interval_ms must be > 0".into());
    }
}

/// A section's path must be set: one rule, four sites (the message names the
/// section's field).
fn require_path(path: &std::path::Path, field: &str, issues: &mut Vec<String>) {
    if path.as_os_str().is_empty() {
        issues.push(format!("{field} must not be empty"));
    }
}

/// Expand one optional `@name` reference to the group's primary HTTP URL.
fn expand_optional_http(
    key: &str,
    value: &mut Option<String>,
    groups: &BTreeMap<String, EndpointGroup>,
    issues: &mut Vec<String>,
) {
    if let Some(url) = value {
        expand_http(key, url, groups, issues);
    }
}

/// Expand one `@name` reference to the group's primary HTTP URL.
fn expand_http(
    key: &str,
    value: &mut String,
    groups: &BTreeMap<String, EndpointGroup>,
    issues: &mut Vec<String>,
) {
    let Some(name) = value.strip_prefix('@') else {
        // A singular field (a write/control target) cannot be a comma list:
        // silently taking the first entry would look like failover but is not.
        if value.contains(',') {
            issues.push(format!(
                "{key}: a single endpoint is required, but the value is a comma-separated list"
            ));
        }
        return;
    };
    let Some(group) = groups.get(name) else {
        issues.push(format!("{key}: unknown endpoint group @{name}"));
        return;
    };
    let mut urls = Vec::new();
    for url in &group.http {
        for part in url.split(',') {
            push_url(&mut urls, part);
        }
    }
    match urls.as_slice() {
        [url] => *value = url.clone(),
        [] => issues.push(format!(
            "{key}: endpoint group @{name} has no http endpoints"
        )),
        _ => issues.push(format!(
            "{key}: endpoint group @{name} resolves to {} endpoints; a single endpoint is required",
            urls.len()
        )),
    }
}

/// Append one URL unless it is already present (the list's dedup rule).
fn push_url(out: &mut Vec<String>, value: &str) {
    let value = value.trim().trim_end_matches('/');
    if !value.is_empty() && !out.iter().any(|known| known == value) {
        out.push(value.to_string());
    }
}

/// Append a group's HTTP entries (each may itself be comma-separated).
fn append_group_http(out: &mut Vec<String>, group: &EndpointGroup) {
    for url in &group.http {
        for part in url.split(',') {
            push_url(out, part);
        }
    }
}

/// Expand `@name` entries in a list to the group's HTTP URLs.
fn expand_http_list(
    key: &str,
    values: &mut Vec<String>,
    groups: &BTreeMap<String, EndpointGroup>,
    issues: &mut Vec<String>,
) {
    let mut out = Vec::new();
    for value in values.drain(..) {
        match value.strip_prefix('@') {
            // The one-env-var form is a comma-separated list for every URL
            // field, not only for the `Endpoints` newtype.
            None => {
                for part in value.split(',') {
                    push_url(&mut out, part);
                }
            }
            Some(name) => match groups.get(name) {
                Some(group) if !group.http.is_empty() => append_group_http(&mut out, group),
                _ => issues.push(format!("{key}: unknown or empty endpoint group @{name}")),
            },
        }
    }
    *values = out;
}

fn check_http_url(key: &str, value: &str, issues: &mut Vec<String>) {
    check_url(key, value, [HTTP_SCHEME, HTTPS_SCHEME], "http(s)", issues);
}

/// A list check: every entry is a concrete HTTP URL (or an unresolved
/// `${VAR}`, which `require_resolved` reports at the owning service).
fn check_http_urls(key: &str, values: &Endpoints, issues: &mut Vec<String>) {
    for (index, value) in values.iter().enumerate() {
        check_url(
            &format!("{key}[{index}]"),
            value,
            [HTTP_SCHEME, HTTPS_SCHEME],
            "http(s)",
            issues,
        );
    }
}

fn check_ws_urls(key: &str, values: &WsEndpoints, issues: &mut Vec<String>) {
    for (index, value) in values.iter().enumerate() {
        check_url(
            &format!("{key}[{index}]"),
            value,
            [crate::endpoints::WS_SCHEME, crate::endpoints::WSS_SCHEME],
            "ws(s)",
            issues,
        );
    }
}

/// One URL check: the value carries one of `schemes` and a host — or it is a
/// deploy-time `${VAR}` reference, validated after expansion (`inspect`
/// reports the unresolved names separately).
fn check_url(key: &str, value: &str, schemes: [&str; 2], what: &str, issues: &mut Vec<String>) {
    if value.contains("${") {
        return;
    }
    if value.contains(',') {
        issues.push(format!(
            "{key} \"{value}\" is a comma-separated list; a single {what} URL is required here"
        ));
        return;
    }
    let host = schemes.iter().find_map(|s| value.strip_prefix(s));
    let ok = matches!(host, Some(h) if !h.is_empty() && !h.starts_with('/') && !h.contains(char::is_whitespace));
    if !ok {
        issues.push(format!("{key} \"{value}\" is not a {what} URL with a host"));
    }
}

/// A 64+ character hex blob is secret material, not a path.
fn looks_like_secret(value: &str) -> bool {
    let hex = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    hex.len() >= 64 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// The anvil shape: the profile's target endpoint, a parseable http URL. One
/// field for both profiles (the endpoint is a deployment fact).
fn check_engine_anvil(anvil: &EngineAnvil, issues: &mut Vec<String>) {
    match &anvil.target_url {
        Some(url) => check_http_url("services.engine.anvil.target_url", url, issues),
        None => issues.push("the engine requires [services.engine.anvil].target_url".into()),
    }
}

/// The engine's section: the anvil shape, the beacon list, the source-sync
/// boundary, the state dirs, and the feed bridge.
fn check_engine(eng: &EngineSection, issues: &mut Vec<String>) {
    check_engine_anvil(&eng.anvil, issues);
    if eng.loops.poll_interval_ms == 0 {
        issues.push("services.engine.loops.poll_interval_ms must be > 0".into());
    }
    if eng.loops.chunk_size == 0 {
        issues.push("services.engine.loops.chunk_size must be > 0".into());
    }
    if eng.loops.status_log_interval_ms == 0 {
        issues.push("services.engine.loops.status_log_interval_ms must be > 0".into());
    }
    if eng.source_sync.interval_ms == 0 {
        issues.push("services.engine.source_sync.interval_ms must be > 0".into());
    }
    if eng.source_sync.max_per_pass == 0 {
        issues.push("services.engine.source_sync.max_per_pass must be > 0".into());
    }
    if eng.source_sync.error_backoff_ms == 0 {
        issues.push("services.engine.source_sync.error_backoff_ms must be > 0".into());
    }
    if eng.source_sync.report_interval_ms == 0 {
        issues.push("services.engine.source_sync.report_interval_ms must be > 0".into());
    }
    if eng.source_sync.request_timeout_secs == 0 {
        issues.push("services.engine.source_sync.request_timeout_secs must be > 0".into());
    }
    if eng.beacon.request_timeout_secs == 0 {
        issues.push("services.engine.beacon.request_timeout_secs must be > 0".into());
    }
    for (i, url) in eng.beacon.urls.iter().enumerate() {
        check_http_url(&format!("services.engine.beacon.urls[{i}]"), url, issues);
    }
    if let Some(url) = &eng.source_sync.engine_url {
        check_http_url("services.engine.source_sync.engine_url", url, issues);
    }
    if let Some(path) = &eng.source_sync.jwt_path {
        if looks_like_secret(&path.to_string_lossy()) {
            issues.push(
                "services.engine.source_sync.jwt_path must be a file path, \
                 not the secret itself"
                    .into(),
            );
        }
    }
    require_path(&eng.state_dir, "services.engine.state_dir", issues);
    require_path(&eng.journal.dir, "services.engine.journal.dir", issues);
    if let Some(fb) = &eng.feed_bridge {
        check_feed_bridge(fb, issues);
    }
}

/// One feed bridge: cadence coherence, at least one feed, unique names.
fn check_feed_bridge(fb: &FeedBridgeSection, issues: &mut Vec<String>) {
    if fb.poll_interval_ms == 0 {
        issues.push("services.engine.feed_bridge.poll_interval_ms must be > 0".into());
    }
    if fb.confirm_window_ms < fb.poll_interval_ms {
        issues.push(
            "services.engine.feed_bridge.confirm_window_ms must be >= poll_interval_ms".into(),
        );
    }
    if fb.feeds.is_empty() {
        issues.push("services.engine.feed_bridge.feeds must not be empty".into());
    }
    let mut names = HashSet::new();
    for (i, feed) in fb.feeds.iter().enumerate() {
        if feed.name.is_empty() {
            issues.push(format!(
                "services.engine.feed_bridge.feeds[{i}].name must not be empty"
            ));
        }
        if !names.insert(feed.name.clone()) {
            issues.push(format!(
                "services.engine.feed_bridge.feeds[{i}].name `{}` is duplicated",
                feed.name
            ));
        }
    }
}

/// The simulator's section: the state dir, at least one market, the cadence
/// and the optional chain URLs.
fn check_venue_sim(sim: &VenueSimSection, issues: &mut Vec<String>) {
    require_path(&sim.state_dir, "services.venue_sim.state_dir", issues);
    if sim.markets.is_empty() {
        issues.push("services.venue_sim.markets must name at least one market".into());
    }
    if sim.poll_interval_ms == 0 {
        issues.push("services.venue_sim.poll_interval_ms must be > 0".into());
    }
    if sim.tick_interval_ms == 0 {
        issues.push("services.venue_sim.tick_interval_ms must be > 0".into());
    }
    if sim.heartbeat_interval_secs == 0 {
        issues.push("services.venue_sim.heartbeat_interval_secs must be > 0".into());
    }
    if sim.rest_timeout_secs == 0 {
        issues.push("services.venue_sim.rest_timeout_secs must be > 0".into());
    }
    if sim.rpc_timeout_secs == 0 {
        issues.push("services.venue_sim.rpc_timeout_secs must be > 0".into());
    }
    if sim.admin_timeout_secs == 0 {
        issues.push("services.venue_sim.admin_timeout_secs must be > 0".into());
    }
    if let Some(url) = &sim.engine_admin_url {
        check_http_url("services.venue_sim.engine_admin_url", url, issues);
    }
    if let Some(url) = &sim.anvil_url {
        check_http_url("services.venue_sim.anvil_url", url, issues);
    }
}

#[cfg(test)]
mod tests {
    use super::FollowHead;

    /// The follow-head tags are the config's own spelling: the mainnet
    /// engine's `follow_tag()` delegates to this mapping (one home; the test
    /// moved here from the engine's ctx, where it pinned nothing of its own).
    #[test]
    fn the_follow_head_tags_are_the_configs_spelling() {
        assert_eq!(FollowHead::Safe.tag(), "safe");
        assert_eq!(FollowHead::Latest.tag(), "latest");
        assert_eq!(FollowHead::Finalized.tag(), "finalized");
    }
}
