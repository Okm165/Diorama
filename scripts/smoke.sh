#!/usr/bin/env bash
# End-to-end smoke test for the mirror stack. Run from the repo root:
#   make mainnet-smoke        (or ./scripts/smoke.sh)
#
# Executes curl+jq from inside the engine pod so every check talks to the
# cluster-internal service names (the same way the engine does).
set -euo pipefail

NS="${NS:-mainnet-mirror}"
ENGINE_POD_SELECTOR="app.kubernetes.io/name=engine"

pass=0; fail=0
ok()   { echo "  ✔ $*"; pass=$((pass+1)); }
bad()  { echo "  ✘ $*"; fail=$((fail+1)); }

echo "== mirror smoke test (namespace $NS) =="

echo "-- pods"
kubectl -n "$NS" get pods --no-headers | awk '{printf "  %-42s %-10s %s\n", $1, $3, $5}'

run() { kubectl -n "$NS" exec -i deploy/engine -- "$@"; }

rpc() { # rpc <url> <method> [params-json]
  local url="$1" method="$2" params="${3:-[]}"
  run curl -sf -m 20 -X POST "$url" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}"
}

echo "-- reth"
if head_hex=$(rpc http://reth:8545 eth_blockNumber | jq -r .result) && [ "$head_hex" != "null" ]; then
  head=$((head_hex)); ok "reth head = $head"
  syncing=$(rpc http://reth:8545 eth_syncing | jq -r '.result')
  if [ "$syncing" = "false" ]; then
    ok "reth synced (eth_syncing=false)"
  else
    # reth reports staged-sync progress even when it has caught up with its own
    # known head: currentBlock == highestBlock means "no known target ahead".
    cur=$(echo "$syncing" | jq -r '.currentBlock // empty'); high=$(echo "$syncing" | jq -r '.highestBlock // empty')
    if [ -n "$cur" ] && [ "$cur" = "$high" ]; then
      ok "reth at its known head (stages: $cur == $high)"
    else
      bad "reth still syncing: current=$cur highest=$high"
    fi
  fi
  # Source liveness, checked locally (no external calls): reth's head vs the
  # local CL's execution head, plus the engine's top-up gap metric.
  cl_slot=$(run curl -sf -m 10 http://lighthouse:5052/eth/v1/beacon/headers/head 2>/dev/null | jq -r '.data.header.message.slot' 2>/dev/null || true)
  if [ -n "$cl_slot" ] && [ "$cl_slot" != "null" ]; then
    cl_head=$(run curl -sf -m 20 "http://lighthouse:5052/eth/v2/beacon/blocks/$cl_slot" 2>/dev/null | jq -r '.data.message.body.execution_payload.block_number // empty' 2>/dev/null || true)
    gap=$(run curl -sf -m 10 localhost:8080/metrics 2>/dev/null | awk '/^mirror_source_sync_gap_blocks /{print $2}' || true)
    if [ -n "$cl_head" ] && [ "$cl_head" != "null" ]; then
      lag=$((cl_head - head))
      if [ "$lag" -le 100 ]; then
        ok "source liveness: reth is $lag blocks behind the local CL's head (top-up gap ${gap:-?})"
      else
        echo "  ! reth is $lag blocks behind the local CL's head (top-up gap ${gap:-?}) - the engine is catching up; see OPERATIONS -> Source liveness"
      fi
    fi
  fi
  bal=$(rpc http://reth:8545 eth_getBlockAccessList "[\"latest\"]" | jq -r '.result | length')
  if [ "${bal:-0}" -gt 0 ] 2>/dev/null; then ok "eth_getBlockAccessList works ($bal accounts)"; else bad "eth_getBlockAccessList empty/failed"; fi
else
  bad "reth unreachable"
fi

echo "-- lighthouse"
syncing=$(run curl -sf -m 20 http://lighthouse:5052/eth/v1/node/syncing | jq -c '.data | {head_slot, sync_distance, is_syncing}' || echo FAIL)
if [ "$syncing" != "FAIL" ]; then ok "beacon node: $syncing"; else bad "lighthouse unreachable"; fi

echo "-- anvil M / T"
for chain in anvil-m anvil-t; do
  h=$(rpc "http://$chain:8545" eth_blockNumber | jq -r .result)
  info=$(rpc "http://$chain:8545" anvil_nodeInfo | jq -c '{head: .result.currentBlockNumber, pin: .result.forkConfig.forkBlockNumber}')
  if [ "$h" != "null" ]; then ok "$chain: $info"; else bad "$chain unreachable"; fi
done

echo "-- engine"
st=$(run curl -sf -m 10 http://engine:8080/status || echo FAIL)
if [ "$st" != "FAIL" ]; then
  ok "engine status: $(echo "$st" | jq -c '{reth_head, m_head, t_head, cursor, frozen, lag}')"
else
  bad "engine /status unreachable"
fi

echo "-- parity sample (last 10 replayed blocks, M vs reth)"
last=$(run curl -sf -m 10 http://engine:8080/status 2>/dev/null | jq -r '.m_head // empty' || true)
if [ -n "$last" ] && [ "$last" != "null" ] && [ "$last" != "0" ]; then
  # Sample a window: blocks whose blob sidecars could not be reconstructed are
  # deliberately skipped by the engine (recorded in drift.jsonl), so the health
  # signal is the parity *rate*, not a single block.
  total=0; match=0; blob_blocks=0
  for i in $(seq 0 9); do
    n=$((last - i))
    [ "$n" -le 0 ] && continue
    q="[\"$(printf '0x%x' "$n")\",false]"
    rb=$(rpc http://reth:8545 eth_getBlockByNumber "$q")
    # Blocks with blob txs are excluded: after PeerDAS a non-supernode beacon
    # source cannot serve their sidecars, so the engine skips those txs by
    # design (recorded in drift.jsonl, state still converged by the repair).
    bg=$(echo "$rb" | jq -r '.result.blobGasUsed // "0x0"')
    if [ "$bg" != "0x0" ] && [ "$bg" != "null" ] && [ -n "$bg" ]; then
      blob_blocks=$((blob_blocks + 1)); continue
    fi
    rtx=$(echo "$rb" | jq -r '.result.transactions | length' 2>/dev/null)
    mtx=$(rpc http://anvil-m:8545 eth_getBlockByNumber "$q" | jq -r '.result.transactions | length' 2>/dev/null)
    [ -z "$rtx" ] || [ -z "$mtx" ] && continue
    total=$((total + 1))
    [ "$rtx" = "$mtx" ] && match=$((match + 1))
  done
  if [ "$total" -gt 0 ]; then
    pct=$((match * 100 / total))
    echo "   tx-count parity: $match/$total non-blob blocks ($blob_blocks blob blocks excluded)"
    if [ "$pct" -ge 80 ]; then
      ok "parity rate ${pct}% (>= 80%; blob-tx blocks are excluded by design)"
    else
      bad "parity rate ${pct}% (< 80%): inspect drift.jsonl and logs-engine"
    fi
  else
    echo "  … no comparable blocks yet ($blob_blocks blob blocks excluded)"
  fi
else
  echo "  … engine has not replayed a block yet (skipping parity check)"
fi

echo
echo "== $pass passed, $fail failed =="
[ "$fail" -eq 0 ]
