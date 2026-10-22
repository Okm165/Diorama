# Testing

Three layers, each runnable on demand:

| Layer | Command | Needs | Covers |
|---|---|---|---|
| Unit | `make test-engine` (cargo test) | nothing | pure logic: BAL flattening, padding, COW granularity, journal, cursor/rebase rules, KZG proof repair, Engine API request encoding |
| Self test | `make selftest` | a public mainnet RPC + beacon API (**test-only** external deps; the runtime stack has none) | the whole pipeline end-to-end on **real mainnet blocks** (replay + verify + diff apply + sampled parity) |
| Smoke | `make smoke` | the deployed stack | service health + parity spot check on M's head vs reth |
| Scenario | `make scenario` | the deployed stack, no external network | self-containment, live parity on **M and T** (the 6 state probes vs reth at the same parent block), and the devnet workflow: faucet, copy-on-write shadowing, snapshot/revert, journal rewind |

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

Divergence classes are recorded per block in `drift.jsonl`
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

## Plan axioms → how they are covered

| # | Fact | Coverage |
|---|---|---|
| A2 | pin byte-identical to mainnet | `anvil` fork semantics; `make pin-show` + smoke |
| A3 | block env set exactly | replay sets all five context fields; an env error shows up as receipt/log-set divergence in the same block |
| A4 | withdrawals are not txs | idempotent `setBalance` credit + withdrawal balance check |
| A5 | in-block-funded txs need the flag | `--disable-pool-balance-checks` in both anvil deployments |
| A6 | blob txs need the network wrapper | `blob.rs` (legacy / eip7594 / bare forms, learned at runtime) |
| A9 | type-4 gas deviation bound | `verify.rs` receipts check |
| A10 | replayed blocks expose real logs | log-set + logsBloom checks |
| A14 | warm restart preserves state | anvil `--state` dump (T 60 s, M 300 s) + on SIGTERM; the engine rebases onto whatever head the dump restores |
| A16 | upstream reorgs detected | parent-hash continuity check → freeze + drill |
| A17 | mirror block hashes differ from mainnet | inherent; divergence recorded as behavioral, state re-converged from the parent BAL |
| A18 | delivery is event-driven | consumers subscribe to anvil M/T WS (`eth_subscribe`) |

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
