# 0007 — The CCTP lanes: real contracts, a substituted attester, rewind parking

**Status:** accepted (R4; extended in the acceptance round).

## Context

The mirrors move USDC between T (Ethereum, domain 0) and V (HyperEVM, domain
19) two ways: the client's mock at `0x17` (untouched, theirs) and the real CCTP V2
contracts already present in both forks (this repo's third element).
The real lane keeps every on-chain check real — message format, domains,
`destinationCaller`, nonce replay, signature threshold, expiration, fee caps —
so the only substitutions are Circle's off-chain authority (a devnet attester
key installed through the contracts' own setters) and the Iris API (a local
service serving the live shapes, measured).

Devnet reality then bites where the real lane never sees it:

- a burn can sit below the fork pin (a re-pin moves the base); anvil forwards
  such `eth_getLogs` ranges upstream, which rejects them (measured: the public
  HyperEVM RPC caps at 1000 blocks, drpc refuses ranges over 10000) — and
  pre-pin events are the *source's* real burns, not this devnet's;
- a snapshot revert rewinds a chain below a watcher's cursor or below a mint;
- an expired message must be recoverable without a fresh burn.

## Decision

- **The scan never reads below the pin.** The watcher clamps its start at
  `forkBlockNumber + 1` (`anvil_nodeInfo`; an absent method means a real chain,
  where the configured start governs) and warns once per base.
- **Rewinds park, never replay.** A pin above the cursor (a re-pin) or a head
  below it (a snapshot revert) resets the scan to the base and parks every
  non-relayed record whose burn transaction no longer exists. The relayer
  mirrors this on the destination: a head below its watermark parks relayed
  records whose nonce is no longer consumed. Stale derivations stay visible,
  never re-owned (the mirror's I6 rule).
- **`POST /v2/reattest/{nonce}` is the deliberate way back.** It re-signs the
  watched message with a fresh expiration window and re-arms the record
  (`signed`) — the only path the relayer acts on after a park. The relayer
  still refuses to mint when the source burn no longer exists (a re-pin
  removed it), re-parking with that reason: a reattest can revive an expired
  message, never a vanished burn.
- **The hook lane is the official wrapper.** A credit-carrying transfer burns
  `depositForBurnWithHook` with `hookData = target(20) ‖ calldata` and
  `destinationCaller = wrapper`; the relay calls the owner-gated
  `CCTPHookWrapper.relay` (Circle's Apache-2.0 sample, bytecode embedded with
  its provenance hash), which mints and then calls the hook — non-atomic by
  design: a failed hook cannot lose the mint.
- **The relayer runs `auto` or `manual`.** `relayer = "manual"` keeps the tick
  to the gas alerts and the rewind check; `POST /control/relay {nonce?}` (the
  one shared relayer instance) is then the only driver — the deterministic way
  to test expiry and reattestation windows. `auto` (the default) relays every
  signed record once per tick.

## Consequences

- `expiration_blocks` is anchored at the *destination's* head (`+N`). Measured:
  the contract rejects at `block.number == expirationBlock`, and the relay's
  gas estimate runs at the pending block (head + 1) — so a relayable window
  needs `N ≥ 2`. `0` stays no-expiry (the default).
- The Iris serves the live shapes exactly: the 404 body, pending
  `attestation: null`, the fee table (`0→19` fast costs 1 unit), `publicKeys`.
- The relayer never re-sends blindly: a submitted tx is recorded and its
  receipt checked first; `usedNonces` is read before sending and after a
  revert — the chain stays the dedup truth.
- Wrapper deployment is a boot step (owner = the relay key, transmitter read
  back); a failed deployment is a warning, and hook transfers refuse by name
  until a later boot lands it.
- The vanished-burn guard (the reattest rule above) reads
  `eth_getTransactionByHash` on the source, so it needs the source to *retain*
  tx lookups: measured on anvil 1.8.6, `--transaction-block-keeper 2` prunes tx
  bodies, receipts and logs beyond 2 blocks — every burn older than that
  false-parks. The parity flags set keeper 1000000 on both chains, which holds for any realistic test window; a false park is safe
  (never minted) and the transfer is simply re-run.
- **Fees stay zero; the lane is finalized.** Every transfer burns with
  `minFinalityThreshold = 2000` and `maxFee = 0`, and the devnet attester fills
  `finalityThresholdExecuted = 2000` — so the destination always takes
  `handleReceiveFinalizedMessage`. The fee knob the plan sketched would be dead
  config: both chains' on-chain surfaces measure zero (the Ethereum messenger
  has no min-fee surface at all; HyperEVM's `minFee() = 0`), so the contract
  charges nothing. The Iris still serves Circle's real fee *table* (0→19 fast =
  1 unit) for shape fidelity. The confirmed/fast lane (1000 — the
  unfinalized dispatch) is a documented deferral: the devnet attester would
  attest it identically, the real HyperEVM source has no fast lane, and no
  acceptance needs it.
