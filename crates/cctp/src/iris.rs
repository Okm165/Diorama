//! The Iris-compatible API (8095, §15.4): the local substitution for Circle's
//! attestation service, serving the **measured live shapes** (§16.2) so
//! daloc's `cctp-relay` consumes it unchanged.
//!
//! Served paths:
//!
//! - `GET /v2/messages/{sourceDomain}?transactionHash=0x…` — the only path
//!   daloc's client reads. Unknown tx → **404**
//!   `{"error":"Message not found for provided parameters"}` (their client
//!   treats 404 as "keep polling"); otherwise
//!   `{"messages":[{"message":"0x…","attestation":"0x…"|null}]}` — `null`
//!   while the emulated PENDING window is open (§15.4's local-Iris shape;
//!   the client also tolerates `PENDING`/empty).
//! - `GET /v2/publicKeys` — `{"publicKeys":[{"publicKey":"0x04…","cctpVersion":2}]}`
//!   with the devnet attester's 66-byte uncompressed key.
//! - `GET /v2/burn/USDC/fees/{source}/{destination}` — the measured fee table
//!   (§16.2): `0→19` = `[{1000,1},{2000,0}]`, `19→0` = `[{1000,0},{2000,0}]`.
//! - `GET /v2/fastBurn/USDC/allowance?sourceDomain=` — `{allowance,
//!   lastUpdated}` (a devnet stand-in value; the shape is what clients read).
//!
//! This module owns only the Iris vocabulary: the store's faults map to 500s
//! here, and the exact 404 body lives in one place ([`IrisError::not_found`]).
use crate::attester::Attester;
use crate::chains::{Domain, FINALITY_THRESHOLD_FINALIZED};
use crate::error::{Error, Result};
use crate::store::{MessageRecord, RelayState, Store};
use crate::transfer::measured_fees;
use alloy_primitives::{B256, U256};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The devnet stand-in for Circle's fast-burn allowance (the live value was
/// 56 854 459.08 USDC when measured, §16.2 — the shape, not the number, is
/// the contract with clients).
const FAST_BURN_ALLOWANCE: f64 = 1_000_000_000.0;

/// Everything the Iris routes read.
pub struct IrisState {
    pub store: Arc<Store>,
    pub attester: Arc<Attester>,
    /// Both served chains: the reattest route anchors the fresh expiration at
    /// the *destination's* head (the field is checked there, §15.1).
    pub chains: Vec<crate::chains::Chain>,
    /// The configured expiration window (0 = no expiry; the same value the
    /// watcher signs with).
    pub expiration_blocks: u64,
}

/// The Iris router (8095).
pub fn router(state: Arc<IrisState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v2/publicKeys", get(public_keys))
        .route("/v2/burn/USDC/fees/{source}/{destination}", get(fees))
        .route("/v2/fastBurn/USDC/allowance", get(allowance))
        .route("/v2/messages/{source_domain}", get(messages))
        .route("/v2/reattest/{nonce}", post(reattest))
        .with_state(state)
}

pub async fn healthz() -> &'static str {
    "ok"
}

/// Serve one router on one listener (the bin's two ports).
pub async fn serve(addr: String, router: Router) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| Error::Io {
            what: "http listener",
            path: addr.clone().into(),
            source,
        })?;
    tracing::info!(addr, "cctp http api listening");
    axum::serve(listener, router).await.map_err(|source| Error::Io {
        what: "http api",
        path: addr.into(),
        source,
    })
}

// ---- response shapes (the measured wire forms, §16.2) ----------------------

#[derive(Debug, Serialize)]
pub(crate) struct PublicKeysResponse {
    #[serde(rename = "publicKeys")]
    pub public_keys: Vec<PublicKeyEntry>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PublicKeyEntry {
    #[serde(rename = "publicKey")]
    pub public_key: String,
    #[serde(rename = "cctpVersion")]
    pub cctp_version: u8,
}

#[derive(Debug, Serialize)]
pub(crate) struct FeeEntry {
    #[serde(rename = "finalityThreshold")]
    pub finality_threshold: u32,
    #[serde(rename = "minimumFee")]
    pub minimum_fee: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct AllowanceResponse {
    pub allowance: f64,
    #[serde(rename = "lastUpdated")]
    pub last_updated: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct MessagesResponse {
    pub messages: Vec<MessageEntry>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MessageEntry {
    pub message: String,
    /// `null` while pending; the 65-byte hex once signed.
    pub attestation: Option<String>,
}

#[derive(Debug, Serialize)]
struct IrisErrorBody {
    error: String,
}

/// The Iris boundary's own refusal vocabulary (status + the exact body the
/// live API serves for it).
#[derive(Debug)]
pub(crate) struct IrisError {
    status: axum::http::StatusCode,
    error: String,
}

impl IrisError {
    /// The exact live 404 body (§16.2), byte for byte: daloc's client keys on
    /// it ("keep polling").
    pub fn not_found() -> Self {
        Self {
            status: axum::http::StatusCode::NOT_FOUND,
            error: "Message not found for provided parameters".into(),
        }
    }

    fn not_found_what(what: String) -> Self {
        Self {
            status: axum::http::StatusCode::NOT_FOUND,
            error: what,
        }
    }

    fn bad_request(reason: String) -> Self {
        Self {
            status: axum::http::StatusCode::BAD_REQUEST,
            error: reason,
        }
    }

    fn internal(e: &Error) -> Self {
        tracing::error!(error = %e, "iris request failed");
        Self {
            status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            error: e.to_string(),
        }
    }
}

impl IntoResponse for IrisError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(IrisErrorBody {
                error: self.error,
            }),
        )
            .into_response()
    }
}

// ---- handlers --------------------------------------------------------------

async fn public_keys(State(state): State<Arc<IrisState>>) -> Json<PublicKeysResponse> {
    Json(PublicKeysResponse {
        public_keys: vec![PublicKeyEntry {
            public_key: state.attester.public_key_hex(),
            cctp_version: 2,
        }],
    })
}

async fn fees(
    Path((source, destination)): Path<(u32, u32)>,
) -> Result<Json<Vec<FeeEntry>>, IrisError> {
    let Some(entries) = measured_fees(source, destination) else {
        return Err(IrisError::not_found_what(format!(
            "no fee data for sourceDomain {source}, destinationDomain {destination}"
        )));
    };
    Ok(Json(
        entries
            .into_iter()
            .map(|(finality_threshold, minimum_fee)| FeeEntry {
                finality_threshold,
                minimum_fee,
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct AllowanceQuery {
    #[serde(rename = "sourceDomain")]
    pub source_domain: Option<Domain>,
}

async fn allowance(Query(query): Query<AllowanceQuery>) -> Result<Json<AllowanceResponse>, IrisError> {
    let Some(source) = query.source_domain else {
        return Err(IrisError::bad_request(
            "sourceDomain is required".into(),
        ));
    };
    if !matches!(
        source,
        crate::chains::DOMAIN_ETHEREUM | crate::chains::DOMAIN_HYPER_EVM
    ) {
        return Err(IrisError::not_found_what(format!(
            "no allowance for sourceDomain {source}"
        )));
    }
    Ok(Json(AllowanceResponse {
        allowance: FAST_BURN_ALLOWANCE,
        last_updated: iso8601_ms(crate::now_ms()),
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessageQuery {
    #[serde(rename = "transactionHash")]
    pub transaction_hash: Option<String>,
}

async fn messages(
    State(state): State<Arc<IrisState>>,
    Path(source_domain): Path<Domain>,
    Query(query): Query<MessageQuery>,
) -> Result<Json<MessagesResponse>, IrisError> {
    let Some(raw_hash) = query.transaction_hash else {
        return Err(IrisError::bad_request("transactionHash is required".into()));
    };
    let hash = raw_hash
        .parse::<alloy_primitives::B256>()
        .map_err(|_| IrisError::bad_request(format!("transactionHash `{raw_hash}` is not a 32-byte hex hash")))?;
    let record = state
        .store
        .with(|s| {
            Ok(s.messages
                .iter()
                .find(|m| m.source_domain == source_domain && m.burn_tx == hash)
                .cloned())
        })
        .map_err(|e| IrisError::internal(&e))?;
    let Some(record) = record else {
        return Err(IrisError::not_found());
    };
    Ok(Json(MessagesResponse {
        messages: vec![entry(&record)],
    }))
}

/// `POST /v2/reattest/{nonce}` — re-sign a watched message with a *fresh*
/// expiration window (§15.4/§15.5). The expiry revert rolled back, so the
/// nonce is unconsumed and the same nonce can be re-served; the refreshed
/// record goes back to `signed`, which is what re-arms the relayer (a
/// deliberate operator action, never a silent retry). The nonce is the burn
/// tx hash (the deterministic policy, §15.3), so the lookup matches the GET's.
async fn reattest(
    State(state): State<Arc<IrisState>>,
    Path(nonce): Path<String>,
) -> Result<Json<MessagesResponse>, IrisError> {
    let nonce = parse_nonce(&nonce)?;
    let record = find_record(&state, nonce)?;
    let expiration = fresh_expiration(&state, &record).await?;
    let message = record
        .decoded()
        .map_err(|e| IrisError::internal(&e))?
        .with_attester_fields(nonce, FINALITY_THRESHOLD_FINALIZED, U256::ZERO, expiration);
    let attestation = state
        .attester
        .sign(&message.encode())
        .map_err(|e| IrisError::internal(&e))?;
    let refreshed = refresh_record(&state, nonce, &message, attestation)?;
    state.store.save().map_err(|e| IrisError::internal(&e))?;
    tracing::warn!(%nonce, expiration = %expiration, "cctp message re-attested with a fresh expiration");
    Ok(Json(MessagesResponse {
        messages: vec![entry(&refreshed)],
    }))
}

/// The path's nonce (a 32-byte hash, the burn tx — the deterministic policy).
fn parse_nonce(raw: &str) -> Result<B256, IrisError> {
    raw.parse::<B256>()
        .map_err(|_| IrisError::bad_request(format!("nonce `{raw}` is not a 32-byte hex hash")))
}

/// The message the nonce names, or the 404.
fn find_record(state: &IrisState, nonce: B256) -> Result<MessageRecord, IrisError> {
    state
        .store
        .with(|s| Ok(s.messages.iter().find(|m| m.nonce == nonce).cloned()))
        .map_err(|e| IrisError::internal(&e))?
        .ok_or_else(IrisError::not_found)
}

/// The re-attestation's `expirationBlock`: 0 (never expires) or the
/// destination chain's head plus the configured window. The expiry revert
/// rolled back, so the nonce is unconsumed and the same nonce can be
/// re-served; the refreshed record goes back to `signed`, which is what
/// re-arms the relayer (a deliberate operator action, never a silent retry).
async fn fresh_expiration(state: &IrisState, record: &MessageRecord) -> Result<U256, IrisError> {
    if state.expiration_blocks == 0 {
        return Ok(U256::ZERO);
    }
    let destination = state
        .chains
        .iter()
        .find(|c| c.domain == record.destination_domain)
        .ok_or_else(|| {
            IrisError::bad_request("no served chain for the message's destination".into())
        })?;
    let head = destination
        .rpc
        .block_number()
        .await
        .map_err(|e| IrisError::internal(&Error::Rpc(e)))?;
    Ok(U256::from(head.saturating_add(state.expiration_blocks)))
}

/// Write the fresh signature back and return the refreshed record (the 404
/// case: the record vanished between the read and the write).
fn refresh_record(
    state: &IrisState,
    nonce: B256,
    message: &crate::transfer::Message,
    attestation: alloy_primitives::Bytes,
) -> Result<MessageRecord, IrisError> {
    state
        .store
        .with(move |s| {
            let Some(rec) = s.messages.iter_mut().find(|m| m.nonce == nonce) else {
                return Ok(None);
            };
            rec.message_hash = message.keccak();
            rec.message = message.encode();
            rec.attestation = Some(attestation);
            rec.state = RelayState::Signed;
            rec.park_reason = None;
            rec.relay_tx = None;
            rec.attempts = 0;
            Ok(Some(rec.clone()))
        })
        .map_err(|e| IrisError::internal(&e))?
        .ok_or_else(IrisError::not_found)
}

fn entry(record: &MessageRecord) -> MessageEntry {
    MessageEntry {
        message: record.message_hex(),
        attestation: record.attestation_hex(),
    }
}

// ---- ISO 8601 (the `lastUpdated` shape; no chrono in the workspace) --------

/// UTC `YYYY-MM-DDTHH:MM:SS.mmmZ` from epoch milliseconds (Howard Hinnant's
/// civil-from-days).
pub(crate) fn iso8601_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        ms % 1000
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MessageRecord, RelayState};
    use crate::transfer::tests_support::sample_raw_message;
    use crate::transfer::Message;
    use alloy_primitives::{Bytes, B256};

    fn record(signed: bool) -> MessageRecord {
        let message = Message::decode(&sample_raw_message())
            .unwrap()
            .with_attester_fields(B256::repeat_byte(0xab), 2000, Default::default(), Default::default());
        MessageRecord {
            source_domain: Domain(0),
            destination_domain: Domain(19),
            nonce: B256::repeat_byte(0xab),
            burn_tx: B256::repeat_byte(0xab),
            burn_block: 1,
            raw_message: sample_raw_message(),
            message_hash: message.keccak(),
            message: message.encode(),
            attestation: signed.then(|| Bytes::from(vec![7u8; 65])),
            state: if signed { RelayState::Signed } else { RelayState::Pending },
            created_ms: 0,
            attest_after_ms: 0,
            relay_tx: None,
            relayed_ms: None,
            attempts: 0,
            park_reason: None,
        }
    }

    /// The measured wire shapes (§16.2), asserted as exact JSON.
    #[test]
    fn response_shapes_are_the_measured_ones() {
        let keys = PublicKeysResponse {
            public_keys: vec![PublicKeyEntry {
                public_key: "0x04aa".into(),
                cctp_version: 2,
            }],
        };
        assert_eq!(
            serde_json::to_string(&keys).unwrap(),
            r#"{"publicKeys":[{"publicKey":"0x04aa","cctpVersion":2}]}"#
        );

        let fees: Vec<FeeEntry> = measured_fees(0, 19)
            .unwrap()
            .into_iter()
            .map(|(finality_threshold, minimum_fee)| FeeEntry {
                finality_threshold,
                minimum_fee,
            })
            .collect();
        assert_eq!(
            serde_json::to_string(&fees).unwrap(),
            r#"[{"finalityThreshold":1000,"minimumFee":1},{"finalityThreshold":2000,"minimumFee":0}]"#
        );

        let pending = MessagesResponse {
            messages: vec![entry(&record(false))],
        };
        let parsed: serde_json::Value = serde_json::to_value(&pending).unwrap();
        assert_eq!(parsed["messages"][0]["attestation"], serde_json::Value::Null);
        let signed = MessagesResponse {
            messages: vec![entry(&record(true))],
        };
        let parsed: serde_json::Value = serde_json::to_value(&signed).unwrap();
        assert_eq!(
            parsed["messages"][0]["attestation"],
            serde_json::Value::String(format!("0x{}", "07".repeat(65)))
        );

        let allowance = AllowanceResponse {
            allowance: 42.5,
            last_updated: iso8601_ms(0),
        };
        assert_eq!(
            serde_json::to_string(&allowance).unwrap(),
            r#"{"allowance":42.5,"lastUpdated":"1970-01-01T00:00:00.000Z"}"#
        );
    }

    /// The 404 body daloc's client keys on, byte for byte.
    #[test]
    fn not_found_is_the_live_body() {
        let error = IrisError::not_found();
        assert_eq!(error.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(error.error, "Message not found for provided parameters");
    }

    #[test]
    fn iso8601_round_trips_known_instants() {
        assert_eq!(iso8601_ms(0), "1970-01-01T00:00:00.000Z");
        // 2026-10-22T00:00:00Z = 1792627200000 ms (checked with `date -u`).
        assert_eq!(iso8601_ms(1_792_627_200_000), "2026-10-22T00:00:00.000Z");
        assert_eq!(iso8601_ms(1_792_627_200_123), "2026-10-22T00:00:00.123Z");
        // A leap day.
        assert_eq!(iso8601_ms(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
    }

    /// A scratch Iris state on a fresh per-test temp dir (the tests may run
    /// concurrently).
    fn iris_state(name: &str) -> (Arc<IrisState>, Arc<Store>, std::path::PathBuf) {
        let dir = crate::test_dir(&format!("iris-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::load(dir.join("cctp.json")).unwrap());
        let state = Arc::new(IrisState {
            store: store.clone(),
            attester: Arc::new(crate::attester::Attester::from_seed("iris-test").unwrap()),
            chains: Vec::new(),
            expiration_blocks: 0,
        });
        (state, store, dir)
    }

    /// The messages handler's three answers: unknown → the exact 404 (daloc's
    /// "keep polling"), pending → `attestation: null`, signed → the hex; a
    /// wrong domain is not the same lookup.
    #[tokio::test]
    async fn messages_handler_serves_404_pending_and_signed() {
        let (state, store, dir) = iris_state("messages");
        let tx = B256::repeat_byte(0xab);
        let query = || MessageQuery {
            transaction_hash: Some(tx.to_string()),
        };

        let err = messages(State(state.clone()), Path(Domain(0)), Query(query()))
            .await
            .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(err.error, "Message not found for provided parameters");

        store
            .with(|s| {
                s.messages.push(record(false));
                Ok(())
            })
            .unwrap();
        let served = messages(State(state.clone()), Path(Domain(0)), Query(query()))
            .await
            .unwrap();
        assert!(served.0.messages[0].attestation.is_none(), "pending serves null");

        store
            .with(|s| {
                s.messages[0].attestation = Some(Bytes::from(vec![9u8; 65]));
                s.messages[0].state = RelayState::Signed;
                Ok(())
            })
            .unwrap();
        let served = messages(State(state.clone()), Path(Domain(0)), Query(query()))
            .await
            .unwrap();
        assert_eq!(
            served.0.messages[0].attestation.as_deref(),
            Some(format!("0x{}", "09".repeat(65)).as_str())
        );

        let err = messages(State(state), Path(Domain(19)), Query(query()))
            .await
            .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `POST /v2/reattest/{nonce}`: a parked message is re-signed and re-armed
    /// (`signed`), the expiration window is fresh, the attestation covers the
    /// new message bytes (with `expiration_blocks` at its default 0 the window
    /// stays 0 = no expiry); an unknown nonce is the live 404 body.
    #[tokio::test]
    async fn reattest_rearms_a_parked_message() {
        let (state, store, dir) = iris_state("reattest");
        let tx = B256::repeat_byte(0xab);
        store
            .with(|s| {
                s.messages.push(record(false));
                s.messages[0].state = RelayState::Parked;
                s.messages[0].park_reason = Some("must be re-signed".into());
                s.messages[0].relay_tx = Some(B256::repeat_byte(0x11));
                Ok(())
            })
            .unwrap();

        let refreshed = reattest(State(state.clone()), Path(tx.to_string()))
            .await
            .unwrap();
        let attestation = refreshed.0.messages[0].attestation.clone().unwrap();
        assert_eq!(attestation.len(), 132, "0x + 65 bytes");
        let record = store.with(|s| Ok(s.messages[0].clone())).unwrap();
        assert_eq!(record.state, RelayState::Signed, "re-armed for the relayer");
        assert!(record.park_reason.is_none());
        assert!(record.relay_tx.is_none());
        assert_eq!(
            record.decoded().unwrap().body.expiration_block,
            U256::ZERO,
            "0 = no expiry, unchanged"
        );

        let err = reattest(State(state), Path(B256::repeat_byte(0x77).to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(err.error, "Message not found for provided parameters");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
