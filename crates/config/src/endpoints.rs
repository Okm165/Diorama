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
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EndpointGroup {
    #[serde(default)]
    pub http: Vec<String>,
    #[serde(default)]
    pub ws: Vec<String>,
    /// Capabilities this group's endpoints declare (`archive`, `trace`,
    /// `bal`, `market_ws`, …). Empty means "no restriction": the endpoints
    /// serve unqualified calls but never claim a role they did not declare.
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
            if group.http.is_empty() {
                return Err(format!("endpoint group @{name} has no http endpoints"));
            }
            for value in &group.http {
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
            if group.ws.is_empty() {
                return Err(format!("endpoint group @{name} has no ws endpoints"));
            }
            for value in &group.ws {
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
}
