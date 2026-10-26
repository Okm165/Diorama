#!/usr/bin/env bash
# End-to-end smoke test for the Venue Mirror stack. Run from the repo root:
#   make venue-smoke        (or NS=diorama-venue ./scripts/venue-smoke.sh)
#
# Executes curl+jq from inside the venue engine pod so every check talks to the
# cluster-internal service names (the same way the engine does). The mainnet
# counterpart is scripts/mainnet-smoke.sh; the checks differ because the venue's facts
# differ: a fork pin instead of a follow head, the simulator's market-plane
# freshness instead of reth/CL liveness, the bridge's counters instead of reth/T
# parity.
set -euo pipefail

NS="${NS:-diorama-venue}"

source "$(dirname "${BASH_SOURCE[0]}")/lib-common.sh"

echo "== venue mirror smoke test (namespace $NS) =="

echo "-- pods"
kubectl -n "$NS" get pods --no-headers | awk '{printf "  %-42s %-10s %s\n", $1, $3, $5}'

run() { kubectl -n "$NS" exec -i deploy/engine -- "$@"; }

# Overrides lib-common's plain rpc: the smoke runs inside the pod.
rpc() { # rpc <url> <method> [params-json]
  local url="$1" method="$2" params="${3:-[]}"
  run curl -sf -m 20 -X POST "$url" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}"
}

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

echo "-- daloc-facing info shape"
if [ "$ready" = "200" ]; then
  meta=$(run curl -sf -m 20 -X POST http://venue-sim:8090/info -H 'Content-Type: application/json' -d '{"type":"meta"}' || true)
  n=$(echo "$meta" | jq -r '.universe | length' 2>/dev/null || echo 0)
  if [ "${n:-0}" -gt 0 ]; then ok "sim /info meta: $n markets"; else bad "sim /info meta empty"; fi
else
  info "sim not ready yet; skipping the /info shape check"
fi

echo
echo "== $pass passed, $fail failed =="
[ "$fail" -eq 0 ]
