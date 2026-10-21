# Operations

## Signals and thresholds

| Metric (`/metrics`) | Threshold | Action |
|---|---|---|
| `mirror_reth_head` | not moving | top-up stalled: check `logs-engine` for "top-up failed" (reth unreachable, beacon API down, or a gap the CL cannot serve) |
| `mirror_source_sync_gap_blocks` | > 0 for long | reth is catching up to the CL head via the Engine API (~10-11 blocks/s measured); a stuck gap means the CL's backfill has not reached the gap window yet |
| `mirror_source_sync_delivered_total` | rate 0 while `gap_blocks` > 0 | the top-up loop is dead or erroring - it is supervised, so check `logs-engine` |
| `mirror_lag_blocks` | > 60 | investigate: reth catching up, engine restart, or slow replay |
| `mirror_lag_blocks` | > `max_gap_blocks` (5000) | engine pauses itself; re-pin drill or restore reth |
| `mirror_last_replay_seconds` | > 10 s warm | investigate; check reth health first (state-fetch bound) |
| `mirror_divergences_total` | increase | structural divergence (counts/order/gas) – inspect `drift.jsonl`; the repair re-converges state on the next block |
| `mirror_value_divergences_total` | increase | expected for blocks with blockhash-dependent txs (A17) – no action |
| `mirror_m_convergence_failures_total` | increase | M's post-repair state ≠ mainnet: stale dump or a missing BAL. Re-pin if it keeps climbing |
| `mirror_m_repair_writes_total` | flat while replaying | the repair is not running: check `loops.repair_m` and reth's BAL endpoint |
| `mirror_reorgs_detected_total` | any increase | upstream reorg; drill: re-pin (engine froze itself) |
| `mirror_pin_age_blocks` | > ~8000 | schedule a re-pin (reth's state window is 10,064 blocks) |
| `mirror_blob_failures_total` | increase | a blob block could not be built (source gap). Placeholder proofs are repaired automatically; a rising counter means the CL cannot serve the sidecars |
| `mirror_receipts_unverified_total` | increase | engine lagged past the 20k receipts window; blocks replayed unverified |
| Anvil RSS (kubectl top) | see table below | restart the pod; the engine rebases and re-converges |

Anvil memory is a *state* cost, not a leak:

| Chain | Growth | Steady state | Notes |
|---|---|---|---|
| `anvil-t` | ~1 MB per applied block | ~2.5 GB | watch-list writes only (~90-500 per block) |
| `anvil-m` | ~15-25 MB per replayed block | grows until the limit | full tx replay: fork cache + 16 state snapshots + 350 MB dump |

`anvil-m` hitting its 24Gi limit is normal over long replays: k8s restarts it,
it reloads its dump, the engine sees the backward head jump and rebases
("cursor not found on chain (reset or re-pin) - rebasing"), then the per-block
repair re-converges state. `mirror_m_convergence_failures_total` may tick up
right after such a restart until stale slots are touched again.

## Day-to-day

```bash
make status            # pods, PVCs, engine status JSON
make images            # re-import the pinned images into k3s (after a bump)
make logs              # tail everything
make logs-engine       # one component (also: logs-reth, logs-lighthouse, logs-anvil-t)
make smoke             # endpoint checks + parity spot check on M's head
make scenario          # self-containment + M/T parity + devnet workflow
make pin-show          # current fork pins
```

Read the flight recorder:

```bash
kubectl -n mainnet-mirror exec deploy/engine -- tail -5 /state/drift.jsonl | jq .
kubectl -n mainnet-mirror exec deploy/engine -- ls -la /state/journal | tail
kubectl -n mainnet-mirror exec deploy/engine -- cat /state/cursor.json
```

Every divergence line carries `divergence_class` (`none` | `value_level` |
`fatal`) and the full `checks` array, so a single drift line explains itself.

## Source liveness (self-contained)

The mirror needs no external nodes at runtime. reth's head (`mirror_reth_head`)
advances when the local Lighthouse feeds it payloads, and for any gap between
the snapshot's head and the CL's checkpoint the engine's top-up loop supplies
the missing ancestors over the Engine API (EIP-8379, see ARCHITECTURE →
"Source liveness"). reth has no P2P peers by design (`--disable-discovery
--trusted-only`), so nothing else can move it.

Check:

```bash
# catch-up progress (should fall to 0 and stay there)
kubectl -n mainnet-mirror exec deploy/engine -- curl -s localhost:8080/metrics | grep -E "source_sync_gap_blocks|source_sync_delivered_total"
# the top-up is delivering (one line per ~5 s while catching up)
kubectl -n mainnet-mirror logs deploy/engine --tail=20 | grep "top-up"
# reth really has no peers
kubectl -n mainnet-mirror exec deploy/engine -- curl -s http://reth:8545 -X POST -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"net_peerCount"}' | jq -r .result
```

- `top-up failed: top-up gap: reth head X, next CL payload Y (missing ancestors
  are outside the CL's retained history)` ⇒ the CL's block backfill has not
  reached the gap window yet (it walks backwards from the checkpoint, ~10-15
  slots/s). It clears by itself; on a fresh bootstrap expect ~10 minutes.
- `top-up failed: ... engine_newPayloadV4: -32602 Invalid params` ⇒ the request
  encoding regressed (see ARCHITECTURE's spec notes).
- `top-up failed: eth_blockNumber -> http://reth:8545` ⇒ reth is restarting;
  the loop retries every 5 s.
- Once `source_sync_gap_blocks` reaches 0, reth tracks the CL head live, and
  Lighthouse's own `forkchoiceUpdated` calls (which fail with "Received
  forkchoice updated message when syncing" during a catch-up) start updating
  reth's `safe`/`finalized` pointers again - the mirror's follow head then jumps
  from the snapshot's value to live.

## Drills

### Restart a chain (state dump restores it)

Anvil dumps to `/state/state.json` (T every 60 s, M every 300 s) and on
SIGTERM. A restarted pod loads the dump and re-attaches to the same fork pin.

```bash
kubectl -n mainnet-mirror rollout restart deploy/anvil-t
kubectl -n mainnet-mirror rollout status  deploy/anvil-t
```

A dump is up to one interval *stale*: the engine notices the backward head jump
and rebases; M re-converges from the parent BALs as slots are touched. For a
guaranteed-clean M, re-pin instead.

### Re-pin (new epoch) — `make repin`

Three occasions:

1. **After a fresh bootstrap**, once the top-up catch-up has finished
   (`mirror_source_sync_gap_blocks` = 0). The first pin is resolved while reth
   is still behind, so the mirror starts near the snapshot's head; re-pinning
   moves M/T to a live, exact base instead of replaying the catch-up window
   (whose blob sidecars may not all be reconstructable).
2. When the pin ages out of reth's state window (~10k blocks ≈ 1.4 days).
3. After an upstream reorg, or when M's convergence failures keep climbing.

The drill:

1. scales `anvil-m`/`anvil-t` down,
2. wipes `/state/pin` and `/state/state.json` (fork cache is keyed by block and
   is kept),
3. scales back up: init containers resolve a fresh pin (`reth head - FORK_OFFSET`,
   default 64),
4. the engine detects the chain reset on its next poll and rebases its cursor
   automatically (start = chain head + 1).

### Rewind applied parent blocks

```bash
kubectl -n mainnet-mirror exec deploy/engine -- \
  curl -s -X POST localhost:8080/admin/rewind -H 'Content-Type: application/json' -d '{"blocks":5}'
```

Restores the before-values recorded in the journal (newest block first) and
moves the T cursor back, so the applier re-applies from there.

### Register devnet writes made outside the block pipeline

Cheatcodes applied directly to T (`anvil_setBalance`, `anvil_setStorageAt`, …)
do not create blocks, so the shadow scanner cannot see them:

```bash
curl -X POST .../admin/shadow -H 'Content-Type: application/json' \
  -d '{"address":"0xYourContract","slots":["0x1","0x2"],"account_fields":true}'
```

### Resume after a freeze

Only relevant with `on_divergence: freeze_m|halt_all` (conformance runs):

```bash
kubectl -n mainnet-mirror exec deploy/engine -- curl -s -X POST localhost:8080/admin/resume
```

With the default `continue` policy nothing freezes; divergences are recorded.

## Storage

| PVC | Size | Contents |
|---|---|---|
| `reth-data` | 400 Gi | reth DB + static files (snapshot + growth, ~300 GB expected) |
| `lighthouse-data` | 500 Gi | beacon DB + 18-day column custody. Measured during the one-time backfill: ~377 KB/slot inside the DA window, blocks only beyond it; ~200 GB expected when it completes |
| `anvil-m-state` | 20 Gi | pin, state dump (~350 MB), fork cache |
| `anvil-t-state` | 40 Gi | same for the devnet chain (dump ~27 MB) |
| `engine-state` | 10 Gi | cursor.json, shadow.json, drift.jsonl, journal/ |

All PVCs use the `mirror-retain` StorageClass (`reclaimPolicy: Retain`):
**deleting a PVC never deletes the data**. The PV stays; re-create a PVC with
the same name to re-attach it, or delete the PV explicitly to free the space.

- Inspect on the host: `/var/lib/rancher/k3s/storage/pvc-*`.
- Grow a PVC: edit `k8s/base/pvcs.yaml` (allowVolumeExpansion is enabled) and
  `make apply`; local-path expands by recreating the directory mount.
- Full reset of a component: `kubectl delete pvc <name>` (data survives),
  then delete the PV if you really want the space back, then `make apply`.
- **Orphans**: deleting a PV leaves its directory in
  `/var/lib/rancher/k3s/storage/pvc-*_<ns>_<pvc>/`. Check with
  `kubectl get pv | grep -v Bound` and remove the directory once nothing
  references it (a Released PV keeps the data until you delete both).

## Failure playbook

| Symptom | Likely cause | Action |
|---|---|---|
| `mirror_reth_head` static for hours | top-up stalled (CL backfill not yet at the gap, reth restarting, beacon API down) | check `logs-engine` for "top-up failed"; see "Source liveness" |
| Lighthouse in CrashLoopBackOff | OOM during checkpoint sync (needs >6 GiB) | keep the 16Gi limit; `logs-lighthouse` shows `State cache missed` before OOM |
| Anvil OOMKilled | state snapshots + fork cache (`--prune-history`) | restart; lower `--prune-history`; see the memory table above |
| reth `eth_syncing` true for a long time | unwind/replay after a restart, or a stale engine buffer | wait; check `logs-reth` for stage progress |
| Lighthouse "no checkpoint" / sync stuck | checkpoint provider down | switch `CHECKPOINT_SYNC_URL` in `mirror-config` (fallback is beaconcha.in) |
| Engine: "cursor not found on chain" | anvil restarted with a stale dump | expected; engine rebases and re-converges |
| Engine: "gap too large" | engine was down > `max_gap_blocks` | re-pin (fast) or raise `max_gap_blocks` and restart the engine |
| M mines empty blocks (`tx_count: reth=N m=0`) | automine is on (anvil restart reset it) | check `anvil_getAutomine` on both anvils; both deployments must run `--no-mining` (the engine also re-asserts it on rebases) |
| `recomputed blob KZG proof` in engine logs | normal on Fulu: the beacon API's blob-level proof is a placeholder, the engine recomputes it | none (one line per blob) |
| `no valid blob sidecar from any source` | the CL cannot reconstruct the blobs (slot outside its column coverage) | block is mined without the blob txs and recorded; state still converges via the BAL repair |
| Senders' nonces hundreds behind | automine incident (chain height ran away) | re-pin for a clean chain |
| `partial: true` in drift lines | blob sidecars unavailable | check the CL's column coverage / `blobs.beacon_urls` |
| `mirror_m_convergence_failures_total` climbing | M's state stale (dump restore, no BAL) | re-pin for a clean M; verify `eth_getBlockAccessList` on reth |
| T state looks stale | diff applier behind, or the account is not watched | check `mirror_cursor_block_t` vs `mirror_cursor_block`; extend `watchlist` |
