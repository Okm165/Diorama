# How the mirror works — technical explainer

A short, precise walk-through: architecture, pipelines, information flow,
algorithms. Every number here is measured; operations live in
[OPERATIONS](OPERATIONS.md), component detail in [ARCHITECTURE](ARCHITECTURE.md),
evidence in [TESTING](TESTING.md).

## 1. What it is

A local, production-shaped mirror of Ethereum mainnet. A local `reth` +
`Lighthouse` pair follow the real chain; a Rust **engine** replays every parent
block 1:1 onto **Anvil M** (strict, engine-only writes, every block verified)
and keeps **Anvil T** fresh by applying the real EIP-7928 block access lists —
per key, copy-on-write. Your transactions and venue simulations live on T, next
to live mainnet state. At runtime nothing outside the namespace is needed: reth
has no P2P peers and is fed by the local Lighthouse over the Engine API.

| | M (`anvil-m`) | T (`anvil-t`) |
|---|---|---|
| Contents | every parent block replayed 1:1 | the parent's *state* (watch-list BAL diffs) + devnet txs |
| Written by | the engine only | the engine (diffs) + your dapp |
| Guarantee | per-block verification + probe parity vs reth | state parity vs reth at the applied parent, except keys you own |
| Use | reference / conformance | development, venue sims, your product |

## 2. Topology

```
   mainnet (bootstrap only)
             | checkpoint sync
             v
   +---------+---------+
   |  Lighthouse (CL)  |
   +-------------------+
   |  semi-supernode   |--------+
   +---------+---------+        | beacon API
             | Engine API       | (blob sidecars)
             v                  |
   +---------+----------+       |
   |  reth (EL)         |       |
   +--------------------+       |
   |  pruned full node  |       |
   +---------+----------+       |
             |                  |
             | blocks, BAL(N)   |
             | receipts, state  |
             v                  v
   +---------+------------------+-----+
   |  ENGINE                          |
   +----------------------------------+
   |  replay -> M  .  BAL diffs -> T  |
   |  verifier, convergence repair,   |
   |  shadow registry, journal        |
   |  API: /status, /metrics,         |
   |  /admin/*                        |
   +-------+----------------+---------+
           | 1:1 replay     | diffs (COW)
           | + verify       | (copy-on-write)
           v                v
      +----+------+    +----+------+
      |  anvil M  |    |  anvil T  | <-- your dapp
      |  strict   |    |  devnet   |      (HTTP/WS)
      +-----------+    +-----------+

   both fork reth at reth_head - 128
```
![topology](img/how-topology.png)

One namespace (`mainnet-mirror`). The engine never talks P2P; its only inputs
are reth (blocks, state, BALs, traces) and the beacon API (blob sidecars).

## 3. Information flow

**Mainnet → mirror.** Lighthouse follows the chain (checkpoint sync + backfill)
and feeds reth over the authenticated Engine API. Per block the engine pulls
from reth: the full block + raw transactions, `eth_getBlockAccessList(n)`,
receipts/logs for verification, and state at historical blocks for probes and
diffs. Blob sidecars come only from the local Lighthouse — a PeerDAS
*semi-supernode* that can reconstruct every blob in the availability window;
Fulu-era placeholder proofs are recomputed and KZG-verified locally. A block
whose blobs cannot be fully reconstructed is **never mined**: M holds and
retries.

**Mirror → M.** Rebuild the parent block (env + withdrawals + raws), mine it,
verify it against reth field by field. Divergences are classified and recorded;
the state is re-converged from the parent BAL, so a fork's inherent
`blockhash` divergence never cascades.

**Mirror → T.** The parent's BAL entries for watched accounts become batched
Anvil cheatcode writes; the block context is aligned; the parent's block is
mined. Devnet transactions are mined in their own block *before* the diffs
(the prepend rule).

**Devnet → registry.** A shadow scanner watches T's blocks: every key your
transactions wrote is marked *devnet-owned* (copy-on-write) and its latest
value is stored durably — that registry is what restores your state when T
re-forks.

## 4. The M pipeline (per parent block `n`)

1. **Convergence repair.** Apply `BAL(n-1)` for watched accounts as cheatcode
   writes (unmined) and prove it: the 12 probes on M vs reth at `n-1`. Gated on
   the previous block's verification. Reason: no fork can reproduce
   `blockhash()`-dependent values (Seaport counters are the classic case), and
   one such divergence would cascade.
2. **Block env.** timestamp · coinbase · prevRandao · gas limit · base fee.
3. **Withdrawals.** `setBalance(pre + amount)`, idempotently.
4. **Raw transactions.** Rebuilt from the parent block (blob sidecars
   attached), one request at a time in block order, chunked ≤500.
5. **Mine.** `anvil_mine(1)`.
6. **Verify.** tx count and order · `blobGasUsed` · receipts (count, statuses,
   gas: exact except EIP-7702, whose delta must lie in `[−12,500 × #auths, 0]`)
   · `logsBloom` · full log set · withdrawals · the 12 probes.
7. **Classify.** *Structural* divergences (tx count/order, blob gas,
   withdrawals) count in `mirror_divergences_total` and can freeze M
   (`on_divergence: freeze_m`). *Behavioral* ones (statuses, gas, logs,
   supplies — the blockhash-gated class) count in
   `mirror_value_divergences_total` and land in `drift.jsonl`; the next repair
   re-converges the state. A real execution problem cannot hide: it surfaces as
   a convergence-probe failure.
8. **Cursor.** Advance and publish metrics.

## 5. The T pipeline (per parent block `n`)

0. **Prepend (bounded, ≤3 rounds).** Mine pending devnet transactions in their
   own block(s), timestamp `last + 1`, before the diffs — so the replayed block
   carries only the parent's effects, and a transaction executes against the
   same state its gas was estimated on (measured: a borrow estimated pre-diff
   reverted at 97% of its limit).
1. **Fetch** `BAL(n)` from reth, filter to the watch list.
2. **Skip devnet-owned keys** — copy-on-write: your state is not mainnet's.
3. **Pad** every slot and value to 32 bytes (anvil rejects anything else).
4. **Write** in batches ≤500: `anvil_setStorageAt` / `setBalance` / `setNonce` /
   `setCode`.
5. **Align the context** — timestamp `max(parent + time_offset, last + 1)`,
   coinbase, prevRandao, gas limit, base fee. Time-dependent views (Aave's
   indexes) only match mainnet with the parent's clock.
6. **Mine.** `anvil_mine(1)` — the parent's block on T.
7. **Journal** before-values per block (ring; what `/admin/rewind` restores).
8. **Parity.** 8 sampled storage keys, then the 12 probes: T@latest vs reth at
   `n`. Devnet-owned targets are skipped (counted in
   `mirror_t_probe_skipped_total` - a devnet write to a shared contract hides
   all of that contract's probes until it is handed back), and the applier
   scans its own prepend blocks first, so a write made in this very apply is
   already marked and cannot fire a spurious alarm. Suspended while a
   `/admin/time` offset is set.
9. **Cursor.** Keyed to the parent height; carries the T block's hash for
   re-fork detection.

## 6. Core algorithms

### 6.1 Copy-on-write shadowing

A key becomes devnet-owned the moment a devnet transaction writes it. The
registry (`shadow.json`) keeps the **marks** (parent diffs skip those keys
forever) and the **latest values** (state is rebuilt from them after a re-fork).

```
   parent diff for key K -> devnet-owned?
                            +-- no  -> write mainnet's
                            +-- yes -> keep devnet's
```
![copy-on-write decision](img/how-cow.png)

Unshadowing (`/admin/shadow`) clears both and *immediately* pushes reth's values
at the cursor back to T; the engine scans T up to its head first, so a
just-mined write cannot re-mark the key (the poisoned-source race).

### 6.2 Watchlist closure

Everything whose mainnet state matters must be watched — and watching a *proxy*
is not enough: Chainlink feeds delegate to their current aggregator, Aave's
WBTC source is *composite* (`BTC/USD × WBTC/BTC`), and the wstETH source reads
stETH's share rate. `make mainnet-closure-check` traces every market-data view's storage
reads on reth and flags unwatched dependencies. A watchlist addition only
affects *future* writes; heal a gap by writing reth's values for the slots the
view reads.

### 6.3 Fork pins and re-basing

Both anvils fork reth at `reth_head − FORK_OFFSET` (128), re-resolved on every
*container* start — an OOM restart re-forks at the live head and re-replays only
the pin window. A re-base skips the range between the old cursor and the new pin
(bounded by `FORK_OFFSET`): those blocks are mainnet's own — the state is exact
by construction — but not re-executed, which
`mirror_m_rebase_skipped_blocks_total` counts and a WARN names. **M's re-base
never touches T.**

### 6.4 Re-fork detection and epochs

T's cursor carries the hash of its block at the cursor height; a mismatch means
the chain was re-forked (OOM restart, re-pin). A *changed* fork pin is a new
epoch: the base moved and the recorded writes are derivations of the old base
(plan 2.2/6.3), so the engine clears the registry - marks *and* values - and
the product re-creates its positions from its own journal (plan 3.6). A reset
at the *same* pin restores the recorded writes (the base is identical); orphan
values are always pruned, never re-owned. The journal is cleared either way.

### 6.5 Reorg and gap guard

M-only parent-hash continuity check; a discontinuity re-bases M behind the
follow head (self-healing, ~1–2 min). The gap guard pauses when the follow head
is more than `max_gap_blocks` (300) ahead of the cursor — except a just-rebased
cursor, which may replay its whole pin window (bounded by reth's 10,064-block
state window).

### 6.6 The devnet clock

`POST /admin/time {"offset_seconds": N}` makes T's clock run N seconds ahead,
persistently: each parent block aligns to `parent + offset`, clamped to
`last + 1` so timestamps stay monotonic. This is how a margin product tests
interest, expiry, and funding on live data (measured: +30 days moved Aave's USDC
income index by a month of interest, debt grew with it, mainnet kept flowing).

### 6.7 Faucet

`POST /admin/deal` finds any ERC-20's balance slot by probing candidates with
`eth_call` + a `stateDiff` override (sentinel value, slots 0..16), writes it
silently, and marks it devnet-owned. ETH is `anvil_setBalance`.

### 6.8 Journal and rewind

Every applied parent block records its before-values; `POST /admin/rewind
{"blocks": N}` rolls T back through the journal and re-enters the replay. The
journal is a ring (1024 blocks); older history is not rewindable.

## 7. State and durability

| Where | Holds | Survival |
|---|---|---|
| `reth-data` | the EL database (pruned full, ~300 GB) | Retain |
| `lighthouse-data` | checkpoint sync + backfill (~200 GB) | Retain |
| `engine-state` | cursors, `shadow.json` (marks + values), journal, `drift.jsonl` | Retain |
| anvil M / T | fork RPC caches, pins | Retain |

StorageClass `mirror-retain`: deleting the namespace never deletes data. The
engine resumes from its cursors; M re-forks from a fresh pin; T restores its
devnet state from the registry.

## 8. Alarms that matter

| Metric | Meaning |
|---|---|
| `mirror_lag_blocks` > 60 (steady 2–3) | M is holding (blobs) or reth/CL is unhealthy |
| `mirror_divergences_total` rising | structural divergence — inspect `drift.jsonl` |
| `mirror_m_convergence_failures_total` > 0 | M's repaired state is not mainnet's |
| `mirror_t_convergence_failures_total` > 0 | T drifted on a key you do not own |
| `mirror_reorgs_detected_total` rising | upstream reorg — the engine re-bases |

Full reference and playbook: [OPERATIONS](OPERATIONS.md).

## 9. Known limits (measured, accepted)

- **Storage proofs**: fork blocks report `stateRoot = 0x0`; contracts that
  verify Merkle proofs need the harness's mock.
- **`blockhash()`-dependent values** cannot be reproduced by any fork; they are
  detected as behavioral divergences and repaired from the parent BAL.
- **T re-forks lose transaction history** (state is restored).
- Both chains are single-sequencer devnets: no consensus; the engine orders
  blocks.
