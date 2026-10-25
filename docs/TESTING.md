# Testing

Three layers, each runnable on demand:

| Layer | Command | Needs | Covers |
|---|---|---|---|
| Unit | `make test-engine` (cargo test) | nothing | pure logic: BAL composition (`bal_from_writes` through the spec validator) and flattening, padding, COW granularity, journal, cursor/rebase rules, the shared wire/HTTP conventions |
| Self test | `make mainnet-selftest` | a public mainnet RPC + beacon API (**test-only** external deps; the runtime stack has none) | the whole pipeline end-to-end on **real mainnet blocks** (compose the parent's BAL → COW filter → apply → sampled storage parity) |
| Smoke | `make mainnet-smoke` | the deployed stack | service health + parity spot check on T's head vs reth |
| Scenario | `make mainnet-scenario` | the deployed stack, no external network | self-containment, live parity on **T** (the 11 state probes vs reth at the same parent block), and the devnet workflow: faucet, copy-on-write shadowing, snapshot/revert, journal rewind |
| Product sim | `make mainnet-product-sim` | the deployed stack, real venues | the margin-product workflow end to end: a levered position built through the faucet + Aave + Uniswap, freshness sampled across consecutive parent blocks, and a bot that sees the `Borrow` event over WS and liquidates after an oracle crash |
| Follow (local) | `make venue-follow-local` | anvil + the venue engine binary (no cluster/Docker) | the venue's source follow natively: a local anvil stands in for the HL public API, V forks it, the watched account is mirrored 1:1, and a devnet write on V survives the next source window (COW) |
| Selftest (local) | `make mainnet-selftest-local` | anvil + the mainnet engine binary (no reth/public RPC/cluster) | the mainnet applier natively: a local source anvil with a storage-writing contract, T forks it, the engine applies the blocks above the pin through the trace rung (anvil's block trace is empty, so the per-tx fallback runs) and the sampled storage parity passes |

## Unit tests (`cargo test --workspace`)

226 tests, all offline, one suite per crate: `mirror-anvil` 16,
`mirror-devnet` 45, `mirror-config` 17, `mirror-mainnet` 9,
`mirror-venue` 108, `mirror-cctp` 31. The suites carry the measured rules
each module documents:

- `mirror-anvil` — the padding rules (odd-length hex is rejected by anvil with
  `-32602`), the batch body shapes, the wire-error predicates (content
  refusals are never retryable; unknown upstream codes stay verbatim), the
  trace flattening (`prestateTracer{diffMode}` — the fallback shape measured
  on anvil 1.8.5; a slot changed to zero is still a write; unchanged fields
  are not), the BAL composer (`bal_from_writes` round-trips through the
  spec's own validator; `flatten_bal` takes the post-states), the backoff cap.
- `mirror-devnet` — COW granularity (storage per slot, account fields as one
  unit, other slots keep syncing; fixture addresses are owned whole), the
  journal round-trip + ring + typed faults on malformed entries, the cursor
  rules (resume is contiguous; fresh/reset chains start at head+1), the
  pin-base predicate (`scan_anchor` clamps at the pin), the shared admin wire
  types, the atomic state-file body (`store`: absent is a fact, a fault and
  corruption are errors), the metric registry convention, the epoch rule's
  decision table (the anchor; fail-closed on an unknown pin, including a
  values-only registry; the journal dropped on every reconcile).
- `mirror-config` — the layered merge (arrays replace, never concatenate),
  `${ENV}` expansion (unresolved is reported, then fatal for services; the
  per-pod contract is `inspect` + `require_resolved` over the pod's own
  sections — one instance file, many readers, each pod carrying only its own
  wiring), `MIRROR__*` overrides, profile coherence, fork-offset bounds; the
  real instance files load and check.
- `mirror-mainnet` — the feed bridge's pause rule (`step` never overwrites a
  scenario; a publish in flight is not a scenario write; a stale pending
  publish is re-sent), the step wrapper's error chain (the chain walk stays
  readable), the bounded error size, the shared HTTP status mapping (one row
  per error variant, all in one place), the metric set's disjointness
  (profile, admin and bridge series do not overlap). The applier's own path —
  BAL flattening, the blob-sidecar and Engine-API encodings — is exercised
  end-to-end by the self test and the deployed stack, not by unit tests
  here.
- `mirror-venue` — the served identity, funding hourly on the oracle,
  liquidation at the mark + the isolated `liq_price` formula, the wire shapes
  (requests omit null optionals; the mixed-case spotMeta parses), the
  CoreWriter/system-address decode (sub-wei remainders floor, USDC
  conversions exact), the budget window, the store's three answers.
- `mirror-cctp` — the attester's seed derivation + keccak-recoverable
  signing, the message encode/decode round trip against the contracts' index
  table, the four attester-filled fields, the fee table, the Iris shapes
  (404/pending/signed), the watcher's pin-as-rest-state predicate.

## Self test (`k8s/bootstrap/selftest-job.yaml`)

One Job, two containers in one pod:

1. `resolve-pin` (alpine) resolves `pin = rpc head - 16`,
2. `anvil-t` forks a public RPC at that pin,
3. `engine selftest` applies blocks `pin+1 .. pin+3`, printing a
   check-by-check report (source, writes/filtered/shadowed, sampled storage
   parity) and exiting non-zero on any failure.

This is the fastest full-pipeline check and it exercises the **trace fallback**
(public RPCs do not serve `eth_getBlockAccessList`).

Expected output shape:

```
-- block 26242174
   apply: source=trace writes=... filtered=... skipped_shadowed=... sampled=8/8 ms=...
```

## Smoke (`scripts/mainnet-smoke.sh`)

Checks pods, reth head + `eth_syncing` + `eth_getBlockAccessList` availability,
Lighthouse sync status, the anvil pin, the engine `/status`, and the engine's
convergence counters (`mirror_t_convergence_failures_total`,
`mirror_full_sweep_mismatches_total`).

## Self-containment checks

Run after any config change; all three must hold:

```bash
# 1. reth has no EL peers and no pipeline/backfill activity
kubectl -n mainnet-mirror exec deploy/engine -- curl -s http://reth:8545 \
  -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"net_peerCount"}' | jq -r .result   # 0x0
kubectl -n mainnet-mirror logs deploy/reth --since=10m | grep -ciE "pipeline|backfill"   # 0

# 2. the top-up is the only thing moving reth's head
kubectl -n mainnet-mirror exec deploy/engine -- curl -s localhost:8080/metrics \
  | grep -E "source_sync_(gap_blocks|delivered_total)"
kubectl -n mainnet-mirror logs deploy/engine --tail=50 | grep -c "top-up"  # >0 while catching up

# 3. blobs come from the local CL only
kubectl -n mainnet-mirror get cm -l app.kubernetes.io/part-of=mainnet-mirror \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}'   # mainnet-instance carries instance.toml
kubectl -n mainnet-mirror get cm mainnet-instance -o jsonpath='{.data.instance\.toml}' \
  | grep -A2 "beacon_urls"   # ["${BEACON_URL}"] -> mirror-config: http://lighthouse:5052
```

## Invariants asserted per applied block (on T)

| Invariant | Class | Where |
|---|---|---|
| sampled storage parity on written keys vs reth at N (all keys with `full_sweep`) | apply | `apply.rs` |
| state probes: `eth_call` parity vs reth at N, devnet-owned targets skipped | convergence | `probes.rs` |
| watch-list filter counts (applied / not-watched / shadowed) | COW | `cow.rs` |
| reth parent-hash continuity between consecutive blocks (reorg detection) | structural | `driver.rs` |
| cursor hash matches the chain (reset detection) | lifecycle | `driver.rs` |

Anomalies are recorded in `drift.jsonl` and counted in metrics
(`mirror_t_convergence_failures_total`, `mirror_full_sweep_mismatches_total`).
Sampled-parity mismatches are the A17 class (blockhash-dependent values) and
re-converge with the next block's absolute writes; the probes are the alarm.

## Measured facts that shaped the applier

| Fact | Evidence |
|---|---|
| Seaport 1.6 `incrementCounter` reads `blockhash(number-1)` | `seaport-core/src/lib/CounterManager.sol`; a fork cannot reproduce it (A17), so the counter (and order validity) diverges — sampled parity records it and the next block's absolute writes re-converge the state |
| A17's observed blast radius | block 26,244,417 tx 124: a MEV bot's `BLOCKHASH`-gated entry (`revert("ph-")`, selector `0x4e00c7fc`) succeeds on mainnet (215,606 gas) and reverts on a fork (38,530); the flipped status cascades into WETH `totalSupply` and Uniswap V2/V3/V4 `Swap`/`Sync` log diffs — all re-converged by the next block's writes |
| Anvil `--prune-history N` keeps N full state snapshots in memory | anvil-t 9.8 GB at 1000; ~2.5 GB at 32 |
| Lighthouse needs >6 GiB during checkpoint sync | CrashLoopBackOff (OOMKilled ×5) at 6Gi |
| Anvil restarts reset automine to *on* | with automine on, every raw tx mines its own block: engine blocks were empty, the height ran away (~14 blocks per parent block) and nonces fell hundreds behind. Fixed with `--no-mining` + re-asserting automine=false on rebases |
| A re-pin moves the chain's base past the shadow watermark | the scan walked the *source's* pre-pin blocks (anvil forwards those ranges upstream) and marked mainnet's real writes devnet-owned (measured: 311 real txs scanned as devnet writes right after a re-pin); every scan is anchored at the pin now (`mirror_devnet::scan::scan_anchor` — the ethereum loop, the applier's inline scan, the admin's hand-back pre-step and the venue driver all use it), and a cursor *at* the base is the legitimate rest state (`cursor_off_base` / `below_fork_base` are strict `<`) |
| A regular beacon node cannot serve blobs after PeerDAS | bpo2 Lighthouse answers HTTP 400 `Insufficient data columns to reconstruct blobs: required 64, but only 4 were found` unless run with `--supernode`/`--semi-supernode`; the deployment runs `--semi-supernode` locally, so the CL can serve blob custody |
| reth rejects non-spec `executionRequests` | `-32602 Invalid params: EmptyExecutionRequest` until elements were type-prefixed with empty types excluded (`engine/prague.md`) |
| jsonwebtoken panics without a crypto provider | alloy's `jwt` feature selects none; the top-up task died silently at the first JWT sign until `rust_crypto` was enabled |
| Beacon API quantities are **decimal strings**, Engine API wants hex | `head_slot()` parsed the slot as hex, silently un-bounding the top-up walk (the official `HeaderResponse` decodes the decimal string; unit-tested in `beacon.rs`) |
| T's heights advance independently of mainnet's | one T block per applied parent block plus one per journal rewind; the engine bounds T by `cursor_t` (not by its height), so a rewind drill cannot stall it. A hash-validated cursor always wins (`cursor + 1`) - jumping to `chain_head + 1` skipped parent blocks and stalled T until the cursor semantics were fixed |
| Watching a proxy is not enough: delegate targets own the state | Chainlink `AggregatorProxy.latestRoundData()` delegatecalls the aggregator; the scenario's T probe caught a one-round-stale price until the aggregators were added to the watchlist |
| The follow head sets the devnet's block cadence | with `reth.follow: safe` the CL updates the safe block once per epoch: the applier burst-mines 32 blocks every ~6.4 min and a devnet tx sent between bursts waited 180 s+ (cast's own receipt timeout, then "transaction not found" after a pool reset). With `latest` the parent stream advances every ~12 s and a tx confirms in ~12 s |
| A devnet tx must not share a block with the parent's diffs | anvil estimates `eth_sendTransaction` gas against `latest`; with the diffs landing in between, an Aave `borrow` estimated at 257,560 used 271,389 (97% of its limit) and reverted, then succeeded with an explicit 1M limit. Fixed structurally by the prepend rule (devnet txs mine in their own block first) |
| A corrupted drift journal hides the history it exists to keep | two tasks appended long lines with separate handles: 10 interleaved lines in 48k. Now serialized, one write per line, divergences only, 16 MiB rotation |
| `cast calldata` rejects `--rpc-url` | a wrapper that appended it silently produced *empty* calldata: the "supply" shipped as a 21,000-gas plain transfer and reverted. Every failed market step in that run traced back to this |
| `anvil_dealERC20` works on both mirror chains - in foundry's argument order `(account, token, amount)`; the reversed order reports "no slot found" (an earlier measurement fell into exactly that trap and concluded "fails on forks"; re-measured 2026-10-23: absolute writes that raise, lower and drain to 0 all landed, on the real USDC proxy and a slot-0 mock alike). `/admin/deal` remains the mirror's own faucet - an `eth_call` + `stateDiff` override: candidate slots 0..16, a sentinel value (a balance that already equals the target makes every slot match - measured), 32-byte padded override values (a quantized value answers "invalid string length") and an explicitly encoded `balanceOf` calldata (`format!("{:064x}", address)` does not pad an alloy Address). Finds WETH slot 3 and USDC's packed slot 9 alike |
| A watchlist gap can hide in the counterparties | Aave's variable debt tokens were unwatched: a devnet borrow writes the user's scaled debt there, the write is not tracked (the shadow scanner follows watched accounts), so `/admin/shadow {all:true}` cannot clear it - a phantom $315 debt against zero collateral broke every later borrow with `LtvValidationFailed`. The proof now uses a fresh position address per run and the closure includes all five aTokens and debt tokens |
| A composite price source hides a whole dependency chain | Aave's WBTC/USD source multiplies `BTC/USD x WBTC/BTC`, each leg a proxy whose aggregator holds the round. None of the four contracts was watched, so T's WBTC price sat stale ($81,811.77 vs mainnet's $81,810.37 at the same cursor) while every probe stayed green - there was no WBTC price probe. Found by the product sim's broad freshness audit (16 views per parent block); fixed by watching all four contracts, adding per-reserve price probes, and healing the read slots via reth's plain `prestateTracer` (a `diffMode` trace of a view call is empty) |
| A devnet reset can make your next tx a no-op that reports success | re-syncing an account's fields puts its nonce back to mainnet's; a byte-identical tx (same nonce, data, gas price) has the same hash, so the node answers with the *old* receipt while the reset state no longer holds its effects (measured: a "successful" approve that changed nothing, then a supply reverting with a bare `revert()`). Vary something - the gas price is enough - or never reset the sender's nonce |
| `cast send --unlocked` needs an explicit `anvil_impersonateAccount` | anvil's `--auto-impersonate` accepts `eth_sendTransaction` from any sender, but cast's own signer check refuses first ("No Signer available"). The suite sends raw `eth_sendTransaction` through the engine pod instead - no signer involved, and the receipt poll is ours |
| `cast index` is offline | it ignores/silently mishandles `--rpc-url`; a slot computed with one comes back empty and the storage read looks like "no value" instead of a wrong slot |
| The tx pool keeps a devnet tx that the engine cannot mine | when the applier bails (T's head behind the cursor after an out-of-band `anvil_reset`), pending txs starve while the probes stay green - watch `mirror_head` vs `cursor_t`, not just the probes |
| Handing a key back kept its recorded value, so a re-fork resurrected it | `unmark` cleared the mark only; the value stayed in the registry, and a later re-fork restored (and re-owned) it. Measured: the scenario's crashed oracle source (USDC source = the WETH/USD feed, price 240,233,000,110 vs mainnet's 99,983,478) came back this way - and because *any* devnet mark on a probe's target skips that probe, the five oracle probes stayed green while the price was poisoned. Fixed three ways: `unmark` now drops the recorded value, orphan values are pruned (never re-owned), and `mirror_t_probe_skipped_total` counts skipped probes so the blind spot is visible. One unshadow call healed the price (`resynced_keys: 4`). The same audit fixed the epoch semantics (a fresh pin clears marks *and* values) - restoring across a moved base is what resurrected the value in the first place |
| Restoring devnet writes onto a *moved* base mixes eras | a +30d time-travel run recorded Aave's `lastUpdateTimestamp` ~13 days in the future; a later re-fork re-applied it and every `supply` panicked with `arithmetic underflow` (product sim 2/13, scenario 31/33). The plan's rule decides it - derived values go stale when the base advances (2.2/6.3): a pin change is a new epoch (clear marks and values), the product re-creates positions from its own journal (3.6). After the epoch reset the suites are green again |

## Capability proof: `scripts/mainnet-proof.sh`

`scripts/mainnet-scenario.sh` proves the *substrate* (apply fidelity, COW, drills).
`scripts/mainnet-proof.sh` proves the *product*: the three things a fork must
do, measured end-to-end against the live stack and left clean afterwards.

| Section | Checks |
|---|---|
| A. live fork | T's head at/ahead of the applied parent cursor; reth's head/safe gap; live oracle prices equal reth at the cursor (USDC *and* WETH) |
| B. mint funds | 10,000 ETH (`anvil_setBalance`); 0.1 WETH (slot 3); 100 USDC (packed `balanceAndBlacklistStates`, slot 9) - each read back through `balanceOf` |
| C. live markets | real Aave pool: `approve` → `supply` (aToken delta) → `borrow` (debt + HF from `getUserAccountData`) → `repay` → `withdraw`; real Uniswap V3 `SwapRouter02.exactInputSingle` on the WETH/USDC 0.05% pool (USDC delta) |
| C2. the financial core | crash WETH's oracle source through the pool owner (the governance path), watch `getUserAccountData` cross below `1e18`, then have a **fresh liquidator funded through `/admin/deal`** call the real `liquidationCall`: debt seized (`5.000004 → 0`), collateral taken with the liquidation bonus, oracle restored via unshadow |
| C3. time travel | a fresh position, then `POST /admin/time` +30 days: the clock is 30 days ahead *and still following mainnet*, the USDC income index advanced by a month of interest, and the borrow's debt grew with it |
| D2. event bridge | WS `eth_subscribe` **delivers WETH `Transfer` logs live for engine-mined blocks** (subscriber connected, address filter applied, log delivered in the next prepend block); `eth_getLogs` returns the same log. Two measured traps: anvil serves WS on its HTTP port (no `--ws` flag exists) and `cast` block-buffers stdout when redirected - without `stdbuf -oL` a working subscription looks dead |
| D. debug surface | `debug_traceTransaction` (callTracer) on the supply; receipt logs; `eth_call` with a `stateDiff` override |
| E. reset | `POST /admin/shadow {clear:true, all:true}` per touched venue, then assert the Uniswap `slot0` is back to reth's value at the cursor |

Every send is simulated with `eth_call` first (the standard simulate-then-send
workflow), so a revert prints its reason before any transaction is spent.
Measured runtime: ~10 min; last full run: 30 passed, 0 failed.

## Product simulation: `scripts/mainnet-product-sim.sh`

Where the capability proof checks *capabilities*, the product sim runs the
workflow a margin product actually executes, in one pass and on live state
(`make mainnet-product-sim`, ~8 min, 9 checks):

| Act | What runs | Asserted |
|---|---|---|
| 1. the trader | faucet funds → `approve` → `supply` 1 WETH → `borrow` 100 USDC → swap 50 USDC→WETH on the real Uniswap pool → `supply` the swap back | the aToken delta, debt > 0 with the HF read |
| 2. live data | the market-data closure the product reads - five Chainlink rounds, the Aave oracle for five assets, the UniV3 pool, three token supplies, two Aave income indexes - sampled once per *parent block* | every view-comparison matches reth at the applied cursor: freshness is continuous and broad, not a one-shot spot check. Devnet-owned views are skipped by design (copy-on-write) and counted |
| 3. the bot | a WS `eth_subscribe` on the pool sees the trader's `Borrow`; the oracle is crashed through its admin path; the bot polls health, calls `liquidationCall`, takes the bonus | the event arrived live, HF crossed below 1e18, the bot's collateral grew with the bonus, the oracle was handed back (unshadow) - and, because the crash leaves the position *underwater*, the liquidation takes Aave's **bad-debt path**: the collateral is consumed entirely and the whole remaining debt token balance is burned while the liquidator repays only what the collateral covered (measured on this deployment: `getReserveData` returns 15 words, the pre-`deficit` struct) |

All three scripts share `scripts/lib-mainnet.sh` - the measured conventions live there
(explicit gas on devnet sends, offline `cast` without `--rpc-url`, polled
receipts) instead of being copied three times. The venue-side dual-mode
scripts (the epoch drill, the capability proof, the order-lane proof) share
`scripts/lib-venue.sh` — and the `cctp-*` family (smoke, roundtrip,
margin-cycle) shares `scripts/lib-cctp.sh`, which sources the venue lib for
the venue-shaped gates (`identity_ok`, `flatten_coin`) and adds the bridge's
own doors (burn, the relay wait, the two chains' reads and the V-side mine)
the same way: one transport per mode (direct URLs for a scratch stack, or
`kubectl exec` from the venue engine pod in the cluster), plus the assertion
helpers and the balance read - so a fix to the transport lands in one place.

## The daloc use-case simulation (`scripts/daloc-sim.py`)

The plan's §17 harness flows, driven end-to-end against a mirror stack: T (a
devnet Ethereum fork), V (a devnet HyperEVM fork), the venue engine, and the
venue simulator. Each **role** is what one daloc-side actor does; each assertion
is a gate the plan names (the client gates of §2.1/§18.3, the time policy of
§17.2, the epoch model of §6.5, the venue mechanics of §5). The roles run in
order, a failure prints the evidence, and the run exits non-zero. **44 checks**
in a green run (six roles: boot, provision, operator, indexer, time, epoch).

The suite re-implements the client gates in Python; the strongest check runs
the **real client**: `crates/daloc-client`'s gated `HyperLiquidClient` (their
`margin_venue.rs` — the code the operator's recall path and the indexer's
poller use) was compiled from their branch and pointed at the sim (measured
2026-10-23). Both account models pass every gate end to end: the model read,
the ≤6 s staleness bound, the dex gate (`perpDexs = [null]`), the unified spot
read, the **hard reconciliation** (`available + maintenance == the collateral
balance`, closing exactly — 230.7312 + 19.2688 = 250.0), the `account_state`
projection (side/size/entry/mark/notional/uPnL), and the book/context parses.
Two sim divergences this run surfaced are fixed and asserted here:
`set_account` used to *replace* the account (a perp-only seed wiped the spot
balance their patch contract keeps) and read the *served* field names
(`balances`/`abstraction`) instead of the patch's
(`spotBalances`/`userAbstraction`), so a mode or spot patch was silently
ignored.

| Role | Drives | Gates |
|---|---|---|
| `boot` | pins both chains' clocks and mines the carrying block; waits for the sim's readiness | T and V clocks start at wall time (±300 s, their gate); `venue-sim /readyz` 200 |
| `provision` | `ensure_gas` on T; sim `reset`; `deal` perp + spot; a seeded position | T gas funded; scenario reset; perp deal returns `balance_after == 1000`; spot deal `== 250`; the hl_mock-shaped seed accepted; the perp-only seed keeps the spot balance (patch, not replace) |
| `operator` | the gated venue reads (`hl_client.rs`) and the info shapes | identity `accountValue == totalRawUsd + Σ signed(positionValue)` at 1e-6; `positionValue` unsigned; `szi` signed; `cumFunding.sinceOpen`; `leverage.type`; flat rows dropped; `liquidationPx` isolated-only; `time` ≤6 s; `totalRawUsd` > 0; `meta.universe` carries `szDecimals`; `openOrders` = `[]`; a non-default dex → empty shape 200; `perpDexs` = `[null]`; `l2Book` exactly 2 sides, ≤20 levels; `userAbstraction` in the enum; a unified account's `available + maintenance == collateral balance` (their reconciliation); an unconfigured account → empty shape 200 (passthrough) |
| `indexer` | the 5 s poller cadence, then the crash arc | three ticks ≤6 s old; the series is live; `crash` accepted and owned; the crash draws the equity down; hand-back recovers the live mark |
| `time` | `advance_time_then_refresh`'s shape and the venue-side drawdown | T accepts a forward `anvil_setTime` jump; V accepts a direct jump; `advance` runs the automaton; an underwater position is liquidated (closed at the mark) |
| `epoch` | the revert-to-blank on both halves | `epoch_new` accepted + `engine_repinned`; the venue engine's `fork_pin` present; the sim's account is gone; T's `/admin/epoch` accepted; T's engine is live after the re-pin |

The endpoints it expects (the scratch stack's ports; point them at a
deployment's NodePorts to run the same arc there):

| Endpoint | Constant | Scratch default | Deployment NodePort |
|---|---|---|---|
| T RPC | `T_RPC` | `http://127.0.0.1:18602` | 30545 |
| T admin (engine) | `T_ADMIN` | `http://127.0.0.1:18680` | 30880 |
| V RPC | `V_RPC` | `http://127.0.0.1:18610` | 31545 |
| V admin (engine) | `V_ADMIN` | `http://127.0.0.1:18681` | 31880 |
| venue-sim info | `SIM_INFO` | `http://127.0.0.1:18790` | 31090 |
| venue-sim control | `SIM_CTRL` | `http://127.0.0.1:18791` | 31091 |

`CAST` defaults to `/tmp/opencode/foundry-bin/cast`; every constant is an env
override (`CAST=... python3 scripts/daloc-sim.py`), so no edit is needed. Run:

```bash
python3 scripts/daloc-sim.py --lane all        # every role, in order
python3 scripts/daloc-sim.py --lane operator   # one role, for a focused run
```

## Venue smoke — running the venue stack locally

The core venue stack is three processes: V (fork-mode Anvil of HyperEVM), the
venue engine (`venue-mirror`), and the simulator (`venue-sim`). The manifests'
shape, without a cluster:

```bash
# 1. V: fork the public chain at its latest block. The flags are the parity
#    set (plan §17.6): cancun (the real chain is Cancun-without-blobs), the
#    gas/code-size/keeper values daloc's entrypoint uses, no-mining (the
#    engine owns block production), auto-impersonate (the bridge's Core->EVM
#    completion sends from the system address).
export HL_RPC_URL=https://rpc.hyperliquid.xyz/evm
PIN=$(cast block-number --rpc-url $HL_RPC_URL)
anvil --host 0.0.0.0 --port 8545 \
  --fork-url $HL_RPC_URL --fork-block-number $PIN \
  --chain-id 999 --hardfork cancun --gas-limit 100000000 \
  --code-size-limit 99999 --disable-pool-balance-checks --order fifo \
  --no-mining --auto-impersonate --prune-history 32 \
  --transaction-block-keeper 1000000 --no-rate-limit \
  --no-request-size-limit --no-fork-node-info
```

```bash
# 2. The venue engine (fork driver + admin API on 8080) and the simulator
#    (info 8090, control 8091). Both read the same instance file; every ${VAR}
#    in it must resolve at load time (a missing one is a boot error naming the
#    variable), so export the full set - including the refs of sections whose
#    service you are not starting (the loader expands the whole file).
export STATE_DIR=/tmp/venue-engine-state SIM_STATE_DIR=/tmp/venue-sim-state
export ANVIL_V_URL=http://127.0.0.1:8545 ENGINE_ADMIN_URL=http://127.0.0.1:8080
export CCTP_STATE_DIR=/tmp/venue-cctp-state T_RPC_URL=http://127.0.0.1:8545
export CCTP_RELAY_KEY=0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d
cargo run -p venue-engine -- run --config config/instances/venue-mirror/instance.toml
cargo run -p venue-sim    -- config/instances/venue-mirror/instance.toml
```

What to check:

```bash
cast chain-id --rpc-url http://127.0.0.1:8545            # 999
cast rpc anvil_getAutomine --rpc-url http://127.0.0.1:8545   # false
curl -s localhost:8080/healthz                           # ok
curl -s localhost:8080/status | jq '{head, cursor, fork_pin, epoch_pin, shadowed_slots}'
curl -s localhost:8080/metrics | grep -E '^venue_'
curl -s localhost:8090/readyz                            # ready once the market plane is fresh
curl -s -X POST localhost:8090/info -d '{"type":"metaAndAssetCtxs"}' | jq '.[0].universe[0]'
curl -s -X POST localhost:8091/control/deal \
  -d '{"address":"0x00000000000000000000000000000000000000aa","class":"perp","amount":"1000"}'
```

- The engine's log shows `anvil is up` and then either `at-pin probes complete`
  (the pin was still the source's head) or `at-pin probes skipped` - the normal
  case on a live source, which advances past the pin before the comparison runs
  (`venue_probe_skipped_total` counts it). Both are healthy; a *failure* count
  is the alarm.
- `/status.fork_pin` and `epoch_pin` are both set once boot's reconcile ran; a
  deal marks the slot it writes, so `shadowed_slots` grows with it.
- **The engine is V's block producer**: the driver mines a non-empty pool once
  per pass (`venue_mined_blocks_total`). T's producer is the applier (one block
  per applied parent block), while V's source windows land as cheatcode writes
  (no block per window) — without the mine the
  bridge's boot txs sit in the pool forever (measured: the attester install
  failed `… was not mined within 120s` on a chain nobody mined). An idle chain
  stays static; the harness's own `anvil_mine` calls are just extra (empty)
  blocks.
- The epoch drill: `curl -s -X POST localhost:8091/control/epoch_new`, then
  watch `fork_pin` advance and `venue_epochs_total` increment.
- In-cluster: `kubectl apply -k k8s/overlays/venue-mirror` (namespace
  `venue-mirror`; NodePorts 31545/31880/31090/31091), then `make venue-smoke`
  (`scripts/venue-smoke.sh`): V's chain 999 / head ≥ pin / automine off, the
  engine's `/healthz`, `/status` (the engine pin must equal V's pin) and
  `/metrics`, the sim's `/healthz`, `/readyz`, control `/health` and
  `venue_sim_ws_connected`, the `venue_bridge_*` counters, and the `meta`
  shape.

### The bridge's token door (measured on the live chain)

The bridge credits EVM→Core only from a `Transfer` log emitted by the token's
*linked* EVM contract (`spotMeta.evmContract`) — and that rule meets two real
chain facts:

- **Circle's USDC blacklists the system addresses** (`isBlacklisted(0x2000…0000)
  == true` live), so a plain USDC transfer to the system address reverts
  ("Blacklistable: account is blacklisted") — and USDC's `spotMeta` link
  (`0x6b9e…`) is a phantom that cannot emit `Transfer` logs at all. USDC moves
  to HyperCore by CCTP mints, never by the system-address ERC-20 path; the
  bridge's refusal is the faithful behavior.
- The faithful proof token is **PURR** (spot index 1, link `0x9b498c…` — a real
  18-dec ERC-20 whose system address is permitted; core wei 5). `make
  venue-proof` uses it for the round trip: transfer 5 PURR to `0x20…01` (the
  credit), then a `sendAsset` naming that address (the completion — the system
  address sends the ERC-20 back), exact balances end to end.

### The wire audit against the live venue (measured live, 2026-10-23)

Every served `/info` type was diffed field-by-field against the real venue's
live answers (a recursive path comparison of the same requests, public reads):

- **`SpotPair` served `is_canonical`** — the venue's key is `isCanonical`, and
  the missing rename silently *dropped* the live value on deserialize (the
  existing parse test could not catch it: `#[serde(default)]` masks the missing
  field). Fixed, and pinned by a name test (`served_field_names_are_the_venues`)
  that asserts the serialized keys themselves.
- **`l2Book` for an unknown coin**: the venue answers HTTP 200 `null`; the sim
  answered 404. Fixed (a typed `Option<L2Book>`); a *configured* market with no
  book yet stays a 503 (warming) — a different fault, kept distinct.
- Everything else matched exactly: `meta`, `metaAndAssetCtxs`, `l2Book`,
  `allMids` (1388 leaf paths — every coin the venue lists), `clearinghouseState`,
  `spotMeta` (after the fix), `spotClearinghouseState`, `exchangeStatus`,
  `perpDexs` (the dex objects' fields, pairs-of-pairs included),
  `userAbstraction`, `marginTable`, `activeAssetData`, `openOrders`.
- Measured venue quirks kept as named divergences (no client path reaches
  them): an unknown `marginTable` id answers 500 `null` on the venue (the sim
  answers 404); a malformed body is 422 on the venue (the sim answers 400 — the
  documented "a malformed body is the caller's fault" rule); an unknown info
  *type* answers `"unrouted"` (hl_mock's contract, the plan's choice).
- The refusal display lost its surface-specific `invalid control request:`
  prefix: the reason carries its own context (`invalid info request: …`, the
  venue's own order strings), so `/info` and the control plane each serve
  exactly what their reason says.

### The freshness gate and the local feed bridge (measured live, 2026-10-23)

- **The staleness drill** (`venue-order-proof` H, live): `POST
  /control/stall {"seconds":45}` holds the live plane; past the shared 30 s
  window `/readyz` answers 503 and an order refuses by name ("the ETH book is
  stale (31340 ms since the last live update)"); clearing the stall restores
  readyz 200 and fills again. A scripted book is exempt — it is the scenario's
  present, never stale.
- **The T-side feed bridge, locally** (`make mainnet-feed-local`, 6 checks):
  the scratch T's feed targets are empty (daloc's fixture deploy installs the
  mocks on the cluster), so the proof installs a hand-assembled
  Chainlink-shaped stub (`decimals`/`latestRoundData`/`updateAnswer`) at each
  configured target — then the bridge publishes the *live mainnet* answers
  (measured: WETH $2396.35, WBTC $81640.39, USDC $0.9999, USDT $0.9989,
  wstETH $2400.25), a scenario's write pauses the feed with its value
  preserved (never overwritten), and the hand-back republishes the live value.
  A publish's tx mines on the next applied parent block, so the waits are
  budgeted past one mainnet block.
- **`noop` and the user-less order** (live): `{"type":"noop"}` answers
  `{"status":"ok","response":{"type":"default"}}` with no `user` field (the
  SDK's shape); an *order* without `user` refuses by name in the err envelope
  (HTTP 200) — the devnet has no keys to derive the signer.

### The unified-margin and CoreWriter doors (measured live, 2026-10-23)

`make cctp-margin-cycle` now runs 34 checks and carries the axiom's two
hardest doors end to end:

- **The CoreWriter order door** (action 1, the HyperEVM path): a real
  `sendRawAction` tx on V is decoded by the bridge (10⁸ units, tif 3 = Ioc)
  and fills through the **same lane** as `/exchange` and `/control/order`
  (measured: the position went 0 → 0.02, then the size closed through the
  control door — both doors, one lane).
- **Unified collateral**: the abstraction is set through the **real path**
  (CoreWriter action 16, not the control fixture), a spot-only deal funds the
  account, and a perp order fills against the collateral — the docs' "USDC
  unifies perps + spot". The account view's unified branch folds the
  collateral token's spot balance into `totalRawUsd`, so daloc's identity
  gate stays exact with it included (measured: `accountValue 250.003 =
  totalRawUsd 225.985 + positionValue 24.018`); a `usdClassTransfer` moves
  between the two sides and never both. Before this fix a unified account
  funded only on the spot side could not open a perp position at all.

The order refusals now speak the venue's **own vocabulary** (plan §16.3's
measured strings), and `l2Book`'s `nSigFigs`/`mantissa` are applied and
validated rather than silently ignored (measured live: `nSigFigs=2` merged
the top asks to one level at `2400`; `nSigFigs=9` and `mantissa` without
`nSigFigs` both answered 400).

### The bringup parity their harness needs (measured live, 2026-10-23)

The daloc harness's own chains run with `--code-size-limit 99999` +
`--transaction-block-keeper 1000000` (T and V) and `--gas-limit 100000000` (V)
— their margin-agent deploys exceed EIP-170, and their failing-run forensics
read the run's own chain. Both manifests carry them (anvil-t.yaml, anvil-v.yaml)
and the direct-mode scratch launch matches. Measured with the flags in place:
a **25,000-byte runtime deploy** lands on both chains (T with a bpo2-compliant
tx gas; V as-is). Two facts the redesign must respect, both measured:

- **T (bpo2) enforces EIP-7825's 16,777,216 per-transaction gas cap** — a tx
  *declaring* more sits pending forever (measured: gas 30,000,000 stayed
  pending while the identical deploy with 16,000,000 landed). V (cancun) has
  no cap. Alloy's estimator stays under the cap for normal txs; a hand-built
  tx must.
- **A fresh fork's head timestamp is the pin's age** (V's head read 30 min
  behind wall right after a same-pin re-fork; the head *block* is the source
  pin until something is mined). Their boot contract pins `anvil_setTime(wall)`
  on every chain, so this is theirs to set; the untouched pass-through check in
  `venue-proof` compares against wall (the real venue's clock), never against
  V's jumped head — the two are independent clocks.

The CCTP expiration window is a config knob with two honest settings: **0 = no
expiry** (the product instances and every proof here) and a short window
(measured with 3 destination blocks) for the re-attestation drill — a record
that expires parks with the contract's own "Message expired and must be
re-signed" and is re-armed by `POST /v2/reattest/{nonce}` (the operator action
the bridge documents, never a silent retry).

### The order lane (measured live, 2026-10-23)

`make venue-order-proof` (37 checks, direct mode against the scratch stack) is
the §4.5 impact lane's acceptance: with a **scripted** base the walk is
deterministic — a 2.5 buy across asks `3001×2, 3002×1` filled at the
size-weighted `3001.20`, the fill's `consumed` recorded the two levels
exactly, the fully consumed level left the served book (asks `3002×0.5`), the
bid side served verbatim, and a second 0.5 buy walked to `3002`. The mark was
scenario-owned for the check and identical after the fill (I3 — a live mark
legitimately moves between reads, so the strict check owns it first). The
impact persisted through live updates while the scripted base owned the coin
(measured: an unscoped prune dropped a scripted level's consumption on the
next live update and the level reappeared — the fix scopes I2/I6 to the
*served* base), a re-scripted base prunes records it no longer carries, and
`set_book` without levels clears the coin's consumptions (`owned_levels` back
to 0). The trader's state: 3 long at the weighted entry, the identity exact
(`accountValue == totalRawUsd + Σ signed positionValue`, within the served
`positionValue`'s 2dp rounding), a reduce-only sell realizing at the bid,
then flat. The `/exchange` door served the venue's own envelopes: a filled
status, a priced `Gtc` resting (`openOrders` serves it, `cancel` closes it),
a non-order action answering the `err` envelope. A live order filled at the
touch (`2400.0`-class prints) — the real depth, not a simulation of it.

`make cctp-margin-cycle` is the crosschain unified-margin cycle (the section above carries the latest measured count),
measured live end to end: the T faucet funded the bridge wallet (the deal
*set* the slot to exactly the amount — used only when short, the roundtrip's
own pattern), the T→V burn relayed and V minted exactly to the trader, the
venue-side spot ledger was credited (the mint analogue — the EVM→Core USDC
path is faithfully refused, see "the bridge's token door"), a **real
CoreWriter USD-class transfer** on V (action 7, 10^8 core-wei units) moved it
spot → perp through the bridge's decoder with exact deltas, an order filled
`0.01 ETH` at the live touch with the identity exact, the reduce-only close
flattened, the trader returned the V USDC and the V→T burn closed the cycle
exactly on T (the burn left the source, the return mint restored it), and the
hand-back cleared the consumptions.

### The served clock after a harness time jump (measured 2026-10-23)

Their harness jumps every chain forward (`anvil_setTime` + mine) and then reads
the venue; a `time` that lags the chain afterwards fails their freshness gate.
The bridge observes V's head timestamp once per new head (one header read at the
top of each scan pass, before the block loop) and the sim serves
`max(wall, sim clock, chain time)`. Measured live: after a +3600 s jump of V, a
**touched** account served `time == V's head timestamp` (1792747268 vs wall
1792740301), and the served clock never moved backwards. An **untouched**
account kept serving the *real venue's* own time — the COW overlay's split:
untouched accounts pass through to the live venue verbatim (which is also why
an unknown address's `clearinghouseState` carries the real venue's `time`,
never the sim's). `make venue-proof`'s leg G encodes exactly this: touched
follows the jump, untouched stays pass-through, neither goes backwards.

The trace lane is real on local blocks: `debug_traceBlockByNumber` (callTracer)
answers for V's own blocks (measured on three). It answers `Method not found`
for **pre-pin source blocks** — anvil cannot re-execute a forked block's state
(measured: a hand-typed pre-pin height) — and the bridge never traces those
(its cursor anchors at `head.max(pin)` and only advances above it), so a
`venue_bridge_trace_faults_total` increment means a real degradation, not the
fork boundary.

### The sim's control verbs (measured 2026-10-23)

`POST /control/set_funding {"coin","rate?"}` answers `{"coin","owned","rate"}`
and the override is served through `metaAndAssetCtxs` (`funding` = the
override); omitting `rate` clears (`owned:false`). `POST /control/set_book
{"coin","levels?"}` takes the **wire shape** — `levels` is the two-sided array
(`[[{px,sz,n}…],[…]]`); a `{bids,asks}` map is refused by serde naming the
field. A scripted book is served verbatim by `l2Book` with `time` stamped at
serve; omitting `levels` clears (`{"owned":false,"levels":0}`) and the live
book returns. `activeAssetCtx` is **not** a served REST type and correctly
answers `"unrouted"` (it is a WS channel; hl_mock's contract).

### Negative venue equity, end-to-end (measured 2026-10-23)

The master plan's P6.1/P6.2 wiring requires the sim to *produce and serve*
negative venue equity (the signed venue term — §12.7's first row), and daloc's
gate must pass it through. Measured live on the sim:

- **the identity is exact both ways** — `accountValue == totalRawUsd + Σ signed
  positionValue` held to the last digit on a seeded long after a −40 % crash
  (`99132.398977… == 999.998977… + 98132.4…`); `totalRawUsd` is the signed
  cost-netted base, not the cash;
- **the stable negative state is a debt with no positions** — an account whose
  base sits below minus the position value serves a negative `accountValue`
  (measured `−1000.000000…`, a signed string) and stays there:
  `liquidate_if_needed` skips position-less accounts by design ("negative
  equity alone is a state the venue reports, not a liquidation to re-run every
  tick");
- **liquidation socializes the residual** — an underwater long is closed at the
  mark (counted in `venue_sim_liquidations_total`), the loss is realized into
  the balance and the residual is burned to zero (§4.6's "collateral consumed,
  debt burned"), so the *pre-liquidation window* is the only place a
  position-bearing negative equity is visible — a fixture that needs it must be
  read within the tick (measured: the first read serves negative, the tick then
  closes it).

### The wire types are pinned against measured samples

Every info shape the sim serves — and every CoreWriter payload the bridge
decodes — is parsed from a trimmed **live sample** in a unit test
(`wire::tests::measured_shapes_parse_through_their_types`,
`bridge::corewriter::tests::*`), and the ERC-20/CoreWriter surfaces are alloy's
own `sol!` types: the `Transfer` event, the `sendRawAction(bytes)` wrapper and
the eight action payloads are decoded by `SolEvent`/`SolCall`/`SolValue`, never
by hand. The reason is measured: `PerpDex` lacked
`#[serde(rename_all = "camelCase")]` and had never parsed the live shape (the
sim serves `[null]` and reads `perpDexs` nowhere), so its field names would
have mismatched silently the day a multi-dex path read one; the shape test now
fails on exactly that class. `perpDexs` also gained the four fields the live
dexes carry (`assetToFundingClamp` / `assetToFundingInterestRate` /
`assetToFundingMultiplier` / `subDeployers`) — complete wire types, unused
fields included.

## Engine symmetry (the two services read the same way)

The two engines are one design over two sources. The shared crates carry the
substrate — `mirror-devnet` (shadow, journal, cursor, admin, probes, metrics,
the epoch rule, the atomic state-file body, the devnet plane) —
`mirror-anvil` (the RPC boundary), `mirror-config` (the layered config plus the
binaries' shared boot conventions: the worker stack budget, the logging init,
the fatal-error report, the runtime shell `run_main` and the default instance
path) — and each project owns its driver
(`crates/mainnet/src/driver.rs`, `crates/venue/src/driver.rs`), its
source-specific loops, its `Engine` impl (the shell's facts), and its extra
routes + error mapping. The admin *semantics* are shared whole: the hand-back carries its own scan-to-head race guard
(`AdminSurface::shadow_hand_back`), the rewind response is assembled once
(`AdminSurface::rewind_response`), and the `/status` shadowed-addresses
presentation is the registry's (`Shadow::shadowed_addresses`). The refusal
taxonomy is shared too: `mirror_devnet::Error::{is_request_refusal,
is_unavailable}` classify 4xx vs 503 once, and both engines map an upstream
transport fault (anvil, the venue API) to 502.

Five more single homes keep the two engines from drifting:

- **the engine shell** (`mirror_devnet::engine` + `mirror_devnet::handlers`):
  the `Engine` trait names exactly the facts that differ (the chain, the
  source URL, the journal semantics, the probes, the scan counters, the
  status document, the metric exposition); everything else — the admin
  surface's construction, `/admin/epoch`, the boot assert (`assert_chain`),
  the handlers (one generic definition per route, where a two-invocation
  macro used to expand them), the route table, `serve`/`spawn_http`, and the
  shutdown tail — has one implementation. `ShellError` is the constructors
  those helpers need (bind / IO / chain identity). A new refusal class or a
  renamed status field lands on both surfaces in one commit.
- **the devnet plane** (`mirror_devnet::Plane`): the shadow registry, the
  journal, the cursor state, the shared admin gauges and the flags (halt,
  freeze, clock offset, scan watermark, re-apply marker) are one bundle both
  contexts embed; the operations that must not drift — save the state, save
  the registry, the halt check, building the admin surface — live there, and
  the admin surface itself holds the plane plus only the profile's own facts
  (target chain, re-sync source, chunk size, journaling policy). The venue's
  no-clock/no-freeze facts are documented once, on the plane. The shared
  primitives take the bundle (the scanner and the deal take the plane or the
  surface, never ten loose references).
- **the epoch rule** (`mirror_devnet::epoch`): a fresh pin drops marks and
  values, the same pin re-applies the recorded writes, an unknown pin beside a
  non-empty registry is fail-closed as fresh, and the transition is persisted
  *before* the pin moves (a crash must not leave the stale registry on disk
  beside an advanced `epoch_pin` — the next boot would read the canary as
  matching and keep it). The pin is anchored *before any writes exist* (T
  explicitly at boot; V through its boot-time reconcile), so a later same-pin
  restart is a provable restore rather than an unprovable epoch. Each engine
  maps the outcome to its own counters (T counts resets *found*; V counts
  epoch transitions and restored writes).
- **the state-file body** (`mirror_devnet::store`): one atomic JSON
  load/save (absent is a fact; a fault and corruption are typed errors; a save
  is temp + rename) used by the registries and by the venue's own sim/feed
  caches — the venue's error enum carries it transparently.
- **the metric registry convention** (`mirror_devnet::metrics::Registry`):
  `set`/`add`/`inc` take the field as the operand and are defined once for
  every registry (the shared gauges, `MainnetMetrics`, `VenueMetrics`); a
  profile's registry cannot grow its own variant of the convention.

`make engine-symmetry` (`scripts/engine-symmetry.sh`, 23 checks) is the
fitness function: it drives both engines' shared verbs live and requires
identical statuses, shapes and refusal wording (digits normalized — a message
may embed each chain's own cursor or ring size), the shared `/status` fields,
the shared admin gauges in `/metrics`, and exactly the documented absences.
The behavioral checks, not just shapes:

- **pause/resume effect**: while paused both report `halted`, refuse
  `/healthz` with 503, and raise `mirror_halted` to 1; resume restores all
  three (measured live). `/livez` stays 200 throughout — it is the
  orchestrator's probe, deliberately pause-agnostic (a probe on `/healthz`
  would drop the engine — NodePort included — out of the Service and restart
  the pod mid-pause).
- **shadow hand-back**: a no-op hand-back (`clear` with no keys) is the same
  200 shape on both, and writes nothing — safe to assert anywhere.
- **valid exposition**: each engine's `/metrics` carries no duplicate
  (name + labels) series — the shared admin gauges were once defined twice,
  with drifted HELP text (fixed 2026-10-23); the check keeps a re-introduction
  visible. The sets themselves are layered: the shared crate renders exactly
  the three admin gauges, and each profile renders its own set beside them
  (`MainnetMetrics` / `VenueMetrics`) — pinned by a unit test on each side.
- **rewind**: a beyond-ring request is one named refusal byte-for-byte; a
  1-block rewind returns the same wire shape on both (direct mode only — on
  the cluster it is a live mutation; the counts legitimately differ: T's
  applier journals parent-diff writes, V may have none).
- **deal**: both discover a slot and write the same balance. On clean chains
  both USDCs are byte-identical FiatTokenV2 deployments (balances mapping at
  slot 9) and the slots must be *the same*. A scenario that replaces one
  chain's token bytecode breaks that premise — daloc's fixtures mock T's USDC
  with an OZ ERC20 (slot 0); measured, each discovery matched the expected
  keccak slot for its layout (`0xa5701d…` = keccak(pad(acct)‖pad(0)),
  `0x9a4401…` = keccak(pad(acct)‖pad(9))) — so such runs set
  `DEAL_LAYOUT_TOLERANT=1`, which keeps the behavioral symmetry required.
- **the documented absences**: `/admin/time` — the ethereum's persistent clock
  offset; V's clock is native anvil (`setNextBlockTimestamp`/`evm_mine`), so a
  route nobody re-aligns would be a no-op (measured: engine-mined venue blocks
  advance the clock ~1 s each, so the head's lag behind wall is the time since
  the last mine, while T's applier stamps each block at its parent's time —
  the harness's own mining drives V's clock, and its boot `setTime` pins it to
  wall). `/admin/feed*` — the L1 feed
  bridge's routes; the venue's counterpart is the simulator's control plane
  (`/control/crash`, `/control/deal`, …).
- **`/status`** — each engine reports its own facts (reth/T vs fork
  pin/probes/source); the shared fields (`cursor`, `halted`, `shadowed*`) are
  present on both, and `cursor` means the *devnet chain's* cursor on both
  (T-height; the plan's clock-labels rule). The mainnet profile names its
  clocks explicitly: `cursor_t` is T's applied-parent cursor.
- **the venue plane's alarm set** — the plan §7.8 alarms are served under the
  sim's own spellings: API lag as `venue_sim_stale_ms{coin}` (per coin, ms —
  finer-grained than a per-feed seconds gauge); the rate budget as
  `venue_sim_budget_weight_used` (with `venue_sim_budget_skips_total`); the
  invariant-violation page as `venue_sim_identity_violations_total`; the
  bridge's actions as the typed `venue_bridge_*_total` family, and its pending
  window as the **cursor lag** (`venue_head` − `venue_cursor`: the bridge
  applies a block's effects and its cursor in one atomic write, so a growing
  lag is a stuck bridge — the block is re-scanned until it lands).
  `venue_feed_overrides` and `venue_sim_advances_total` are served under those
  names. The CCTP bridge's own queue is a separate service's gauge
  (`cctp_messages_pending`, the emulated PENDING window). There is no
  `venue-closure` target: the venue side has no diff applier/watchlist by
  chain nature (no BALs on HyperEVM), and its V-side wiring is verified at
  boot by the bridge's `verify_wiring` (chain id, contracts, roles) — the
  closure check that applies.
- **rewind semantics** (deliberate): T's journal holds the applier's
  parent-diff before-values, so `/admin/rewind` on T undoes *applied parent
  blocks*; a T-side engine write (the deal faucet) is undone by the hand-back
  (`/admin/shadow {clear}`), which re-syncs the source's value. On V the
  journal holds the engine's own writes (the follow's source windows are not
  journaled: a rewind restores devnet writes, never source history), so one
  rewind covers the deal's before-value
  (measured: a V rewind restores a deal's before-value,
  `restored_values: 1`). Divergences are recorded, never fatal — no run
  freezes itself.

The behavioral contracts behind the surface are also verified by the drills
(measured live 2026-10-23): `pause` freezes both engines' cursors (T's head
held for 12 s; V's pool accumulated while the cursor stayed) and `resume`
drains and advances; `/admin/deal {amount:"0"}` is the drain (0 on both
chains); a successful `rewind` restores the journal's before-values (the
venue: `restored_values: 1`, the balance returned to its pre-deal value); the
T-side shadow round-trip on the extracted path (`register` → `clear` →
`clear all`) was re-verified after the extraction (resynced 1, then 2 keys).

Entry shape follows the need, not symmetry for its own sake: the ethereum's
lib owns a `main_entry` with subcommands (`run` / `selftest` / `rewind`); the
venue crate exposes `main_entry` (the engine) and `sim_entry` (the simulator);
the cctp crate exposes `main_entry` — and every bin is the same thin shell
over its entry (the composition roots live in the crates, plan §11.3 rule 10),
with the same shutdown path shared by all four binaries:
`boot::shutdown_signal` waits for **SIGINT *and* SIGTERM** (Kubernetes sends
SIGTERM; measured 2026-10-23: before the fix, no service log had ever shown
its shutdown line — a `ctrl_c`-only wait was dead code under a rollout), then
the engine aborts its tasks (every mutation persists as it happens), the sim
saves its state, and the CCTP bridge saves its store. There is no venue
counterpart of `mainnet-closure-check`: the venue has no incoming parent diffs
to close a scope over — its standing scope check is the at-pin probe set
(`/admin/probes`, `venue_probe_*`), which the venue-drill and smoke exercise.

There is also no unified "facade" service (plan §7): each mirror's
scenario surface is its own — `/admin/*` on the two engines, `/control/*` on
the simulator — and the Make targets parameterize by `INSTANCE`. A proxy would
add a hop without adding a fact: the harness speaks each surface directly
(§7.7), and the shared crates already make the two surfaces one vocabulary
(`mirror_devnet::admin`, the same control shapes).

## The feed bridge — measured behavior

The bridge is the single price writer for the devnet's mock aggregators in
mirror mode (the harness's periodic refresher is off, plan §17.2): it reads
each feed's *source* (the parent chain's own aggregator, via reth) and
publishes `updateAnswer` to the configured *target* on T. Verified live on the
scratch T (2026-10-23; ETH/USD: source = mainnet's aggregator, target = a mock
aggregator deployed for the test):

- the target may not exist yet (their fixture deploy installs it): the bridge
  *waits*, reporting `decimals: null`; a transport fault or a
  present-but-mismatched target (decimals must equal the source's) is a fault,
  not a wait;
- a re-pin (epoch) wipes a devnet-deployed target: the bridge forgets the
  feed's decimals **and** value state and returns to *waiting* — not an error
  loop — and the reappeared target is first contact again (the epoch rule:
  the devnet's values are gone with the old base). Measured 2026-10-23
  (daloc-sim's epoch re-pin, then a fixture re-deploy at the same addresses):
  all five feeds went to waiting with **one warn each and zero pass errors**,
  then republished and confirmed on first contact;
- a publish that does not land within `confirm_window_ms` (default 60 s) is
  **re-sent** — the outcome-unknown reconciliation. An anvil pool is not
  durable and a same-nonce sender can displace ours: measured, daloc's deploy
  shares anvil account 0 with the writer, a publish racing the deploy's last
  txs was displaced, and the feed waited on `pending` forever. Measured live
  after the fix (applier paused so nothing could mine): an override + hand-back
  left a pending publish; past the 20 s scratch window the bridge logged
  `did not land within the confirmation window; re-publishing age_ms=23224`
  and the feed confirmed when mining resumed. `/admin/feed` carries
  `pending_since_ms` so the wait is visible. **Give the writer a dedicated
  account** — never the deployer's — so concurrent senders cannot race it;
- first contact publishes the source's answer; the next pass confirms it
  (`confirmed_answer`, `feed_bridge_updates_total{feed}`);
- a scenario write to the target pauses the feed on the next pass
  (`feed_bridge_pauses_total{feed}`, `paused: true`, the last confirmed answer
  kept) and the bridge **never overwrites** it — verified across repeated
  passes;
- `POST /admin/feed/resume` forgets the confirmed value; the next passes
  re-establish the source's answer (publish, then confirm);
- `POST /admin/feed/override {name, answer}` publishes a scripted value and
  pauses — the scripted crash; a later `resume` re-establishes the source.
  While the target is not installed the override is refused (a crash written
  to an empty address would change nothing);
- an unknown feed is refused `404 feed bridge: no feed named \`…\``;
- pass faults count in `feed_bridge_pass_errors_total` and the feed's
  `last_error` names the cause — a stuck feed is visible, never a silent stale
  success.

Verified against **their fixture aggregators** (measured 2026-10-23, after
their `deploy-contracts` ran on the scratch T): all five feeds the fixtures
install (`CHAINLINK_FEEDS`: WETH/WBTC/USDC/USDT/wstETH, 8 decimals) were
published and confirmed — each aggregator's `latestRoundData()` then returned
the *same answer* as the live mainnet source, and its `updatedAt` was a **T
block timestamp** (chain-native stamping, plan §17.2: the feeds are stamped in
the devnet chain's own domain).

## daloc's `deploy-contracts` against mirror T (acceptance)

Measured 2026-10-23: their `docker/scripts/deploy-contracts.sh` ran
end-to-end against the scratch T (`ANVIL_RPC_URL` → T, `CONTRACTS_DIR`, the
host's `forge`/`cast`/`jq`; the repo's submodules `lib/hyper-evm-lib` +
`lib/forge-std` and `contracts/lib`'s OZ deps must be present). It emitted a
54-line `deployed.env`. What it produced and what that taught us:

- **fixtures**: `MockERC20Fixed` over the canonical tokens (USDC/WETH/WBTC/
  DAI — the fixture list's set), five `MockChainlinkAggregator`s (the
  `CHAINLINK_FEEDS` addresses the feed bridge publishes to), the CCTP mock at
  `0x17`, the mock swap router, and seeded Aave reserves.
- **the fixture lists are exact against the deploy script's source** (audited
  2026-10-23): the only invisible writes are its `set_code` calls —
  `install_token` for USDC/WETH/WBTC/DAI on **every** chain, plus WHYPE/wstHYPE
  when `CHAIN_ID=999` (so the chain-999 list is those six and no more). Two
  subtleties the audit pinned: **USDT is declared as an env var but never
  installed** (never add it "for completeness"), and the aToken "seeding" is a
  **tx** (`fund_token` mints the underlying *to* the aToken) — auto-marked, so
  the L1 list's aToken entries exist for *scenario* cheatcode writes, not the
  deploy. The CCTP mock at `0x17` needs no entry: it is not in the watchlist
  (the closure derives from market-data views) and the address is empty on
  mainnet, so no diff can reach it.
- **the protocol set is the devnet's own**: the hub/registries/underwriter/
  executors land at *deployer-derived* addresses (their `DeployFactory` is
  deployed by the devnet deployer), **not** the addresses in their
  `contracts/deployments/ethereum.json` (which records their *mainnet*
  production deployment). The watchlist carries the mainnet set (faithful
  mirror); the devnet set is tx-deployed on T and shadow-marked by the
  scanner, so it needs no static watch.
- `SCAN_START_BLOCK` is emitted (their scanner's cold-start cursor), the CCTP
  mock is wired (`CCTP_MESSENGER_ADDRESS=0x17`), and the oracle adapters are
  recorded.
- on a no-mining chain the deploy needs blocks: with the engine **paused** and
  a modest `anvil_mine` loop the run is clean; resuming the engine then scans
  the deploy's blocks (their writes marked). The fixtures script is **not
  idempotent across runs** (a second run reverts); the clean path is a fresh
  re-pin + one run.
- **`anvil_dealERC20` works against their `MockERC20Fixed`** (the §17.4
  re-check): deal, lower and drain to 0 all land, and `/admin/deal` discovers
  the same slot — so their `fund_token_on`/drain paths need no mirror-side
  shim.

## The CCTP bridge — measured facts and acceptance

The real lane (`cctp-bridge`, Iris 8095 / control 8096) moves native USDC
between T and V over the real V2 contracts; the only substitutions are the
devnet attester (installed through the contracts' own setters) and the local
Iris. Proven live on scratch forks:

- bidirectional burns with exact mints, both directions through the service
  (measured 2026-10-23: a V burn 19→0 relayed to T, the wallet's T USDC 0 →
  2,000,000; the T→V leg mints on V). The scratch chains have no block producer
  of their own, so the burn's receipt needs a manual `evm_mine` loop; the
  service's own waits then return named outcomes (`mined:false`, "not mined
  within the window; the next boot retries") instead of hanging. Replay →
  `Nonce already used`; tampered signatures and empty attestations rejected by
  the real verification;
- PENDING emulation (`finality_delay_secs`): measured deliberately with the
  window raised to 30 s — the Iris served `{messages:[{message,
  attestation:null}]}` on eight consecutive polls over ~32 s, then the 65-byte
  hex (the watcher's log gap: watched 09:19:20 → attested 09:19:50, i.e. 30.5 s);
  at the 5 s default the same gap measured 5.5 s. A poll that starts after the
  window sees only a non-null attestation — the window is real, not a test
  artifact;
- `expiration_blocks` (anchored at the destination's head): measured — the
  contract rejects at `block.number == expirationBlock` and the relay's
  estimate runs at the pending block (head + 1), so a relayable window needs
  `N ≥ 2`. An expired message reverts "Message expired and must be re-signed"
  and the record parks after the send-refusal budget;
- `POST /v2/reattest/{nonce}` re-signs with a fresh window and re-arms the
  record; the relayer then mints — the full expiry → park → reattest → mint
  loop is proven;
- source rewind: a reverted-away burn is parked by the watcher ("the burn
  transaction no longer exists"); destination rewind: a rewound mint is parked
  by the relayer ("the mint is no longer on-chain"); reattest re-arms it and
  the mint lands again — but a reattest of a *vanished* burn is re-parked by
  the relayer ("the source burn no longer exists"), never minted;
- the manual relay mode (`relayer = "manual"` in `[services.cctp_bridge]`): a burn
  reaches `signed` and stays there (the tick keeps only the gas alerts and the
  rewind check); `POST /control/relay {nonce}` relays it on demand —
  `{"confirmed":1}`, minted — the deterministic way to test expiry/reattest
  windows. In `auto` (the default) the same burn relays within one tick of
  signing. Refusals are 400s naming the state (`no watched message with nonce
  …`, `… is Relayed; only SIGNED messages relay`); `{}` relays every signed
  record (a nudge that skips the poll interval in auto mode);
- a coordinated restart can fail the bridge's *first* boot: a receipt read times
  out while the engines restore their registries (measured:
  `eth_getTransactionReceipt: transport … operation timed out` right after both
  engines' boots). The deployment's restart backoff is the retry — the boot
  steps are idempotent (`enableAttester`/`setSignatureThreshold` re-run
  cleanly; a hook redeploy lands a fresh wrapper and the store records it) —
  and every service now waits (bounded) for its chains before the first read
  (`mirror_anvil::Rpc::wait_until_up`: the engines 900 s/120 s, the bridge
  300 s), so only a *slow-but-up* chain can still fail a boot;
- the vanished-burn guard reads `eth_getTransactionByHash` on the source and
  is **keeper-sensitive**: with `--transaction-block-keeper 2` anvil prunes tx
  bodies, receipts *and* logs beyond 2 blocks (measured), so any burn older
  than 2 blocks false-parks ("the source burn no longer exists"); the parity
  flags set keeper 1000000 on both chains (plan §17.6) and the lookup survives
  (re-verified live: a burn 10 blocks old relayed);
- the hook lane: `depositForBurnWithHook` + `destinationCaller = wrapper` →
  `wrapper.relay` mints **and** runs the encoded call in one tx (a USDC
  transfer hook moved the mint through the wrapper to the recipient); a
  deliberately failing hook still mints (tx status 1, the wrapper kept the
  funds, the hook failure is isolated) — the wrapper's documented
  non-atomicity;
- the pin clamp: a start block below the fork pin is clamped to the pin with
  one warning (anvil forwards pre-pin `eth_getLogs` ranges upstream, which
  rejects them — measured);
- the boot wiring asserts cover §15.1's five contracts: chain id,
  `MT.localDomain()`, `MT.version()`, `TM.messageBodyVersion()`,
  `TM.localMessageTransmitter()`, `TM.localMinter()` (measured 2026-10-23: the
  getter is `localMinter()` — a `localTokenMinter()` probe reverts), and
  `TM.remoteTokenMessengers(peer)`; a mismatch is a boot error naming the
  field, never a silent wrong address;
- the real reverts, string for string: source-side `Max fee must be less than
  amount`, `Mint recipient must be nonzero`, `Hook data is empty`; a message
  pinned to another `destinationCaller` → `Invalid caller for message` (the
  relayer pre-parks such records instead of paying for the revert); a message
  for domain 19 delivered to T (the wrong destination chain) reverts
  `Invalid destination domain` — proven with a *valid* attestation via
  `eth_call`, so the check is the contract's own, not the signature's.
- service-level refusals are typed and name the fix: a burn above the wallet's
  source balance answers 400 `invalid request: sender 0x7099…79C8 holds 0 USDC
  on ethereum, needs 1000000 (deal via the chain's admin surface)`, and a
  source read that needs an uncached fork slot while the fork upstream is down
  answers 502 carrying the whole chain (`… eth_call: error code -32603: failed
  to get storage for 0xb883…: MPP HTTP request to https://rpc.hyperliquid.xyz/
  failed: … operation timed out`) — a boundary fault is never a bare 500;
- **daloc's real `cctp-relay` cannot relay a real CCTP V2 message as written**
  (branch `feat/risk-engine-refactored-dev` @ `a400dbe`, measured 2026-10-23
  against this service with a real burn on the scratch T, and against the live
  Iris with a real mainnet burn): its scanner queues the burn keyed by
  `keccak256` of the **event** message (`services/cctp-relay/src/relay.rs`),
  but the Iris — Circle's and ours, faithfully — serves the
  **attester-patched** message: measured, the event message carries nonce `0`
  and `finalityThresholdExecuted 0`, the served message a 32-byte nonce and
  `2000` (the only byte ranges that differ on a live sample). `attempt_relay`'s
  `find(keccak256(m.message) == hash)` therefore never matches: the message sat
  `pending: 1` for 4+ minutes, never relayed, never parked; even on a match it
  would submit the queued *raw* message, which the real verification rejects.
  The fix is theirs and small — the tx-keyed Iris response is authoritative:
  select by tx (or match on the unpatched header fields) and submit
  `matched.message`. The same burn relayed through this service's own path
  mints exactly (`{"confirmed":1}`, 100 USDC on V), so the lane, the message
  and the attestation are sound; only their client's match is not. The plan's
  "their `cctp-relay` runs unchanged" acceptance needs this one code change.

Operator surface: `GET /health` (parked counts), `GET /control/state`,
`POST /control/transfer` (`hook: {target, calldata}` for the credit lane),
`POST /control/relay` (`{nonce?}` — relay now), `make cctp-smoke` (health +
store coherence + metric set) and `make cctp-roundtrip` (the scripted burn →
mint with the exact delta; `SOURCE`/`DESTINATION`/`AMOUNT` env-driven).

## Plan axioms → how they are covered

| # | Fact | Coverage |
|---|---|---|
| A1 | fork blocks have no trie | scenario: T head `stateRoot = 0x0`; proofs impossible by design (§6) |
| A2 | pin byte-identical to mainnet | scenario: pin block hash T == reth; `make pin-show` |
| A3 | block env aligned exactly | the applier sets all five context fields from the source header; an env error shows up as a probe divergence in the same block |
| A4 | withdrawals arrive as state | the BAL carries withdrawal recipients' post-balances; the applier writes them like any other balance change |
| A5 | in-block-funded txs need the flag | `--disable-pool-balance-checks` in the anvil deployment |
| A6 | devnet txs are never mixed with the source's writes | the prepend rule (own block, `last+1`), asserted by the scenario |
| A7 | blob data is not executed | the applier never fetches sidecars (blob txs execute by versioned hash); the CL keeps custody |
| A8 | apply is source-fetch-bound | `mirror_last_apply_seconds`; the local reth keeps it ~1 s/block even cold (remote measured 55-256 s) |
| A9 | sampled parity catches the A17 class | `drift.jsonl` anomaly entries + `mirror_full_sweep_mismatches_total` with the sweep on |
| A10 | T exposes the source's logs only through reth | T executes only devnet txs; parent-block logs live in reth (within its 10,064-block window) |
| A11 | `deal` writes storage silently | scenario: a WETH deal sets the balance with **no** Transfer log; the same probe finds USDC's packed `balanceAndBlacklistStates` slot (the sentinel stays below 2^255 so packed reads return it unmasked) |
| A12 | exactly one T block per parent block | scenario head parity + `anvil_mine(1)` |
| A13 | local writes never appear in the source's diffs | COW: the shadow filter skips owned keys; the scenario asserts the skip counters |
| A14 | warm restart preserves state | anvil `--state` removed (13 GB dumps, leaked tmp dirs, filled T's PVC); a fresh pin re-applies the pin window and the epoch rule restores the recorded writes at the *same* pin (a moved base clears them - plan 2.2/6.3; measured: a +30d time-travel write came back as a future `lastUpdateTimestamp` and every Aave supply panicked with an underflow) |
| A15 | memory bounded by restart | anvil-t steady state ~6.5 GB after a session of drills; recycles at the 16Gi limit |
| A16 | upstream reorgs detected | parent-hash continuity check; the applier continues on the new branch (absolute writes self-heal) |
| A17 | fork block hashes differ | inherent (verified: anvil exposes no blockhash/beacon-root setter); sampled parity records the class and the next block's absolute writes re-converge the state. Measured: ~45% of blocks carry such a mismatch, `mirror_t_convergence_failures_total` stays 0 in steady state |
| A18 | delivery is event-driven | consumers subscribe to anvil T WS (`eth_subscribe`) |
| A19 | scoped manipulation leaves parity intact | scenario: a shadowed slot survives further parent diffs; unshadow re-syncs from reth; T's probes skip the shadowed target |
| A20 | journal round-trip is faithful | scenario: snapshot/revert + `/admin/rewind` restore T's cursor and re-apply cleanly |

### The market-impact chain, measured live on T (2026-10-23)

The §4.5 invariants were run end-to-end against the running stack: a real
Uniswap V3 swap on the **watched** USDC/WETH 0.05% pool (50 WETH → 119,608
USDC, fee 500, via the canonical SwapRouter; the actor and its WETH came from
`/admin/deal`). Measured:

- **I1 scope** — the swap's 13 keys (the pool's `slot0` / `liquidity` /
  `feeGrowth` / `observations` / balances + the actor's) entered the registry
  (`shadowed_slots` 15 → 28); untouched neighbours kept following mainnet.
- **I2 persistence** — after 90 s of applied parent blocks the pool's `slot0`
  still held the swap's value while mainnet's own had moved on (drpc
  `16173…` vs T `16201…`); the passes that wrote those keys logged
  `skipped_shadowed=4`.
- **I3 oracle independence** — the Chainlink WETH/USD feed never moved across
  the swaps (`239282207741` before and after); the pool moved, the feed did
  not.
- **I4 reversibility** — `POST /admin/shadow {clear, all}` re-synced 13 keys
  (28 → 17) and restored reth's value at the cursor (`16184…` — the last
  applied parent, not the source head, by design).
- **I5 isolation** — every mainnet comparison above reads drpc; nothing was
  written to it.

The run forced a **fix**: the shadow scan used to run only *after* the pass's
parent writes, so the filter saw the previous pass's marks. A devnet tx mined
by the pass's own prepend — or out-of-band by the harness — could therefore be
clobbered before its mark existed, and a hot key like a pool's `slot0` is
written by essentially every parent block. The apply now scans (marking the
devnet's blocks) **before** the parent's writes are filtered, and a scan fault
holds the apply (writing on stale marks is the clobber). One footgun surfaced
and closed with it: a bare `{clear: true}` with no keys silently returned
`resynced_keys: 0` — it now refuses, naming `slots` / `account_fields` /
`all`.

### The same axioms on the Venue Mirror (plan §12.6)

The venue mirror shares the substrate axioms by **construction** (`crates/anvil` +
`crates/devnet` are literally the same modules) and drops the replay axioms by **chain nature**
(HyperEVM has no BALs, no beacon, no node replay, and the public RPC exposes no traces/proofs to
verify against). Verdicts:

| Verdict | Axioms | Where |
|---|---|---|
| **SAME** (same modules) | A1, A2, A5, A8, A11, A13, A14, A15, A18, A19, A20 + the pins/epochs/journal/COW/automine decisions | `crates/anvil` + `crates/devnet`, exercised by `make venue-smoke` and the epoch drill (same-pin restore / new-pin epoch) |
| **IMPOSSIBLE (permanent)** | A3, A4, A6, A7, A9, A12, A16, A17 + the mainnet guarantee (per-block BAL apply, sampled parity, the probes) | no replay exists to verify; the fork keeps the source's exact state and hashes at the pin |
| **REFRAMED** | L9 freshness → the pin (chain) + the live public API (venue data); the T guarantee → at-pin parity | `venue_fork_pin`, probes-at-pin (skip+count when the pin ≠ source head — measured: the source ignores the block parameter) |
| **NEW (venue plane)** | sim invariants: the served identity (`venue_sim_identity_violations_total` must stay 0), funding cadence, liquidation determinism, book/mark ownership (I3), bridge round-trip exactness | `crates/venue`; the identity is re-checked at the serve boundary |

Measured on the scratch stacks (2026-10-22) — facts an integration run can rely on:

- **`anvil_dealERC20` works on both mirror chains** (foundry 1.8.6; re-measured 2026-10-23 on T and
  V: absolute writes that raise, lower and drain to 0 — on the real USDC proxy and a slot-0 mock
  alike). The one trap is the argument order: foundry's is `(account, token, amount)`, and the
  reversed order reports "no slot found" — the earlier "fails on forks" measurement was that
  artifact, not the chain. daloc's `deal_erc20` / `add_erc20` / drain paths therefore work
  **unchanged**; the mirror's `POST /admin/deal` is the equivalent faucet its own scripts use
  (measured: slot 9 discovered, 200 USDC landed).
- **The real HyperEVM USDC blacklists its own system address** (`isBlacklisted(0x2000…0) == true`,
  measured on the live chain), so EVM→Core USDC is impossible with the real token; the bridge's
  credit path runs over the harness's fixture mocks (which replace the canonical ERC20s), and the
  bridge only credits a `Transfer` log emitted by the token's *linked* contract.
- **WS delivery works on both mirror chains**: subscribing `newHeads` + `logs`, two empty mines and
  one log-emitting tx (a WETH/WHYPE `deposit()`) delivered **3 newHeads + 1 log on T and on V**.
  **Re-tested on the *live* T (read-only, 2026-10-23): 4 `newHeads` in 50 s** (the applier mines
  every ~12 s) and **0 `logs`** — the earlier live-T silence was a no-mining window, not the WS,
  and 0 logs is by design (the applied blocks carry no txs; §A18). Their scanner's logs-filter +
  HTTP backup is unaffected either way.
  **§17.4's logs acceptance, closed on scratch T (2026-10-23)**: a `cast logs --subscribe`
  subscriber on the WETH address (their filter) stayed connected while a devnet tx emitted a
  `Transfer` — the log was delivered (block 26253639, `logIndex 0`), so the anomaly does not
  persist after an anvil restart; the HTTP backup stays the fallback for empty blocks.
  Re-confirmed on the **7-hour-old scratch T with no restart** (a second run: the WETH
  `Transfer` at block 26254224 delivered live) — long uptime alone does not silence the
  subscription; v1.8.7's log-filter-expiry fix is the watch item (OPERATIONS).
- **`eth_getLogs` ranges on T**: a 2000-block range answers locally (their chain-1 setting);
  a range reaching below the fork pin is forwarded to reth, which refuses above its 20 000-result
  cap with `query exceeds max results …, retry with the range …` — a wording their detector's
  `range`+`exceed` pattern already matches.
- **The hook (credit-carrying) lane is proven live** (§15.5's acceptance, measured
  2026-10-23 on the scratch stack): a burn with `hookData = USDC(20) ‖ approve(spender, 1)`
  and `destinationCaller` = the wrapper was relayed through the wrapper; one relay tx
  produced the mint (**5,000,000 at the wrapper**) *and* the encoded call
  (`allowance(wrapper, spender) == 1`) — the protocol-native `receiveRecovery` shape.
- **The relay settles by the *effect*, never the tx status** — a bug found by that
  proof. A `daloc-sim` epoch re-pins V, which removes the wrapper's code (it was
  deployed on the old base); a hook relay then called a **code-less address** — a
  vacuous *success* with no logs. The old `settle` trusted `receipt.status()` and
  marked the lost message `relayed` (a false confirmation). Now `settle` checks the
  chain's dedup truth first (**the destination nonce consumed ⟺ the mint happened**,
  for both the atomic `receiveMessage` and the non-atomic wrapper `relay`) and parks
  "succeeded but unconsumed" by name. The boot reconciler already handled the fallout:
  it parked both affected records ("destination rewound or re-pinned: the mint is no
  longer on-chain…") on the next restart. Two guards close the stale-wrapper window:
  the control path refuses to burn to a wrapper with **no code** (naming the re-pin),
  and the relay path parks the same case; a restart redeploys the wrapper, reinstalls
  the attester (both are re-pin-sensitive devnet deployments) and reconciles.
- **Negative equity flows end-to-end** (the master plan's venue-term requirement): seeding
  `totalRawUsd = -80000` with a long BTC position and crashing the mark served signed
  `accountValue = -2771.65…`; the next evaluation (an account read, the 1 Hz tick, or `advance` —
  §14.8's read-time evaluation) liquidated it, and the bare negative balance
  (`-2784.00…`, zero positions) still served — and the identity counter stayed 0, so the gated
  read passes negative values through.
- **The bridge burns sub-wei remainders like the real chain** (plan §16.3): EVM→Core credits floor
  at the Core wei, Core→EVM floors at the EVM wei — a non-round amount transfers with the dust
  burned instead of being refused. Unit-tested (`sub_wei_remainders_are_floored_not_refused`).
- **Pass-through reads spend the rate budget**: an untouched account's `/info` is forwarded to the
  live venue and accounted at the conservative heavy weight, so test reads and the poller share one
  budget (plan §18.3).
- **The venue's hand-back needs a fresh pin**: `/admin/shadow {clear}` re-syncs the source's truth
  at the pin, and the public RPC serves its *latest* state for `eth_call` (measured — the block
  parameter is ignored), so the re-sync is refused while the pin lags the source head. The refusal
  names the truth: a re-pin restores the *whole* source state (the re-fork is the wholesale
  hand-back), while a *selective* hand-back is only possible while the source's head is still the
  pin — seconds on a 1s-block source, so reliably only against a scratch/slow source. The
  registry is no substitute (its values are the *devnet's* writes, for the restore); the
  before-state lives in the journal, and a journal-based selective fallback (oldest entry, gated
  on the ring covering the epoch) is the planned slice. The mainnet's reth can serve the cursor's
  state, so its hand-back works at any time — the one deliberate asymmetry, and it is fail-closed:
  the refusal names the remedy, it never writes a wrong value.

## Known limitations (accepted)

1. **Storage proofs are impossible** on fork blocks (`stateRoot = 0x0`); use
   mock-storage-proofs in the harness.
2. **No chain interleaving**: mainnet txs and devnet txs cannot share one parent
   hash; T's heights advance at a different rate than mainnet's.
3. **Derived values go stale** when the base advances; persistence rides on the
   journal (operations), not on values.
4. **Diff application is silent**: state applied from the parent emits no logs on
   T; events above the pin come from the parent RPC.
5. **Blob-tx replay depends on the local CL's column coverage.** The mirror runs
   Lighthouse with `--semi-supernode`, which reconstructs any blob inside the
   18-day data-availability window (verified: sidecars for slots ~1.2 days old
   are served). Placeholder blob proofs are recomputed locally and verified
   against the tx's versioned hash. If a slot is outside the CL's coverage, the
   block is mined without its blob txs and recorded; **T's state still
   converges** through the next parent BAL (every state change is carried
   there).
6. **No replay means no memory ceiling**: the removed M cached every replayed
   tx (~15-25 MB/block) and long replays ended in a k8s restart; T applies the
   parent's BAL and never replays, so it is ~10× leaner.
7. **Only the last `--prune-history` blocks of a devnet chain are traceable**
   (`debug_traceTransaction`), because anvil keeps state snapshots in memory.
   reth serves `debug_traceBlockByNumber` inside its 10,064-block window for
   anything older.
8. **reth's tx lookups are pruned**: `eth_getTransactionByHash` and
   `eth_getTransactionReceipt(hash)` return `null` on reth;
   `eth_getBlockReceipts`/`eth_getBlockByNumber` work. The mirror's verifier and
   the devnet do not need lookups.
9. **Freshness = reth's head, driven by the local CL.** The engine's top-up
   loop supplies any missing ancestors over the Engine API (EIP-8379); no P2P
   or public RPC is involved. During a large catch-up reth's `safe`/`finalized`
   pointers stay at the snapshot's values (Lighthouse's forkchoice updates
   reference blocks reth has not executed yet), so the mirror's follow head is
   frozen until the catch-up completes, then jumps to live. Re-pin after the
   first catch-up for an exact, live base. See OPERATIONS → "Source liveness".
