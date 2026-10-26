# Diorama — local, production-shaped mirrors of Ethereum mainnet and Hyperliquid

Diorama is one workspace with two mirrors: the **Mainnet Mirror** (a local
replica of Ethereum mainnet) and the **Venue Mirror** (a local replica of the
Hyperliquid venue). They are built from the same crates — one spine, two
parents — and both do the same four things:

1. **Follow the parent 1:1** — the mirror's chain stays a faithful copy of the
   real state, kept fresh by the parent's own data: Ethereum's EIP-7928 block
   access lists, the venue's public market API.
2. **Accept your writes** — a key your dapp writes is *devnet-owned* forever
   (copy-on-write): parent updates never clobber it, and a hand-back
   (`/admin/shadow`) re-syncs it from the source on demand.
3. **Mint liquidity** — `/admin/deal` discovers any ERC-20's balance slot and
   writes it silently; the venue's simulator adds account-level deals,
   funding and liquidations on the same chain.
4. **Measure the divergence** — probes, parity checks, a shared drift log and
   per-service metrics turn "did my write behave like the real market would?"
   into counters and facts.

| | Mainnet Mirror | Venue Mirror |
|---|---|---|
| parent | Ethereum mainnet, served locally by `reth` + `Lighthouse` | the Hyperliquid venue (public API + HyperEVM) |
| chain | **T** (`anvil-t`): fresh from real EIP-7928 block access lists | **V** (`anvil-v`, chain 999): fresh from the venue's poll windows |
| engine | `diorama-mainnet-engine` | `diorama-venue-engine` |
| venue plane | — | `diorama-venue-sim`: the venue API from live market data (info 8090 / control 8091) |
| admin surface | deal / shadow / epoch / rewind / probes / pause / resume / time / feed | deal / shadow / epoch / rewind / probes / pause / resume |
| namespace | `diorama-mainnet` | `diorama-venue` |

Between the two sits the **CCTP bridge**: the real Circle V2 lane (the real
contracts on both chains, a local Iris-compatible attester) that moves USDC
T ↔ V — the substrate for cross-mirror margin workflows.

```
   Mainnet Mirror                        Venue Mirror
   +---------------------+               +----------------------+
   |  reth + Lighthouse  |               |  the venue's API     |
   |  local fork source  |               |  (REST + WS, live)   |
   +----------+----------+               +----------+-----------+
              | blocks, BAL(N), receipts             | poll windows
              v                                      v
   +----------+----------+               +----------+-----------+
   |  the mirror engine  |               |  the mirror engine   |
   |  compose . COW .    |               |  compose . COW .     |
   |  journal . probes   |               |  journal . probes    |
   |  /status /metrics   |               |  /status /metrics    |
   |  /admin/*           |               |  /admin/*            |
   +----------+----------+               +----------+-----------+
              | diffs (COW)                          | diffs (COW)
              v                                      v
        +-----+-----+                          +-----+-----+
        |  anvil T  | <-- your dapp            |  anvil V  | <-- your dapp
        |  devnet   |     (HTTP/WS)            |  devnet   |     (HTTP/WS)
        +-----------+                          +-----------+
```

**Status.** Verified end to end on a single-node k3s cluster: workspace tests
271/0, `make fitness` rc=0 (lint, design assertions, config check, AST
budgets), the standing pass `make attest` (8 pods clean · log-surface 11/11 ·
engine-symmetry 21/21), the shadow audit 51/51 (eleven staged legs), the CCTP
round trip 9/9, and the benchmark session's seven portfolio studies green
(117 checks). `docs/TESTING.md` carries the full suite table;
`docs/OPERATIONS.md` the runbooks (rewind, re-pin, the shadow audit's legs).

## How a mirror works

Each mirror runs one **engine** over one fork-mode Anvil chain. The engine
composes the parent's writes, filters them through the copy-on-write
registry, applies them, verifies with probes and sampled parity, records
every anomaly in `drift.jsonl`, and exposes Prometheus metrics plus an admin
API (fork controls: rewind, pause/resume, clock offset, faucet, shadow
marking).

| | Mainnet Mirror | Venue Mirror |
|---|---|---|
| the parent's data | one real EIP-7928 block access list per block (a `debug` trace fallback covers public RPCs) | REST snapshots + WS pushes, composed into poll windows |
| the chain | T mirrors the parent block-for-block; consumers read T, or `reth` for the pristine reference within its 10,064-block state window | V mirrors the venue's read model (accounts, books, marks) plus the HyperEVM state at the fork pin |
| the venue plane | — | the simulator serves the venue's API from live market data; a COW account plane holds seeded accounts, funding and liquidations |
| the bridge | — | the Core↔EVM boundary mirrored on the same chain (the bridge credits only the token's linked contract) |
| one accepted limitation | storage proofs: fork blocks report `stateRoot = 0x0` | no replay exists: the fork keeps the source's exact state and hashes at the pin |

**Self-contained at runtime (mainnet side).** Nothing outside the namespace is
needed once bootstrapped: reth has **no P2P peers** (`--disable-discovery
--trusted-only`) and gets its chain from the local Lighthouse over the Engine
API — including the snapshot→head gap, which the engine fills with an EIP-8379
top-up loop (`diorama_source_sync_gap_blocks` falls to 0, ~10-11 blocks/s
measured). Blob custody is the CL's (blob data is not executed; the applier
never needs sidecars). The only external dependencies are bootstrap-time: the
reth snapshot download and Lighthouse's checkpoint sync (both documented in
[ARCHITECTURE](docs/ARCHITECTURE.md#source-liveness-self-contained-by-construction)).

**Freshness check (mainnet).** `diorama_reth_head` must advance with the
chain. While the top-up catches up, `diorama_source_sync_gap_blocks` counts
down; once it is 0, run `make mainnet-repin` so T starts from a live, exact
base. See
[OPERATIONS → Source liveness](docs/OPERATIONS.md#source-liveness-self-contained).

## Quickstart

Requirements: a single-node k3s cluster with `kubectl` and `podman` on the
host. The Mainnet Mirror's bootstrap downloads a reth snapshot (~50 min,
154 GB) and checkpoint-syncs Lighthouse; the Venue Mirror needs no snapshot.

Mainnet Mirror:

```bash
make ns mainnet-secrets images  # namespace + StorageClass, JWT secret, pinned images into k3s
make mainnet-bootstrap          # + snapshot download + the full stack (reth, Lighthouse, anvil-t, engine)
make status                     # pods, PVCs, engine status
make mainnet-smoke              # end-to-end checks (parity spot check included)
```

Venue Mirror (a second namespace in the same cluster):

```bash
kubectl apply -k k8s/overlays/diorama-venue   # V + the venue engine + the simulator (namespace diorama-venue)
make venue-smoke                              # V, engine, sim and bridge checks
```

The mainnet bootstrap is ordered on purpose: the reth snapshot lands in the
PVC first, then reth starts, Lighthouse checkpoint-syncs to the CL head, and
the anvil resolves its pin and the engine starts applying.

## Common operations

Mainnet Mirror:

```bash
make logs-reth              # or logs-lighthouse / logs-engine / logs-anvil-t
make mainnet-selftest       # apply real blocks' BALs against a public RPC (no reth needed)
make mainnet-product-sim    # the margin-product workflow end to end on T (leverage, live data, liquidation)
make mainnet-closure-check  # every market-data view's storage deps must be in the watchlist
make pin-show               # resolved fork pin of T
make mainnet-repin          # re-pin drill: a fresh pin; engine rebases, a new epoch starts
```

Venue Mirror:

```bash
make venue-selftest      # follow real HyperEVM windows against a public RPC (no cluster stack needed)
make venue-scenario      # epoch scenario: a write survives a same-pin reset, dies with a new pin
make venue-proof         # capability proof: mint, sim deal, crash⇒liquidation, bridge round-trip
make venue-order-proof   # order-lane proof: fills vs the real book, the market-impact invariants
make cctp-margin-cycle   # crosschain unified-margin cycle: T mint -> CCTP -> venue trade -> CCTP back
make engine-symmetry     # both engines' shared verbs answer identically (cross-mirror contract)
```

Engine admin API (each mirror's own controls):

```bash
kubectl -n diorama-mainnet exec deploy/engine -- curl -s localhost:8080/status | jq
kubectl -n diorama-mainnet exec deploy/engine -- curl -s localhost:8080/metrics
curl -X POST .../admin/rewind -d '{"blocks":5}'      # journal-based rewind of applied parent blocks
curl -X POST .../admin/shadow -d '{"address":"0x..","slots":["0x1"],"account_fields":true}'
curl -X POST .../admin/time -d '{"offset_seconds":2592000}'  # devnet clock 30 days ahead, persistent
curl -X POST .../admin/deal -d '{"token":"0x..","to":"0x..","amount":"1000000"}'  # ERC-20 faucet
curl -X POST .../admin/resume                        # clear a freeze (freeze_m/halt_all runs only)
```

Divergences are **recorded, not fatal**: a fork cannot reproduce
`blockhash(...)` values (Seaport counters are the classic case), so the
applier's sampled parity can legitimately mismatch on such blocks. Every
anomaly is written to `drift.jsonl` and counted in metrics; the next parent
block's absolute writes re-converge the state. The alarm that matters is
`diorama_t_convergence_failures_total` (probes vs reth at the applied parent
block); `verifier.sweep = "full"` re-reads every applied key for conformance
runs.

## Repository layout

```
crates/anvil,devnet,config  the spine: anvil conventions, devnet semantics, the layered config
crates/mainnet              the Mainnet Mirror project (BAL/trace composer, applier, verifier, feed bridge)
crates/venue                the Venue Mirror project (fork driver, simulator planes, API, Core<->EVM bridge)
crates/cctp                 the CCTP bridge element (Iris-compatible attestation, watcher, relayer, transfer)
bins/diorama-mainnet-engine  the Mainnet Mirror engine binary
bins/diorama-venue-engine    the Venue Mirror engine binary (fork driver)
bins/diorama-venue-sim       the venue simulator (market + account planes; info 8090 / control 8091)
bins/diorama-cctp-bridge     the CCTP bridge service (Iris 8095 / control 8096)
k8s/base/diorama-mainnet/    the mainnet base: reth, lighthouse, anvil-t, engine
k8s/base/diorama-venue/      the venue base: anvil-v, engine, sim
k8s/base/cctp-bridge/        the cross-mirror bridge (deploys beside the venue stack; reaches T cross-namespace)
k8s/base/shared/             the diorama-retain StorageClass
k8s/overlays/diorama-mainnet/  single-node k3s overlay (mainnet mirror)
k8s/overlays/diorama-venue/    the venue stack overlay (V, engine, sim, bridge, NodePorts)
k8s/bootstrap/               one-shot Jobs: reth snapshot, CL blob prune, engine selftest, re-pin wipe
scripts/                     lib-mainnet.sh + lib-venue.sh + lib-cctp.sh (the shared plumbing), the
                             mainnet suites (mainnet-smoke, mainnet-scenario, mainnet-proof,
                             mainnet-product-sim, mainnet-closure-check), the venue suites
                             (venue-smoke, venue-scenario, venue-proof, venue-order-proof),
                             the cross-mirror suites (cctp-smoke, cctp-roundtrip,
                             cctp-margin-cycle, engine-symmetry, log-surface, attest,
                             shadow-audit), and the benchmark studies (portfolio-sim,
                             basis-capture, portfolio-reaction, crosschain-margin,
                             crosschain-recovery, capital-frontier, hedged-book,
                             impact-scenarios)
docs/                        HOW-IT-WORKS.md, HOW-IT-WORKS-VENUE.md, ARCHITECTURE.md, SPINE.md, STRUCTURE.md, OPERATIONS.md, TESTING.md, INTEGRATION.md, adr/
```

## Docs

- **[docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md)** — the Mainnet Mirror
  explainer: architecture, pipelines, information flow, algorithms; its
  counterpart is
  [docs/HOW-IT-WORKS-VENUE.md](docs/HOW-IT-WORKS-VENUE.md).
- **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** — the components and data
  flow of both mirrors, the measured lean profile, design decisions, the
  config reference, source liveness, the simulator and the Core↔EVM bridge.
- **[docs/STRUCTURE.md](docs/STRUCTURE.md)** — the file-by-file map of both
  crates: which files pair, the judged asymmetries, and what the AST readings
  found.
- **[docs/SPINE.md](docs/SPINE.md)** — the spine: the information both mirrors
  carry, the interfaces they share, the complete divergence list, the event
  model, and the spec cross-references (EIP-7928/EIP-8379/reth for the
  mainnet; the Hyperliquid docs and live probes for the venue).
- **[docs/OPERATIONS.md](docs/OPERATIONS.md)** — signals/thresholds, day-to-day,
  source-liveness playbook, drills, storage/PVC handling, failure playbook,
  venue operations.
- **[docs/INTEGRATION.md](docs/INTEGRATION.md)** — the integration surface index:
  every endpoint, shape and named refusal both mirrors expose, plus the
  deliberately deferred lanes.
- **[docs/TESTING.md](docs/TESTING.md)** — test layers, per-block invariants and
  their classes, the measured facts behind the verifier, the venue smoke,
  known limitations.
- **[docs/adr/](docs/adr/README.md)** — the architecture decision records (one
  page each: context, decision, consequences).
