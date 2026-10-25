# 0009 — The venue's unshadow truth: the pin's window, the journal's before-values

**Status:** accepted (hardening round; the selective fallback is planned).

## Context

`/admin/shadow {clear}` re-syncs a handed-back key from the *source*: reth at
the cursor on the ethereum, the fork source at the pin on the venue. The venue
source cannot serve history — its `eth_call` returns the *latest* state
(measured: the block parameter is ignored), so a read is only exact while the
source's head is still the pin. On a 1 s-block source that window is seconds:
measured, a re-pin's own fork fetch takes long enough that the pin is already
stale by completion, and every hand-back attempt against the live source was
refused with the named error.

An attempt to close the gap by falling back to the shadow registry's recorded
values was **measured unsound and reverted**: `shadow.values` holds the
*devnet's* latest writes (what a same-pin re-fork restores — ADR 0003), not
the source's before-state; serving them would hand back a poisoned value.

## Decision

- **Fail-closed, with the truth in the refusal.** A hand-back whose source is
  past the pin is refused, naming both options: a re-pin restores the *whole*
  source state (the re-fork itself is the wholesale hand-back), while a
  *selective* hand-back needs the source's head at the pin.
- **The registry is not the substitute; the journal is.** Each engine write
  records its before-value in the journal (the deal does), and within an epoch
  the *oldest* journal entry for a key holds the source's value — each later
  entry's before-value is a devnet value. A journal-based selective fallback
  (oldest entry, gated on the ring covering the epoch:
  `first_block + keep > head`) is the planned slice; it is not built.
- **The epoch remains the wholesale reset**: a re-pin clears marks and values
  and the re-fork restores the source's state (ADR 0003).

## Consequences

- The venue's hand-back is the one deliberate asymmetry with the mainnet's
  (reth can serve the cursor's state at any time); it is documented in
  TESTING's limitations and the module doc, and it never writes a wrong value.
- The refusal's wording is load-bearing: it was corrected once (the earlier
  "re-pin before handing keys back" implied a window that the fork fetch
  itself closes).
- A future selective hand-back must keep the three-answer discipline: the
  source read (exact while at the pin), the journal's oldest entry (exact
  while the ring covers the epoch), a named refusal otherwise.
