# 0004 — The simulator's served identity is computed, never seeded

**Status:** accepted (R3).

## Context

daloc's venue client gates on `accountValue == totalRawUsd + Σ
signed(positionValue)` at 1e-6, with `positionValue` unsigned (the side comes
from `szi`), the mark derived as `positionValue/|szi|`, and money carried as
decimal text. A seeded state that carries its own `accountValue` can silently
disagree with its positions; f64 arithmetic fails the identity at the edges
(measured on the live sample).

## Decision

The simulator stores *inputs* only - the perp wallet's raw USDC balance,
positions with entry prices, spot balances - and computes every served figure
per read from the live marks:

```
totalRawUsd   = balance − Σ szi·entryPx
accountValue  = balance + Σ szi·(mark − entryPx) = totalRawUsd + Σ szi·mark
```

Seeding preserves `totalRawUsd` exactly (`balance = totalRawUsd + Σ
szi·entryPx`), so the operator's recall amount round-trips byte-for-byte.

## Consequences

- The identity holds by construction (verified exact on the live sample by a
  wire test); money is `rust_decimal` end to end.
- Margin, funding, and the liquidation check price from the same composed marks
  daloc reads - a crashed mark is the number the sim priced with.
- A seeded account cannot serve a summary that disagrees with its positions.
- A wire change that alters the derivation breaks a unit test, not a live test
  run.
