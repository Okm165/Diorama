#!/usr/bin/env bash
# End-to-end smoke test for either mirror stack. Run from the repo root:
#   ./scripts/smoke.sh --side mainnet
#   ./scripts/smoke.sh --side venue
#
# The two mirrors have different facts (a fork pin vs a follow head, the
# simulator's market plane vs reth/CL liveness, the bridge counters vs reth/T
# parity), so the checks are side-specific. The action is one generic smoke
# runner; the side is the CLI specialization.
set -euo pipefail

SIDE="mainnet"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --side) SIDE="$2"; shift 2 ;;
    --side=*) SIDE="${1#*=}"; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

case "$SIDE" in
  mainnet) NS="${NS:-diorama-mainnet}" ;;
  venue) NS="${NS:-diorama-venue}" ;;
  *) echo "ERROR: --side must be mainnet or venue (got $SIDE)" >&2; exit 2 ;;
esac

source "$(dirname "${BASH_SOURCE[0]}")/lib-common.sh"

echo "== $SIDE mirror smoke test (namespace $NS) =="

echo "-- pods"
kubectl -n "$NS" get pods --no-headers | awk '{printf "  %-42s %-10s %s\n", $1, $3, $5}'

run() { kubectl -n "$NS" exec -i deploy/engine -- "$@"; }

# The smoke's JSON-RPC helper: every call runs inside the engine pod.
rpc() { # rpc <url> <method> [params-json]
  local url="$1" method="$2" params="${3:-[]}"
  run curl -sf -m 20 -X POST "$url" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}"
}

if [[ "$SIDE" = "mainnet" ]]; then
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
      gap=$(run curl -sf -m 10 localhost:8080/metrics 2>/dev/null | awk '/^diorama_source_sync_gap_blocks /{print $2}' || true)
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

  echo "-- anvil T"
  h=$(rpc "http://anvil-t:8545" eth_blockNumber | jq -r .result)
  info=$(rpc "http://anvil-t:8545" anvil_nodeInfo | jq -c '{head: .result.currentBlockNumber, pin: .result.forkConfig.forkBlockNumber}')
  if [ "$h" != "null" ]; then ok "anvil-t: $info"; else bad "anvil-t unreachable"; fi

  echo "-- engine"
  st=$(run curl -sf -m 10 http://engine:8080/status || echo FAIL)
  if [ "$st" != "FAIL" ]; then
    ok "engine status: $(echo "$st" | jq -c '{reth_head, head, cursor_t, lag}')"
  else
    bad "engine /status unreachable"
  fi

  echo "-- state parity (the engine's own convergence counters)"
  m=$(run curl -sf -m 10 http://engine:8080/metrics 2>/dev/null || true)
  if [ -n "$m" ]; then
    t_fail=$(echo "$m" | awk '/^diorama_t_convergence_failures_total /{print $2}' || true)
    sweep=$(echo "$m" | awk '/^diorama_full_sweep_mismatches_total /{print $2}' || true)
    lag=$(echo "$m" | awk '/^diorama_lag_blocks /{print $2}' || true)
    if [ "${t_fail:-1}" = "0" ] && [ "${sweep:-1}" = "0" ]; then
      ok "probe parity: t_convergence_failures=0, full_sweep_mismatches=0 (lag ${lag:-?} blocks)"
    else
      bad "parity counters non-zero: t_convergence_failures=${t_fail:-?}, full_sweep_mismatches=${sweep:-?} - inspect drift.jsonl and logs-engine"
    fi
  else
    bad "engine /metrics unreachable"
  fi
else
  echo "-- anvil V (the fork-mode devnet chain, chain-id 999)"
  v_head=""; v_pin=""
  if head_hex=$(rpc http://anvil-v:8545 eth_blockNumber | jq -r .result) && [ -n "$head_hex" ] && [ "$head_hex" != "null" ]; then
    v_head=$((head_hex))
    chain=$(rpc http://anvil-v:8545 eth_chainId | jq -r .result)
    if [ "$chain" = "0x3e7" ]; then
      ok "anvil-v: chain 999, head $v_head"
    else
      bad "anvil-v chain id $chain (want 0x3e7)"
    fi
    v_pin=$(rpc http://anvil-v:8545 anvil_nodeInfo | jq -r '.result.forkConfig.forkBlockNumber // empty')
    if [ -n "$v_pin" ] && [ "$v_pin" != "null" ]; then
      if [ "$v_head" -ge "$v_pin" ]; then
        ok "anvil-v fork pin = $v_pin (head $v_head >= pin)"
      else
        bad "anvil-v head $v_head is below its pin $v_pin"
      fi
    else
      # A no-params anvil_reset drops the fork entirely (measured):
      # the pin must always be explicit.
      bad "anvil-v has no fork pin (anvil_reset must carry the explicit forking config)"
    fi
    automine=$(rpc http://anvil-v:8545 anvil_getAutomine | jq -r .result)
    if [ "$automine" = "false" ]; then
      ok "automine off (the engine owns block production)"
    else
      bad "automine=$automine: devnet txs would mine outside the driver"
    fi
  else
    bad "anvil-v unreachable"
  fi

  echo "-- engine (venue profile)"
  if h=$(run curl -sf -m 10 http://engine:8080/healthz); then ok "engine /healthz: $h"; else bad "engine /healthz unreachable"; fi
  status=$(run curl -sf -m 10 http://engine:8080/status || echo FAIL)
  if [ "$status" != "FAIL" ]; then
    ok "engine status: $(echo "$status" | jq -c '{head, cursor, fork_pin, epoch_pin, halted}')"
    e_pin=$(echo "$status" | jq -r '.fork_pin // empty')
    if [ -n "$v_pin" ] && [ "$e_pin" = "$v_pin" ]; then
      ok "engine pin matches V's fork pin ($e_pin)"
    elif [ -n "$v_pin" ]; then
      bad "engine pin $e_pin != V's fork pin $v_pin (the driver is on a different base)"
    fi
  else
    bad "engine /status unreachable"
  fi
  metrics=$(run curl -sf -m 10 http://engine:8080/metrics || true)
  if echo "$metrics" | grep -q '^venue_head '; then
    ok "engine /metrics serves the venue set ($(echo "$metrics" | grep -c '^venue_') series)"
  else
    bad "engine /metrics missing venue_head"
  fi

  echo "-- venue-sim"
  if h=$(run curl -sf -m 10 http://venue-sim:8090/healthz); then ok "sim /healthz: $h"; else bad "sim /healthz unreachable"; fi
  ready=$(run curl -s -o /dev/null -w '%{http_code}' -m 10 http://venue-sim:8090/readyz || true)
  case "$ready" in
    200) ok "sim /readyz: the market plane is fresh" ;;
    503) bad "sim /readyz: market plane warming up (no fresh ctxs yet)" ;;
    *) bad "sim /readyz unreachable (HTTP $ready)" ;;
  esac
  if c=$(run curl -sf -m 10 http://venue-sim:8091/health); then ok "sim control /health: $c"; else bad "sim control /health unreachable"; fi
  sim_metrics=$(run curl -sf -m 10 http://venue-sim:8091/metrics || true)
  if echo "$sim_metrics" | grep -q '^venue_sim_ws_connected'; then
    ws=$(echo "$sim_metrics" | awk '/^venue_sim_ws_connected /{print $2}')
    ok "market-plane ws connected: $ws"
  else
    bad "sim /metrics missing venue_sim_ws_connected"
  fi

  echo "-- bridge (Core<->EVM, sim-side counters)"
  if echo "$sim_metrics" | grep -q '^venue_bridge_credits_total'; then
    ok "bridge metrics live: $(echo "$sim_metrics" | awk '/^venue_bridge_(credits|completions|refusals|malformed)_total /{printf "%s=%s ", $1, $2}')"
  else
    bad "bridge metrics missing (venue_bridge_*)"
  fi

  echo "-- client-facing info shape"
  if [ "$ready" = "200" ]; then
    meta=$(run curl -sf -m 20 -X POST http://venue-sim:8090/info -H 'Content-Type: application/json' -d '{"type":"meta"}' || true)
    n=$(echo "$meta" | jq -r '.universe | length' 2>/dev/null || echo 0)
    if [ "${n:-0}" -gt 0 ]; then ok "sim /info meta: $n markets"; else bad "sim /info meta empty"; fi
  else
    info "sim not ready yet; skipping the /info shape check"
  fi
fi

echo
echo "== $pass passed, $fail failed =="
[ "$fail" -eq 0 ]
