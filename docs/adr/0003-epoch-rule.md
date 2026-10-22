# 0003 — The epoch rule: clear on a new pin, restore on the same pin

**Status:** accepted (R3; semantics shared with the mainnet mirror).

## Context

A V reset is ambiguous: the base may have moved (a re-pin, a fresh fork) or not
(a same-pin restart, an out-of-band `anvil_reset`). Measured on T: restoring
across a moved base re-applied a +30d time-travel write as a future
`lastUpdateTimestamp`, and every Aave supply panicked with an arithmetic
underflow. Measured on V: a re-pin to a later base keeps every cursor hash
valid, so the old hash check would have missed the reset entirely (ADR 0002).

## Decision

Key the rule on the fork pin, fail-closed on the pin read (without it the
decision would default to "same pin" and re-apply writes across a pin change):

- **pin != `epoch_pin` - new epoch:** drop marks *and* values
  (`venue_epochs_total`, WARN); the devnet re-creates its state from its own
  journal.
- **pin == `epoch_pin` - restore:** re-apply the recorded writes, keep the
  marks; orphan values (keys handed back earlier) are pruned, never re-owned;
  marked-but-unrecorded keys are handed back.

Either way the journal is cleared (before-values of the old chain) and the
cursor is anchored at the pin - V's state *is* the source's state at the pin.

## Consequences

- One decision, two outcomes, both observable (`venue_epochs_total`,
  `venue_restore_writes_total`).
- A re-pin loses the devnet's writes by design; the simulator's `epoch_new`
  performs the sim reset and the engine re-pin in one call.
- A same-pin reset is transparent: the recorded writes come back.
- The clear path stays the default - restoring across a moved base is the
  measured underflow hazard.
