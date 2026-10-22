# How the mirror works — technical explainer

A short, precise walk-through of the architecture, the pipelines, the
information flow, and the algorithms. Everything stated here is measured; the
operational layer lives in [OPERATIONS](OPERATIONS.md), the component details in
[ARCHITECTURE](ARCHITECTURE.md), the evidence in [TESTING](TESTING.md).

## 1. What it is

A local, production-shaped mirror of Ethereum mainnet. A local `reth` +
`Lighthouse` pair follow the real chain; a Rust **engine** replays every parent
block 1:1 onto **Anvil M** (strict; only the engine writes; every block is
verified) and keeps a second chain, **Anvil T**, fresh by applying the real
EIP-7928 **block access lists** — per-key, copy-on-write. Your transactions and
venue simulations live on T, next to live mainnet state, while M stays a pristine
reference. At runtime nothing outside the namespace is needed: reth has no P2P
peers and is fed by the local Lighthouse over the Engine API.

| | **M** (`anvil-m`) | **T** (`anvil-t`) |
|---|---|---|
| Contents | every parent block replayed 1:1 | the parent's *state* (watch-list BAL diffs) + devnet txs |
| Written by | the engine only | the engine (diffs) + your dapp |
| Guarantee | per-block verification + probe parity vs reth | state parity vs reth at the applied parent, except keys you own |
| Use | reference / conformance | development, venue sims, your product |

## 2. Topology

```
                          ┌───────────────────┐
   mainnet                │  Lighthouse (CL)  │────┐
   (bootstrap only) ─────►│  semi-supernode   │    │ beacon API
                          │  checkpoint sync  │    │ (blob sidecars)
                          └──────────┬────────┘    │
                                     │ Engine API  │
                                     ▼             │
                          ┌────────────────────┐   │
                          │  reth (EL)         │   │
                          │  pruned full node  │   │
                          └──────────┬─────────┘   │
 blocks · BAL(N) · receipts · state  │             │
                                     ▼             ▼
   ┌─────────────────────────────────────────────────────────────┐
   │  ENGINE                                                     │
   │  replay loop → M          ·          BAL diff applier → T   │
   │  verifier · convergence repair · shadow registry · journal  │
   │  API: /status · /metrics · /admin/*                         │
   └─────────┬───────────────────────────────────┬───────────────┘
             │ 1:1 replay + verification         │ watch-list BAL diffs (COW)
             │ (every parent block)              │ (copy-on-write)
             ▼                                   ▼
      ┌───────────┐                       ┌───────────┐
      │  anvil M  │                       │  anvil T  │ ◄── your dapp
      │   strict  │                       │   devnet  │     (HTTP / WS)
      └───────────┘                       └───────────┘
```

Everything above lives in one namespace (`mainnet-mirror`); the engine never
talks P2P, and the only arrows into it are reth (blocks, state, BALs) and the
beacon API (blob sidecars).

## 3. Information flow

**Mainnet → mirror.** Lighthouse follows the chain (checkpoint sync + backfill)
and feeds reth over the authenticated Engine API; reth executes and stores
blocks, receipts, and state. The engine polls reth for the follow head and, per
block, pulls: the full block + raw transactions, `eth_getBlockAccessList(n)`,
receipts/logs for verification, and state at historical blocks for probes and
diffs. Blob sidecars come from the local Lighthouse only (it runs as a
PeerDAS *semi-supernode*, so it can reconstruct every blob in the availability
window); Fulu-era placeholder proofs are recomputed and KZG-verified locally
before a blob tx is used. A block whose blobs cannot be fully reconstructed is
**never mined**: M holds and retries.

**Mirror → M.** The engine rebuilds each parent block (env + withdrawals + raws)
and mines it; then verifies it against reth field by field. Divergences are
classified and recorded; M's state is re-converged from the parent BAL so a
fork's inherent `blockhash` divergence never cascades.

**Mirror → T.** The parent's BAL entries for watched accounts become batched
Anvil cheatcode writes, the block context is aligned, and the parent's block is
mined on T. Devnet transactions are mined in their **own** block *before* the
diffs (the prepend rule, §5 step 0).

**Devnet → registry.** A shadow scanner watches T's new blocks: every storage
key or account field your transactions wrote is marked *devnet-owned*
(copy-on-write) and its latest value is stored durably. From then on, parent
BAL writes for those keys are skipped, and the value registry is what restores
your state if T re-forks.

## 4. The M pipeline (per parent block `n`)

1. **Convergence repair.** Apply `BAL(n-1)` for the watched accounts to M as
   cheatcode writes (unmined), then prove it: the 12 configured probes on M vs
   reth at `n-1`. Gated on the previous block's verification passing. Reason: a
   fork cannot reproduce `blockhash()`-dependent values (Seaport counters are
   the classic case); one such divergence would otherwise cascade into invalid
   transactions and permanently wrong state.
2. **Block env.** timestamp · coinbase · prevRandao · gas limit · base fee.
3. **Withdrawals.** `setBalance(pre + amount)`, idempotently.
4. **Raw transactions.** Rebuilt from the parent block (blob sidecars attached),
   submitted one request at a time in block order, chunked ≤500.
5. **Mine.** `anvil_mine(1)`.
6. **Verify.** tx count and order · `blobGasUsed` · receipts (count, status, gas:
   exact except EIP-7702 type-4, whose delta must lie in
   `[−12,500 × #auths, 0]`) · `logsBloom` · full log set · withdrawals · the 8
   state probes.
7. **Classify.** Structural divergences (tx count, tx order, blob gas,
   withdrawal count) count in `mirror_divergences_total` and can freeze M
   (`on_divergence: freeze_m` for conformance runs). Behavioral ones (receipt
   statuses, gas, logs, supplies - the blockhash-gated A17 class) count in
   `mirror_value_divergences_total` and land in `drift.jsonl`; the next block's
   repair re-converges the state. A *real* execution problem cannot hide behind
   that: it shows up as a convergence-probe failure (`mirror_m_convergence_*`),
   which is why the receipts check is classified behavioral.
8. **Cursor + metrics.** Advance, publish.

## 5. The T pipeline (per parent block `n`)

0. **Prepend (bounded loop, ≤3 rounds).** Mine the devnet's pending
   transactions in their own block(s), `timestamp = last + 1`, *before* the
   parent's diffs. Two reasons: the replayed block must contain only the
   parent's effects, and Anvil estimates gas at submission — if the diffs land
   between estimate and execution, the transaction executes against a different
   state and can run out of gas (measured: a borrow estimated 257,560, used
   271,389, reverted at 97% of its limit).
1. **Fetch** `BAL(n)` from reth; filter to the watch list.
2. **Skip shadowed keys** — copy-on-write: your state is not mainnet's.
3. **Pad** every hex quantity to 32 bytes (Anvil rejects odd-length storage
   values; 58% of raw BAL values are odd).
4. **Write** in batches ≤500: `anvil_setStorageAt` / `setBalance` / `setNonce` /
   `setCode`.
5. **Align the block context** — timestamp (`max(parent + time_offset, last+1)`,
   monotonic by construction), coinbase, prevRandao, gas limit, base fee.
   Time-dependent views (Aave's indexes) only match mainnet with the parent's
   clock.
6. **Mine.** `anvil_mine(1)` — the parent's block on T.
7. **Journal + prune.** Before-values recorded per block (ring).
8. **Sampled parity.** 8 storage keys from the filtered set vs reth at `n` (the
   drift lens).
9. **Probe parity on T.** The same 12 probes against T@latest vs reth at `n`;
   devnet-owned targets are skipped, and the check is suspended while a clock
   offset is set (views are then *meant* to differ). The applier scans its own
   devnet blocks (the prepend) *before* the probes run, so a write made in this
   very apply is already marked and cannot fire a spurious alarm. Alarm:
   `mirror_t_convergence_failures_total` — it stays silent while a manipulation
   lives on a devnet-owned account, and fires only when the divergence is
   visible through a contract the devnet does not own (cumulative; watch
   `_last_failed`).
10. **Cursor + metrics.** The cursor is keyed to the parent height and carries
    the T block's hash for re-fork detection.

## 6. Core algorithms

### 6.1 Copy-on-write shadowing

A key becomes devnet-owned the moment a devnet transaction writes it. The
registry (`shadow.json`) keeps two things: the **marks** (which keys/accounts are
yours — parent BAL writes for them are skipped forever) and the **latest
values** (so your state can be rebuilt from scratch). Unshadowing
(`/admin/shadow`) clears both and *immediately* pushes mainnet's current values
(read from reth at the T cursor) back to T — deterministic, not deferred to the
next diff; before clearing, the engine scans T up to its head so a devnet write
mined moments ago cannot re-mark the key after the re-sync (the "poisoned
source" race). `/status.shadowed` lists the devnet-owned accounts — parity
checks must skip them, which is why the scenario's probes are COW-aware.

### 6.2 Fork pins and re-basing

Both Anvils fork reth at `reth_head − FORK_OFFSET` (128). M re-resolves the pin
on every *container* start (an OOM restart re-forks at the live head and
re-replays only the pin window, instead of replaying an aging fork until the
next OOM — the measured memory death spiral). T re-resolves the same way, and
its devnet state is rebuilt from the shadow registry. **M's re-base never
touches T**: the two chains share a source, not a lifetime. A re-base *skips*
the range between the old cursor and the new pin (bounded by `FORK_OFFSET`):
those blocks are mainnet's own — the fork base makes the state exact by
construction — but they are not re-executed, and
`mirror_m_rebase_skipped_blocks_total` plus a WARN naming the range make that
visible rather than silent.

### 6.3 Re-fork detection and devnet-state restore

T's cursor carries the hash of its block at the cursor height. When the chain
under it is re-forked (container restart, re-pin, OOM), the hash at that height
no longer matches → the engine restores: re-applies the registry's recorded
values, keeps the marks, *unshadows* keys with no recorded value (they can no
longer be protected), and clears the journal. State survives; transaction
history does not.

### 6.4 Reorg detection and the gap guard

M-only parent-hash continuity check (`mirror_reorgs_detected_total`); a
discontinuity triggers the self-healing re-base. The gap guard pauses the
engine when the follow head is more than `max_gap_blocks` (300) ahead of the
cursor — except a cursor that was just rebased, which may replay its whole pin
window (bounded by reth's 10,064-block state window).

### 6.5 The devnet clock offset

`POST /admin/time {"offset_seconds": N}` makes T's clock run N seconds ahead of
mainnet, persistently: each parent block aligns to `parent + offset`, clamped to
`last + 1` so timestamps stay monotonic whatever the offset does. This is how a
margin product tests interest accrual, expiry, and funding on *live* data
(measured: +30 days moved Aave's USDC income index by a month of interest and
grew a borrow's debt in the same read, with the clock still following mainnet).

### 6.6 The faucet (`/admin/deal`)

Unlimited funds for any ERC-20 without knowing its storage layout: the engine
probes candidate balance slots (0..16) with `eth_call` + a `stateDiff`
override holding a sentinel value, keeps the slot that changes `balanceOf`, then
writes it via `anvil_setStorageAt` and marks it devnet-owned. ETH is
`anvil_setBalance`.

### 6.7 Journal and rewind

Every applied parent block records its before-values. `POST /admin/rewind
{"blocks": N}` rolls T back through the journal (and re-enters the replay of the
rewound range). The journal is a ring (1024 blocks); older history is not
rewindable.

## 7. State, storage, and durability

| Where | Holds | Survival |
|---|---|---|
| `reth-data` PVC | the EL database (pruned full) | Retain |
| `lighthouse-data` PVC | checkpoint sync + backfill (semi-supernode) | Retain |
| `engine-state` PVC | `/state`: cursors, `shadow.json` (marks + values), journal, `drift.jsonl` | Retain |
| anvil M/T PVCs | fork RPC caches, pins | Retain |

The StorageClass is `mirror-retain`: deleting the namespace never deletes the
data. The engine resumes from its cursors after a restart; M re-forks from a
fresh pin (cheap by design), T restores its devnet state from the registry.

## 8. Invariants asserted every block

- tx-count and tx-order parity on M
- `blobGasUsed` parity
- `logsBloom` parity and full log-set equality
- receipt count, statuses, and gas (type-4 within the refund rule)
- withdrawal totals
- 12 state probes on M (post-repair) and on T (post-alignment, COW-aware)
- 8 sampled storage keys on T (drift lens)

## 9. Alarms that matter

| Metric | Meaning |
|---|---|
| `mirror_lag_blocks` > 60 (steady 2–3) | M is holding (blobs) or reth/CL is unhealthy |
| `mirror_divergences_total` rising | structural divergence — inspect `drift.jsonl` |
| `mirror_m_convergence_failures_total` > 0 | M's repaired state is not mainnet's |
| `mirror_t_convergence_failures_total` > 0 | T drifted on a key you do not own |
| `mirror_reorgs_detected_total` rising | upstream reorg — the engine re-bases |

Full reference and the playbook: [OPERATIONS](OPERATIONS.md).

## 10. Known limits (measured, accepted)

- **Storage proofs**: fork blocks report `stateRoot = 0x0`; contracts that verify
  Merkle proofs need the harness's mock.
- **`blockhash()`-dependent values** cannot be reproduced by any fork; they are
  detected as value-level divergences and repaired from the parent BAL.
- **T re-forks lose transaction history** (state is restored from the shadow
  registry).
- Both chains are single-sequencer devnets: no consensus, the engine orders
  blocks.
