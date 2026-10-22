# Mainnet Mirror — local, production-shaped Ethereum mainnet mirror

A local Ethereum mainnet **fork source** (`reth` + `Lighthouse`) drives a
**mirror engine** that maintains two Anvil chains:

| Chain | What it is | Written by |
|---|---|---|
| **M** (`anvil-m`) | Every parent block replayed 1:1 and asserted per block; state re-converged from the parent BAL so a fork's inherent `blockhash` divergence never cascades | engine only |
| **T** (`anvil-t`) | The devnet: watch-list state kept fresh by applying real EIP-7928 block access lists; your writes live here | engine (diffs) + your dapp |

Consumers connect to **T** (or **M** for the pristine reference) over HTTP/WS.
The engine verifies M against reth on every block, records everything in
`drift.jsonl`, and exposes Prometheus metrics plus an admin API (fork controls:
rewind, pause/resume, clock offset, faucet, shadow marking).
The one accepted limitation is storage proofs (fork blocks report
`stateRoot = 0x0`).

```
   +---------------------+
   |  reth + Lighthouse  |
   +---------------------+
   |  local fork source  |
   +----------+----------+
              | blocks, BAL(N)
              v
   +----------+-----------------------+
   |  ENGINE                          |
   +----------------------------------+
   |  replay -> M  .  BAL diffs -> T  |
   |  verify, shadow registry,        |
   |  journal                         |
   |  API: /status, /metrics,         |
   |  /admin/*                        |
   +-------+----------------+---------+
           | replay         | diffs
           | + verify       | (COW)
           v                v
      +----+------+    +----+------+
      |  anvil M  |    |  anvil T  | <-- your dapp
      |  strict   |    |  devnet   |      (HTTP/WS)
      +-----------+    +-----------+
```
![readme overview](docs/img/readme-overview.png)

## The Venue Mirror (in the same workspace)

The same workspace also carries the **Venue Mirror**: a fork-mode Anvil of
HyperEVM (**V**, chain 999, forked from `rpc.hyperliquid.xyz/evm`) driven by
its own engine (`venue-mirror`), with a **simulator** (`venue-sim`) that serves
the venue's API from live market data and a copy-on-write account plane (info
8090, control 8091), and a bridge that mirrors the Core↔EVM boundary. The two
mirrors share the common crates (`anvil` conventions, `devnet` semantics, the
layered config) and never each other. Detail:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#the-venue-mirror).

| | Mainnet Mirror | Venue Mirror |
|---|---|---|
| chain | T (`anvil-t`, an Ethereum fork kept fresh by BAL diffs) | V (`anvil-v`, a HyperEVM fork, static between re-pins) |
| engine | `mirror-engine` | `venue-mirror` |
| venue plane | — | `venue-sim` (market + account planes, an `hl_mock` superset) |
| admin | deal / shadow / epoch / time / rewind | deal / shadow / epoch / pause / resume |
| namespace | `mainnet-mirror` | `venue-mirror` |

## Quickstart

```bash
make ns secrets images   # namespace, StorageClass, JWT, pinned images into k3s
make bootstrap           # + snapshot download (~50 min, 154 GB) + full stack
make status              # pods, PVCs, engine status
make smoke               # end-to-end checks (parity spot check included)
```

Venue Mirror (a second namespace in the same cluster; no snapshot needed):

```bash
kubectl apply -k k8s/overlays/venue-mirror   # V + venue engine + venue-sim (namespace venue-mirror)
make smoke-venue                             # V, engine, sim and bridge checks
python3 scripts/daloc-sim.py --lane all      # the daloc use-case arc (42 gates; point the
                                             # script's constants at your endpoints first)
```

The local, no-cluster shape (Anvil V + the two binaries) is in
[docs/TESTING.md](docs/TESTING.md) → "Venue smoke".

The bootstrap is ordered on purpose: the reth snapshot lands in the PVC first,
then reth starts, Lighthouse checkpoint-syncs to the CL head, and the anvils
resolve their pins and the engine starts replaying.

**Self-contained at runtime.** Nothing outside the namespace is needed once
bootstrapped: reth has **no P2P peers** (`--disable-discovery --trusted-only`)
and gets its chain from the local Lighthouse over the Engine API - including
the snapshot→head gap, which the engine fills with an EIP-8379 top-up loop
(`mirror_source_sync_gap_blocks` falls to 0, ~10-11 blocks/s measured). Blob
sidecars come from the local `--semi-supernode` Lighthouse only, with
placeholder blob proofs recomputed and verified locally with c-KZG. The only
external dependencies are bootstrap-time: the reth snapshot download and
Lighthouse's checkpoint sync (both documented in
[ARCHITECTURE](docs/ARCHITECTURE.md#source-liveness-self-contained-by-construction)).

**Freshness check:** `mirror_reth_head` must advance with the chain. While the
top-up catches up, `mirror_source_sync_gap_blocks` counts down; once it is 0,
run `make repin` so M/T start from a live, exact base. See
[OPERATIONS → Source liveness](docs/OPERATIONS.md#source-liveness-self-contained).

## Common operations

```bash
make logs-reth           # or logs-lighthouse / logs-engine / logs-anvil-t
make selftest            # replay+apply 3 real blocks against a public RPC (no reth needed)
make product-sim         # the margin-product workflow end to end on T (leverage, live data, liquidation)
make closure-check       # every market-data view's storage deps must be in the watchlist
make pin-show            # resolved fork pins of M and T
make repin               # re-pin drill: fresh pins; engine rebases, a new epoch starts
```

Engine admin API (controls that belong to the mirror itself):

```bash
kubectl -n mainnet-mirror exec deploy/engine -- curl -s localhost:8080/status | jq
kubectl -n mainnet-mirror exec deploy/engine -- curl -s localhost:8080/metrics
curl -X POST .../admin/rewind -d '{"blocks":5}'      # journal-based rewind of applied parent blocks
curl -X POST .../admin/shadow -d '{"address":"0x..","slots":["0x1"],"account_fields":true}'
curl -X POST .../admin/time -d '{"offset_seconds":2592000}'  # devnet clock 30 days ahead, persistent
curl -X POST .../admin/deal -d '{"token":"0x..","to":"0x..","amount":"1000000"}'  # ERC-20 faucet
curl -X POST .../admin/resume                        # clear a freeze (freeze_m/halt_all runs only)
```

Divergences are **recorded, not fatal**, by default: a fork cannot reproduce
`blockhash(...)` values (Seaport counters are the classic case), so the engine
classifies every divergence (structural vs behavioral), writes it to
`drift.jsonl`, counts it in metrics, and re-converges M's state from the parent
BAL before the next block. Set `loops.on_divergence: freeze_m` for conformance
runs.

## Repository layout

```
crates/anvil,devnet,config  common: anvil conventions, devnet semantics, the layered config
crates/ethereum             the Mainnet Mirror project (replay, verifier, BAL diff applier, feed bridge)
crates/hyperliquid          the Venue Mirror project (fork driver, simulator planes, API, Core<->EVM bridge)
bins/mirror-engine          the Mainnet Mirror engine binary
bins/venue-mirror           the Venue Mirror engine binary (fork driver)
bins/venue-sim              the venue simulator (market + account planes; info 8090 / control 8091)
k8s/base/                   kustomize base: reth, lighthouse, anvil-m/t, engine (+ venue/ for V, engine, sim)
k8s/overlays/mainnet-mirror/  single-node k3s overlay (mainnet mirror)
k8s/overlays/venue-mirror/    the venue stack overlay (V, engine, sim, NodePorts)
k8s/bootstrap/              one-shot Jobs: reth snapshot, engine selftest, re-pin wipe
scripts/                    lib.sh, smoke.sh, smoke-venue.sh, scenario.sh, capability-proof.sh,
                            product-sim.sh, closure-check.sh, closure-matrix.sh, daloc-sim.py
docs/                       HOW-IT-WORKS.md, ARCHITECTURE.md, OPERATIONS.md, TESTING.md, adr/
```

## Docs

- **[docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md)** — the short technical
  explainer: architecture, pipelines, information flow, algorithms.
- **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** — components, data flow, the
  measured lean profile, design decisions (convergence repair, divergence
  policy, EIP-7702 nonce gaps, COW, journal), config reference, source
  liveness, and the Venue Mirror (fork driver, simulator, Core↔EVM bridge,
  feed bridge).
- **[docs/OPERATIONS.md](docs/OPERATIONS.md)** — signals/thresholds, day-to-day,
  source-liveness playbook, drills, storage/PVC handling, failure playbook,
  venue operations.
- **[docs/TESTING.md](docs/TESTING.md)** — test layers, per-block invariants and
  their classes, the measured facts behind the verifier, the venue smoke, the
  daloc use-case simulation, known limitations.
- **[docs/adr/](docs/adr/README.md)** — the architecture decision records (one
  page each: context, decision, consequences).
