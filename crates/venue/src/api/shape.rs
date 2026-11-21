//! The venue's request-shape gate for `/exchange`.
//!
//! The venue deserializes an action strictly before it looks at the signature:
//! an unknown field, a missing required one, a field of the wrong type, an
//! address that is not one, or a value outside a closed set is the plain-text
//! 422 `Failed to deserialize the JSON body into the target type`. These rules
//! were measured with zero-signature probes (`fixtures/real_exchange.json`),
//! and every one of them is checked here on the raw action so the typed enum
//! in `wire.rs` can stay lenient about what the sim does not model.
//!
//! Two things are *not* shape errors on the venue and so are not here: the
//! `destination` of `usdSend`, `spotSend`, `sendAsset` and `withdraw3` is a
//! free string (it is validated after the signature), and
//! `signatureChainId` is a hex quantity of any length (with or without `0x`).
use serde_json::Value;

/// How one field's value must look.
#[derive(Clone, Copy)]
enum Kind {
    Str,
    Bool,
    /// A JSON number that is an unsigned integer.
    U64,
    /// A positive unsigned integer (a leverage of 0 is refused at the parse).
    Positive,
    /// A 20-byte hex address (`0x` + 40 hex digits).
    Addr,
    /// `Mainnet` or `Testnet`.
    Chain,
    /// A hex quantity, `0x` optional, at least one digit.
    ChainId,
    /// One of the account abstraction names.
    Abstraction,
    /// The `grouping` of an order batch.
    Grouping,
    /// Anything (the typed parse checks it).
    Any,
}

/// One field: its name, kind and whether it must be present.
type Field = (&'static str, Kind, bool);

const ABSTRACTIONS: [&str; 5] = [
    "default",
    "disabled",
    "unifiedAccount",
    "portfolioMargin",
    "dexAbstraction",
];

/// The fields of a user-signed action (without the three every one carries).
fn user_fields(tag: &str) -> Option<&'static [Field]> {
    use Kind::*;
    Some(match tag {
        "usdSend" => &[
            ("destination", Str, true),
            ("amount", Str, true),
            ("time", U64, true),
        ],
        "spotSend" => &[
            ("destination", Str, true),
            ("token", Str, true),
            ("amount", Str, true),
            ("time", U64, true),
        ],
        "withdraw3" => &[
            ("destination", Str, true),
            ("amount", Str, true),
            ("time", U64, true),
        ],
        "usdClassTransfer" => &[
            ("amount", Str, true),
            ("toPerp", Bool, true),
            ("nonce", U64, true),
        ],
        "sendAsset" => &[
            ("destination", Str, true),
            ("sourceDex", Str, true),
            ("destinationDex", Str, true),
            ("token", Str, true),
            ("amount", Str, true),
            ("fromSubAccount", Str, true),
            ("nonce", U64, true),
        ],
        "approveAgent" => &[
            ("agentAddress", Addr, true),
            ("agentName", Str, false),
            ("nonce", U64, true),
        ],
        "approveBuilderFee" => &[
            ("maxFeeRate", Str, true),
            ("builder", Addr, true),
            ("nonce", U64, true),
        ],
        "userSetAbstraction" => &[
            ("user", Addr, true),
            ("abstraction", Abstraction, true),
            ("nonce", U64, true),
        ],
        _ => return None,
    })
}

/// The fields of an L1 action the sim models (the `order` objects inside an
/// `order`, `modify` or `batchModify` are not closed on the venue, so they are
/// checked only for the `tif` set below).
fn l1_fields(tag: &str) -> Option<&'static [Field]> {
    use Kind::*;
    Some(match tag {
        "noop" => &[],
        "order" => &[
            ("orders", Any, true),
            ("grouping", Grouping, false),
            ("builder", Any, false),
        ],
        "cancel" => &[("cancels", Any, true), ("f", Bool, false)],
        "cancelByCloid" => &[("cancels", Any, true), ("f", Bool, false)],
        "modify" => &[("oid", Any, true), ("order", Any, true), ("a", Bool, false)],
        "batchModify" => &[("modifies", Any, true), ("a", Bool, false)],
        "scheduleCancel" => &[("time", U64, false)],
        "updateLeverage" => &[
            ("asset", U64, true),
            ("isCross", Bool, true),
            ("leverage", Positive, true),
        ],
        "updateIsolatedMargin" => &[
            ("asset", U64, true),
            ("isBuy", Bool, true),
            ("ntli", Any, true),
        ],
        "agentSetAbstraction" => &[("abstraction", Str, true)],
        _ => return None,
    })
}

fn is_address(text: &str) -> bool {
    let digits = text.strip_prefix("0x").unwrap_or(text);
    digits.len() == 40 && digits.bytes().all(|b| b.is_ascii_hexdigit())
}

fn kind_holds(kind: Kind, value: &Value) -> bool {
    match kind {
        Kind::Str => value.is_string(),
        Kind::Bool => value.is_boolean(),
        Kind::U64 => value.is_u64(),
        Kind::Positive => value
            .as_u64()
            .is_some_and(|n| n >= 1 && n <= u64::from(u32::MAX)),
        Kind::Addr => value.as_str().is_some_and(is_address),
        Kind::Chain => matches!(value.as_str(), Some("Mainnet" | "Testnet")),
        Kind::ChainId => value.as_str().is_some_and(|s| {
            let digits = s.strip_prefix("0x").unwrap_or(s);
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit())
        }),
        Kind::Abstraction => value.as_str().is_some_and(|s| ABSTRACTIONS.contains(&s)),
        Kind::Grouping => matches!(value.as_str(), Some("na" | "normalTpsl" | "positionTpsl")),
        Kind::Any => true,
    }
}

/// The fields hold their kinds and no key is outside the list (plus `type`).
fn closed(action: &serde_json::Map<String, Value>, fields: &[Field], extra: &[&str]) -> bool {
    let known = |key: &str| {
        key == "type" || extra.contains(&key) || fields.iter().any(|(name, _, _)| *name == key)
    };
    if !action.keys().all(|k| known(k)) {
        return false;
    }
    fields.iter().all(|(name, kind, required)| {
        match action.get(*name) {
            None => !required,
            // an optional field may be an explicit null
            Some(Value::Null) if !required => true,
            Some(value) => kind_holds(*kind, value),
        }
    })
}

/// Every order object's `t` must be a `limit` with a known `tif`, or a trigger.
fn orders_hold(orders: Option<&Value>) -> bool {
    let Some(Value::Array(orders)) = orders else {
        return true;
    };
    orders.iter().all(|o| {
        o.pointer("/t/limit/tif")
            .is_none_or(|t| matches!(t.as_str(), Some("Alo" | "Ioc" | "Gtc")))
    })
}

/// Whether the action has the venue's shape (see the module docs). Tags the
/// sim does not model, and tags it does not know, pass: the typed parse and
/// the tag list decide those.
pub(crate) fn conforms(action: &Value, venue_shaped: bool) -> bool {
    let Some(map) = action.as_object() else {
        return true;
    };
    let Some(tag) = map.get("type").and_then(Value::as_str) else {
        return true;
    };
    if let Some(fields) = user_fields(tag) {
        // The keyless harness names `user` and may leave the signing fields out.
        if !venue_shaped {
            return true;
        }
        let common: &[Field] = &[
            ("hyperliquidChain", Kind::Chain, true),
            ("signatureChainId", Kind::ChainId, true),
        ];
        let all: Vec<Field> = fields.iter().chain(common.iter()).copied().collect();
        return closed(map, &all, &[]);
    }
    if let Some(fields) = l1_fields(tag) {
        let ok = closed(map, fields, &[]);
        return ok
            && match tag {
                "order" => orders_hold(map.get("orders")),
                "modify" => orders_hold(
                    map.get("order")
                        .map(|o| Value::Array(vec![o.clone()]))
                        .as_ref(),
                ),
                "batchModify" => map
                    .get("modifies")
                    .and_then(Value::as_array)
                    .is_none_or(|ms| {
                        ms.iter().all(|m| {
                            orders_hold(
                                m.get("order")
                                    .map(|o| Value::Array(vec![o.clone()]))
                                    .as_ref(),
                            )
                        })
                    }),
                _ => true,
            };
    }
    true
}

/// The refusals the venue makes before it recovers the signer: a user-signed
/// action whose own `nonce`/`time` is not the request's nonce is "Nonce
/// mismatch.", and one for the other network is "Mainnet and testnet require
/// different signature.". `None` when neither applies (or the body is not a
/// user-signed action).
pub(crate) fn pre_signature_refusal(body: &[u8]) -> Option<&'static str> {
    let request: Value = serde_json::from_slice(body).ok()?;
    let action = request.get("action")?;
    user_fields(action.get("type")?.as_str()?)?;
    let outer = request.get("nonce").and_then(Value::as_u64)?;
    let own = action
        .get("nonce")
        .or_else(|| action.get("time"))
        .and_then(Value::as_u64)?;
    if own != outer {
        return Some("Nonce mismatch.");
    }
    if action.get("hyperliquidChain").and_then(Value::as_str) == Some("Testnet") {
        return Some("Mainnet and testnet require different signature.");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(clippy::needless_pass_by_value)] // a table helper: callers pass literals
    fn user(extra: Value) -> Value {
        let mut base = json!({
            "type": "usdClassTransfer", "hyperliquidChain": "Mainnet",
            "signatureChainId": "0x66eee", "amount": "1", "toPerp": true, "nonce": 5
        });
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn user_signed_shapes_are_closed_and_typed() {
        assert!(conforms(&user(json!({})), true));
        assert!(conforms(&user(json!({"signatureChainId": "66eee"})), true));
        assert!(conforms(
            &user(json!({"signatureChainId": "0xffffffffffffffffffff"})),
            true
        ));
        assert!(conforms(
            &user(json!({"hyperliquidChain": "Testnet"})),
            true
        ));
        for bad in [
            json!({"hyperliquidChain": "mainnet"}),
            json!({"hyperliquidChain": ""}),
            json!({"signatureChainId": "0x"}),
            json!({"signatureChainId": "zz"}),
            json!({"signatureChainId": 421_614}),
            json!({"amount": 1}),
            json!({"toPerp": "true"}),
            json!({"nonce": "5"}),
            json!({"junk": 1}),
        ] {
            assert!(!conforms(&user(bad.clone()), true), "{bad}");
        }
    }

    #[test]
    fn destinations_are_free_strings_but_addresses_are_typed() {
        let send = json!({"type": "usdSend", "hyperliquidChain": "Mainnet",
            "signatureChainId": "0x66eee", "destination": "0x12", "amount": "1", "time": 1});
        assert!(conforms(&send, true));
        let agent = json!({"type": "approveAgent", "hyperliquidChain": "Mainnet",
            "signatureChainId": "0x66eee", "agentAddress": "0x12", "nonce": 1});
        assert!(!conforms(&agent, true));
        let abs = json!({"type": "userSetAbstraction", "hyperliquidChain": "Mainnet",
            "signatureChainId": "0x66eee", "user": "0x0000000000000000000000000000000000000001",
            "abstraction": "zzz", "nonce": 1});
        assert!(!conforms(&abs, true));
    }

    #[test]
    fn l1_actions_are_closed_and_typed() {
        let leverage = |extra: Value| {
            let mut a =
                json!({"type": "updateLeverage", "asset": 0, "isCross": true, "leverage": 5});
            for (k, v) in extra.as_object().unwrap() {
                a[k] = v.clone();
            }
            a
        };
        assert!(conforms(&leverage(json!({})), true));
        assert!(!conforms(&leverage(json!({"leverage": 0})), true));
        assert!(!conforms(&leverage(json!({"leverage": "5"})), true));
        assert!(!conforms(&leverage(json!({"zz": 1})), true));
        let mut no_cross = leverage(json!({}));
        no_cross.as_object_mut().unwrap().remove("isCross");
        assert!(!conforms(&no_cross, true));
        assert!(!conforms(&json!({"type": "noop", "zz": 1}), true));
        assert!(!conforms(
            &json!({"type": "cancel", "cancels": [], "zz": 1}),
            true
        ));
        let order = |tif: &str, grouping: &str| {
            json!({"type": "order", "grouping": grouping, "orders": [
                {"a": 0, "b": true, "p": "1", "s": "1", "r": false, "t": {"limit": {"tif": tif}}}
            ]})
        };
        assert!(conforms(&order("Gtc", "na"), true));
        assert!(!conforms(&order("Zz", "na"), true));
        assert!(!conforms(&order("Gtc", "zz"), true));
    }
}
