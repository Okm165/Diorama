# Mainnet Mirror — local, production-shaped Ethereum mainnet mirror

A local Ethereum mainnet **fork source** (`reth` + `Lighthouse`) drives a
**mirror engine** that maintains two Anvil chains:

| Chain | What it is | Written by |
|---|---|---|
| **M** (`anvil-m`) | Every parent block replayed 1:1 and asserted per block; state re-converged from the parent BAL so a fork's inherent `blockhash` divergence never cascades | engine only |
| **T** (`anvil-t`) | The devnet: watch-list state kept fresh by applying real EIP-7928 block access lists; your writes live here | engine (diffs) + your dapp |

Consumers connect to **T** (or **M** for the pristine reference) over HTTP/WS.
The engine verifies M against reth on every block, records everything in
`drift.jsonl`, and exposes Prometheus metrics plus a Tenderly-style admin API.
The one accepted limitation is storage proofs (fork blocks report
`stateRoot = 0x0`).

```
reth + Lighthouse ──► ENGINE ──► anvil-m (replay + verify + BAL convergence)
  blocks/receipts       │ replay   anvil-t (watch-list BAL diffs + your writes)
  BAL(N) / traces       └ verify   drift.jsonl · journal · /status /metrics
```

## Quickstart

```bash
make ns secrets          # namespace, StorageClass, reth<->lighthouse JWT
make bootstrap           # + snapshot download (~50 min, 154 GB) + full stack
make status              # pods, PVCs, engine status
make smoke               # end-to-end checks (parity spot check included)
```

The bootstrap is ordered on purpose: the reth snapshot lands in the PVC first,
then reth starts, Lighthouse checkpoint-syncs to the CL head, and the anvils
resolve their pins and the engine starts replaying.

**Freshness check:** the mirror is only as fresh as `mirror_reth_head`. If reth
is stuck at the snapshot block (no P2P gap-fill), the mirror faithfully mirrors
the snapshot's state – see
[OPERATIONS → Source liveness](docs/OPERATIONS.md#source-liveness) for the
options (re-snapshot, open P2P, or point the mirror at a public RPC).

## Common operations

```bash
make logs-reth           # or logs-lighthouse / logs-engine / logs-anvil-t
make selftest            # replay+apply 3 real blocks against a public RPC (no reth needed)
make pin-show            # resolved fork pins of M and T
make repin               # re-pin drill (new epoch; engine rebases automatically)
```

Engine admin API (controls that belong to the mirror itself):

```bash
kubectl -n mainnet-mirror exec deploy/engine -- curl -s localhost:8080/status | jq
kubectl -n mainnet-mirror exec deploy/engine -- curl -s localhost:8080/metrics
curl -X POST .../admin/rewind -d '{"blocks":5}'      # journal-based rewind of applied parent blocks
curl -X POST .../admin/shadow -d '{"address":"0x..","slots":["0x1"],"account_fields":true}'
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
engine/                 Rust engine (replay + verifier + BAL diff applier + convergence repair)
k8s/base/               kustomize base: reth, lighthouse, anvil-m/t, engine
k8s/overlays/local/     single-node k3s overlay (engine image tag)
k8s/bootstrap/          one-shot Jobs: reth snapshot, engine selftest, re-pin wipe
scripts/                gen-secret.sh, smoke.sh
docs/                   ARCHITECTURE.md, OPERATIONS.md, TESTING.md
```

## Docs

- **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** — components, data flow, the
  measured lean profile, design decisions (convergence repair, divergence
  policy, EIP-7702 nonce gaps, COW, journal), config reference, source liveness.
- **[docs/OPERATIONS.md](docs/OPERATIONS.md)** — signals/thresholds, day-to-day,
  source-liveness playbook, drills, storage/PVC handling, failure playbook.
- **[docs/TESTING.md](docs/TESTING.md)** — test layers, per-block invariants and
  their classes, the measured facts behind the verifier, known limitations.
