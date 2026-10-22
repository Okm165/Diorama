# 0002 — The chain-instance canary over cursor-hash reset detection

**Status:** accepted (R3).

## Context

The ethereum profile detects a re-fork by re-reading the cursor's block hash.
On V that check is unsound: heights below the fork resolve to the *source's*
real blocks, so a re-pin to a later base keeps every cursor hash valid
(measured live: the driver kept scanning a stale range), and a same-pin restart
leaves no trace at all. The epoch rule (ADR 0003) needs a reset signal that
covers both cases.

## Decision

One storage word - address `0x…ca11ca11`, slot 0 - is written with the current
fork pin by every reconcile, and read once per steady-state pass. "The canary
reads back the recorded pin" means "this is still the chain instance the
registry was built against"; anything else (empty, a different pin, a wiped
state) triggers the epoch rule. The address is scratch by construction - never
a watched account.

## Consequences

- Reset detection is a fact the chain itself carries; a fresh fork cannot carry
  the canary, so a re-pin and a same-pin restart are both detected.
- The cost is one storage read per second and one write per reconcile.
- The canary must be rewritten at the end of every reconcile, or every pass
  would read "reset".
- A canary read fault is the driver's error (fail-closed), never "steady".
