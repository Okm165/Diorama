# 0001 — Two engine binaries over one profile-routing binary

**Status:** accepted (R3).

## Context

The design sketched **one** engine binary, `bins/mirror-engine`, with a
`profile: ethereum | hyperliquid` flag selecting the behavior. The workspace
instead has two projects (`crates/mainnet`, `crates/venue` — one
vocabulary across the tree: the projects, the `mainnet`/`venue` profiles,
the bins and the namespaces) that share
the common crates (`anvil`, `config`, `devnet`) and never each other, plus a
simulator service whose closure must stay narrow. Measured: with one package,
every build compiled the whole graph; a profile-routing binary
would link both projects into every venue build, and `venue-sim` must never
compile the mainnet stack.

## Decision

One binary per profile: `bins/diorama-mainnet-engine` (depends only on
`diorama-mainnet`) and `bins/diorama-venue-engine` (only on `diorama-venue`), plus
`bins/diorama-venue-sim` for the simulator. The names are mirror-qualified (`mainnet-`
/ `venue-`, matching the `diorama-mainnet` / `diorama-venue` instances) so
neither mirror is the unqualified default; the deployments are both `engine`
in their own namespace. The allowed internal edges are declared (`venue-engine -> diorama-venue + devnet
+ anvil + config`, no ethereum edge; `venue-sim` likewise) - the structural
contract is a law, not a convention.

## Consequences

- A venue change cannot break the ethereum build and vice versa; the
  dependency edges keep the two graphs apart.
- The bins are thin shells over their project crates: the entrypoint (CLI,
  boot order, wiring) lives in the crate's `main_entry` (a crate hosting a
  second service exposes `<service>_entry` — the venue crate's `sim_entry`);
  the bin is the shared runtime shell's call site. All four bins
  (`mainnet-engine`, `venue-engine`, `venue-sim`, `cctp-bridge`) are the same
  shell and declare the same two dependencies (`diorama-config` + their crate).
  The service CLIs share one shape (`run --config <path>`,
  `diorama_config::boot::service_config`).
- Two entrypoints and two manifests to keep aligned - the shared `Instance`
  loader and the per-instance TOML are the single home of the config.
- The plan's single-bin sketch is superseded; the two projects stay siblings
  (no cross edges, ever).
