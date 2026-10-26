//! The bridge's control API (8096): health, metrics, the message listing, and
//! the one mutation a test needs — trigger a transfer.
//!
//! `POST /control/transfer` builds and submits a real `depositForBurn` on the
//! source chain (approve + burn, §15.1's exact call), so a scenario can start
//! a CCTP transfer without hand-crafting calldata. The wallet is the same
//! funded relay key; the mint recipient defaults to that wallet (a
//! self-bridge). The watcher picks the `MessageSent` event up from the chain —
//! the control route never inserts records itself (the chain stays the source
//! of truth).
use crate::chains::{by_domain, Chain, Domain, FINALITY_THRESHOLD_FINALIZED, TOKEN_MESSENGER_V2};
use crate::error::{Error, Result};
use crate::metrics::{render_counts, CctpMetrics};
use crate::relayer::{RelayNow, Relayer};
use crate::store::{MessageRecord, RelayState, Store};
use crate::transfer::{
    validate_max_fee, wait_for_receipt, IERC20, ITokenMessengerV2, TxSender,
};
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Everything the control routes read.
pub struct ControlState {
    pub store: Arc<Store>,
    pub chains: Vec<Chain>,
    pub sender: Arc<TxSender>,
    pub metrics: Arc<CctpMetrics>,
    /// The one relayer: the manual trigger drives it, the auto tick shares it
    /// (one dedup truth — the destination's nonce table).
    pub relayer: Arc<Relayer>,
    /// The boot-probed min-fee surface per source domain (§15.2: absent on
    /// Ethereum, `0` on HyperEVM).
    pub min_fees: BTreeMap<Domain, Option<U256>>,
    pub receipt_timeout: Duration,
}

/// The control router (8096).
pub fn router(state: Arc<ControlState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/control/state", get(state_listing))
        .route("/control/transfer", post(transfer))
        .route("/control/relay", post(relay))
        .with_state(state)
}

/// The HTTP mapping of the bridge's error taxonomy — the same classes the
/// engines use: request refusals are 4xx, an unserved domain is 404, a
/// transport fault at the chain boundary is 502, a wedged store is 503
/// (cannot serve correct answers until restarted), everything else is a
/// server-side fault.
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Error::BadRequest { .. } => axum::http::StatusCode::BAD_REQUEST,
            Error::UnknownDomain { .. } => axum::http::StatusCode::NOT_FOUND,
            Error::Rpc(_) => axum::http::StatusCode::BAD_GATEWAY,
            Error::Poisoned { .. } => axum::http::StatusCode::SERVICE_UNAVAILABLE,
            _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == axum::http::StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "control request failed");
        }
        (status, self.to_string()).into_response()
    }
}

// ---- health / metrics ------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct HealthResponse {
    /// `ok` with no parked messages, `degraded` when one is parked (never
    /// dropped, never hidden).
    pub status: &'static str,
    pub pending: usize,
    pub signed: usize,
    pub relayed: usize,
    pub parked: usize,
    pub chains: Vec<ChainHealth>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChainHealth {
    pub name: &'static str,
    pub domain: Domain,
}

pub(crate) async fn health(State(state): State<Arc<ControlState>>) -> Result<Json<HealthResponse>> {
    let counts = state.store.counts()?;
    Ok(Json(HealthResponse {
        status: if counts.parked == 0 { "ok" } else { "degraded" },
        pending: counts.pending,
        signed: counts.signed,
        relayed: counts.relayed,
        parked: counts.parked,
        chains: state
            .chains
            .iter()
            .map(|chain| ChainHealth {
                name: chain.name,
                domain: chain.domain,
            })
            .collect(),
    }))
}

pub async fn metrics(State(state): State<Arc<ControlState>>) -> Result<String> {
    let counts = state.store.counts()?;
    let mut out = state.metrics.render();
    render_counts(&mut out, &counts);
    Ok(out)
}

// ---- the message listing ---------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct MessageSummary {
    pub source_domain: Domain,
    pub destination_domain: Domain,
    pub burn_tx: B256,
    pub burn_block: u64,
    pub nonce: B256,
    pub state: RelayState,
    pub message: String,
    pub attestation: Option<String>,
    pub relay_tx: Option<B256>,
    pub relayed_ms: Option<u64>,
    pub park_reason: Option<String>,
}

impl MessageSummary {
    fn of(record: &MessageRecord) -> Self {
        Self {
            source_domain: record.source_domain,
            destination_domain: record.destination_domain,
            burn_tx: record.burn_tx,
            burn_block: record.burn_block,
            nonce: record.nonce,
            state: record.state,
            message: record.message_hex(),
            attestation: record.attestation_hex(),
            relay_tx: record.relay_tx,
            relayed_ms: record.relayed_ms,
            park_reason: record.park_reason.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StateResponse {
    pub(crate) messages: Vec<MessageSummary>,
}

pub(crate) async fn state_listing(State(state): State<Arc<ControlState>>) -> Result<Json<StateResponse>> {
    let messages = state
        .store
        .with(|s| Ok(s.messages.iter().map(MessageSummary::of).collect::<Vec<_>>()))?;
    Ok(Json(StateResponse { messages }))
}

// ---- relay ----------------------------------------------------------------

/// `POST /control/relay`: relay now — one SIGNED message by nonce, or every
/// SIGNED message. Manual mode's only driver; in auto mode a nudge
/// that skips the poll interval.
#[derive(Debug, Deserialize)]
pub(crate) struct RelayRequest {
    /// Absent = every SIGNED message.
    #[serde(default)]
    pub nonce: Option<B256>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RelayResponse {
    /// Messages confirmed by this pass (`receiveMessage` mined, or the nonce
    /// already consumed on-chain).
    pub confirmed: usize,
}

pub(crate) async fn relay(
    State(state): State<Arc<ControlState>>,
    Json(request): Json<RelayRequest>,
) -> Result<Json<RelayResponse>> {
    if let Some(nonce) = request.nonce {
        let record = state
            .store
            .find_message(|m| m.nonce == nonce)?
            .ok_or_else(|| Error::bad_request(format!("no watched message with nonce {nonce}"),))?;
        if record.state != RelayState::Signed {
            return Err(Error::bad_request(format!(
                    "message {nonce} is {:?}; only SIGNED messages relay (reattest an expired one first)",
                    record.state
                ),));
        }
    }
    let confirmed = state.relayer.relay_now(RelayNow::of(request.nonce)).await?;
    Ok(Json(RelayResponse { confirmed }))
}

// ---- transfer --------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct TransferRequest {
    /// The source CCTP domain (0 = T, 19 = V).
    pub source: Domain,
    /// The destination CCTP domain (must be the other mirror).
    pub destination: Domain,
    /// Base units (100 USDC = 100_000_000).
    pub amount: U256,
    /// Defaults to the relay wallet (a self-bridge).
    #[serde(default)]
    pub mint_recipient: Option<Address>,
    /// Optional protocol-native credit: the burn carries
    /// `hookData = target(20) ‖ calldata` and pins `destinationCaller` to the
    /// hook wrapper, so one relay tx mints **and** calls the target.
    #[serde(default)]
    pub hook: Option<HookSpec>,
}

/// The hook a transfer carries — the credit-carrying lane.
#[derive(Debug, Deserialize)]
pub(crate) struct HookSpec {
    /// The contract the wrapper calls after the mint (e.g. daloc's
    /// `ProofExecutor.receiveRecovery(positionId, amount)`).
    pub target: Address,
    /// The call's calldata.
    #[serde(default)]
    pub calldata: Bytes,
}

/// The burn receipt's own state (a test may also poll the tx; the watcher
/// absorbs it either way — a reverted burn is an error, never this state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BurnReceipt {
    /// The receipt arrived within the window.
    Observed,
    /// The window elapsed first: the tx is still in flight.
    Pending,
}

/// Whether the burn carried a hook: the relay then goes through the wrapper
/// (the credit-carrying lane).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HookCarriage {
    /// A hook rode the burn: the relay mints and calls through the wrapper.
    Carried,
    /// A plain burn: the relay mints directly.
    Plain,
}

#[derive(Debug, Serialize)]
pub(crate) struct TransferResponse {
    pub burn_tx: B256,
    /// The receipt's own state.
    pub receipt: BurnReceipt,
    pub source_domain: Domain,
    pub destination_domain: Domain,
    pub amount: U256,
    pub mint_recipient: Address,
    /// The burn's carriage.
    pub hook: HookCarriage,
}

/// The destination-side pin a burn carries: the hook wrapper for
/// hook-carrying transfers (their `destinationCaller`, §15.4), zero for plain
/// ones. A hook without a deployed wrapper is refused by name — never sent
/// as a plain burn that would drop the credit.
async fn hook_pin(state: &ControlState, request: &TransferRequest) -> Result<(B256, Option<Bytes>)> {
    match &request.hook {
        Some(hook) => {
            let wrapper = state
                .store
                .with(|s| Ok(s.hooks.get(&request.destination).copied()))?
                .ok_or_else(|| Error::bad_request(format!(
                        "no hook wrapper deployed on domain {} (its boot step deploys one; the bridge log names the outcome)",
                        request.destination
                    ),))?;
            // A re-pin removes the wrapper's code: pinning `destinationCaller`
            // to a dead address would make the burn unrelayable. Refuse by
            // name before spending the burn.
            let chain = by_domain(&state.chains, request.destination)?;
            if !crate::transfer::has_code(&chain.rpc, wrapper).await? {
                return Err(Error::bad_request(format!(
                        "the hook wrapper {wrapper} has no code on {} (a re-pin?); restart the bridge to redeploy it",
                        chain.name
                    ),));
            }
            Ok((
                wrapper.into_word(),
                Some(crate::hook::hook_data(hook.target, &hook.calldata)),
            ))
        }
        None => Ok((B256::ZERO, None)),
    }
}

/// The source-side burn calldata: `depositForBurn` or its hook variant. The
/// `sol!` call types *are* §15.1's signatures — their `mintRecipient` is the
/// destination-domain address as bytes32 (the contracts' `toBytes32`
/// convention) — so building the typed call is the whole builder.
fn burn_calldata(
    chain: &Chain,
    request: &TransferRequest,
    mint_recipient: Address,
    destination_caller: B256,
    hook_data: Option<&Bytes>,
) -> Bytes {
    match hook_data {
        Some(hook_data) => ITokenMessengerV2::depositForBurnWithHookCall {
            amount: request.amount,
            destinationDomain: request.destination.0,
            mintRecipient: mint_recipient.into_word(),
            burnToken: chain.usdc,
            destinationCaller: destination_caller,
            maxFee: U256::ZERO,
            minFinalityThreshold: FINALITY_THRESHOLD_FINALIZED,
            hookData: hook_data.clone(),
        }
        .abi_encode()
        .into(),
        None => ITokenMessengerV2::depositForBurnCall {
            amount: request.amount,
            destinationDomain: request.destination.0,
            mintRecipient: mint_recipient.into_word(),
            burnToken: chain.usdc,
            destinationCaller: destination_caller,
            maxFee: U256::ZERO,
            minFinalityThreshold: FINALITY_THRESHOLD_FINALIZED,
        }
        .abi_encode()
        .into(),
    }
}

pub(crate) async fn transfer(
    State(state): State<Arc<ControlState>>,
    Json(request): Json<TransferRequest>,
) -> Result<Json<TransferResponse>> {
    let chain = by_domain(&state.chains, request.source)?;
    validate_request(&state, &request)?;
    let mint_recipient = request.mint_recipient.unwrap_or(state.sender.address());
    let (destination_caller, hook_data) = hook_pin(&state, &request).await?;
    ensure_allowance(&state, chain, request.amount).await?;

    let calldata = burn_calldata(chain, &request, mint_recipient, destination_caller, hook_data.as_ref());
    let hash = submit_burn(&state, chain, &calldata).await?;
    let receipt = match wait_for_receipt(&chain.rpc, hash, state.receipt_timeout).await? {
        Some(receipt) if receipt.status() => BurnReceipt::Observed,
        Some(_) => return Err(Error::TxReverted { chain: chain.name, tx: hash }),
        None => BurnReceipt::Pending,
    };
    Ok(Json(TransferResponse {
        burn_tx: hash,
        receipt,
        source_domain: request.source,
        destination_domain: request.destination,
        amount: request.amount,
        mint_recipient,
        hook: if hook_data.is_some() {
            HookCarriage::Carried
        } else {
            HookCarriage::Plain
        },
    }))
}

/// The request's own invariants, checked before any chain read: distinct
/// domains, a known destination, a nonzero amount, and the source-side fee
/// rule the contract would enforce — refused here with the numbers instead of
/// paying for a revert (devnet maxFee is 0).
fn validate_request(state: &ControlState, request: &TransferRequest) -> Result<()> {
    if request.destination == request.source {
        return Err(Error::bad_request("source and destination are the same domain",));
    }
    by_domain(&state.chains, request.destination)?;
    if request.amount.is_zero() {
        return Err(Error::bad_request("amount must be nonzero",));
    }
    validate_max_fee(
        state.min_fees.get(&request.source).copied().flatten(),
        request.amount,
        U256::ZERO,
    )
}

/// Funding and allowance: the messenger pulls from the sender's balance
/// (`_depositAndBurn`), so both must be in place before the burn — the approve
/// is sent only when the allowance is short.
async fn ensure_allowance(state: &ControlState, chain: &Chain, amount: U256) -> Result<()> {
    let balance = chain.usdc_balance(state.sender.address()).await?;
    if balance < amount {
        return Err(Error::bad_request(format!(
                "sender {} holds {balance} USDC on {}, needs {amount} (deal via the chain's admin surface)",
                state.sender.address(),
                chain.name,
            ),));
    }
    let allowance = chain
        .usdc_allowance(state.sender.address(), TOKEN_MESSENGER_V2)
        .await?;
    if allowance >= amount {
        return Ok(());
    }
    let approve = IERC20::approveCall {
        spender: TOKEN_MESSENGER_V2,
        amount: U256::MAX,
    }
    .abi_encode();
    let hash = state
        .sender
        .send(&chain.rpc, chain.usdc, &approve, U256::ZERO)
        .await?;
    match wait_for_receipt(&chain.rpc, hash, state.receipt_timeout).await? {
        Some(receipt) if receipt.status() => {}
        Some(_) => return Err(Error::TxReverted { chain: chain.name, tx: hash }),
        None => {
            return Err(Error::bad_request(format!(
                    "approve tx {hash} was not mined within {}s; retry the transfer",
                    state.receipt_timeout.as_secs()
                ),))
        }
    }
    tracing::info!(chain = chain.name, tx = %hash, "cctp approve submitted");
    Ok(())
}

/// Submit `depositForBurn` and log it.
async fn submit_burn(state: &ControlState, chain: &Chain, calldata: &Bytes) -> Result<B256> {
    let hash = state
        .sender
        .send(&chain.rpc, TOKEN_MESSENGER_V2, calldata, U256::ZERO)
        .await?;
    tracing::info!(
        chain = chain.name,
        tx = %hash,
        "cctp depositForBurn submitted"
    );
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::tests_support::sample_raw_message;
    use crate::transfer::Message;

    #[test]
    fn message_summary_serves_hex_and_state() {
        let message = Message::decode(&sample_raw_message()).unwrap();
        let record = MessageRecord {
            source_domain: Domain(0),
            destination_domain: Domain(19),
            nonce: B256::repeat_byte(0xab),
            burn_tx: B256::repeat_byte(0xab),
            burn_block: 5,
            raw_message: sample_raw_message(),
            message_hash: message.keccak(),
            message: message.encode(),
            attestation: Some(alloy_primitives::Bytes::from(vec![1u8; 65])),
            state: RelayState::Signed,
            created_ms: 0,
            attest_after_ms: 0,
            relay_tx: None,
            relayed_ms: None,
            attempts: 0,
            park_reason: None,
        };
        let summary = MessageSummary::of(&record);
        assert!(summary.message.starts_with("0x"));
        assert_eq!(summary.attestation.as_deref().unwrap().len(), 2 + 130);
        assert_eq!(summary.state, RelayState::Signed);
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"state\":\"signed\""), "{json}");
    }

    /// The shared taxonomy, pinned per service (the same test name lives in
    /// both engines): a request refusal is 400, a transport fault at a chain
    /// boundary is 502, a wedged store is 503, an unserved domain is 404,
    /// everything else is 500.
    #[test]
    fn the_http_status_mapping_follows_the_shared_convention() {
        use axum::http::StatusCode;
        let status = |e: Error| e.into_response().status();
        assert_eq!(
            status(Error::bad_request("no")),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(Error::UnknownDomain { domain: Domain(5) }),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(Error::Rpc(diorama_anvil::Error::Status {
                endpoint: "anvil-t".into(),
                method: "eth_call".into(),
                status: 503,
                body: String::new(),
            })),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            status(Error::Poisoned {
                what: "cctp store"
            }),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(Error::MessageTooShort { len: 1, want: 2 }),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
