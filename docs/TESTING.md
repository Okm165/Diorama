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
kubectl -n mainnet-mirror get cm engine-config -o jsonpath='{.data.engine\.yaml}' \
  | grep -A3 "blobs:"   # beacon_urls: [http://lighthouse:5052]
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
| A14 | warm restart preserves state | anvil `--state` removed (it dumped per-block history: 13 GB/dump, leaked tmp dirs, filled T's PVC); the engine rebases instead - M re-replays FORK_OFFSET blocks (~1-2 min, measured). T's chain is re-forked too, but the devnet's writes are *restored* from the shadow registry (marks + latest values); only transaction history is lost |
| A15 | memory bounded by restart | measured ~20 MB per replayed block steady state; M recycles at its 24Gi limit (~2.5-4 h) and recovers in ~1-2 min |
| A16 | upstream reorgs detected | parent-hash continuity check (M loop only); the engine records it and re-bases M behind the follow head - self-healing in ~1-2 min. With `on_divergence: freeze_m` it freezes for inspection instead |
| A17 | mirror block hashes differ | inherent (verified: anvil exposes no blockhash/beacon-root setter); recorded as behavioral, state re-converged from the parent BAL. Measured rate: ~45% of blocks carry a value-level divergence (the blockhash-gated MEV/Seaport class), 0 structural in steady state, and `mirror_m_convergence_failures_total` stays 0 - the state is exact even when the execution diverges |
| A18 | delivery is event-driven | consumers subscribe to anvil M/T WS (`eth_subscribe`) |
| A19 | scoped manipulation leaves replay intact | scenario: a shadowed slot survives further parent diffs; unshadow re-syncs from reth; M's probes stay green |
| A20 | journal round-trip is faithful | scenario: snapshot/revert + `/admin/rewind` restore T's cursor and re-apply cleanly |

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
