# Architecture

## Components

| Component | Image / build | Role | Measured footprint |
|---|---|---|---|
| `reth` | `ghcr.io/paradigmxyz/reth:v2.8.0` | Source of truth: head state, blocks, receipts, EIP-7928 BALs, `debug` traces | ~300 GB disk, ~2.6 GB RAM idle / ~5.5 GB while following and serving |
| `lighthouse` | `docker.io/sigp/lighthouse:v8.3.0` | Consensus driver: checkpoint sync, drives reth's head via the engine API (no validator), serves payloads + blob sidecars to the engine | ~160 GB disk and growing to ~200 GB (18-day column custody), ~5-7.5 GB RAM while backfilling (16Gi limit) |
| `anvil-m` | `ghcr.io/foundry-rs/foundry:v1.8.6` | Mirror chain (M): replays every parent tx, then re-converges from the BAL | ~15-25 MB per replayed block; OOMs into a fresh pin every ~2.5-4 h by design (24Gi limit) |
| `anvil-t` | same | Devnet chain (T): watch-list state + your writes, never replays parent txs | ~2.5 GB after thousands of applied blocks (16Gi limit) |
| `engine` | `localhost/mirror-engine:v1` (this repo) | Replay loop, verifier, BAL diff applier, convergence repair, journal, admin API | ~100 MB RAM |

## Data flow

The per-block exchange between the three processes, top to bottom (the
numbered list below then walks M's side of it step by step):

```
   reth                          ENGINE                          anvil M / T
   │                             │                               │
   │ block(N): txs, env,         │                               │
   │   withdrawals               │                               │
   ├────────────────────────────►│ replay loop                   │
   │ receipts · logs · traces    │                               │
   ├────────────────────────────►│ verifier: counts · gas ·      │
   │                             │ logsBloom · log set · probes  │
   │                             │ send raws in order → mine     │
   │                             ├──────────────────────────────►│ M
   │ BAL(N) for the watch list   │                               │
   ├────────────────────────────►│ diff applier                  │
   │                             │ pad → write ≤500 → mine(1)    │
   │                             ├──────────────────────────────►│ T
   │                             │ sampled parity + T probes     │
   │                             │ journal + drift.jsonl         │
```

Per replayed block on M the engine does, in order:

0. **Convergence repair** – apply the parent BAL (`N-1`) to M as cheatcode writes,
   then prove it with the configured probes: `eth_call` on M's repaired (unmined)
   state vs reth at `N-1`. A mismatch here is a real repair failure - not the
   expected blockhash-level noise - so it is the counter to alarm on
   (`mirror_m_convergence_failures_total`, must stay 0).
1. Block env: timestamp, coinbase, prevRandao, gas limit, base fee (all five fields).
2. Withdrawals credited idempotently (`setBalance(pre + amount)`).
3. Raw txs built from the parent block (blob sidecars from the beacon API).
4. Txs sent one request at a time (pool order is load-bearing), then `anvil_mine(1)`.
5. Verification: tx count/order, blob gas, receipts (count/status/gas), logsBloom,
   full log set, withdrawals. (State probes deliberately do *not* re-run here:
   post-execution they only re-detect the blockhash-level divergence the log and
   receipt checks already record - measured on a `BLOCKHASH`-gated MEV bot.)
6. Divergence entry (`drift.jsonl`, only when something diverged) + cursor +
   metrics. Clean blocks are counted in the metrics, never written to disk.

### Devnet transactions: the prepend rule

The applier mines the devnet's pending transactions in **their own block,
before** the parent's diffs are applied (`diff.rs` step 0), then applies the
diffs and mines the parent's block. Two reasons, both measured:

1. **The replayed block must contain only the parent's effects** (plan,
   "Prepend rules"). Without it, a devnet tx and a parent's state change land in
   the same T block and the block no longer means anything.
2. **Gas estimates stay honest.** Anvil estimates the gas for
   `eth_sendTransaction` against `latest`; if the parent's diffs land between
   the estimate and the execution, the tx executes against a different state and
   can run out of gas (measured: an Aave `borrow` estimated at 257,560 used
   271,389 - reverted at 97% of its limit, succeeded with an explicit 1M limit).
   The prepend therefore loops (at most 3 rounds): a tx that arrives *while* the
   diffs are being written still gets its own block and still executes against
   the state its gas was estimated on.

After the parent's block is mined, the applier runs the plan's T-side invariant
("probe parity on T after context alignment"): the same probe set against
T@latest vs reth at the applied parent block. Probes whose target the devnet
owns are skipped (copy-on-write: that state is the devnet's, not mainnet's) and
the check is suspended while a `/admin/time` offset is set (views are then
*meant* to differ). The applier scans its own devnet blocks (the prepend) before
the probes run, so a write made in this very apply is already marked and cannot
fire a spurious alarm. The alarm is `mirror_t_convergence_failures_total`.

T's head therefore drifts ahead of the parent cursor by the number of devnet
blocks (measured: ~70 after a session of drills); that is normal. The cursor stays keyed to the parent height, and the
applier only bails when T's head is *behind* the cursor by more than one block
(an `anvil_reset` outside the engine, which the incremental diff applier cannot
recover from).

### Devnet time: the clock offset (`/admin/time`)

A margin product is a function of time: interest accrual, option expiry, funding
intervals. A static fork gets that from `evm_increaseTime`, but on a live mirror
the next parent block re-aligns the clock and the jump is gone (and worse: the
alignment then fails as non-monotonic, so the block gets anvil's default
timestamp and the context drifts silently - measured).

`POST /admin/time {"offset_seconds": 2592000}` makes the devnet's clock run that
far ahead of mainnet, *persistently*: the applier aligns each parent block to
`parent + offset`, clamped to `last + 1` so timestamps stay monotonic whatever
the offset does. Mainnet's data keeps flowing while the devnet lives 30 days in
the future - measured: the USDC reserve's income index advanced by a month of
interest and a borrow's debt grew with it, with the clock still tracking the
parent. Lowering the offset works; the clock then converges at mainnet's pace
rather than jumping backwards (timestamps cannot). While the offset is non-zero
the T-side probe parity is skipped: time-dependent views are *supposed* to
differ.

### Watchlist closure: proxies need their aggregators

A watched account only stays fresh while mainnet writes to *that* account.
Chainlink feeds are `AggregatorProxy` contracts: `latestRoundData()` is a
delegatecall into the *current aggregator*, whose storage holds the round, so
watching the proxy alone freezes the price at the pin era. The same applies to
Aave oracle sources - they are feeds, and at least the WETH/USD source is itself
a proxy (measured: 8 rounds / ~$44 stale while USDC's direct-aggregator source
stayed fresh and hid the gap). Resolve each source's aggregator and watch it:

```bash
cast call <source> "aggregator()(address)" --rpc-url $RETH
```

**Composite sources need the whole chain.** Aave's WBTC/USD source is not a
feed at all: it multiplies `BTC/USD x WBTC/BTC`, each leg a proxy whose
aggregator holds the round. None of those four contracts was watched, so the
WBTC price on T sat stale (measured: $81,811.77 vs mainnet's $81,810.37 at the
same cursor) while *every* probe stayed green - there was no WBTC price probe.
Found by the product sim's broad freshness audit, which compares the whole
market-data closure per parent block. Heal for this class: trace the source's
read path on reth with the plain `prestateTracer` (no `diffMode` - a view call
changes nothing, so a diff-mode trace is empty), then write the read slots'
values to T. The full price closure now has a probe per reserve, so the class
alarms continuously instead of waiting for an audit.

**Closure runs both ways.** The watchlist must contain every account whose
*mainnet* state the mirror needs, **and** every account the devnet's own writes
land on: a devnet write to an unwatched account is safe (no diff ever touches
it) but *invisible* to `/admin/shadow`, so a reset cannot clear it and a
position keeps a phantom component. Measured: Aave's **variable debt tokens**
were missing, so a reset left `$315` of debt against zero collateral and every
later borrow failed with `LtvValidationFailed`. The full reserve closure is
`getReserveAToken` + `getReserveVariableDebtToken` for each asset.

A watchlist addition only affects *future* writes; a value that went stale
before the entry existed heals on the account's next mainnet write. To heal it
immediately, hand the stale slots back to mainnet sync with
`/admin/shadow {clear: true, slots: [...]}` - the engine re-syncs them from reth
at the cursor (used for the WETH/USD aggregator: hot-vars slot + the
transmission mapping slots, `keccak(roundId, 17)`).

### Fork pins (M refreshes, T persists)

M's fork pin is **re-resolved on every start** (`reth head - FORK_OFFSET`,
64 by default): a full-replay fork is recycled by k8s when it reaches its
memory limit (~2.5-4 h at the measured ~20 MB/replayed block), and a fresh pin
keeps the post-restart re-replay at 128 blocks (~1-2 min) instead of the whole
epoch - a stale pin makes the replay OOM before it can catch up (measured).
T's pin is **re-resolved on every container start** as well (init container +
container wrapper, like M). It used to be persistent so that a restart could
not lose the devnet's writes; the engine now keeps those writes - keys *and*
values - in the shadow registry and re-applies them after a re-fork (verified
by killing the pod: the dealt balances came back), so a fresh pin loses nothing
and bounds the re-replay to `FORK_OFFSET` (~128 blocks) instead of the pin age
(~1.5 h measured). M's own re-base never touches T: T's epoch is its own.
Watch `mirror_t_fork_age_blocks` (reth serves reads below T's fork block, and
its state window is 10,064 blocks) and `mirror_t_resets_total`.

## The lean profile (measured, not guessed)

**Snapshot** (`reth download --minimal`): 154 GB download → ~287 GB on disk.
State (266 GB), headers (14 GB), last 50k blocks of transactions (2 GB) +
receipts (1 GB) + changesets (1 GB). Chosen over `--full` (728 GB / 990 GB)
because the mirror only replays *live* blocks.

**Runtime pruning** (`k8s/base/reth-config.yaml`):

| Segment | Retained | Why |
|---|---|---|
| account + storage history | 10,064 blocks | reth's minimum unwind-safe distance; the Anvil pin (`head-64`) lives well inside it |
| receipts | 20,000 blocks | verifier slack + `eth_getLogs` debugging |
| bodies | 20,000 blocks | replayable catch-up backlog after an engine restart |
| senders, tx lookup | pruned | the mirror never serves historical lookups (a pruned lookup is why `eth_getTransactionByHash`/`getTransactionReceipt(hash)` return `null` on reth; `eth_getBlockReceipts` still works) |

**Lighthouse**: checkpoint sync (no genesis sync), no validator, no light-client
server, no QUIC/UPnP, 40 target peers, `--semi-supernode` (half the data
columns = enough to reconstruct any blob within the DA window),
`--prune-payloads=false` (the top-up needs payloads the EL has not executed),
blob/column pruning and backfill rate limiting left at their defaults - all
bounded, see "Backfill limits" below. Measured: the initial sync + state caches
need >6 GiB (it OOM-looped at 6Gi) - the deployment requests 2Gi and limits 16Gi.

**Anvil `--prune-history`** is a memory knob, not a disk knob: it keeps up to N
full *state snapshots* in memory for rollback/tracing. Measured with 1000:
anvil-t reached 9.8 GB in minutes and anvil-m OOM'd at 12Gi. Now: T keeps 32
(its state is small and the devnet uses `evm_snapshot`/`evm_revert`), M keeps 2
(it replays every parent tx, so snapshots are huge and M is recycled by design).
Tracing a historical block on M is therefore limited to the last ~2 replayed
blocks; T's own journal (`/admin/rewind`) is engine-side and unaffected.

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

**Blob-tx replay works without any public API.** The local Lighthouse runs
`--semi-supernode`: it custodies half the data columns (enough to reconstruct
any blob) for the 18-day data-availability window, so the engine's only blob
source is `http://lighthouse:5052`. Two measured quirks are handled:

- For Fulu blocks the beacon API's `kzg_proof` field is a *placeholder*
  (`0xc0…`, the point at infinity): the stored container only has cell proofs.
  The engine recomputes the blob proof locally with c-KZG and re-verifies the
  triple. This is a real check, not a bypass - a recomputed proof verifies if
  and only if the blob is the one the commitment commits to, and the tx's
  versioned hash anchors the commitment. Covered by a KZG round-trip unit test.
- A block whose blob txs still cannot be built (source gap) is mined with
  whatever the pool holds after three attempts and recorded; the parent-BAL
  repair keeps M's state exact and T is unaffected.

**Divergence policy: record, don't freeze** (`loops.on_divergence`, default
`continue`). Every divergence is classified and written to `drift.jsonl` +
metrics; `freeze_m`/`halt_all` are for conformance runs. Measured reasons
divergences are inherent:
- blockhash-dependent values (Seaport counters, any `blockhash(n-1)` reader);
- near-full blocks: mainnet block 26,233,028 used 59,975,282 / 60,000,000 gas;
  a small gas delta (type-4 refund accounting is the only *known* systematic
  delta) pushes the last txs out of M's block;
- cascades of the above *within* a block (statuses, log values). Root cause
  measured on block 26,244,417 tx 124: a MEV bot's `BLOCKHASH`-gated entry
  (`revert("ph-")`) succeeds on mainnet and reverts on M; the flipped status
  cascades into WETH supply and Uniswap pool logs, all repaired from the parent
  BAL before the next block.

Structural checks (tx count/order, blob gas, receipt count, withdrawals) are
what an engine bug would break; they are reported separately
(`mirror_divergences_total`) so `continue` stays honest.

**Follow `latest`** (`reth.follow: latest|safe|finalized`). This is a devnet
requirement, not a preference: the applier mines one T block per applied parent
block, so the follow head *is* the devnet's block cadence. With `safe` the CL
updates the safe block once per epoch, the applier burst-mines 32 blocks every
~6.4 min, and a devnet tx sent between bursts waits minutes for a receipt
(measured: 180 s+ timeouts, then "transaction not found" after a pool reset).
With `latest` the parent stream advances every ~12 s and a devnet tx confirms in
~12 s (measured). A small replay margin (`reth.follow_margin_blocks`, default 2)
sits between the head and the applier: the beacon cannot serve blob sidecars for
the newest block yet, and without the margin M held and retried on ~every blob
block (measured: 20 retry errors / 20 min, ~4 blocks/min instead of 5). The cost
is exposure to shallow reorgs; those are detected
(`mirror_reorgs_detected_total`) and M is re-based behind the new head by the
same self-heal path that handles a receding follow head. The mirror follows
*reth's* head: see "Source liveness" below.

**Pins and epochs.** Anvil forks at an absolute pin (`reth head - FORK_OFFSET`,
128 blocks). The pin is re-resolved on every *container* start (the init
container for a fresh pod, the wrapper on a container restart) and persisted in
`/state/pin` as the fallback for when reth is unreachable, so an OOM restart
re-forks at the live head instead of replaying an aging fork until the next
OOM. Reads above the pin come from the replayed chain (M) or from applied diffs
(T); reads below it come from reth's state window (which serves deep history:
10,064 blocks of state, 20,000 blocks of receipts/bodies).

Two constraints bound the pin depth:
1. reth's state window (10,064 blocks ≈ 1.4 days) - reth must be able to serve
   `eth_getProof`/`debug_trace*` reads at the pin;
2. the range of slots the local supernode can reconstruct blobs for (18-day DA
   window, minus anything pruned) - blob txs in the replayed window need
   sidecars.

Depth is a trade-off, not a requirement: 128 keeps the mirror live with a fast,
exact bootstrap (~26 min of replayed mainnet history); raising it replays more
history but costs replay time and M memory. When the pin ages out of reth's
state window, run `make repin` - a new epoch. The engine detects the chain reset
and rebases automatically.

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
never stalls the mirror. Blob sidecars are fetched from the local Lighthouse
only, validated with the bundled mainnet trusted setup, and placeholder proofs
are recomputed (see above).

**Epochs own the devnet's writes.** A re-pin starts a new epoch: the anvils
fork at a fresh pin and the devnet's writes are gone, so the engine clears the
shadow set when it sees a new fork pin (persisted in `/state/cursor.json` as
`epoch_pin`). Keeping stale shadows would silently stop T from syncing keys it
no longer owns. An anvil restart (OOM, dump restore) keeps the same pin and the
same writes, so its shadows stay.

**T's heights are its own.** T mines one block per applied parent block, plus
one per journal rewind, so its height is not a mainnet block number - the
engine tracks progress by `cursor_t` (last applied parent block) and only M,
which replays 1:1, is compared against the follow head by height. Two rules
follow from this and both were bugs once: a hash-validated cursor always wins
(resuming at `cursor + 1`, even behind the chain head - jumping to `head + 1`
silently skipped parent blocks and stalled T), and after a reset T resumes from
the fork pin (`pin + 1`), because its state *is* the pin's state.

**Measured rules baked into the code** (each cost a real run to discover):
padding all hex to 32 bytes / even length (anvil rejects odd-length hex with
`-32602`); JSON-RPC batches chunked by bytes as well as items (bodies ≥2 MB are
rejected with `-32600`); raw txs sent sequentially (batch items execute
concurrently and scramble pool order); withdrawals credited via `setBalance`
(idempotent - `addBalance` double-credits on retries); type-4 gas delta bound
`[-12,500 × #auths, 0]`; blob sidecars from the beacon API using
`parentBeaconBlockRoot` of EL block N+1; beacon-API quantities are **decimal
strings** while the Engine API wants hex (parsing a decimal slot as hex silently
un-bounds the top-up walk); withdrawal credits read their pre-balance from reth
so a retried block can never double-credit (reading M at N-1 does exactly that,
because anvil's "N-1" is the live state when the block is not mined yet).

## Source liveness: self-contained by construction

At runtime the mirror needs **no external nodes**: no public beacon API, no
public RPC, no EL P2P peers. reth's chain comes from the local Lighthouse over
the Engine API (JWT on 8551):

1. `lighthouse` checkpoint-syncs, then feeds `newPayload`/`forkchoiceUpdated`
   for new blocks to reth as usual.
2. For the gap between the snapshot's head and the CL's checkpoint (measured:
   8.4k blocks ≈ 1.2 days), reth cannot fetch ancestors from peers - it has
   none. The engine's **top-up loop** (`source_sync`, EIP-8379) walks the CL's
   canonical chain from reth's head forward, slot by slot, and feeds each
   ancestor payload to `engine_newPayloadV4` in ascending order followed by
   `engine_forkchoiceUpdatedV3`. reth buffers and executes them contiguously;
   measured ~10-11 blocks/s, so a fresh bootstrap catches up in ~15 minutes.

Spec details that cost real debugging runs:

- **Quantities.** The beacon API serves every quantity as a *decimal string*
  (`"26243147"`, `"1792611803"`); the Engine API wants *hex quantities*. The
  engine normalizes both.
- **`executionRequests`** (EIP-7685 / `engine/prague.md`): each element is
  `request_type || SSZ(request_data)`, ordered by type, and **elements with
  empty request_data MUST be excluded** - all-empty is `[]`, not `["0x","0x","0x"]`.
  reth answers anything else with `-32602 Invalid params: EmptyExecutionRequest`.
  The three request types are re-encoded from the beacon API's structured
  objects (EIP-6110 deposits 192 B, EIP-7002 withdrawals 76 B, EIP-7251
  consolidations 116 B per element; SSZ quantities little-endian).
- **JWT.** alloy's `jwt` feature pulls `jsonwebtoken` without selecting a
  crypto provider, so signing panics at runtime ("Could not automatically
  determine the process-level CryptoProvider"). The engine depends on
  jsonwebtoken with `rust_crypto`; feature unification fixes it graph-wide.
- **A dead top-up task is invisible** (reth simply stops advancing), so the
  loop runs under a supervisor that logs and restarts it on panic/exit.

**Backfill limits** (what each node downloads and keeps, and why). Lighthouse
v8.3.0 has **no backfill-depth flag** - verified against the image itself: the
only backfill-related flags are `--disable-backfill-rate-limiting`,
`--genesis-backfill` (goes *deeper*, never used), `--shutdown-after-sync` and
the pruning knobs. Depth is therefore bounded by design at the weak-subjectivity
point; everything else is a rate/pruning choice:

| Node | Bounded by | Measured / notes |
|---|---|---|
| Lighthouse block backfill depth | weak-subjectivity point (~5 months, ~1.03M slots) | Design bound since v4.2.0; no knob reduces it. One-time: ends with `INFO Historical block download complete` |
| Lighthouse backfill rate | default rate limiter ("typically <20 slots/s") | 6.7-15 slots/s measured; covers the 8.4k-slot snapshot gap in ~10 min, then continues gently in the background |
| Lighthouse block cost | ~150-200 KB/slot | ~150 GB for the full 5-month range; retained in the freezer (blocks are not pruned) |
| Lighthouse column/blob cost | **not bounded to the DA window during the backfill** (measured twice) | The backfill fetches blob/column data for *every* backfilled slot: `blobs_db` grew 13.9-18.6 GB/h (~600-650 KB/slot) at 3-8 slots/s, well past the 18-day window it will ever serve (84 GB at 145k slots covered). Projected peak for the full 5-month range: ~900 GB (blobs) + ~185 GB (blocks). local-path volumes are thin, so this consumes the node's disk (2.4 TB free) rather than a PVC quota; the PVC is sized honestly (1500Gi) and `make prune-blobs` reclaims it |
| Lighthouse prune cadence | `--epochs-per-blob-prune=1` (was 256) | Default 256 epochs (~27 h) can let ~20 GB/h of backfill blobs accumulate before the first prune - more than the PVC headroom. Pruning every epoch bounds the peak; only data older than the DA boundary is touched, which the mirror never needs |
| Lighthouse payloads | retained (`--prune-payloads=false`) | Default pruning rebuilds payloads from the EL on demand - impossible for blocks the EL has not executed, which is exactly the top-up gap. This is what makes the backfilled range cost full block size |
| reth P2P | none (`--disable-discovery --trusted-only`) | 0 peers; no discovery chatter; no pipeline/backfill runs (measured 0 pipeline log lines) - the EL is driven purely by the Engine API |
| reth gap-closing | `--engine.backfill-threshold=1024` (max) | Gaps up to 1024 blocks are closed by fetching blocks directly instead of a *pipeline* run. With no peers both are inert, but a pipeline run can unwind a detached head, so the maximum keeps reth on the live/CL-driven path |
| reth retention | prune profile (see table above) | receipts/bodies 20,000 blocks, account/storage history 10,064 blocks |
| reth bulk download | one-time snapshot, components selected explicitly | `reth download` takes fine-grained flags (`--with-txs-distance`, `--with-receipts-distance`, `--with-state-history-distance`, `--with-senders`, `--with/without-rocksdb`); the job selects the same distances as the runtime prune profile (20k/20k/10,064) instead of the `--minimal` preset, so nothing is downloaded only to be pruned. **RocksDB stays**: it holds the EIP-7928 BAL store the engine reads via `eth_getBlockAccessList`. The state itself (~266 GB) is the floor - 154 GB download / ~287 GB on disk |

Total expected footprint once the one-time backfill completes: ~850 GB of
blobs (mostly beyond the DA window, so prunable) + ~185 GB of blocks in
`lighthouse-data`, and ~300 GB in `reth-data`.

**Reclaiming the backfill's blob storage** - `make prune-blobs` (one-time, after
the backfill logs `INFO Historical block download complete`). It scales the CL
down, runs `lighthouse db prune-blobs` as a Job with the PVC mounted, scales it
back up and prints the new sizes. Stopping the node is required: MDBX is locked
while it runs (measured: `Fatal error: LevelDbError(... lock
/data/beacon/chain_db/LOCK: Resource temporarily unavailable)`). The mirror
survives the pause - reth buffers the CL's payloads and the top-up resumes.
Progress and sizes: `make backfill`.

Pruning removes everything older than the DA boundary - the mirror only ever
needs the replay window (10,064 blocks ≈ 1.4 days, bounded by reth's state
window; the pin itself is only 128 blocks), so this is safe. It leaves ~50 GB of
columns plus the blocks. The alternative trade-off is dropping `--semi-supernode`:
~650 GB less disk, but the mirror loses blob-tx fidelity on M (blob blocks are
mined without their blob txs and recorded; state stays exact via the BAL).

**Follow head during a catch-up.** The engine follows reth's `safe`/`latest` block, which
Lighthouse updates via `forkchoiceUpdated`. While reth is far behind the CL's
head, those updates reference blocks reth has not executed, so reth ignores them
("Received forkchoice updated message when syncing") and its `safe`/`finalized`
pointers stay at the snapshot's values - the mirror's follow head is frozen
until the catch-up completes, then jumps to live. The top-up's own
`forkchoiceUpdated` passes zero hashes for safe/finalized (spec: zero = no
update), so it never clobbers them. After a fresh bootstrap, re-pin once the
catch-up has finished: M/T then start from a live, exact pin instead of
replaying the (partially degraded) catch-up window.

## Pinned versions (tag + digest, no floating references)

Every image reference in the repo is pinned by **full version tag and digest**;
nothing uses `latest`, a minor-only tag, or an unpinned reference. The digest is
verified to be present in the k3s containerd store, so a pod starts with **zero
registry access** (measured: `Container image ... already present on machine`).

| Component | Image | Notes |
|---|---|---|
| reth | `ghcr.io/paradigmxyz/reth:v2.8.0@sha256:796c8664…` | latest release; opens the v2.7 snapshot DB (a manual `reth db migrate-v2` exists, so older layouts stay supported) |
| Lighthouse | `docker.io/sigp/lighthouse:v8.3.0@sha256:ca59d3a3…` | latest release |
| Foundry (anvil) | `ghcr.io/foundry-rs/foundry:v1.8.6@sha256:4a16a86f…` | latest release |
| init containers / wipe job | `docker.io/library/alpine:3.24.2@sha256:294b683c…` | 3.24.3 exists but the registry was unreachable from this node (DNS/IPv6: `dial tcp [2600:…]: connect: network is unreachable`). Bump with `make images` once it is; the running set is complete without it |
| engine | `localhost/mirror-engine:v1` | built from this repo; the tag lives in `k8s/base/engine.yaml` and the Makefile reads it from there |

**Single source of truth.** The Makefile derives the image list for `make images`
from the manifests (`grep 'image:' k8s/`), so a version bump is a one-line
manifest edit - there is no second list to drift.

**How to resolve a digest.** Use the client that populates the store (podman /
k3s), not the registry API: for the same tag the API can return a different
manifest depending on the `Accept` header. Measured: Lighthouse v8.3.0 is
`ca59d3a3…` in containerd but `6d76f50b…` via the API. After pinning, verify
with `sudo k3s ctr images ls -q | grep <image>@` - the digest must be listed, or
the pod needs a registry pull (which fails when DNS is flaky, as it was here).

`make images` pre-pulls and imports the pinned references into the k3s containerd
store and then runs **`make check-pins`**, which mechanically enforces the policy:
every `image:` in `k8s/` must carry a tag *and* a digest (or be the local engine
build), the digest must be present in the containerd store, and no `latest` may
appear anywhere. `make bootstrap` runs it first.

The engine's own dependencies are pinned in `Cargo.lock`: the plan's `alloy 2.5`
pin (EIP-7928 BAL types) is deliberate and only patch updates are applied
(measured: `tokio 1.54.0 -> 1.54.1` was the only in-range update). The image
build copies `Cargo.lock` and builds with `--locked`, so the shipped binary
cannot drift from the lock (a stale lock fails the build instead). The build
also prefers IPv4 (`gai.conf`), because this host has no IPv6 route while
crates.io resolves AAAA first. Build bases are pinned the same way
(`rust:1.99.1-slim-trixie`, `debian:13.7-slim`).

## Engine configuration (`k8s/base/engine-config.yaml`)

Parsing is strict: an unknown key is a startup error, never a silently ignored
setting (`deny_unknown_fields` on every config struct).

```yaml
reth:   { url, follow: safe|latest|finalized, follow_margin_blocks,
          request_timeout_secs }
anvil:  { m_url, t_url }
source_sync: { enabled, interval_ms, max_per_pass,
               engine_url, jwt_path }   # EIP-8379 top-up via reth's Engine API
loops:  { poll_interval_ms, max_gap_blocks, chunk_size,
          replay_into_m, apply_into_t, repair_m, fork_offset,
          on_divergence: continue|freeze_m|halt_all }
verifier: { enabled, drift_sample_keys, type4_refund_per_auth, probes[] }
watchlist: [ { address, note } ]     # all slots of a watched account sync to T
journal: { dir, blocks }             # ring size
blobs:  { enabled, beacon_urls[] }   # sidecar source: local Lighthouse only
http:   { addr, port }
state_dir: /state                    # cursors, shadow.json, drift.jsonl
```

**Watch list** (seed = the plan's §3.5 closure): Chainlink feeds (WETH/WBTC/
STETH/USDC/USDT-USD) **and their aggregators**, assets (USDC/USDT/WETH/WBTC/
wstETH), Uniswap Universal Router + WETH/USDC 0.05% pool, Aave V3 (Pool,
Oracle, PoolAddressesProvider, aEthUSDC/aEthWETH/aEthUSDT, WETH Gateway).
Extend with your own deployment addresses. Only watched accounts are synced to
T – add your contracts or their state will stay at pin values.

Closure rule that bit once (measured): a Chainlink feed is an
`AggregatorProxy`; `latestRoundData()` is a **delegatecall** into the current
aggregator, whose storage holds the round. Watching only the proxy left the
price one round stale on T. Resolve the aggregator with `aggregator()`
(selector `0x245a7bfc`) and watch it too; re-check after a feed upgrade, when
the address changes.

**Probes** (per-block `eth_call` parity reth vs M, and reth vs M *after* the
repair): USDC/WETH totalSupply, Chainlink WETH/USD `latestRoundData`, UniV3
`slot0`, aEthUSDC totalSupply, Aave `getReserveNormalizedIncome(USDC)` (the
time-dependence proof – it only matches when the block context is aligned).

## The admin surface (plan §4.4, mapped)

Most of the plan's admin surface is *stock Anvil* - the engine adds only what
Anvil cannot do. This is the mapping, so nothing is ambiguous:

| Plan §4.4 group | Where it lives | Notes |
|---|---|---|
| Network customization | anvil: `evm_increaseTime`, `evm_setNextBlockTimestamp`, `evm_mine`/`anvil_mine` | a devnet override is deliberate and is re-aligned by the next parent block |
| Send | anvil: `eth_sendTransaction` (impersonate first - `anvil_impersonateAccount`, auto-impersonate alone answers "No Signer available"), `eth_createAccessList` | unsigned sends are the devnet's normal path |
| Unlimited faucet | anvil: `anvil_setBalance`/`anvil_addBalance`; **engine: `POST /admin/deal`** (= the plan's §4.4 ERC-20 faucet) | the engine's deal discovers the balance slot with `eth_call` + a `stateDiff` override probe (sentinel value, slots 0..16), writes it silently and marks it devnet-owned. Anvil's own `anvil_dealERC20` cannot do this on a forked, no-mining chain ("no slot found", measured). The plan's synthetic `Transfer` log has no equivalent - use a real transfer |
| Storage / code | anvil: `anvil_setStorageAt`, `anvil_setCode`; engine: `POST /admin/shadow` hands keys back to mainnet sync (immediate: reth's values at the cursor are pushed to T; the engine scans T up to its head first so a just-mined devnet write cannot re-mark the key) | |
| Snapshots | anvil: `evm_snapshot`/`evm_revert`; engine: `POST /admin/rewind` = the plan's §4.4 sync-revert control (rolls T back per parent block through the journal) | |
| Time travel | engine: `POST /admin/time` (persistent clock offset, survives parent blocks) *and* anvil's `evm_increaseTime` (which the next alignment would overwrite - use the offset) | the capability a margin product needs; a static fork's `evm_increaseTime` alone is not enough on a live mirror |
| Mirror sync control | engine: `POST /admin/pause` = the plan's §4.4 stop-sync, `POST /admin/resume` = start-sync, `GET /status` = sync status (`frozen`/`halted` = active, `lag`, `cursor`/`cursor_t` = latest synced block, `follow_head` = parent latest block, `time_offset_seconds` = the `/admin/time` offset) | the plan's sync-next has no counterpart: the loops are continuous, and after a pause the next parent block is applied as soon as you resume |

The Solidity precompile (M6) and the bridge relay (M7) are future deliverables -
the engine's equivalent today is the HTTP surface above plus the cheatcodes.

## Engine HTTP API

| Endpoint | Purpose |
|---|---|
| `GET /healthz` | 200 unless halted on divergence |
| `GET /status` | heads, cursors, lag, frozen, shadow counts, blob form |
| `GET /metrics` | Prometheus text: heads, lag, durations, divergences (structural + behavioral), repair writes, convergence probes, writes, pin age |
| `POST /admin/rewind` | `{"blocks": N}` – journal-based rewind of T |
| `POST /admin/resume` | clear a divergence freeze |
| `POST /admin/shadow` | register devnet writes that bypassed the block pipeline; with `"clear": true` hand the given slots/account fields *back* to mainnet sync and re-sync their values from reth at T's cursor (a key with a stale value and no shadow is the most confusing state there is) |
