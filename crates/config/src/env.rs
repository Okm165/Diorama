//! Deploy-time wiring (`${VAR}`) and the `MIRROR__*` override layer (§19.4).
use crate::ConfigError;

/// Expand `${VAR}` references in every string value. Returns the names that
/// could not be resolved; the raw reference is kept in place (the service
/// entry [`crate::Instance::load`] turns them into a boot error; the tooling
/// entry reports them).
pub(crate) fn expand(value: &mut toml::Value, vars: &[(String, String)]) -> Vec<String> {
    let mut unresolved = Vec::new();
    expand_in(value, vars, &mut unresolved);
    // One report per variable, in first-appearance order.
    let mut seen = std::collections::HashSet::new();
    unresolved.retain(|name| seen.insert(name.clone()));
    unresolved
}

fn expand_in(value: &mut toml::Value, vars: &[(String, String)], unresolved: &mut Vec<String>) {
    match value {
        toml::Value::String(s) => {
            if s.contains("${") {
                *s = expand_str(s, vars, unresolved);
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                expand_in(item, vars, unresolved);
            }
        }
        toml::Value::Table(map) => {
            for (_, item) in map.iter_mut() {
                expand_in(item, vars, unresolved);
            }
        }
        _ => {}
    }
}

fn expand_str(s: &str, vars: &[(String, String)], unresolved: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) if !after[..end].is_empty() => {
                let name = &after[..end];
                if let Some((_, value)) = vars.iter().find(|(key, _)| key == name) {
                    out.push_str(value);
                } else {
                    out.push_str(&rest[start..start + 2 + end + 1]);
                    unresolved.push(name.to_string());
                }
                rest = &after[end + 1..];
            }
            Some(end) => {
                // `${}`: not a reference; keep it literal.
                out.push_str(&rest[start..start + 2 + end + 1]);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(rest);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// `MIRROR__<PATH__SEGMENTS>=<value>`: operator debug overrides applied after
/// the merge. The path must already exist (typo protection); every applied
/// override is logged loudly at boot.
pub(crate) fn apply_overrides(
    value: &mut toml::Value,
    vars: &[(String, String)],
) -> Result<(), ConfigError> {
    for (key, raw) in vars {
        let Some(path) = key.strip_prefix("MIRROR__") else {
            continue;
        };
        let segments: Vec<String> = path.split("__").map(str::to_ascii_lowercase).collect();
        set_path(value, &segments, parse_scalar(raw)).map_err(|reason| ConfigError::Invalid {
            issues: vec![format!("override {key}: {reason}")],
        })?;
        tracing::warn!(
            override_ = %format!("{key} -> {}", segments.join(".")),
            "MIRROR__ override in effect"
        );
    }
    Ok(())
}

fn set_path(value: &mut toml::Value, segments: &[String], new: toml::Value) -> Result<(), String> {
    let (last, path) = segments.split_last().ok_or("empty override path")?;
    let mut cursor = value;
    for segment in path {
        cursor = cursor
            .get_mut(segment)
            .ok_or_else(|| format!("no such key `{}`", segments.join(".")))?;
    }
    let slot = cursor
        .get_mut(last)
        .ok_or_else(|| format!("no such key `{}`", segments.join(".")))?;
    *slot = new;
    Ok(())
}

/// Env values arrive as text; the common scalars are typed so a numeric
/// override lands as a number (the typed instance would reject "2000" as a
/// string for a u64 field).
fn parse_scalar(raw: &str) -> toml::Value {
    match raw {
        "true" => toml::Value::Boolean(true),
        "false" => toml::Value::Boolean(false),
        _ => {
            if let Ok(i) = raw.parse::<i64>() {
                toml::Value::Integer(i)
            } else if let Ok(f) = raw.parse::<f64>() {
                toml::Value::Float(f)
            } else {
                toml::Value::String(raw.to_string())
            }
        }
    }
}
