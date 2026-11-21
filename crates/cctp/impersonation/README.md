# CCTP V2 impersonation

A mirror chain must look like a production chain to anything that speaks
Circle's CCTP V2: an off-chain relay asserts the transmitter/messenger getters
at boot, decodes the packed `MessageSent` payload, signs a raw
`keccak256(message)` attestation, and delivers `receiveMessage(message,
attestation)` on the destination. A private mock format — however convenient
locally — is invisible to that relay.

## When this applies

Two ways a mirror can present the CCTP surface, in order of preference:

1. **The fork base carries the production stack** — the mirror serves Circle's
   real proxies at the canonical CREATE2 addresses, and its only mutation is
   its attester: `crates/cctp/src/attester.rs` impersonates the live
   `attesterManager()` and enables the devnet key in the transmitter's own
   attester set. The *authority* is impersonated; the *verification* stays
   real. **This is what the current deployment runs** (verified 2026-11-20:
   ERC-1967 proxies, per-chain implementations, `localDomain` 0/19, the
   messenger's `messageBodyVersion` = 1, `localMinter` = the canonical minter;
   a valid attestation from a non-designated caller is refused with the
   production message "Invalid caller for message").
2. **The base carries no Circle deployment** — this directory. `install.sh`
   writes `MirrorCctpV2` at the deployment's transmitter address so the same
   relay runs unchanged. It is a fallback, not the deployment's normal state.

## The surface

`MirrorCctpV2.sol` is the minimal contract that presents the production
surface:

| Surface | Value / behavior |
|---|---|
| `localDomain()`, `version()`, `messageBodyVersion()` | deployment facts; version 1 |
| `localMessageTransmitter()`, `remoteTokenMessengers(domain)` | one contract serves both messenger roles; peers are deployment facts |
| `minFee()` | 0 (the mirror's only fee bound is `maxFee < amount`) |
| `depositForBurn(amount, domain, mintRecipient, burnToken, caller, maxFee, minFinality)` | pulls the token, emits the packed V2 burn message |
| `depositForBurnWithHook(…, hookData)` | the same burn with `hookData` appended at offset 376 — the entry point the protocol's `ProofExecutor` uses for the recovery lane |
| `receiveMessage(message, attestation)` | verifies the attester over `keccak256(message)`, then the `destinationCaller` guard (a named caller is the only consumer, zero = anyone), guards the nonce, pays out `amount - feeExecuted` |
| `attesterManager()` / `enableAttester` / `setSignatureThreshold` | the setters a relay installs through (by impersonation) |

Two mirror-specific facts are deliberate and documented in the contract: the
fee taken is the caller's `maxFee` (the conservative bound; delivery credits
`amount - maxFee`), and delivery pays out tokens the contract already holds
(the burn deposits of the opposite lane) rather than minting new supply.

## Install (fallback only)

`install.sh` writes the compiled code at the deployment's transmitter address,
clears the previous fixture's storage, initializes the facts and verifies the
surface. It takes only environment inputs (RPC, addresses, domains) — no host
is named here. The install is `anvil_setCode` + an `initialize` transaction, so
it is a **chain-state write the engine's shadow registry replays** across
same-pin restarts; a fresh epoch (a moved fork pin clears the shadow registry)
re-runs this script as part of the epoch's bring-up, before the deployment
client boots against the chain.

```
RPC_URL=… TARGET_ADDRESS=… LOCAL_DOMAIN=0 LOCAL_TOKEN=… \
ATTESTER_MANAGER=… PEER_DOMAIN=19 PEER_MESSENGER=… SENDER=… \
  crates/cctp/impersonation/install.sh
```

## Test

```
forge test --root crates/cctp/impersonation
```

The layout test pins every offset the relay's decoder reads (header 148 bytes,
body 228, hook data appended at 376) and the header word the delivery guard
reads (108); the delivery test exercises both burn entry points and the
caller guard end to end. The acceptance test is a live cross-chain transfer
carried by the bridge.
