# Testing

Three layers, each runnable on demand:

| Layer | Command | Needs | Covers |
|---|---|---|---|
| Unit | `make test-engine` (cargo test) | nothing | pure logic: BAL flattening, padding, COW granularity, journal, cursor/rebase rules, KZG proof repair, Engine API request encoding |
| Self test | `make selftest` | a public mainnet RPC + beacon API (**test-only** external deps; the runtime stack has none) | the whole pipeline end-to-end on **real mainnet blocks** (replay + verify + diff apply + sampled parity) |
| Smoke | `make smoke` | the deployed stack | service health + parity spot check on M's head vs reth |
| Scenario | `make scenario` | the deployed stack, no external network | self-containment, live parity on **M and T** (the 12 state probes vs reth at the same parent block), and the devnet workflow: faucet, copy-on-write shadowing, snapshot/revert, journal rewind |
| Product sim | `make product-sim` | the deployed stack, real venues | the margin-product workflow end to end: a levered position built through the faucet + Aave + Uniswap, freshness sampled across consecutive parent blocks, and a bot that sees the `Borrow` event over WS and liquidates after an oracle crash |

## Unit tests (`engine/src/**`)

11 tests, all offline:

- `bal::storage_values_are_padded_to_64` / `quantities_are_minimal_but_even_length`
  — the measured padding rules (odd-length hex is rejected by anvil with `-32602`).
- `bal::bal_flatten_uses_post_states` — last-change-wins per slot (EIP-7928
  ordering guarantee), balance/nonce post-states.
- `bal::trace_diff_flattening` — the `prestateTracer{diffMode}` fallback shape
  (measured on anvil 1.8.5).
- `shadow::cow_granularity` — storage shadows per slot, account fields as one
  unit, other slots of the same account keep syncing.
- `journal::journal_roundtrip_and_prune` — per-block before-values + ring.
- `state::resume_beats_warmup` / `fresh_or_reset_chain_uses_warmup_clamped_to_head`
  — restart vs first-run/re-pin cursor rules.
- `blob::kzg_validation_accepts_a_valid_sidecar` — builds a blob + commitment +
  proof with c-KZG, validates it through the replay path's validator, and
  rejects a corrupted blob. Proves the validator (and the proof repair) is
  correct, so "invalid KZG proof" from a source means the source's data is bad.
- `blob::execution_requests_follow_engine_api_encoding` — `engine/prague.md`:
  type-prefixed SSZ elements, little-endian quantities, empty request types
  excluded (all-empty ⇒ `[]`).
- `blob::versioned_hash_mapping` — commitment → versioned hash shape.

## Self test (`k8s/bootstrap/selftest-job.yaml`)

One Job, three containers in one pod:

1. `resolve-pin` (alpine) resolves `pin = rpc head - 16`,
2. `anvil-m` / `anvil-t` fork a public RPC at that pin,
3. `engine selftest` replays and applies blocks `pin+1 .. pin+3`, printing a
   check-by-check report (tx-count, blobGasUsed, logsBloom, log set, receipts
   with the type-4 gas rule, withdrawals; T: writes/filtered/shadowed,
   sampled storage parity) and exiting non-zero on any failure.

This is the fastest full-pipeline check and it exercises the **trace fallback**
(public RPCs do not serve `eth_getBlockAccessList`) and the **blob path**
(sidecars fetched from the beacon API, network-form encoding).

Expected output shape:

```
-- block 26242174
   M ✔ tx_count                 reth=251 m=251
   M ✔ blob_gas_used            reth=786432 m=786432
   M ✔ logs_bloom
   M ✔ log_set                  1103 logs
   M ✔ receipts                 251 receipts
   M ✔ withdrawals              16 withdrawals
   ...
   M replay: txs=251 replayed=251 skipped_blobs=0 ms=...
   T apply:  source=trace writes=... filtered=... skipped_shadowed=... sampled=8/8 ms=...
```

## Smoke (`scripts/smoke.sh`)

Checks pods, reth head + `eth_syncing` + `eth_getBlockAccessList` availability,
Lighthouse sync status, both anvil pins, the engine `/status`, and a parity
spot check: `logsBloom` and tx count of M's head block vs reth at the same
height.

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

## Invariants asserted per replayed block (on M)

| Invariant | Class | Where |
|---|---|---|
| tx-count parity | structural | `verify.rs` |
| tx order (hashes in position) | structural | `verify.rs` |
| `blobGasUsed` parity | structural | `verify.rs` |
| receipt count | structural | `verify.rs` |
| withdrawal recipient balances equal reth at N | structural | `verify.rs` |
| `logsBloom` parity | behavioral | `verify.rs` |
| log-set equality (address, topics, data) | behavioral | `verify.rs` |
| receipt statuses identical; gas Δ only on type-4, in `[-12,500 × #auths, 0]` | behavioral | `verify.rs` |
| post-repair convergence (probes: `eth_call` on M's unmined repaired state vs reth at N-1) | convergence invariant | `verify.rs::convergence_check` |
| M chain continuity (head == N-1 before replay) | structural | `replay.rs` |
| reth parent-hash continuity between consecutive blocks (reorg detection) | structural | `main.rs` |
| T sampled storage parity on written keys | diff | `diff.rs` |

Divergence classes are recorded in `drift.jsonl` (divergences only)
(`divergence_class: none|value_level|fatal`) and counted separately in metrics
(`mirror_divergences_total` structural, `mirror_value_divergences_total`
behavioral). The default policy (`on_divergence: continue`) records all of them;
`freeze_m`/`halt_all` turn them into stops for conformance runs.

## Measured facts that shaped the verifier

| Fact | Evidence |
|---|---|
| Type-4 txs are the only *systematic* gas delta (refund accounting ≤12,500/auth) | first replay runs, plan §1.1 |
| Self-sponsored EIP-7702 txs advance the sender nonce by 2, so blocks contain nonce gaps (`0x1e6, 0x1e8, 0x1ea`) | block 26,232,893; anvil's pool parked the gap txs as *queued* until the engine adjusted the pool's nonce view |
| Near-full blocks amplify any gas delta | block 26,233,028: mainnet 59,975,282 / 60,000,000 gas; a ~36k delta pushed the last 2 txs out of M's block |
| Seaport 1.6 `incrementCounter` reads `blockhash(number-1)` | `seaport-core/src/lib/CounterManager.sol`; a fork cannot reproduce it (A17), so the counter (and order validity) diverges without the BAL repair |
| One unhandled divergence cascades | the same block then produced status mismatches, wrong probes, and 60+ structurally short blocks |
| A17's observed blast radius | block 26,244,417 tx 124: a MEV bot's `BLOCKHASH`-gated entry (`revert("ph-")`, selector `0x4e00c7fc`) succeeds on mainnet (215,606 gas) and reverts on M (38,530); the flipped status cascades into WETH `totalSupply` and Uniswap V2/V3/V4 `Swap`/`Sync` log diffs - all repaired from the parent BAL before the next block |
| EIP-7702 nonce gaps are expected, not anomalies | every warned gap traced to a type-4 authorization bumping its authority mid-block (block 26,244,442: auth nonce 513 for sender `0xf28225…`, then that sender's tx at nonce 514; all warned senders were `0xef0100…` delegated accounts). The tracker recovers authorities (`recover_authority`) and logs these at INFO; WARN is reserved for unexplained gaps |
| Anvil `--prune-history N` keeps N full state snapshots in memory | anvil-t 9.8 GB at 1000; ~2.5 GB at 32 |
| Lighthouse needs >6 GiB during checkpoint sync | CrashLoopBackOff (OOMKilled ×5) at 6Gi |
| Anvil restarts reset automine to *on* | with automine on, every raw tx mines its own block: engine blocks were empty (`m=0`), the height ran away (~14 blocks per parent block) and nonces fell hundreds behind. Fixed with `--no-mining` + re-asserting automine=false on rebases |
| A regular beacon node cannot serve blobs after PeerDAS | bpo2 Lighthouse answers HTTP 400 `Insufficient data columns to reconstruct blobs: required 64, but only 4 were found` unless run with `--supernode`/`--semi-supernode`; the mirror runs `--semi-supernode` locally (64 of 128 columns = enough to reconstruct) |
| The beacon API's blob-level `kzg_proof` is a placeholder on Fulu | our semi-supernode served `kzg_proof = 0xc0…` (point at infinity) with a correct blob and commitment; a KZG round-trip unit test proved the validator correct, so the engine recomputes the proof locally |
| reth rejects non-spec `executionRequests` | `-32602 Invalid params: EmptyExecutionRequest` until elements were type-prefixed with empty types excluded (`engine/prague.md`) |
| jsonwebtoken panics without a crypto provider | alloy's `jwt` feature selects none; the top-up task died silently at the first JWT sign until `rust_crypto` was enabled |
| Beacon API quantities are **decimal strings**, Engine API wants hex | `head_slot()` parsed the slot as hex, silently un-bounding the top-up walk (`parse_quantity` now accepts both; unit-tested) |
| A withdrawal credit must read its pre-balance from **reth**, not from M at N-1 | when a block is retried before mining, anvil's "N-1" *is* the live state, which already contains the first attempt's credit - measured once as `withdrawals` divergence (M = reth_post + amount) |
| T's heights advance independently of mainnet's | one T block per applied parent block plus one per journal rewind; the engine bounds T by `cursor_t` (not by its height), so a rewind drill cannot stall it. A hash-validated cursor always wins (`cursor + 1`) - jumping to `chain_head + 1` skipped parent blocks and stalled T until the cursor semantics were fixed |
| Watching a proxy is not enough: delegate targets own the state | Chainlink `AggregatorProxy.latestRoundData()` delegatecalls the aggregator; the scenario's T probe caught a one-round-stale price until the aggregators were added to the watchlist |
| The follow head sets the devnet's block cadence | with `reth.follow: safe` the CL updates the safe block once per epoch: the applier burst-mines 32 blocks every ~6.4 min and a devnet tx sent between bursts waited 180 s+ (cast's own receipt timeout, then "transaction not found" after a pool reset). With `latest` the parent stream advances every ~12 s and a tx confirms in ~12 s |
| A devnet tx must not share a block with the parent's diffs | anvil estimates `eth_sendTransaction` gas against `latest`; with the diffs landing in between, an Aave `borrow` estimated at 257,560 used 271,389 (97% of its limit) and reverted, then succeeded with an explicit 1M limit. Fixed structurally by the prepend rule (devnet txs mine in their own block first) |
| A corrupted drift journal hides the history it exists to keep | two tasks appended long lines with separate handles: 10 interleaved lines in 48k. Now serialized, one write per line, divergences only, 16 MiB rotation |
| `cast calldata` rejects `--rpc-url` | a wrapper that appended it silently produced *empty* calldata: the "supply" shipped as a 21,000-gas plain transfer and reverted. Every failed market step in that run traced back to this |
| `anvil_dealERC20` cannot work on this fork, but `eth_call` overrides can | it discovers balance slots by writing and re-reading, and on a no-mining fork its probe never lands ("no slot found", measured). `/admin/deal` probes with `eth_call` + a `stateDiff` override instead: candidate slots 0..16, a sentinel value (a balance that already equals the target makes every slot match - measured), 32-byte padded override values (a quantized value answers "invalid string length") and an explicitly encoded `balanceOf` calldata (`format!("{:064x}", address)` does not pad an alloy Address). Finds WETH slot 3 and USDC's packed slot 9 alike |
| A watchlist gap can hide in the counterparties | Aave's variable debt tokens were unwatched: a devnet borrow writes the user's scaled debt there, the write is not tracked (the shadow scanner follows watched accounts), so `/admin/shadow {all:true}` cannot clear it - a phantom $315 debt against zero collateral broke every later borrow with `LtvValidationFailed`. The proof now uses a fresh position address per run and the closure includes all five aTokens and debt tokens |
| A composite price source hides a whole dependency chain | Aave's WBTC/USD source multiplies `BTC/USD x WBTC/BTC`, each leg a proxy whose aggregator holds the round. None of the four contracts was watched, so T's WBTC price sat stale ($81,811.77 vs mainnet's $81,810.37 at the same cursor) while every probe stayed green - there was no WBTC price probe. Found by the product sim's broad freshness audit (16 views per parent block); fixed by watching all four contracts, adding per-reserve price probes, and healing the read slots via reth's plain `prestateTracer` (a `diffMode` trace of a view call is empty) |
| A devnet reset can make your next tx a no-op that reports success | re-syncing an account's fields puts its nonce back to mainnet's; a byte-identical tx (same nonce, data, gas price) has the same hash, so the node answers with the *old* receipt while the reset state no longer holds its effects (measured: a "successful" approve that changed nothing, then a supply reverting with a bare `revert()`). Vary something - the gas price is enough - or never reset the sender's nonce |
| `cast send --unlocked` needs an explicit `anvil_impersonateAccount` | anvil's `--auto-impersonate` accepts `eth_sendTransaction` from any sender, but cast's own signer check refuses first ("No Signer available"). The suite sends raw `eth_sendTransaction` through the engine pod instead - no signer involved, and the receipt poll is ours |
| `cast index` is offline | it ignores/silently mishandles `--rpc-url`; a slot computed with one comes back empty and the storage read looks like "no value" instead of a wrong slot |
| The tx pool keeps a devnet tx that the engine cannot mine | when the applier bails (T's head behind the cursor after an out-of-band `anvil_reset`), pending txs starve while the probes stay green - watch `mirror_t_head` vs `cursor_t`, not just the probes |
| Handing a key back kept its recorded value, so a re-fork resurrected it | `unmark` cleared the mark only; the value stayed in the registry, and a later re-fork restored (and re-owned) it. Measured: the scenario's crashed oracle source (USDC source = the WETH/USD feed, price 240,233,000,110 vs mainnet's 99,983,478) came back this way - and because *any* devnet mark on a probe's target skips that probe, the five oracle probes stayed green while the price was poisoned. Fixed three ways: `unmark` now drops the recorded value, orphan values are pruned (never re-owned), and `mirror_t_probe_skipped_total` counts skipped probes so the blind spot is visible. One unshadow call healed the price (`resynced_keys: 4`). The same audit fixed the epoch semantics (a fresh pin clears marks *and* values) - restoring across a moved base is what resurrected the value in the first place |
| Restoring devnet writes onto a *moved* base mixes eras | a +30d time-travel run recorded Aave's `lastUpdateTimestamp` ~13 days in the future; a later re-fork re-applied it and every `supply` panicked with `arithmetic underflow` (product sim 2/13, scenario 31/33). The plan's rule decides it - derived values go stale when the base advances (2.2/6.3): a pin change is a new epoch (clear marks and values), the product re-creates positions from its own journal (3.6). After the epoch reset the suites are green again |

## Capability proof: `scripts/capability-proof.sh`

`scripts/scenario.sh` proves the *substrate* (replay fidelity, COW, drills).
`scripts/capability-proof.sh` proves the *product*: the three things a fork must
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

## Product simulation: `scripts/product-sim.sh`

Where the capability proof checks *capabilities*, the product sim runs the
workflow a margin product actually executes, in one pass and on live state
(`make product-sim`, ~8 min, 9 checks):

| Act | What runs | Asserted |
|---|---|---|
| 1. the trader | faucet funds → `approve` → `supply` 1 WETH → `borrow` 100 USDC → swap 50 USDC→WETH on the real Uniswap pool → `supply` the swap back | the aToken delta, debt > 0 with the HF read |
| 2. live data | the market-data closure the product reads - five Chainlink rounds, the Aave oracle for five assets, the UniV3 pool, three token supplies, two Aave income indexes - sampled once per *parent block* | every view-comparison matches reth at the applied cursor: freshness is continuous and broad, not a one-shot spot check. Devnet-owned views are skipped by design (copy-on-write) and counted |
| 3. the bot | a WS `eth_subscribe` on the pool sees the trader's `Borrow`; the oracle is crashed through its admin path; the bot polls health, calls `liquidationCall`, takes the bonus | the event arrived live, HF crossed below 1e18, the bot's collateral grew with the bonus, the oracle was handed back (unshadow) - and, because the crash leaves the position *underwater*, the liquidation takes Aave's **bad-debt path**: the collateral is consumed entirely and the whole remaining debt token balance is burned while the liquidator repays only what the collateral covered (measured on this deployment: `getReserveData` returns 15 words, the pre-`deficit` struct) |

All three scripts share `scripts/lib.sh` - the measured conventions live there
(explicit gas on devnet sends, offline `cast` without `--rpc-url`, polled
receipts) instead of being copied three times.

## The daloc use-case simulation (`scripts/daloc-sim.py`)

The plan's §17 harness flows, driven end-to-end against a mirror stack: T (a
devnet Ethereum fork), V (a devnet HyperEVM fork), the venue engine, and the
venue simulator. Each **role** is what one daloc-side actor does; each assertion
is a gate the plan names (the client gates of §2.1/§18.3, the time policy of
§17.2, the epoch model of §6.5, the venue mechanics of §5). The roles run in
order, a failure prints the evidence, and the run exits non-zero. **42 checks**
in a green run.

| Role | Drives | Gates |
|---|---|---|
| `boot` | pins both chains' clocks and mines the carrying block; waits for the sim's readiness | T and V clocks start at wall time (±300 s, their gate); `venue-sim /readyz` 200 |
| `provision` | `ensure_gas` on T; sim `reset`; `deal` perp + spot; a seeded position | T gas funded; scenario reset; perp deal returns `balance_after == 1000`; spot deal `== 250`; the hl_mock-shaped seed accepted |
| `operator` | the gated venue reads (`hl_client.rs`) and the info shapes | identity `accountValue == totalRawUsd + Σ signed(positionValue)` at 1e-6; `positionValue` unsigned; `szi` signed; `cumFunding.sinceOpen`; `leverage.type`; flat rows dropped; `liquidationPx` isolated-only; `time` ≤6 s; `totalRawUsd` > 0; `meta.universe` carries `szDecimals`; `openOrders` = `[]`; a non-default dex → empty shape 200; `perpDexs` = `[null]`; `l2Book` exactly 2 sides, ≤20 levels; `userAbstraction` in the enum; an unconfigured account → empty shape 200 (passthrough) |
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

`CAST` points at `/tmp/opencode/foundry-bin/cast` (a constant in the script -
edit it for your environment). Run:

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
cargo run -p venue-mirror -- config/instances/venue-mirror.toml
cargo run -p venue-sim    -- config/instances/venue-mirror.toml
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
- The epoch drill: `curl -s -X POST localhost:8091/control/epoch_new`, then
  watch `fork_pin` advance and `venue_epochs_total` increment.
- In-cluster: `kubectl apply -k k8s/overlays/venue-mirror` (namespace
  `venue-mirror`; NodePorts 31545/31880/31090/31091), then `make smoke-venue`
  (`scripts/smoke-venue.sh`): V's chain 999 / head ≥ pin / automine off, the
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

## The CCTP bridge — measured facts and acceptance

The real lane (`cctp-bridge`, Iris 8095 / control 8096) moves native USDC
between T and V over the real V2 contracts; the only substitutions are the
devnet attester (installed through the contracts' own setters) and the local
Iris. Proven live on scratch forks:

- bidirectional burns with exact mints; replay → `Nonce already used`;
  tampered signatures and empty attestations rejected by the real verification;
- PENDING emulation (`finality_delay_secs`): the Iris serves
  `{messages:[{message, attestation:null}]}` until the window elapses;
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
  the mint lands again;
- the hook lane: `depositForBurnWithHook` + `destinationCaller = wrapper` →
  `wrapper.relay` mints **and** runs the encoded call in one tx (a USDC
  transfer hook moved the mint through the wrapper to the recipient); a
  deliberately failing hook still mints (tx status 1, the wrapper kept the
  funds, the hook failure is isolated) — the wrapper's documented
  non-atomicity;
- the pin clamp: a start block below the fork pin is clamped to the pin with
  one warning (anvil forwards pre-pin `eth_getLogs` ranges upstream, which
  rejects them — measured);
- the real reverts, string for string: source-side `Max fee must be less than
  amount`, `Mint recipient must be nonzero`, `Hook data is empty`; a message
  pinned to another `destinationCaller` → `Invalid caller for message` (the
  relayer pre-parks such records instead of paying for the revert).

Operator surface: `GET /health` (parked counts), `GET /control/state`,
`POST /control/transfer` (`hook: {target, calldata}` for the credit lane).

## Plan axioms → how they are covered

| # | Fact | Coverage |
|---|---|---|
| A1 | fork blocks have no trie | scenario: M head `stateRoot = 0x0`; proofs impossible by design (§6) |
| A2 | pin byte-identical to mainnet | scenario: pin block hash M == reth; `make pin-show` |
| A3 | block env set exactly | replay sets all five context fields; an env error shows up as receipt/log-set divergence in the same block |
| A4 | withdrawals are not txs | idempotent `setBalance` credit + withdrawal balance check |
| A5 | in-block-funded txs need the flag | `--disable-pool-balance-checks` in both anvil deployments |
| A6 | blob txs need the network wrapper | `blob.rs` (legacy / eip7594 / bare forms, learned at runtime) |
| A7 | blob data for any age | narrowed by design: the mirror serves blobs only from the local CL's own window (self-containment); older blobs would need an external source, which the runtime never uses |
| A8 | mining is state-fetch-bound | `mirror_last_replay_seconds`; the local reth keeps it ~2 s/block even cold (remote measured 55-256 s) |
| A9 | type-4 gas deviation bound | `verify.rs` receipts check |
| A10 | replayed blocks expose real logs | log-set + logsBloom checks |
| A11 | `deal` writes storage silently | scenario: a WETH deal sets the balance with **no** Transfer log; the same probe finds USDC's packed `balanceAndBlacklistStates` slot (the sentinel stays below 2^255 so packed reads return it unmasked) |
| A12 | exactly one M block per parent block | scenario head parity (`m_head == follow_head`) + `anvil_mine(1)` |
| A13 | local writes never touch M | the COW/shadow section runs on T; M's probes and the drift gate are asserted after it |
| A14 | warm restart preserves state | anvil `--state` removed (it dumped per-block history: 13 GB/dump, leaked tmp dirs, filled T's PVC); the engine rebases instead - M re-replays FORK_OFFSET blocks (~1-2 min, measured). T re-forks at a fresh pin = a **new epoch**: the devnet's writes are *not* replayed onto the moved base (values are derivations of the old base - plan 2.2/6.3; measured: a +30d time-travel write came back as a future `lastUpdateTimestamp` and every Aave supply panicked with an underflow). The registry is cleared (marks *and* values) and the product re-creates positions from its own journal (plan 3.6); a reset at the *same* pin restores the recorded writes |
| A15 | memory bounded by restart | measured ~20 MB per replayed block steady state; M recycles at its 24Gi limit (~2.5-4 h) and recovers in ~1-2 min |
| A16 | upstream reorgs detected | parent-hash continuity check (M loop only); the engine records it and re-bases M behind the follow head - self-healing in ~1-2 min. With `on_divergence: freeze_m` it freezes for inspection instead |
| A17 | mirror block hashes differ | inherent (verified: anvil exposes no blockhash/beacon-root setter); recorded as behavioral, state re-converged from the parent BAL. Measured rate: ~45% of blocks carry a value-level divergence (the blockhash-gated MEV/Seaport class), 0 structural in steady state, and `mirror_m_convergence_failures_total` stays 0 - the state is exact even when the execution diverges |
| A18 | delivery is event-driven | consumers subscribe to anvil M/T WS (`eth_subscribe`) |
| A19 | scoped manipulation leaves replay intact | scenario: a shadowed slot survives further parent diffs; unshadow re-syncs from reth; M's probes stay green |
| A20 | journal round-trip is faithful | scenario: snapshot/revert + `/admin/rewind` restore T's cursor and re-apply cleanly |

### The same axioms on the Venue Mirror (plan §12.6)

The venue mirror shares the substrate axioms by **construction** (`crates/anvil` +
`crates/devnet` are literally the same modules) and drops the replay axioms by **chain nature**
(HyperEVM has no BALs, no beacon, no node replay, and the public RPC exposes no traces/proofs to
verify against). Verdicts:

| Verdict | Axioms | Where |
|---|---|---|
| **SAME** (same modules) | A1, A2, A5, A8, A11, A13, A14, A15, A18, A19, A20 + the pins/epochs/journal/COW/automine decisions | `crates/anvil` + `crates/devnet`, exercised by `make smoke-venue` and the epoch drill (same-pin restore / new-pin epoch) |
| **IMPOSSIBLE (permanent)** | A3, A4, A6, A7, A9, A12, A16, A17 + the M guarantee (per-block verification, probe parity, convergence repair) | no replay exists to verify; the fork keeps the source's exact state and hashes at the pin |
| **REFRAMED** | L9 freshness → the pin (chain) + the live public API (venue data); the T guarantee → at-pin parity | `venue_fork_pin`, probes-at-pin (skip+count when the pin ≠ source head — measured: the source ignores the block parameter) |
| **NEW (venue plane)** | sim invariants: the served identity (`venue_sim_identity_violations_total` must stay 0), funding cadence, liquidation determinism, book/mark ownership (Phase 2), bridge round-trip exactness | `crates/hyperliquid`; the identity is re-checked at the serve boundary |

Measured on the scratch stacks (2026-10-22) — facts an integration run can rely on:

- **`anvil_dealERC20` fails on forks** (foundry 1.8.6; measured on a *fresh* HyperEVM fork for the
  real USDC proxy **and** a plain slot-0 ERC20 — not V's state). The mirror's `POST /admin/deal`
  works on V (measured: slot 9 discovered, 200 USDC landed). daloc's external mode should route
  **both** its funding (`fund_token_on`) and drain paths through `/admin/deal`, not only the drain.
- **The real HyperEVM USDC blacklists its own system address** (`isBlacklisted(0x2000…0) == true`,
  measured on the live chain), so EVM→Core USDC is impossible with the real token; the bridge's
  credit path runs over the harness's fixture mocks (which replace the canonical ERC20s), and the
  bridge only credits a `Transfer` log emitted by the token's *linked* contract.
- **WS delivery works on both mirror chains**: subscribing `newHeads` + `logs`, two empty mines and
  one log-emitting tx (a WETH/WHYPE `deposit()`) delivered **3 newHeads + 1 log on T and on V**
  (the live-T silence recorded earlier did not reproduce on the copies; their scanner's
  logs-filter + HTTP backup is unaffected either way).
- **Negative equity flows end-to-end** (the master plan's venue-term requirement): seeding
  `totalRawUsd = -80000` with a long BTC position and crashing the mark served signed
  `accountValue = -2771.65…`; after the 1 Hz tick liquidated it, the bare negative balance
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
  parameter is ignored), so the re-sync is refused while the pin lags the source head ("re-pin
  before handing keys back"). The mainnet's reth can serve the cursor's state, so its hand-back
  works at any time — the one deliberate asymmetry, and it is fail-closed: the refusal names the
  remedy (an epoch re-pin), it never writes a wrong value.

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
   block is mined without its blob txs and recorded; **M's state still
   converges** through the parent-BAL repair, and T is unaffected (the BAL
   carries every state change).
6. **M is memory-hungry**: every replayed tx is cached by anvil
   (~15-25 MB/block). Long replays end in a k8s restart; the engine rebases and
   re-converges. T (the devnet) is ~10× leaner because it never replays txs.
7. **Only the last `--prune-history` blocks of M are traceable**
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
