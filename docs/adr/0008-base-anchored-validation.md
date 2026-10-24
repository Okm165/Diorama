# 0008 — The base is the anchor: scans, cursors and applies clamp at the fork pin

**Status:** accepted (hardening round; both mirrors).

## Context

A re-pin moves a chain's fork base forward and wipes everything above it. Four
call sites read or wrote across that boundary, each with a measured failure:

- the shadow scan walked `(watermark, head]` with no anchor, so after a re-pin
  it read the **source's** pre-pin blocks (anvil forwards those ranges
  upstream) and marked mainnet's real writes devnet-owned — measured: 311 real
  txs / 2586 writes scanned as devnet writes right after a re-pin;
- the mainnet engine validated its cursor by hashing the cursor block *on the
  chain*; after a re-pin that is a pre-pin block forwarded to the source, and
  the public RPCs rate-limit or time out on those — measured: a re-pin while
  the engine ran left the rebase unfired for minutes (the documented
  "auto-rebase on the next poll" did not hold);
- the CCTP watcher's rewind predicate treated `cursor == pin` — the legitimate
  rest state right after a re-pin, until the first block mines — as off-base:
  a warn + a parking pass every 500 ms while the fresh chain sat at its base;
- an apply racing a re-pin landed parent *n*'s writes on the new base and
  mined at the wrong height; measured: the applier then sat silent on the
  resetting anvil.

## Decision

- **Every scan is anchored at the pin** (`mirror_devnet::scan::scan_anchor`):
  the ethereum shadow loop, the applier's inline scan, the admin hand-back
  pre-step and the venue driver all clamp `watermark.max(pin)`; a cursor at
  the base is the rest state.
- **The base proves staleness — no remote read.** `below_fork_base(cursor,
  pin)` (strict `<`) fires the epoch rule directly; the hash check remains for
  rewinds (a block above the base that no longer exists). The same strict
  shape fixes the CCTP watcher (`cursor_off_base`).
- **An apply re-reads the base after its slow fetch** and aborts with
  `ResetDuringApply` when it moved: the next pass reconciles (the cursor sits
  below the base → the epoch rule clears what a half-applied block left).

## Consequences

- A re-pin reconciles on the next poll, deterministically, whatever the
  source's latency (re-verified live: ~7 s on a running engine).
- The predicates are strict (`<`), so "cursor == base" never fires; both are
  unit-tested (`scan_anchor_clamps_at_the_pin`, `cursor_off_base_keeps_the_
  pin_as_the_rest_state`, `below_fork_base_is_strict`).
- The admin surface answers the same way on both chains for the same request:
  `AdminSurface::rewind` checks the ring (a static property) before the cursor
  (state) — measured: with the cursor first, a chain without one answered "no
  cursor" while its peer answered "beyond the ring" (engine-symmetry 15/15
  after the reorder).
- The guard is belt-and-braces: the abort is cheap (one local read per apply)
  and the reconciling path is the same one a restart takes.
