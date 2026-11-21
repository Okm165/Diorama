//! One or more endpoints, in preference order — HTTP and WS spellings.
//!
//! The config grammar accepts all three shapes a deployment naturally has:
//!
//! ```toml
//! source_urls = "${RETH_URL}"          # one endpoint
//! source_urls = "${HL_RPC_URLS}"       # one env var, comma-separated
//! source_urls = ["${HL_RPC_1}", "${HL_RPC_2}"]  # an explicit list
//! source_urls = "@hyperevm"            # a named group from [endpoints]
//! ```
//!
//! The value is parsed once, at the config boundary: empty entries are
//! dropped, duplicates are removed in first-seen order, and every entry must
//! be an `http(s)://` URL (or `ws(s)://` for [`WsEndpoints`]). A malformed
//! list is a load error naming the endpoint, never a runtime surprise in the
//! engine. `@name` is a reference to `[endpoints.<name>]`; the loader expands
//! it before validation, so one URL list has one home.
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The two schemes an HTTP endpoint may carry. One spelling, shared with the
/// instance validator so a new scheme cannot be admitted in one place and
/// refused in another.
pub(crate) const HTTP_SCHEME: &str = "http://";
pub(crate) const HTTPS_SCHEME: &str = "https://";
pub(crate) const WS_SCHEME: &str = "ws://";
pub(crate) const WSS_SCHEME: &str = "wss://";

/// Whether `value` starts with an accepted HTTP RPC scheme.
pub(crate) fn is_http_url(value: &str) -> bool {
    value.starts_with(HTTP_SCHEME) || value.starts_with(HTTPS_SCHEME)
}

/// Whether `value` starts with an accepted WS scheme.
pub(crate) fn is_ws_url(value: &str) -> bool {
    value.starts_with(WS_SCHEME) || value.starts_with(WSS_SCHEME)
}

/// Whether `value` is a named-group reference (`@hyperevm`).
fn is_ref(value: &str) -> bool {
    value.len() > 1 && value.starts_with('@')
}

/// The canonical capability vocabulary — the one place where the differences
/// between providers and endpoints are named.
///
/// Config files, the pool's endpoint specs and the boot capability audit all
/// speak this language. The names match `diorama_devnet::source::Capability`
/// one-to-one (which maps from these), plus the two market-plane roles and
/// `batch` — the three provider differences that measurement keeps surfacing
/// (see `scripts/probe-endpoint.sh`, the acceptance test that fills these in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Serves state at historical blocks (pinned `eth_call` past the fork age).
    /// The legacy config spelling `archive` aliases into this.
    #[serde(alias = "archive")]
    Historical,
    /// `debug_*`/`trace_*` namespace.
    Traces,
    /// `eth_getBlockAccessList` (EIP-7928).
    Bal,
    /// `eth_getSystemTxsByBlockNumber` — the Diorama follow pool's gate.
    SystemTxs,
    /// Receipts for system transactions.
    SystemTxReceipts,
    /// Accepts JSON-RPC **batch** bodies. Re-measured 2026-11-17: ZAN
    /// accepts batch bodies too (the earlier "ZAN-class providers refuse
    /// them" no longer holds); what still separates providers is pinned
    /// reads — ZAN serves `latest`-block reads only, so the pinned-deployment
    /// fork source stays off the read pool.
    Batch,
    /// Hyperliquid `/info` REST plane.
    MarketRest,
    /// Hyperliquid subscription-protocol WebSocket plane.
    MarketWs,
}

impl Capability {
    /// The canonical snake_case name (also the config spelling).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Historical => "historical",
            Self::Traces => "traces",
            Self::Bal => "bal",
            Self::SystemTxs => "system_txs",
            Self::SystemTxReceipts => "system_tx_receipts",
            Self::Batch => "batch",
            Self::MarketRest => "market_rest",
            Self::MarketWs => "market_ws",
        }
    }

    /// Parse a config spelling. Accepts the historical aliases (`archive`,
    /// `trace`) the older flat `roles` lists used.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "archive" | "historical" => Self::Historical,
            "trace" | "traces" => Self::Traces,
            "bal" => Self::Bal,
            "system_txs" => Self::SystemTxs,
            "system_tx_receipts" => Self::SystemTxReceipts,
            "batch" => Self::Batch,
            "market_rest" => Self::MarketRest,
            "market_ws" => Self::MarketWs,
            _ => return None,
        })
    }
}

/// One declared endpoint inside a group — the nested, per-URL form.
///
/// ```toml
/// [[endpoints.hyperevm.entries]]
/// url = "https://rpc.hyperliquid.xyz/evm"
/// capabilities = ["system_txs", "batch"]
/// max_log_range = 900
/// ```
///
/// `url` selects the scheme (http(s) for the JSON-RPC plane, ws(s) for the
/// WebSocket plane); the plane the group is consumed from decides which list
/// the entry lands in, so one group can carry an http/ws pair per provider.
/// Every field except `url` is the provider's difference, surfaced: a missing
/// capability is a *refusal to claim*, not an oversight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointEntry {
    pub url: String,
    /// The WebSocket spelling for ws-plane groups. A paired entry may carry
    /// both (`url` + `ws`) — the provider's http/ws pair in one declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws: Option<String>,
    /// What this endpoint is *measured* to serve. Empty claims nothing: the
    /// endpoint serves unqualified calls but is filtered out of any
    /// capability-gated admission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Relative preference in pool admission (default 1).
    #[serde(default = "one")]
    pub weight: u32,
    /// The widest `eth_getLogs` range this endpoint accepts (default: the
    /// client's 10,000-block conservative cap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_log_range: Option<u64>,
    /// The chain id this endpoint must answer (default: the group's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,
}

/// A named endpoint group: the single home of a deployment's URL lists.
///
/// ```toml
/// [endpoints.hyperevm]
/// http = ["${HL_RPC_1}", "${HL_RPC_2}"]
/// ws = ["wss://rpc.example/ws"]
/// ```
///
/// A group may be referenced from any URL field as `@hyperevm`; the loader
/// replaces the reference with the group's list. A group with no `http` (or
/// no `ws`) is valid only if nothing asks for that side.
///
/// The flat `http`/`ws` lists and the nested `[[entries]]` compose: the flat
/// lists keep the env-var forms (`${HL_RPC_URLS}`) and the nested entries add
/// per-endpoint capabilities, weights and log-range caps. Entry URLs are
/// appended after the flat lists (flat = deployment preference, entries =
/// provider detail).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EndpointGroup {
    #[serde(default)]
    pub http: Vec<String>,
    #[serde(default)]
    pub ws: Vec<String>,
    /// Group-level capabilities, applied to every flat-list URL. Nested
    /// entries declare their own.
    #[serde(default)]
    pub roles: Vec<String>,
    /// The chain id every endpoint in this group must answer. `None` leaves
    /// the assert to the consumer (the engine's boot pins its own chain).
    #[serde(default)]
    pub chain_id: Option<u64>,
    /// Relative preference in pool admission (default 1).
    #[serde(default = "one")]
    pub weight: u32,
    /// The widest `eth_getLogs` block range these endpoints accept in one
    /// call. Absent keeps the client's conservative 10,000-block default; a
    /// public provider with a smaller cap pins it here (the public HyperEVM
    /// source uses 900).
    #[serde(default)]
    pub max_log_range: Option<u64>,
    /// The nested per-endpoint declarations (capabilities, weights, log
    /// caps, http/ws pairs). Flat lists and entries compose.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<EndpointEntry>,
}

impl EndpointGroup {
    /// Every HTTP URL of the group: the flat list first (the deployment's
    /// preference order), then the nested entries' `url`s.
    pub fn http_all(&self) -> Vec<String> {
        let mut out = self.http.clone();
        for entry in &self.entries {
            if !out.contains(&entry.url) {
                out.push(entry.url.clone());
            }
        }
        out
    }

    /// Every WebSocket URL of the group: flat list, then entries' `ws`
    /// spellings (an http/ws pair entry contributes its `ws` here).
    pub fn ws_all(&self) -> Vec<String> {
        let mut out = self.ws.clone();
        for entry in &self.entries {
            if let Some(ws) = &entry.ws {
                if !out.contains(ws) {
                    out.push(ws.clone());
                }
            }
        }
        out
    }

    /// The capabilities a URL claims: its nested entry's list when the URL is
    /// an entry, else the group-level `roles`. Every name is parsed through
    /// the typed vocabulary — a typo in config is a load error here, never a
    /// silently unclaimed capability.
    pub fn capabilities_for(&self, url: &str) -> Result<Vec<Capability>, String> {
        let names: &[String] = match self
            .entries
            .iter()
            .find(|e| e.url == url || Some(&e.ws) == Some(&Some(url.to_owned())))
        {
            Some(entry) => &entry.capabilities,
            None => &self.roles,
        };
        names
            .iter()
            .map(|n| {
                Capability::parse(n)
                    .ok_or_else(|| format!("unknown capability {n:?} on endpoint {url:?}"))
            })
            .collect()
    }

    /// The effective per-URL attribute overrides (weight, log cap, chain)
    /// for a URL that is a nested entry; `None` when the URL rides the
    /// group-level attributes.
    pub fn entry_attrs(&self, url: &str) -> Option<&EndpointEntry> {
        self.entries
            .iter()
            .find(|e| e.url == url || Some(&e.ws) == Some(&Some(url.to_owned())))
    }
}

const fn one() -> u32 {
    1
}

/// The shared parser for both spellings: split comma-separated values, trim,
/// keep first-seen order, and validate the scheme. `@name` is kept verbatim
/// for the loader's expansion.
fn parse_values(
    values: impl IntoIterator<Item = String>,
    allowed: &[&str],
    label: &str,
) -> Result<Vec<String>, String> {
    let mut endpoints = Vec::new();
    for value in values {
        for part in value.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if !part.contains("${")
                && !is_ref(part)
                && !allowed.iter().any(|scheme| part.starts_with(scheme))
            {
                return Err(format!("endpoint {part:?} is not a {label} URL"));
            }
            if !endpoints.iter().any(|known| known == part) {
                endpoints.push(part.to_string());
            }
        }
    }
    if endpoints.is_empty() {
        return Err("at least one endpoint is required".into());
    }
    Ok(endpoints)
}

/// Append a value unless it is already present, trimming a trailing slash so
/// `http://a:8545/` and `http://a:8545` are one endpoint.
fn push_unique(out: &mut Vec<String>, value: &str) {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() || out.iter().any(|known| known == value) {
        return;
    }
    out.push(value.to_string());
}

/// A non-empty, validated, ordered set of HTTP(S) JSON-RPC endpoints.
///
/// `primary()` is the endpoint the deployment declares first: the one the
/// fork source and `/admin/epoch` use. The engine's read client fans out over
/// the whole set (see `diorama_anvil::EndpointPool`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Endpoints(Vec<String>);

impl Endpoints {
    /// Parse a list of raw values. Each value may itself be a comma-separated
    /// list (the one-env-var form) or a `@name` group reference. The order is
    /// the deployment's preference order; duplicates are removed.
    pub fn parse(values: impl IntoIterator<Item = String>) -> Result<Self, String> {
        parse_values(values, &[HTTP_SCHEME, HTTPS_SCHEME], "http(s)").map(Self)
    }

    /// Validate every endpoint after `${VAR}` expansion and `@name` group
    /// expansion: no reference may survive, and each entry must be an
    /// `http(s)://` URL. This is the consumer's boundary (the loader
    /// deliberately keeps unresolved references so a pod that does not own a
    /// section is not failed by it).
    pub fn validate(&self) -> Result<(), String> {
        for endpoint in &self.0 {
            if endpoint.contains("${") {
                return Err(format!("endpoint {endpoint:?} is an unresolved reference"));
            }
            if is_ref(endpoint) {
                return Err(format!(
                    "endpoint {endpoint:?} is an unexpanded group reference"
                ));
            }
            if !is_http_url(endpoint) {
                return Err(format!("endpoint {endpoint:?} is not an http(s) URL"));
            }
        }
        Ok(())
    }

    /// Replace `@name` entries with the group's `http` list. The loader calls
    /// this once, after `${VAR}` expansion and before validation.
    pub fn expand(&mut self, groups: &BTreeMap<String, EndpointGroup>) -> Result<(), String> {
        let mut out = Vec::new();
        for entry in std::mem::take(&mut self.0) {
            let Some(name) = entry.strip_prefix('@') else {
                push_unique(&mut out, &entry);
                continue;
            };
            let group = groups
                .get(name)
                .ok_or_else(|| format!("unknown endpoint group @{name}"))?;
            if group.http.is_empty() && group.entries.iter().all(|e| e.url.is_empty()) {
                return Err(format!("endpoint group @{name} has no http endpoints"));
            }
            for value in group.http_all() {
                for part in value.split(',') {
                    push_unique(&mut out, part);
                }
            }
        }
        if out.is_empty() {
            return Err("at least one endpoint is required".into());
        }
        self.0 = out;
        Ok(())
    }

    /// The declared primary endpoint (the first one).
    pub fn primary(&self) -> &str {
        &self.0[0]
    }

    /// Every endpoint, in preference order.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Iterate the endpoints in preference order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// The number of endpoints (always at least one).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set is empty. Always `false` after [`Self::parse`]; kept
    /// for the standard collection vocabulary.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de> Deserialize<'de> for Endpoints {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }

        let raw = OneOrMany::deserialize(deserializer)?;
        let values = match raw {
            OneOrMany::One(value) => vec![value],
            OneOrMany::Many(values) => values,
        };
        Self::parse(values).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Endpoints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(","))
    }
}

impl From<Endpoints> for Vec<String> {
    fn from(endpoints: Endpoints) -> Self {
        endpoints.0
    }
}

/// The websocket spelling of [`Endpoints`]: one or more `ws(s)://` URLs, in
/// preference order. The venue's market feed rotates over them on reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WsEndpoints(Vec<String>);

impl WsEndpoints {
    pub fn parse(values: impl IntoIterator<Item = String>) -> Result<Self, String> {
        parse_values(values, &[WS_SCHEME, WSS_SCHEME], "ws(s)").map(Self)
    }

    pub fn validate(&self) -> Result<(), String> {
        for endpoint in &self.0 {
            if endpoint.contains("${") {
                return Err(format!("endpoint {endpoint:?} is an unresolved reference"));
            }
            if is_ref(endpoint) {
                return Err(format!(
                    "endpoint {endpoint:?} is an unexpanded group reference"
                ));
            }
            if !is_ws_url(endpoint) {
                return Err(format!("endpoint {endpoint:?} is not a ws(s) URL"));
            }
        }
        Ok(())
    }

    /// Replace `@name` entries with the group's `ws` list.
    pub fn expand(&mut self, groups: &BTreeMap<String, EndpointGroup>) -> Result<(), String> {
        let mut out = Vec::new();
        for entry in std::mem::take(&mut self.0) {
            let Some(name) = entry.strip_prefix('@') else {
                push_unique(&mut out, &entry);
                continue;
            };
            let group = groups
                .get(name)
                .ok_or_else(|| format!("unknown endpoint group @{name}"))?;
            if group.ws.is_empty() && !group.entries.iter().any(|e| e.ws.is_some()) {
                return Err(format!("endpoint group @{name} has no ws endpoints"));
            }
            for value in group.ws_all() {
                for part in value.split(',') {
                    push_unique(&mut out, part);
                }
            }
        }
        if out.is_empty() {
            return Err("at least one websocket endpoint is required".into());
        }
        self.0 = out;
        Ok(())
    }

    pub fn primary(&self) -> &str {
        &self.0[0]
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de> Deserialize<'de> for WsEndpoints {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }

        let raw = OneOrMany::deserialize(deserializer)?;
        let values = match raw {
            OneOrMany::One(value) => vec![value],
            OneOrMany::Many(values) => values,
        };
        Self::parse(values).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for WsEndpoints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(value: &str) -> Endpoints {
        Endpoints::parse([value.to_string()]).expect("one endpoint")
    }

    #[test]
    fn a_single_string_is_one_endpoint() {
        assert_eq!(one("http://reth:8545").as_slice(), ["http://reth:8545"]);
    }

    #[test]
    fn a_comma_separated_string_is_a_list() {
        let endpoints = one("https://a.example/evm, https://b.example/evm ,https://c.example/evm");
        assert_eq!(
            endpoints.as_slice(),
            [
                "https://a.example/evm",
                "https://b.example/evm",
                "https://c.example/evm"
            ]
        );
        assert_eq!(endpoints.primary(), "https://a.example/evm");
    }

    #[test]
    fn a_list_may_also_carry_comma_separated_entries() {
        let endpoints = Endpoints::parse([
            "https://a.example/evm,https://b.example/evm".to_string(),
            "https://c.example/evm".to_string(),
        ])
        .expect("a mixed list");
        assert_eq!(endpoints.len(), 3);
    }

    #[test]
    fn duplicates_are_removed_in_first_seen_order() {
        let endpoints = Endpoints::parse([
            "https://b.example/evm".to_string(),
            "https://a.example/evm".to_string(),
            "https://b.example/evm".to_string(),
        ])
        .expect("a deduplicated list");
        assert_eq!(
            endpoints.as_slice(),
            ["https://b.example/evm", "https://a.example/evm"]
        );
    }

    #[test]
    fn an_unresolved_reference_is_kept_for_the_owning_service() {
        let endpoints = one("${HL_RPC_URL}");
        assert_eq!(endpoints.primary(), "${HL_RPC_URL}");
        assert!(
            endpoints.validate().is_err(),
            "the reference must be resolved before use"
        );
    }

    #[test]
    fn a_non_http_endpoint_is_refused_by_name() {
        let error =
            Endpoints::parse(["wss://a.example/ws".to_string()]).expect_err("not an http url");
        assert!(error.contains("wss://a.example/ws"), "{error}");
    }

    #[test]
    fn an_empty_set_is_refused() {
        assert!(Endpoints::parse(["".to_string(), " , ".to_string()]).is_err());
    }

    #[test]
    fn deserializes_from_a_string_or_a_sequence() {
        let one: Endpoints = toml::from_str("url = \"http://a:1\"")
            .map(|t: Table| t.url)
            .expect("one");
        assert_eq!(one.as_slice(), ["http://a:1"]);
        let many: Endpoints = toml::from_str("url = [\"http://a:1\", \"http://b:2\"]")
            .map(|t: Table| t.url)
            .expect("many");
        assert_eq!(many.as_slice(), ["http://a:1", "http://b:2"]);
    }

    #[test]
    fn a_group_reference_expands_to_the_group_list() {
        let mut groups = BTreeMap::new();
        groups.insert(
            "hyperevm".to_string(),
            EndpointGroup {
                http: vec!["http://a:8545".into(), "http://b:8545".into()],
                ws: vec!["wss://a/ws".into()],
                roles: Vec::new(),
                chain_id: None,
                weight: 1,
                max_log_range: None,
                entries: Vec::new(),
            },
        );
        let mut endpoints = one("@hyperevm");
        endpoints.expand(&groups).expect("expands");
        assert_eq!(endpoints.as_slice(), ["http://a:8545", "http://b:8545"]);
        endpoints.validate().expect("resolved");
    }

    #[test]
    fn an_unknown_group_is_refused_by_name() {
        let mut endpoints = one("@missing");
        let error = endpoints.expand(&BTreeMap::new()).expect_err("unknown");
        assert!(error.contains("@missing"), "{error}");
    }

    #[test]
    fn the_ws_spelling_accepts_ws_and_refuses_http() {
        let ws = WsEndpoints::parse(["wss://a/ws".to_string()]).expect("ws");
        assert_eq!(ws.primary(), "wss://a/ws");
        assert!(WsEndpoints::parse(["https://a".to_string()]).is_err());
    }

    #[derive(Deserialize)]
    struct Table {
        url: Endpoints,
    }

    #[test]
    fn nested_entries_compose_with_flat_lists_and_carry_capabilities() {
        let toml = r#"
            http = ["https://flat.example/evm", "${FLAT_VAR}"]
            roles = ["system_txs"]
            chain_id = 999
            max_log_range = 900

            [[entries]]
            url = "https://entry.example/evm"
            ws = "wss://entry.example/ws"
            capabilities = ["system_txs", "batch", "historical"]
            weight = 2
            max_log_range = 10_000

            [[entries]]
            url = "https://second.example/evm"
        "#;
        let group: EndpointGroup = toml::from_str(toml).expect("group parses");
        assert_eq!(group.weight, 1, "group default");
        // Composition: flat first (preference), then entries, dedup'd.
        let http = group.http_all();
        assert_eq!(http[0], "https://flat.example/evm");
        assert!(http.contains(&"${FLAT_VAR}".to_string()));
        assert!(http.contains(&"https://entry.example/evm".to_string()));
        assert!(http.contains(&"https://second.example/evm".to_string()));
        // The ws pair rides the same entry.
        assert!(group
            .ws_all()
            .contains(&"wss://entry.example/ws".to_string()));
        // Entry capabilities are typed and complete; flat urls ride roles.
        let entry_caps = group.capabilities_for("https://entry.example/evm").unwrap();
        assert_eq!(
            entry_caps,
            vec![
                Capability::SystemTxs,
                Capability::Batch,
                Capability::Historical
            ]
        );
        let flat_caps = group.capabilities_for("https://flat.example/evm").unwrap();
        assert_eq!(flat_caps, vec![Capability::SystemTxs]);
        // Per-entry overrides beat the group attributes.
        let entry = group.entry_attrs("https://entry.example/evm").unwrap();
        assert_eq!(entry.weight, 2);
        assert_eq!(entry.max_log_range, Some(10_000));
        assert!(group.entry_attrs("https://second.example/evm").is_some());
        assert!(group.entry_attrs("https://flat.example/evm").is_none());
    }

    #[test]
    fn capability_vocabulary_parses_and_aliases_the_legacy_spellings() {
        assert_eq!(Capability::parse("system_txs"), Some(Capability::SystemTxs));
        assert_eq!(Capability::parse("batch"), Some(Capability::Batch));
        // The legacy flat-roles spellings alias into the typed vocabulary.
        assert_eq!(Capability::parse("archive"), Some(Capability::Historical));
        assert_eq!(Capability::parse("trace"), Some(Capability::Traces));
        assert_eq!(Capability::parse("bal"), Some(Capability::Bal));
        // Round-trip: the canonical spelling is stable.
        for cap in [
            Capability::SystemTxs,
            Capability::MarketRest,
            Capability::Batch,
        ] {
            assert_eq!(Capability::parse(cap.as_str()), Some(cap));
        }
        assert_eq!(Capability::parse("nope"), None);
    }

    #[test]
    fn unknown_capability_names_are_a_parse_error_not_a_silent_noop() {
        let toml = r#"
            http = ["https://a.example/evm"]
            [[entries]]
            url = "https://a.example/evm"
            capabilities = ["system_txs", "batchh"]
        "#;
        let group: EndpointGroup = toml::from_str(toml).expect("group parses");
        let error = group.capabilities_for("https://a.example/evm").unwrap_err();
        assert!(error.contains("batchh"), "error names the typo: {error}");
    }

    #[test]
    fn a_group_of_pure_entries_expands_and_a_missing_plane_is_refused() {
        let group: EndpointGroup = toml::from_str(
            r#"
            [[entries]]
            url = "https://only.example/evm"
            capabilities = ["batch"]
            "#,
        )
        .unwrap();
        // http expansion: entries count as the http plane.
        let mut urls = Endpoints::parse(["@only".to_string()]).unwrap();
        let mut groups = BTreeMap::new();
        groups.insert("only".to_string(), group.clone());
        urls.expand(&groups).unwrap();
        assert_eq!(urls.as_slice(), &["https://only.example/evm"]);
        // A group with entries but no ws spelling refuses the ws plane.
        let mut ws = WsEndpoints::parse(["@only".to_string()]).unwrap();
        let error = ws.expand(&groups).unwrap_err();
        assert!(error.contains("no ws endpoints"), "{error}");
    }
}
