# 0010 — Fixture ownership: the mirror-side `fixture_addresses` list, whole-account

**Status:** accepted (integration audit; both mirrors).

## Context

the client's fixtures write state two ways: **txs** (their mock aggregators'
`updateAnswer`, the swap router's rate, oracle-adapter syncs — all impersonated
txs) and **cheatcodes** (`anvil_setCode` installing `MockERC20Fixed` over the
canonical token addresses, `setBalance`, `deal_erc20`). The shadow scanner sees
only **mined txs**; a cheatcode write is invisible. Without ownership, the next
parent diff over a watched address would clobber the mock — the exact COW
failure the registry exists to prevent (measured precedent: a devnet-only write
overwritten by the parent stream).

The tempting alternative — asking the harness to register its writes — was
rejected: it makes correctness depend on their code calling our endpoint, and
the audit showed it is unnecessary.

## Decision

- The engine owns a **`fixture_addresses` boot list** (the instance's
  `[services.engine.fixture_addresses]`): every listed address is
  **whole-account owned** — *all* storage diffs for it are skipped, not just
  the slots it wrote. The canonical mocks are re-installed wholesale by
  `anvil_setCode`, so slot-level granularity would be a lie.
- **No harness-side registration.** The list is complete because only *watched*
  addresses can ever be clobbered: the engine applies parent diffs to the
  watchlist only, the canonical mocks are the watched addresses the client replaces,
  and everything else the harness writes targets unwatched addresses or goes
  through txs (auto-marked by the scanner).
- The venue mirror uses the same list shape for V's fixture set (the canonical
  HyperEVM assets their deploy touches).

## Consequences

- `anvil_setCode` on an existing-state contract (the real USDC) is measured to
  succeed on anvil 1.8.6 — the fixture pattern works on both mirrors; the
  boot-time getProof dependency their comment mentioned does not reproduce
  against the public RPC.
- Hand-back (`/admin/shadow {clear}`) still applies: the fixture list is
  ownership, and ownership is reversible like any other mark.
- A fixture address the client *stops* replacing keeps its parent diffs skipped
  until the list is edited — the list is instance data, one edit moves it
  (and `diorama-config check` + `closure-check` read the same file).
