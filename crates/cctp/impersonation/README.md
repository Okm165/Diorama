# CCTP V2 impersonation

A mirror chain must look like a production chain to anything that speaks
Circle's CCTP V2: an off-chain relay asserts the transmitter/messenger getters
at boot, decodes the packed `MessageSent` payload, signs a raw
`keccak256(message)` attestation, and delivers `receiveMessage(message,
attestation)` on the destination. A private mock format — however convenient
locally — is invisible to that relay.

`MirrorCctpV2.sol` is the minimal contract that presents the production
surface:

| Surface | Value / behavior |
|---|---|
| `localDomain()`, `version()`, `messageBodyVersion()` | deployment facts; version 1 |
| `localMessageTransmitter()`, `remoteTokenMessengers(domain)` | one contract serves both messenger roles; peers are deployment facts |
| `minFee()` | 0 (the mirror's only fee bound is `maxFee < amount`) |
| `depositForBurn(amount, domain, mintRecipient, burnToken, caller, maxFee, minFinality)` | pulls the token, emits the packed V2 burn message |
| `receiveMessage(message, attestation)` | verifies the attester over `keccak256(message)`, guards the nonce, pays out `amount - feeExecuted` |
| `attesterManager()` / `enableAttester` / `setSignatureThreshold` | the setters a relay installs through (by impersonation) |

Two mirror-specific facts are deliberate and documented in the contract: the
fee taken is the caller's `maxFee` (the conservative bound; delivery credits
`amount - maxFee`), and delivery pays out tokens the contract already holds
(the burn deposits of the opposite lane) rather than minting new supply.

## Install

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
body 228, hook data appended); the acceptance test is a live cross-chain
transfer carried by the bridge.
