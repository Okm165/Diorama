# 0011 — The simulator's clock self-advances: lazy evaluation on read, plus a 1 Hz tick

**Status:** accepted (plan §14.8; venue mirror).

## Context

daloc's tests never call `advance` — they **set state and read**. A simulator
whose funding accrual, liquidation checks and mark-derived figures only move on
an explicit control call would serve frozen numbers to every read: funding
would never accrue, a crashed mark would never liquidate, and a scenario that
asserts "the position was liquidated" would wait forever. The alternative —
their tests calling `advance` everywhere — is a change to *their* code for our
convenience, and it makes every read's freshness depend on test discipline.

## Decision

- **Read-time evaluation**: every account-scoped read (`clearinghouseState`,
  the control view, the indexer's poll) runs the same evaluation first — accrue
  funding to now, check liquidations against the current marks, persist if
  dirty. The served figures are therefore always the ones the mechanics would
  produce at read time, and the served identity holds by construction.
- **A 1 Hz tick** runs the same evaluation for background effects (funding
  cadence, liquidations that no read would trigger), so the sim's state is
  alive even between reads.
- **`advance(seconds, n)` stays** for deterministic scripting (time travel,
  N liquidation passes in one call) — it is the same evaluation, invoked
  explicitly, and it counts in `venue_sim_advances_total`.

## Consequences

- Measured: the served `time` never lags wall, the sim clock or V's head
  (unit-tested `served_time_never_lags`); a crash liquidates on the next read
  or tick without any control call; funding accrues hourly on the oracle
  price with the exact formula.
- Evaluation is idempotent (accrual is keyed to `funded_at_ms`), so the read
  path and the tick cannot double-apply.
- The control surface's fixture reads are deliberately *not* re-derived: a
  patch read-back is stable across mark moves (the hl_mock contract), while
  `/info` serves the live view — the two surfaces answer different questions
  (ADR 0004).
