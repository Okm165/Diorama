# 0005 — The bridge applies a block's effects and its cursor atomically

**Status:** accepted (R3).

## Context

The bridge's effects are additive (an EVM→Core credit is `+= amount`), so a
duplicated credit is a silent balance lie - "value in = value recorded" at every
token door (`token-admission-policy`). The scan and the simulator state live in
the same service, so the failure window is a crash between applying a block's
effects and recording how far the scan got. The scan also must not mistake the
source's own blocks for devnet activity: blocks at or below the fork pin are
the source's real blocks, not the devnet's.

## Decision

Anchor the scan at the pin (a first pass, a re-pin, or a chain reset anchors at
`head.max(pin)`), and commit per block: decode -> apply -> advance the cursor ->
one atomic JSON save (write temp, rename). The effects and the cursor land
together.

## Consequences

- A crash mid-block loses both and re-scans the block, instead of half-applying
  it; the persisted cursor is always consistent with the persisted state.
- The cursor lives in `sim.json`, so a restart does not lose boundary activity
  mined while the service was down.
- Source blocks are never scanned, so their transfers are never credited.
- A trace fault degrades to top-level txs only and is counted
  (`venue_bridge_trace_faults_total`) - internal calls are missed that block,
  never silently dropped.
