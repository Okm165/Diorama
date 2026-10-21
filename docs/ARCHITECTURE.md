# Architecture

## Components

| Component | Image / build | Role | Measured footprint |
|---|---|---|---|
| `reth` | `ghcr.io/paradigmxyz/reth:v2.7.0` | Source of truth: head state, blocks, receipts, EIP-7928 BALs, `debug` traces | ~300 GB disk, ~2.6 GB RAM (idle) |
| `lighthouse` | `sigp/lighthouse:v8.3.0` | Consensus driver: checkpoint sync, drives reth's head via the engine API (no validator) | ~25 GB disk, ~5 GB RAM synced (16Gi limit) |
| `anvil-m` | `ghcr.io/foundry-rs/foundry:v1.8.5` | Mirror chain (M): replays every parent tx, then re-converges from the BAL | ~15-25 MB per replayed block (24Gi limit) |
| `anvil-t` | same | Devnet chain (T): watch-list state + your writes, never replays parent txs | ~2.5 GB after thousands of applied blocks (12Gi limit) |
| `engine` | `localhost/mirror-engine:v1` (this repo) | Replay loop, verifier, BAL diff applier, convergence repair, journal, admin API | ~100 MB RAM |

## Data flow

```
                 eth_getBlockByNumber(N, full)                eth_getBlockReceipts(N)
reth  ──────────────────────────────────────►  replay loop ─────────────────────────► verify vs M
  │                                                 │
  │  eth_getBlockAccessList(N)                      │  send raws in order (sequential), anvil_mine(1)
  │  (fallback: debug_traceBlockByNumber prestate)  │
  ├──────────────────────────────────────────►  M convergence repair (BAL N-1, before replay)
  │                                                 │
  │  eth_getBlockAccessList(N)                      ▼
  └──────────────────────────────────────────►  diff applier ──► T (batched cheatcodes, mine 1)
                                                    │  watch list → skip shadowed → pad → write
                                                    │  align timestamp/baseFee/coinbase/prevrandao
                                                    └─ drift.jsonl + journal/<block>.jsonl
```

Per replayed block on M the engine does, in order:

0. **Convergence repair** – apply the parent BAL (`N-1`) to M as cheatcode writes,
   then prove it with the configured probes (`eth_call` on M vs reth at `N-1`).
1. Block env: timestamp, coinbase, prevRandao, gas limit, base fee (all five fields).
2. Withdrawals credited idempotently (`setBalance(pre + amount)`).
3. Raw txs built from the parent block (blob sidecars from the beacon API).
4. Txs sent one request at a time (pool order is load-bearing), then `anvil_mine(1)`.
5. Verification: tx count/order, blob gas, receipts (count/status/gas), logsBloom,
   full log set, withdrawals, probes.
6. Drift entry (`drift.jsonl`) + cursor + metrics.

## The lean profile (measured, not guessed)

**Snapshot** (`reth download --minimal`): 154 GB download → ~287 GB on disk.
State (266 GB), headers (14 GB), last 50k blocks of transactions (2 GB) +
receipts (1 GB) + changesets (1 GB). Chosen over `--full` (728 GB / 990 GB)
because the mirror only replays *live* blocks.

**Runtime pruning** (`k8s/base/reth-config.yaml`):

| Segment | Retained | Why |
|---|---|---|
| account + storage history | 10,064 blocks | reth's minimum unwind-safe distance; the Anvil pin (`head-2000`) lives inside it |
| receipts | 20,000 blocks | verifier slack + `eth_getLogs` debugging |
| bodies | 20,000 blocks | replayable catch-up backlog after an engine restart |
| senders, tx lookup | pruned | the mirror never serves historical lookups (a pruned lookup is why `eth_getTransactionByHash`/`getTransactionReceipt(hash)` return `null` on reth; `eth_getBlockReceipts` still works) |

**Lighthouse**: checkpoint sync (no genesis sync), no validator, no light-client
server, no QUIC/UPnP, 40 target peers. Measured: the initial sync + state caches
need >6 GiB (it OOM-looped at 6Gi) – the deployment requests 2Gi and limits 16Gi.

**Anvil `--prune-history`** is a memory knob, not a disk knob: it keeps up to N
full *state snapshots* in memory for rollback/tracing. Measured with 1000:
anvil-t reached 9.8 GB in minutes and anvil-m OOM'd at 12Gi. Now: T keeps 32,
M keeps 16. Tracing a historical block on M is therefore limited to the last
~16 replayed blocks; T's own journal (`/admin/rewind`) is engine-side and
unaffected.

**State dumps**: M's dump is ~350 MB (it replays every parent tx), T's is ~27 MB
(watch list only). M dumps every 300 s, T every 60 s. A restored dump is a
*stale* state: the engine detects the backward head jump and rebases, then the
per-block repair re-converges M as slots are touched again.

## Design decisions

**Why two chains.** M replays every parent tx, so its receipts/logs/traces exist
and its execution can be *verified*. T is the devnet: watch-list state is kept
fresh by BAL diffs, your writes live there and are never overwritten. Without M
a divergence would be ambiguous ("engine bug or my scenario?").

**M is executed *and* converged.** A fork cannot reproduce `blockhash(...)`
(A17): Seaport 1.6's `incrementCounter` uses `blockhash(number-1)` as its
quasi-random interval, so its counter (and everything derived from it, e.g.
order validity) diverges on any fork. Because the BAL is the authoritative
post-state of every changed entry, the engine applies the parent BAL to M
*before* each replay. Execution still runs (traces + verification stay real),
but state stays exact: `mirror_m_convergence_failures_total` stays 0 in steady
state. Without this, one Seaport counter divergence cascaded into invalid txs
(queued senders), missing txs, and permanently wrong state within ~10 blocks.

**Known open gap: blob-tx replay after PeerDAS.** Sidecars come from a public
beacon API (a non-supernode Lighthouse cannot reconstruct blobs post-EIP-7594).
Blocks whose blob txs are rejected by the pool are skipped after three attempts
and recorded; the BAL repair keeps M's state exact, and T is unaffected. See
TESTING → Known limitations #5 for the fix direction.

**Divergence policy: record, don't freeze** (`loops.on_divergence`, default
`continue`). Every divergence is classified and written to `drift.jsonl` +
metrics; `freeze_m`/`halt_all` are for conformance runs. Measured reasons
divergences are inherent:
- blockhash-dependent values (Seaport counters, any `blockhash(n-1)` reader);
- near-full blocks: mainnet block 26,233,028 used 59,975,282 / 60,000,000 gas;
  a small gas delta (type-4 refund accounting is the only *known* systematic
  delta) pushes the last txs out of M's block;
- cascades of the above *within* a block (statuses, log values, probes).

Structural checks (tx count/order, blob gas, receipt count, withdrawals) are
what an engine bug would break; they are reported separately
(`mirror_divergences_total`) so `continue` stays honest.

**Follow the `safe` head by default** (`reth.follow: safe|latest|finalized`).
Safe is ~1 epoch (32-64 blocks) behind head but immune to shallow reorgs.
`latest` trades that safety for freshness. The mirror follows *reth's* head:
see "Source liveness" below.

**Pins and epochs.** Anvil forks at an absolute pin (`reth head - 2000`),
resolved once per epoch by an init container and persisted in `/state/pin`, so
restarts keep the same base. Reads above the pin come from the replayed chain
(M) or from applied diffs (T); reads below it come from reth's state window.
When the pin ages out of that window (~10k blocks ≈ 1.4 days), run `make repin`
– a new epoch. The engine detects the chain reset and rebases automatically.

**EIP-7702 nonce gaps.** Every authorization in a type-4 tx bumps the
authority's nonce, so a self-sponsored delegation advances the sender's nonce by
2 and blocks legitimately contain nonce gaps (measured: `0x1e6, 0x1e8, 0x1ea`).
Anvil's pool only accepts txs that extend the contiguous pending sequence, so
the engine temporarily moves the sender's nonce to the gap tx's nonce for pool
acceptance and restores the pre-block value before mining (execution then
re-validates in order, with the real auth bumps).

**Copy-on-write (T).** Storage shadows per slot; account fields
(balance+nonce+code) shadow as one unit. The applier skips any parent write
whose key is shadowed, so your writes are never overwritten. Shadowed keys are
discovered by watching T's own blocks: tx sender, value recipients, and storage
writes via `debug_traceTransaction` with `prestateTracer{diffMode}`. Direct
cheatcode calls that bypass T's block pipeline are registered through
`POST /admin/shadow`.

**Journal + rewind.** Before every applied write the applier records the
before-value (`/state/journal/<block>.jsonl`, ring 1024 blocks).
`POST /admin/rewind` restores those values newest-first and moves the cursor
back, so the applier re-applies from there.

**Automine is disabled on both anvils** (`--no-mining`) and the engine
re-asserts `anvil_setAutomine(false)` on every rebase. This is load-bearing:
anvil restarts (re-pin, OOM) reset automine to its default (on), and with
automine on every raw tx mines its own block, so the engine's `mine(1)` produces
empty blocks and the chain height runs away (measured: ~14 blocks mined per
parent block, every block `m=0`, senders' nonces hundreds behind).

**A block that cannot be replayed is skipped, not fatal.** After three failed
attempts the engine mines whatever the pool holds and records the divergence;
the parent-BAL repair still converges M's state, so one block's missing txs
(e.g. a beacon source serving a sidecar anvil rejects with "invalid KZG proof")
never stalls the mirror. Blob sidecars are tried in `blobs.beacon_urls` order
and the first sidecar whose commitments match the tx's versioned hashes wins;
publicnode is listed first because the local Lighthouse returned a mismatching
sidecar for block 26232853.

**Measured rules baked into the code** (each cost a real run to discover):
padding all hex to 32 bytes / even length (anvil rejects odd-length hex with
`-32602`); JSON-RPC batches chunked by bytes as well as items (bodies ≥2 MB are
rejected with `-32600`); raw txs sent sequentially (batch items execute
concurrently and scramble pool order); withdrawals credited via `setBalance`
(idempotent - `addBalance` double-credits on retries); type-4 gas delta bound
`[-12,500 × #auths, 0]`; blob sidecars from the beacon API using
`parentBeaconBlockRoot` of EL block N+1.

## Source liveness (read this before trusting freshness)

The mirror's freshness is **reth's head**, and reth's head only advances when
the consensus layer drives it:

1. `lighthouse` checkpoint-syncs, then feeds `newPayload`/`forkchoiceUpdated`
   to reth over the engine API (JWT on 8551).
2. reth executes the payloads *in order*. If the snapshot's head is older than
   the CL's first payload, reth needs the missing blocks – from **P2P** – or it
   buffers the live payloads unexecuted (`eth_blockNumber` stays at the
   snapshot block, `connected_peers: 0`).

Measured in this environment: reth's discv5 discovery works
(`reth_discv5_established_sessions_raw_total` climbs) but no RLPx session forms,
so a snapshot older than the CL's checkpoint cannot be gap-filled by P2P here.
The mirror then faithfully mirrors the *snapshot's* state and window. Options:

- Re-run the snapshot bootstrap (`make snapshot`) for a fresher base (the
  official `--minimal` snapshot lags the chain by hours to ~a day).
- Point the mirror at any mainnet RPC that serves blocks/receipts and
  `debug_traceBlockByNumber` (the BAL falls back to the trace diff): set
  `reth.url` in `k8s/base/engine-config.yaml` and the `RETH_URL` env of both
  anvil deployments. Public endpoints are rate-limited and serve no BAL, so the
  engine uses its prestateTracer fallback per block.
- If P2P is available (host networking, port 30303 open), reth backfills the
  gap by itself and everything follows live.

Watch `mirror_reth_head` vs the real mainnet head; if it does not move, the
mirror is static by construction, not broken.

## Engine configuration (`k8s/base/engine-config.yaml`)

```yaml
reth:   { url, follow: safe|latest|finalized, request_timeout_secs }
anvil:  { m_url, t_url }
loops:  { poll_interval_ms, max_gap_blocks, chunk_size,
          replay_into_m, apply_into_t, repair_m,
          on_divergence: continue|freeze_m|halt_all }
verifier: { enabled, drift_sample_keys, type4_refund_per_auth, probes[] }
watchlist: [ { address, note } ]     # all slots of a watched account sync to T
journal: { dir, blocks }             # ring size
blobs:  { enabled, beacon_urls[] }   # sidecar source (local Lighthouse first)
http:   { addr, port }
```

**Watch list** (seed = the plan's §3.5 closure): Chainlink feeds (WETH/WBTC/
STETH/USDC/USDT-USD), assets (USDC/USDT/WETH/WBTC/wstETH), Uniswap Universal
Router + WETH/USDC 0.05% pool, Aave V3 (Pool, Oracle, PoolAddressesProvider,
aEthUSDC/aEthWETH/aEthUSDT, WETH Gateway). Extend with your own deployment
addresses. Only watched accounts are synced to T – add your contracts or their
state will stay at pin values.

**Probes** (per-block `eth_call` parity reth vs M, and reth vs M *after* the
repair): USDC/WETH totalSupply, Chainlink WETH/USD `latestRoundData`, UniV3
`slot0`, aEthUSDC totalSupply, Aave `getReserveNormalizedIncome(USDC)` (the
time-dependence proof – it only matches when the block context is aligned).

## Engine HTTP API

| Endpoint | Purpose |
|---|---|
| `GET /healthz` | 200 unless halted on divergence |
| `GET /status` | heads, cursors, lag, frozen, shadow counts, blob form |
| `GET /metrics` | Prometheus text: heads, lag, durations, divergences (structural + behavioral), repair writes, convergence probes, writes, pin age |
| `POST /admin/rewind` | `{"blocks": N}` – journal-based rewind of T |
| `POST /admin/resume` | clear a divergence freeze |
| `POST /admin/shadow` | register devnet writes that bypassed the block pipeline |
