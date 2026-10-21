# Testing

Three layers, each runnable on demand:

| Layer | Command | Needs | Covers |
|---|---|---|---|
| Unit | `make test-engine` (cargo test) | nothing | pure logic: BAL flattening, padding, COW granularity, journal, cursor/rebase rules |
| Self test | `make selftest` | a public mainnet RPC + beacon API | the whole pipeline end-to-end on **real mainnet blocks** (replay + verify + diff apply + sampled parity) |
| Smoke | `make smoke` | the deployed stack | service health + parity spot check on M's head vs reth |

## Unit tests (`engine/src/**`)

9 tests, all offline:

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
- `blob::versioned_hash_mapping` — commitment → versioned hash shape.

## Self test (`k8s/bootstrap/selftest-job.yaml`)

One Job, three containers in one pod:

1. `resolve-pin` (alpine) resolves `pin = rpc head - 16`,
2. `anvil-m` / `anvil-t` fork a public RPC at that pin,
3. `engine selftest` replays and applies blocks `pin+1 .. pin+3`, printing a
   check-by-check report (tx-count, blobGasUsed, logsBloom, log set, receipts
   with the type-4 gas rule, withdrawals, probes; T: writes/filtered/shadowed,
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
   M ✔ probe:usdc_total_supply
   ...
   M replay: txs=251 replayed=251 skipped_blobs=0 ms=...
   T apply:  source=trace writes=... filtered=... skipped_shadowed=... sampled=8/8 ms=...
```

## Smoke (`scripts/smoke.sh`)

Checks pods, reth head + `eth_syncing` + `eth_getBlockAccessList` availability,
Lighthouse sync status, both anvil pins, the engine `/status`, and a parity
spot check: `logsBloom` and tx count of M's head block vs reth at the same
height.

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
| probe calls (`eth_call` parity, pre-repair) | behavioral | `verify.rs` |
| post-repair convergence (probes M vs reth at N-1) | convergence | `verify.rs` |
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
| Anvil `--prune-history N` keeps N full state snapshots in memory | anvil-t 9.8 GB at 1000; ~2.5 GB at 32 |
| Lighthouse needs >6 GiB during checkpoint sync | CrashLoopBackOff (OOMKilled ×5) at 6Gi |
| Anvil restarts reset automine to *on* | with automine on, every raw tx mines its own block: engine blocks were empty (`m=0`), the height ran away (~14 blocks per parent block) and nonces fell hundreds behind. Fixed with `--no-mining` + re-asserting automine=false on rebases |
| A regular beacon node cannot serve blobs after PeerDAS | bpo2 Lighthouse answers HTTP 400 `Insufficient data columns to reconstruct blobs: required 64, but only 4 were found` unless run with `--supernode`/`--semi-supernode`; the engine uses a public beacon API for sidecars |

## Plan axioms → how they are covered

| # | Fact | Coverage |
|---|---|---|
| A2 | pin byte-identical to mainnet | `anvil` fork semantics; `make pin-show` + smoke |
| A3 | block env set exactly | replay sets all five context fields; probes (Aave income) fail otherwise |
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
5. **Blob-tx replay needs a sidecar source that survives PeerDAS.** After
   EIP-7594 (live on bpo2) a regular beacon node cannot reconstruct blobs: a
   non-supernode Lighthouse answers HTTP 400 `Insufficient data columns to
   reconstruct blobs: required 64, but only 4 were found`. The engine's
   configured sources (publicnode first) serve the legacy 4844 sidecars, and
   blocks whose blob txs are still rejected (`-32003 invalid KZG proof`) are
   skipped after three attempts and recorded; **M's state still converges**
   through the parent-BAL repair, and T is unaffected (the BAL carries every
   state change). To close the gap: run Lighthouse with
   `--semi-supernode`/`--supernode`, or add a source that serves EIP-7594 cell
   proofs and pick the wrapper form from the proof type
   (`kzg_proof` → legacy, `proofs` → eip7594).
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
9. **Freshness = reth's head**: if the CL cannot drive reth forward (snapshot
   older than the checkpoint and P2P unavailable), the mirror mirrors the
   snapshot's state and window. See OPERATIONS → "Source liveness".
