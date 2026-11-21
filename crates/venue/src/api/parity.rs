//! Parity with the real venue: every info response the frontend consumes is
//! compared, shape for shape, against a sample captured live from the real API
//! (`fixtures/real.json`, arrays trimmed to two items, every address replaced
//! by one placeholder). The world has three dexes: the default one, a USDC
//! builder dex (`xyz`) and a USDH builder dex (`flx`), so the per-dex answers
//! are compared too, and the venue's answers for an unlisted dex, for
//! `ALL_DEXS` and for the other bad requests are replayed from the capture
//! (`errors`).
//!
//! The comparison is structural: the same keys, the same JSON types, at every
//! depth. `null` matches anything (the venue nulls fields the sim fills and the
//! other way round), an empty array matches any array, and a key the venue
//! itself omits at times (`cloid`, `builderFee`) may be absent. Values are not
//! compared: the real sample is one moment of one account.
//!
//! The fork's capture also covers the websocket channels (`ws`, `wsSilence`,
//! `wsErrors`, `dexOrder`); those belong to step 8, when the ws lane is ported.
//! The fork's ws cases are listed at the bottom of this module so the port's
//! remaining surface is visible.
//!
//! The capture rows that exercise deferred machinery are not compared: this
//! step keeps one USDC clearinghouse and no HIP-3 fee-scale fields, so a
//! builder dex's clearinghouse and meta are marked deferred (see
//! [`DEFERRED_INFO`]) rather than compared against an empty stand-in.
use super::{info, ApiState};
use crate::feed::MarketFeed;
use crate::sim::order::{Order, Tif};
use crate::sim::{Deal, DealClass};
use crate::wire::{AssetInfo, L2Book, Level, MarginTable, MarginTier, Meta, PerpDex};
use alloy_primitives::Address;
use axum::body::Bytes;
use axum::extract::State;
use rust_decimal::Decimal;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

const REAL: &str = include_str!("fixtures/real.json");

/// The capture rows this step defers, one entry per deferred machinery (the
/// fork's harness has no skip mechanism; this list is the port's own, and the
/// info test checks each row is still in the capture). Values are the venue's
/// answer key and the reason.
const DEFERRED_INFO: [(&str, &str); 5] = [
    (
        "meta:xyz",
        "step-9: a builder dex's HIP-3 fee-scale fields (growthMode, \
         lastFeeScaleChangeTime, deployerFeeScale) are not modeled",
    ),
    (
        "metaAndAssetCtxs:xyz",
        "step-9: the same HIP-3 fee-scale fields in the embedded meta",
    ),
    (
        "allPerpMetas[1..]",
        "step-9: the same HIP-3 fee-scale fields on a builder dex's meta",
    ),
    (
        "clearinghouseState:xyz",
        "step-9: the per-dex clearinghouse ledger",
    ),
    (
        "clearinghouseState:flx",
        "step-9: the per-dex clearinghouse ledger",
    ),
];

fn real() -> Value {
    serde_json::from_str(REAL).expect("fixture")
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `controlOwned` and `fundingOwned` are the mirror's own marks (a scenario
/// owns the figure); the venue has no such field and no frontend reads them.
const OPTIONAL: [&str; 9] = [
    "controlOwned",
    "fundingOwned",
    "cloid",
    "builderFee",
    "twapId",
    "isSnapshot",
    "premium",
    "impactPxs",
    "midPx",
];

fn same_shape(real: &Value, sim: &Value, path: &str) {
    if real.is_null() || sim.is_null() {
        return;
    }
    assert_eq!(
        kind(real),
        kind(sim),
        "{path}: the venue sends a {} and the sim a {}",
        kind(real),
        kind(sim)
    );
    match (real, sim) {
        (Value::Object(r), Value::Object(s)) => {
            for key in r.keys() {
                assert!(
                    s.contains_key(key) || OPTIONAL.contains(&key.as_str()),
                    "{path}: the sim is missing `{key}`"
                );
            }
            for key in s.keys() {
                assert!(
                    r.contains_key(key) || OPTIONAL.contains(&key.as_str()),
                    "{path}: the sim sends `{key}`, the venue does not"
                );
            }
            for (key, rv) in r {
                if let Some(sv) = s.get(key) {
                    same_shape(rv, sv, &format!("{path}.{key}"));
                }
            }
        }
        (Value::Array(r), Value::Array(s)) => {
            // A tuple (`["", {...}]`, `[[px...], [...]]`) matches position by
            // position, a list matches its first item.
            if !r.is_empty() && r.len() == s.len() && r.len() <= 3 {
                for (i, (rv, sv)) in r.iter().zip(s).enumerate() {
                    same_shape(rv, sv, &format!("{path}[{i}]"));
                }
            } else if let (Some(rv), Some(sv)) = (r.first(), s.first()) {
                same_shape(rv, sv, &format!("{path}[0]"));
            }
        }
        _ => {}
    }
}

fn book(coin: &str) -> L2Book {
    let level = |px: &str, sz: &str| Level {
        px: px.parse().unwrap(),
        sz: sz.parse().unwrap(),
        n: 2,
    };
    L2Book {
        coin: coin.into(),
        time: crate::now_ms(),
        levels: [
            vec![level("100", "50"), level("99", "50"), level("98", "50")],
            vec![level("101", "50"), level("102", "50"), level("103", "50")],
        ],
        spread: None,
    }
}

/// A builder dex's meta: its own margin table and collateral token. The
/// HIP-3 fee-scale fields the venue's builder metas carry are deferred with
/// the rest of the builder-dex meta shape (see [`DEFERRED_INFO`]).
fn builder_meta(coin: &str, table: u32, collateral_token: u32) -> Meta {
    Meta {
        universe: vec![AssetInfo {
            name: coin.into(),
            sz_decimals: 5,
            max_leverage: 10,
            margin_table_id: Some(table),
            margin_mode: None,
            only_isolated: None,
            is_delisted: None,
        }],
        margin_tables: vec![(
            table,
            MarginTable {
                description: String::new(),
                margin_tiers: vec![MarginTier {
                    lower_bound: Decimal::ZERO,
                    max_leverage: 10,
                }],
            },
        )],
        collateral_token,
    }
}

/// One builder dex's `perpDexs` record, with the fixture's own field set (the
/// venue sends `null` for an absent oracle updater, not an omitted key).
fn perp_dex(name: &str) -> PerpDex {
    let placeholder = Address::repeat_byte(0x11);
    PerpDex {
        name: name.into(),
        full_name: name.to_uppercase(),
        deployer: placeholder,
        oracle_updater: None,
        fee_recipient: Some(placeholder),
        asset_to_streaming_oi_cap: vec![
            ("xyz:AAOI".into(), "25000000".parse().unwrap()),
            ("xyz:AAPL".into(), "200000000".parse().unwrap()),
        ],
        asset_to_funding_clamp: Vec::new(),
        asset_to_funding_interest_rate: vec![
            ("xyz:EUR".into(), Decimal::ZERO),
            ("xyz:GBP".into(), Decimal::ZERO),
        ],
        asset_to_funding_multiplier: vec![
            ("xyz:AAOI".into(), Decimal::new(5, 1)),
            ("xyz:AAPL".into(), Decimal::new(5, 1)),
        ],
        sub_deployers: vec![
            ("registerAsset".into(), vec![placeholder, placeholder]),
            ("setOracle".into(), vec![placeholder]),
        ],
    }
}

/// A venue with one funded account that holds a position, a resting order, a
/// fill and a funding payment on the default dex and on the `xyz` builder dex
/// (a USDC dex); `flx` is a USDH dex the account never touched. The registry
/// order is default, xyz, flx (not alphabetical).
fn world() -> (Arc<ApiState>, Address) {
    world_on("http://127.0.0.1:1")
}

/// [`world`] with the venue's public API at `venue` (the pass-through target).
#[allow(clippy::too_many_lines)] // one fixture world: dexes, funding, positions, resting orders, fills, funding hour
fn world_on(venue: &str) -> (Arc<ApiState>, Address) {
    let feed = Arc::new(MarketFeed::memory(vec![
        "ETH".into(),
        "xyz:TSLA".into(),
        "flx:TSLA".into(),
    ]));
    let now = crate::now_ms();
    feed.put_meta(crate::sim::meta_with_table("ETH", 10), now);
    feed.put_dex_meta("xyz".into(), builder_meta("xyz:TSLA", 30, 0), now);
    feed.put_dex_meta("flx".into(), builder_meta("flx:TSLA", 10, 360), now);
    feed.put_perp_dexs(vec![None, Some(perp_dex("xyz")), Some(perp_dex("flx"))]);
    feed.put_oi_caps(Vec::new());
    feed.put_oi_caps_of("xyz", Vec::new());
    feed.put_oi_caps_of("flx", Vec::new());
    for coin in ["ETH", "xyz:TSLA", "flx:TSLA"] {
        feed.put_ctx(coin.into(), crate::wire::funding_ctx("101", "101"), now);
    }
    feed.put_book(book("ETH"), now);
    feed.put_book(book("xyz:TSLA"), now);
    feed.put_mids(
        [
            ("ETH", "101"),
            ("@107", "5"),
            ("xyz:TSLA", "101"),
            ("flx:TSLA", "101"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.parse().unwrap()))
        .collect(),
        now,
    );
    let api = crate::feed::rest::RestClient::new(&[venue.to_string()]).unwrap();
    let state = Arc::new(
        ApiState::new(
            feed.clone(),
            Arc::new(crate::sim::Sim::memory()),
            api,
            None,
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap(),
    );
    let user = Address::repeat_byte(0xaa);
    // step-9: the account plane keeps one ledger, so the deal has no dex and
    // the second funding is the same balance. A builder coin's position is
    // still opened (and margined) by the order lane through the feed's union
    // meta; the per-dex *clearinghouse* partition is what stays deferred.
    state
        .sim
        .deal(
            &feed,
            &Deal {
                address: user,
                class: DealClass::Perp,
                coin: None,
                amount: Decimal::from(10_000),
            },
        )
        .unwrap();
    let order = |coin: &str, is_buy: bool, size: &str, px: Option<&str>, tif: Tif| Order {
        coin: coin.into(),
        is_buy,
        size: size.parse().unwrap(),
        limit_px: px.map(|p| p.parse().unwrap()),
        tif,
        reduce_only: false,
        cloid: Some(alloy_primitives::B128::repeat_byte(0x11)),
        trigger: None,
    };
    for coin in ["ETH", "xyz:TSLA"] {
        state
            .sim
            .order(&feed, user, &order(coin, true, "1", None, Tif::Ioc))
            .unwrap();
        state
            .sim
            .place(&feed, user, &order(coin, true, "1", Some("90"), Tif::Gtc))
            .unwrap();
    }
    // An hour passes: one funding payment per position.
    state.sim.advance(&feed, 3600, 1).unwrap();
    // The default dex ends at its open interest cap (the trades above are done).
    feed.put_oi_caps(vec!["ETH".to_string()]);
    (state, user)
}

/// The answer's status and body text (for the venue's `null` and plain-text
/// answers, which are not all JSON objects).
async fn info_raw(state: &Arc<ApiState>, body: Value) -> (u16, String) {
    let response = info::info(State(state.clone()), Bytes::from(body.to_string()))
        .await
        .expect("info");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn info_json(state: &Arc<ApiState>, body: Value) -> Value {
    let response = info::info(State(state.clone()), Bytes::from(body.to_string()))
        .await
        .expect("info");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("json body")
}

/// The info requests compared against their captured answers. Rows whose
/// machinery is deferred ([`DEFERRED_INFO`]) are absent on purpose.
#[allow(clippy::too_many_lines)] // one table of info requests
fn info_cases(user: Address) -> Vec<(&'static str, Value)> {
    vec![
        ("allPerpMetas", serde_json::json!({"type": "allPerpMetas"})),
        ("meta", serde_json::json!({"type": "meta"})),
        (
            "metaAndAssetCtxs",
            serde_json::json!({"type": "metaAndAssetCtxs"}),
        ),
        (
            "clearinghouseState",
            serde_json::json!({"type": "clearinghouseState", "user": user}),
        ),
        (
            "spotClearinghouseState",
            serde_json::json!({"type": "spotClearinghouseState", "user": user}),
        ),
        (
            "openOrders",
            serde_json::json!({"type": "openOrders", "user": user}),
        ),
        (
            "openOrders:xyz",
            serde_json::json!({"type": "openOrders", "user": user, "dex": "xyz"}),
        ),
        (
            "frontendOpenOrders",
            serde_json::json!({"type": "frontendOpenOrders", "user": user}),
        ),
        (
            "frontendOpenOrders:ALL_DEXS",
            serde_json::json!({"type": "frontendOpenOrders", "user": user, "dex": "ALL_DEXS"}),
        ),
        (
            "activeAssetData",
            serde_json::json!({"type": "activeAssetData", "user": user, "coin": "ETH"}),
        ),
        (
            "activeAssetData:xyz:TSLA",
            serde_json::json!({"type": "activeAssetData", "user": user, "coin": "xyz:TSLA"}),
        ),
        (
            "l2Book",
            serde_json::json!({"type": "l2Book", "coin": "ETH"}),
        ),
        (
            "l2Book:xyz:TSLA",
            serde_json::json!({"type": "l2Book", "coin": "xyz:TSLA"}),
        ),
        (
            "perpsAtOpenInterestCap:xyz",
            serde_json::json!({"type": "perpsAtOpenInterestCap", "dex": "xyz"}),
        ),
        (
            "perpsAtOpenInterestCap",
            serde_json::json!({"type": "perpsAtOpenInterestCap"}),
        ),
        (
            "marginTable:30",
            serde_json::json!({"type": "marginTable", "id": 30}),
        ),
        (
            "marginTable",
            serde_json::json!({"type": "marginTable", "id": 50}),
        ),
        (
            "maxBuilderFee",
            serde_json::json!({"type": "maxBuilderFee", "user": user, "builder": user}),
        ),
        (
            "userFills",
            serde_json::json!({"type": "userFills", "user": user}),
        ),
        (
            "userFunding",
            serde_json::json!({"type": "userFunding", "user": user, "startTime": 0}),
        ),
        (
            "preTransferCheck",
            serde_json::json!({"type": "preTransferCheck", "user": user, "source": user}),
        ),
        (
            "userAbstraction",
            serde_json::json!({"type": "userAbstraction", "user": user}),
        ),
        (
            "exchangeStatus",
            serde_json::json!({"type": "exchangeStatus"}),
        ),
    ]
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one table of info requests against the captured responses
async fn info_responses_have_the_venues_shapes() {
    let (state, user) = world();
    let real = real();
    for (name, body) in info_cases(user) {
        let sim = info_json(&state, body).await;
        let real = &real["info"][name];
        assert!(!real.is_null(), "{name}: the capture lost its sample");
        assert!(!sim.is_null(), "{name}: the sim answered null");
        same_shape(real, &sim, name);
    }
    // Every deferred row is still in the capture, with a reason (a row that
    // vanished from the fixture would otherwise look compared).
    for (name, reason) in DEFERRED_INFO {
        let key = name.split('[').next().unwrap();
        assert!(
            real["info"].get(key).is_some(),
            "{name}: not in the capture"
        );
        assert!(!reason.is_empty(), "{name}: no deferred reason");
    }

    // `allPerpMetas`: one entry per dex. The default entry is the venue's
    // shape; a builder entry's HIP-3 fee-scale fields are deferred.
    let metas = info_json(&state, serde_json::json!({"type": "allPerpMetas"})).await;
    let real_metas = &real["info"]["allPerpMetas"];
    assert_eq!(metas.as_array().unwrap().len(), 3);
    same_shape(&real_metas[0], &metas[0], "allPerpMetas[default]");
    assert_eq!(metas[1]["universe"][0]["name"], "xyz:TSLA");
    assert_eq!(metas[2]["collateralToken"], 360, "flx is a USDH dex");
    // `meta {dex}` is that dex's entry of `allPerpMetas`.
    for (i, dex) in [(0, ""), (1, "xyz"), (2, "flx")] {
        let one = info_json(&state, serde_json::json!({"type": "meta", "dex": dex})).await;
        assert_eq!(one, metas[i], "meta for `{dex}`");
    }

    // `perpDexs`: `null` for the default dex, a record per builder dex in the
    // registry's order.
    let dexs = info_json(&state, serde_json::json!({"type": "perpDexs"})).await;
    let dexs = dexs.as_array().unwrap();
    assert_eq!(dexs.len(), 3);
    assert!(dexs[0].is_null());
    assert_eq!(
        [&dexs[1]["name"], &dexs[2]["name"]],
        [&Value::from("xyz"), &Value::from("flx")]
    );
    same_shape(&real["info"]["perpDexs"][1], &dexs[1], "perpDexs[1]");

    // `allMids`: coin-keyed maps compare their values and key families. The
    // default dex carries the default perps and the spot keys, a builder dex
    // only its own coins, `ALL_DEXS` all of them.
    let keys =
        |mids: &Value| -> Vec<String> { mids.as_object().unwrap().keys().cloned().collect() };
    let default = info_json(&state, serde_json::json!({"type": "allMids"})).await;
    assert_eq!(keys(&default), ["@107", "ETH"]);
    assert!(default.as_object().unwrap().values().all(Value::is_string));
    let explicit = info_json(&state, serde_json::json!({"type": "allMids", "dex": ""})).await;
    assert_eq!(explicit, default, "an empty dex is the default dex");
    let xyz = info_json(&state, serde_json::json!({"type": "allMids", "dex": "xyz"})).await;
    assert_eq!(keys(&xyz), ["xyz:TSLA"]);
    assert!(real["info"]["allMids:xyz"]
        .as_object()
        .unwrap()
        .values()
        .all(Value::is_string));
    let all = info_json(
        &state,
        serde_json::json!({"type": "allMids", "dex": "ALL_DEXS"}),
    )
    .await;
    assert_eq!(keys(&all), ["@107", "ETH", "flx:TSLA", "xyz:TSLA"]);
    assert!(real["info"]["allMids:ALL_DEXS"]
        .as_object()
        .unwrap()
        .values()
        .all(Value::is_string));
}

/// Every decimal the venue prints carries a point and no trailing zero: a whole
/// number is `"64891.0"` (never `"64891"`), a zero `"0.0"`, a fraction trimmed
/// (`"0.1"`, never `"0.10"`). Measured on every real info answer and websocket
/// channel (compressed frames included): none holds a bare integer string, so a
/// client that compares or parses the text sees the same on the sim.
fn decimal_text_problems(value: &Value, path: &str, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            let numeric = !text.is_empty()
                && text
                    .trim_start_matches('-')
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.')
                && text
                    .trim_start_matches('-')
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit());
            if numeric {
                let trimmed = text.trim_end_matches('0');
                if !text.contains('.')
                    || (trimmed.ends_with('.') && !text.ends_with(".0"))
                    || (text.ends_with('0') && !text.ends_with(".0"))
                {
                    out.push(format!("{path} = {text}"));
                }
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                decimal_text_problems(v, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                decimal_text_problems(v, &format!("{path}.{k}"), out);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn decimals_are_printed_the_venues_way() {
    let (state, user) = world();
    let bodies = [
        serde_json::json!({"type": "clearinghouseState", "user": user}),
        serde_json::json!({"type": "clearinghouseState", "user": user, "dex": "xyz"}),
        serde_json::json!({"type": "spotClearinghouseState", "user": user}),
        serde_json::json!({"type": "openOrders", "user": user}),
        serde_json::json!({"type": "frontendOpenOrders", "user": user, "dex": "ALL_DEXS"}),
        serde_json::json!({"type": "activeAssetData", "user": user, "coin": "ETH"}),
        serde_json::json!({"type": "activeAssetData", "user": user, "coin": "xyz:TSLA"}),
        serde_json::json!({"type": "l2Book", "coin": "ETH"}),
        serde_json::json!({"type": "meta", "dex": "xyz"}),
        serde_json::json!({"type": "metaAndAssetCtxs"}),
        serde_json::json!({"type": "metaAndAssetCtxs", "dex": "xyz"}),
        serde_json::json!({"type": "allMids", "dex": "ALL_DEXS"}),
        serde_json::json!({"type": "marginTable", "id": 30}),
        serde_json::json!({"type": "userFills", "user": user}),
        serde_json::json!({"type": "userFunding", "user": user, "startTime": 0}),
        serde_json::json!({"type": "userNonFundingLedgerUpdates", "user": user, "startTime": 0}),
    ];
    let mut problems = Vec::new();
    for body in bodies {
        let name = body["type"].as_str().unwrap().to_string();
        decimal_text_problems(&info_json(&state, body).await, &name, &mut problems);
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The sim's own per-dex figures: the resting orders split by dex (`ALL_DEXS`
/// lists them all, newest first), a table id resolves across the dexes'
/// tables, and a builder coin's active data uses its own mark.
///
/// step-9: the positions are *not* split by dex — the account plane keeps one
/// clearinghouse, so a named builder dex answers the venue's empty shape
/// (`info.rs` marks the read) and every position lives on the default one.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one fixture world read per dex
async fn the_per_dex_reads_hold_only_that_dexs_figures() {
    let (state, user) = world();
    let coins = |rows: &Value| -> Vec<String> {
        rows.as_array()
            .unwrap()
            .iter()
            .map(|o| o["coin"].as_str().unwrap().to_string())
            .collect()
    };
    for kind in ["openOrders", "frontendOpenOrders"] {
        let rows = |dex: &str| {
            let body = serde_json::json!({"type": kind, "user": user, "dex": dex});
            let state = state.clone();
            async move { info_json(&state, body).await }
        };
        assert_eq!(coins(&rows("").await), ["ETH"], "{kind}");
        assert_eq!(coins(&rows("xyz").await), ["xyz:TSLA"], "{kind}");
        assert!(coins(&rows("flx").await).is_empty(), "{kind}");
        assert_eq!(
            coins(&rows("ALL_DEXS").await),
            ["xyz:TSLA", "ETH"],
            "{kind}"
        );
        let absent = info_json(&state, serde_json::json!({"type": kind, "user": user})).await;
        assert_eq!(coins(&absent), ["ETH"], "{kind}: no dex is the default dex");
    }

    // Table ids are the dexes' own: 30 is xyz's, 10 is flx's, 50 the default's.
    for (id, leverage) in [(30u64, 10u64), (10, 10), (50, 10), (1, 1)] {
        let table = info_json(&state, serde_json::json!({"type": "marginTable", "id": id})).await;
        assert_eq!(
            table["marginTiers"][0]["maxLeverage"], leverage,
            "table {id}"
        );
    }
    // The builder coin's active data uses the coin's own mark.
    let data = info_json(
        &state,
        serde_json::json!({"type": "activeAssetData", "user": user, "coin": "xyz:TSLA"}),
    )
    .await;
    assert_eq!(data["coin"], "xyz:TSLA");
    assert_eq!(data["markPx"], "101.0");
}

/// What the venue answers for an unlisted dex, for `ALL_DEXS` where the type
/// does not take it, for a `null` dex and for an unknown table or coin:
/// replayed from the capture, status and body.
#[tokio::test]
async fn unlisted_dexes_and_bad_requests_answer_like_the_venue() {
    let (state, user) = world();
    let real = real();
    let errors = real["errors"].as_object().unwrap();
    assert!(errors.len() >= 15, "the capture lost its error cases");
    for (name, case) in errors {
        let request: Value = serde_json::from_str(
            &case["request"]
                .to_string()
                .replace("$USER", &user.to_string()),
        )
        .unwrap();
        let (status, body) = info_raw(&state, request).await;
        assert_eq!(
            i64::from(status),
            case["status"].as_i64().unwrap(),
            "{name}"
        );
        assert_eq!(body, case["body"].as_str().unwrap(), "{name}");
    }
}

/// A venue standing in for the real API: answers `/info` with the capture's
/// sample for the type when it has one, else echoes what it was sent.
async fn mock_venue() -> String {
    let real = real();
    let app = axum::Router::new().route(
        "/info",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let real = real.clone();
            async move {
                let kind = body["type"].as_str().unwrap_or_default().to_string();
                match real["info"].get(&kind) {
                    Some(sample) => axum::Json(sample.clone()),
                    None => axum::Json(serde_json::json!({"forwarded": body})),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// The market-wide types the sim does not model are the real venue's answer,
/// forwarded verbatim; a body missing a field the venue requires is the
/// venue's 422, and the account-specific types stay unrouted (the venue's 422
/// for a type it cannot parse here).
#[tokio::test]
#[allow(clippy::too_many_lines)] // one table of forwarded types
async fn unmodelled_market_types_pass_through_and_unrouted_ones_are_422() {
    let venue = mock_venue().await;
    let (state, user) = world_on(&venue);
    let real = real();
    let vault = "0xdfc24b077bc1425ad1dea75bcb6f8158e10df303";
    let forwarded: Vec<Value> = vec![
        serde_json::json!({"type": "spotMetaAndAssetCtxs"}),
        serde_json::json!({"type": "fundingHistory", "coin": "xyz:TSLA", "startTime": 1}),
        serde_json::json!({"type": "predictedFundings"}),
        serde_json::json!({"type": "perpDexLimits", "dex": "xyz"}),
        serde_json::json!({"type": "perpDexStatus", "dex": "xyz"}),
        serde_json::json!({"type": "maxMarketOrderNtls"}),
        serde_json::json!({"type": "vaultSummaries"}),
        serde_json::json!({"type": "tokenDetails", "tokenId": "0x6d1e7cde53ba9467b783cb7c530ce054"}),
        serde_json::json!({"type": "perpDeployAuctionStatus"}),
        serde_json::json!({"type": "spotPairDeployAuctionStatus"}),
        serde_json::json!({"type": "spotDeployState", "user": user}),
        serde_json::json!({"type": "outcomeMeta"}),
        serde_json::json!({"type": "allBorrowLendReserveStates"}),
        serde_json::json!({"type": "borrowLendReserveState", "token": 0}),
        serde_json::json!({"type": "leadingVaults", "user": user}),
        serde_json::json!({"type": "vaultDetails", "vaultAddress": vault}),
        serde_json::json!({"type": "vaultDetails", "vaultAddress": vault, "user": user}),
    ];
    for request in forwarded {
        let (status, body) = info_raw(&state, request.clone()).await;
        assert_eq!(status, 200, "{request}");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["forwarded"], request, "forwarded verbatim");
    }
    // The annotations and categories are the venue's lists, never a string.
    for kind in ["perpConciseAnnotations", "perpCategories"] {
        let (status, body) = info_raw(&state, serde_json::json!({"type": kind})).await;
        assert_eq!(status, 200);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body, real["info"][kind], "{kind}");
        assert!(body.is_array(), "{kind}");
    }
    // `candleSnapshot` is market data the sim has no trade tape for: the
    // venue's answer, in the capture's candle shape.
    let candles = info_json(
        &state,
        serde_json::json!({
            "type": "candleSnapshot",
            "req": {"coin": "BTC", "interval": "1h", "startTime": 0}
        }),
    )
    .await;
    same_shape(&real["info"]["candleSnapshot"], &candles, "candleSnapshot");
    // A missing required field never reaches the venue.
    for request in [
        serde_json::json!({"type": "perpDexLimits"}),
        serde_json::json!({"type": "fundingHistory", "coin": "BTC"}),
        serde_json::json!({"type": "tokenDetails"}),
        serde_json::json!({"type": "vaultDetails"}),
        serde_json::json!({"type": "borrowLendReserveState"}),
        // A type the venue does not know, and a ledger read without its user.
        serde_json::json!({"type": "noSuchInfoType", "user": user}),
        serde_json::json!({"type": "userFees"}),
        serde_json::json!({"type": "userFillsByTime", "user": user}),
    ] {
        let (status, body) = info_raw(&state, request.clone()).await;
        assert_eq!(status, 422, "{request}");
        assert_eq!(
            body,
            "Failed to deserialize the JSON body into the target type"
        );
    }
}

/// The account-scoped types the venue serves and the sim has no typed route
/// for: a simulated account answers from its own ledger (`userFillsByTime`,
/// `historicalOrders`) or like a fresh venue account; any other user is the
/// venue's verbatim answer.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one table of account types
async fn account_types_without_a_typed_route_answer_for_a_simulated_account() {
    let venue = mock_venue().await;
    let (state, user) = world_on(&venue);
    // userFillsByTime: the account's fills from `startTime`, oldest first.
    let all = info_json(
        &state,
        serde_json::json!({"type": "userFills", "user": user}),
    )
    .await;
    let all = all.as_array().unwrap();
    assert!(all.len() >= 2);
    let by_time = info_json(
        &state,
        serde_json::json!({"type": "userFillsByTime", "user": user, "startTime": 0}),
    )
    .await;
    let by_time = by_time.as_array().unwrap();
    assert_eq!(by_time.len(), all.len());
    let times: Vec<u64> = by_time
        .iter()
        .map(|f| f["time"].as_u64().unwrap())
        .collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "oldest first");
    let after = times[times.len() - 1] + 1;
    let none = info_json(
        &state,
        serde_json::json!({"type": "userFillsByTime", "user": user, "startTime": after}),
    )
    .await;
    assert_eq!(none, serde_json::json!([]));
    // historicalOrders: the resting orders, as `open`, in the orderStatus row shape.
    let history = info_json(
        &state,
        serde_json::json!({"type": "historicalOrders", "user": user}),
    )
    .await;
    let rows = history.as_array().unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        for key in ["order", "status", "statusTimestamp"] {
            assert!(row.get(key).is_some(), "{key}");
        }
        assert_eq!(row["order"]["tif"], "Gtc");
    }
    assert!(rows.iter().all(|r| r["status"] == "open"));
    // A fresh account: the static answers, and the venue asked about a neutral account.
    for (kind, expected) in [
        ("userRole", serde_json::json!({"role": "user"})),
        ("subAccounts", Value::Null),
        ("isVip", serde_json::json!(false)),
        ("userVaultEquities", serde_json::json!([])),
        ("extraAgents", serde_json::json!([])),
    ] {
        let got = info_json(&state, serde_json::json!({"type": kind, "user": user})).await;
        assert_eq!(got, expected, "{kind}");
    }
    let fees = info_json(
        &state,
        serde_json::json!({"type": "userFees", "user": user}),
    )
    .await;
    assert_eq!(
        fees["forwarded"]["user"],
        "0x0000000000000000000000000000000000000001"
    );
    // Another user is the venue's, verbatim.
    let stranger = "0x00000000000000000000000000000000000000bb";
    let theirs = info_json(
        &state,
        serde_json::json!({"type": "userFees", "user": stranger}),
    )
    .await;
    assert_eq!(theirs["forwarded"]["user"], stranger);
    let market = info_json(
        &state,
        serde_json::json!({"type": "recentTrades", "coin": "BTC"}),
    )
    .await;
    assert_eq!(market["forwarded"]["coin"], "BTC");
}

/// The venue's order of the two ledgers: fills newest first, funding oldest
/// first.
#[tokio::test]
async fn fills_come_newest_first_and_funding_oldest_first() {
    let (state, user) = world();
    let fills = info_json(
        &state,
        serde_json::json!({"type": "userFills", "user": user}),
    )
    .await;
    let times: Vec<u64> = fills
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["time"].as_u64().unwrap())
        .collect();
    assert!(!times.is_empty());
    assert!(times.windows(2).all(|w| w[0] >= w[1]), "{times:?}");
    let funding = info_json(
        &state,
        serde_json::json!({"type": "userFunding", "user": user, "startTime": 0}),
    )
    .await;
    assert_eq!(funding[0]["delta"]["type"], "funding");
}

/// The real API's answer to a type it does not know.
#[tokio::test]
async fn an_unknown_info_type_is_the_venues_422() {
    let (state, _) = world();
    let response = info::info(
        State(state),
        Bytes::from(r#"{"type":"fooBar"}"#.to_string()),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 422);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        "Failed to deserialize the JSON body into the target type"
    );
}

/// The transfer ledger: every kind of row the sim writes has the shape of a
/// real `userNonFundingLedgerUpdates` row (`send` and `spotTransfer` with their
/// `fee`, `nativeTokenFee` and `feeToken`, `accountClassTransfer`), the window
/// applies, an address the sim never held is the venue's own answer, and
/// `preTransferCheck` has the real shape: `fee` is the activation the source
/// still owes (Circle's first-outbound rule), `userExists` the destination's
/// standing.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one script: four transfers, then every read of their rows
async fn the_transfer_ledger_and_the_activation_check_have_the_venues_shapes() {
    use crate::sim::fees::RowKind;
    use crate::sim::transfer::{AssetSend, Wallet};
    use crate::sim::{ClassTransfer, Direction, TokenRef};
    let (state, user) = world();
    let real = real();
    let usdc = TokenRef {
        index: 0,
        coin: "USDC",
    };
    // `user` funds `other` (a new account), which then sends on (its first
    // outbound transfer pays the activation) and moves money between its own
    // wallets.
    let (other, third) = (Address::repeat_byte(0xbb), Address::repeat_byte(0xcc));
    state
        .sim
        .credit_spot(user, usdc, Decimal::from(100))
        .unwrap();
    let send = AssetSend {
        from: user,
        to: other,
        token: usdc,
        amount: Decimal::from(10),
        source: Wallet::Spot,
        destination: Wallet::Spot,
        kind: RowKind::Send,
    };
    state.sim.send_asset(&send, &state.feed).unwrap();
    let before = info_json(
        &state,
        serde_json::json!({"type": "preTransferCheck", "user": third, "source": other}),
    )
    .await;
    state
        .sim
        .spot_send((other, third), usdc, Decimal::from(5), &state.feed)
        .unwrap();
    state
        .sim
        .usd_class_transfer(
            other,
            &ClassTransfer {
                token: usdc,
                amount: Decimal::ONE,
                direction: Direction::ToPerp,
            },
            &state.feed,
        )
        .unwrap();
    state
        .sim
        .credit_evm_deposit(
            Address::repeat_byte(0xdd),
            usdc,
            Decimal::from(3),
            &state.feed,
        )
        .unwrap();

    let rows = info_json(
        &state,
        serde_json::json!({"type": "userNonFundingLedgerUpdates", "user": other, "startTime": 0}),
    )
    .await;
    let rows = rows.as_array().unwrap();
    let kinds: Vec<&str> = rows
        .iter()
        .map(|r| r["delta"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["send", "spotTransfer", "accountClassTransfer"]);
    for row in rows {
        let sample = real["info"]["userNonFundingLedgerUpdates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["delta"]["type"] == row["delta"]["type"])
            .expect("a real sample of the kind");
        same_shape(sample, row, "userNonFundingLedgerUpdates");
    }
    assert_eq!(rows[0]["delta"]["fee"], "0.0", "the receipt carries no fee");
    assert_eq!(
        rows[1]["delta"]["fee"], "1.0",
        "the first outbound send does"
    );
    assert_eq!(rows[1]["delta"]["feeToken"], "USDC");
    // The deposit's row, on the recipient's side, is a system-address transfer.
    let deposit = info_json(
        &state,
        serde_json::json!({"type": "userNonFundingLedgerUpdates", "user": Address::repeat_byte(0xdd)}),
    )
    .await;
    assert_eq!(
        deposit[0]["delta"]["user"],
        "0x2000000000000000000000000000000000000000"
    );
    // The window: nothing after the last row.
    let later = rows.last().unwrap()["time"].as_u64().unwrap() + 1;
    let none = info_json(
        &state,
        serde_json::json!({"type": "userNonFundingLedgerUpdates", "user": other, "startTime": later}),
    )
    .await;
    assert_eq!(none, serde_json::json!([]));

    // preTransferCheck: `user` is the destination, `source` the sender.
    // `other` owed the activation before its send, `user` never does.
    assert_eq!(
        before,
        serde_json::json!({"isSanctioned": false, "userExists": false, "fee": "1.0", "userHasSentTx": false})
    );
    same_shape(
        &real["info"]["preTransferCheck"],
        &before,
        "preTransferCheck",
    );
    let after = info_json(
        &state,
        serde_json::json!({"type": "preTransferCheck", "user": third, "source": other}),
    )
    .await;
    assert_eq!(after["fee"], "0.0", "the debt was paid");
    assert_eq!(after["userExists"], true);
    let funded = info_json(
        &state,
        serde_json::json!({"type": "preTransferCheck", "user": Address::repeat_byte(0xee), "source": user}),
    )
    .await;
    assert_eq!(
        (&funded["userExists"], &funded["fee"]),
        (&false.into(), &"0.0".into())
    );
}

// step-8: the fork's websocket parity cases land with the ws lane. They are
// `websocket_acks_and_frames_have_the_venues_shapes`,
// `l2_pushes_a_diff_in_the_venues_shapes_then_goes_quiet`,
// `subscription_errors_read_like_the_venues`,
// `the_fill_stream_pushes_a_snapshot_then_only_new_fills`,
// `the_funding_stream_serves_flat_rows`,
// `bbo_order_updates_and_user_events_push_changes_only`,
// `the_venues_ws_silences_and_errors_are_mirrored`,
// `per_dex_pushes_list_the_dexes_in_the_venues_order`, and
// `fast_asset_ctxs_is_one_map_over_every_dex`. They need the per-dex
// `Subscription` variants and the `pushes`/`request_reply` session surface
// this step's older `ws.rs` does not expose yet.
