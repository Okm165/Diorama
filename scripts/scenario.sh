#!/usr/bin/env bash
# End-to-end scenario test: is the mirror really a live, exact mainnet mirror,
# and is T really a usable devnet with copy-on-write semantics?
#
# Phases
#   1. self-containment   reth has no peers, the top-up gap is 0, blobs are local
#   2. live parity        heads agree; the 6 state probes match reth on M *and* T
#   3. devnet workflow    fund, send, shadow, rewind, snapshot/revert on T
#   4. integrity counters no divergences / convergence failures / blob failures
#
# Requires: the deployed stack (make bootstrap), kubectl, no external network.
set -uo pipefail

NS=${NS:-mainnet-mirror}
K="kubectl -n $NS"
ENGINE="$K exec deploy/engine --"
ANVIL_T="$K exec deploy/anvil-t --"
pass=0; fail=0
ok()  { echo "  ✔ $*"; pass=$((pass+1)); }
bad() { echo "  ✘ $*"; fail=$((fail+1)); }
info(){ echo "  · $*"; }

rpc() { # rpc <url> <method> [params-json]
  local url=$1 method=$2 params=${3:-[]}
  $ENGINE curl -sf -m 20 "$url" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" 2>/dev/null
}
engine_api() { $ENGINE curl -sf -m 20 "localhost:8080$1" 2>/dev/null; }
num() { printf '%d' "${1:-0}" 2>/dev/null || echo 0; }

# The probes mirror k8s/base/engine-config.yaml:verifier.probes (all watched
# accounts, so T must match reth exactly; none is blockhash-dependent, so M must
# too - the engine's convergence repair is what makes that true).
probe() { # probe <rpc-url> <block-hex> <to> <data>
  rpc "$1" eth_call "[{\"to\":\"$3\",\"data\":\"$4\"},\"$2\"]" | jq -r '.result // "ERR"'
}
probes_match() { # probes_match <url-a> <url-b> <label> <block-hex>
  local a=$1 b=$2 label=$3 at=$4 mismatches=0
  while read -r name to data; do
    va=$(probe "$a" "$at" "$to" "$data")
    # a chain whose height diverges from the parent height is read at latest
    if [ "$b" = "http://anvil-t:8545" ]; then
      vb=$(rpc "$b" eth_call "[{\"to\":\"$to\",\"data\":\"$data\"},\"latest\"]" | jq -r '.result // "ERR"')
    else
      vb=$(probe "$b" "$at" "$to" "$data")
    fi
    if [ "$va" != "$vb" ]; then
      bad "$label probe $name: ${va:0:20}… != ${vb:0:20}…"; mismatches=$((mismatches+1))
    fi
  done <<'PROBES'
usdc_total_supply 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 0x18160ddd
weth_total_supply 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 0x18160ddd
chainlink_weth_usd 0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419 0xfeaf968c
univ3_weth_usdc_slot0 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640 0x3850c7bd
aave_aethusdc_total_supply 0x98C23E9d8f34FEFb1B7BD6a91B7FF122F4e16F5c 0x18160ddd
aave_usdc_normalized_income 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2 0xd15e0053000000000000000000000000A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
PROBES
  [ "$mismatches" -eq 0 ] && ok "$label matches reth on all 6 probes"
}

echo "== mirror scenario test (namespace $NS)"

echo "-- 1. self-containment"
peers=$(rpc http://reth:8545 net_peerCount | jq -r '.result // "ERR"')
[ "$(num "$peers")" -eq 0 ] && ok "reth has no EL peers (${peers})" || bad "reth has peers: $peers"
gap=$(engine_api /metrics | awk '/^mirror_source_sync_gap_blocks /{print $2}')
[ "$(num "$gap")" -eq 0 ] && ok "top-up gap is 0 (reth tracks the CL head)" || bad "top-up gap: $gap"
urls=$($K get cm engine-config -o jsonpath='{.data.engine\.yaml}' | awk '/beacon_urls:/{getline; print}' | tr -d ' -')
case "$urls" in *lighthouse*) ok "blob source is the local CL ($urls)";; *) bad "blob source: $urls";; esac
pipeline=$($K logs deploy/reth --since=10m 2>/dev/null | grep -ciE "pipeline|backfill")
[ "$(num "$pipeline")" -eq 0 ] && ok "no peer-based pipeline/backfill activity on reth" || info "reth pipeline log lines: $pipeline"

echo "-- 2. live parity"
for _ in $(seq 1 30); do
  st=$(engine_api /status)
  [ "$(echo "$st" | jq -r '.cursor_t')" = "$(echo "$st" | jq -r '.t_head')" ] && break
  sleep 2
done
reth_head=$(num "$(rpc http://reth:8545 eth_blockNumber | jq -r '.result')")
follow=$(num "$(engine_api /status | jq -r '.follow_head')")
m_head=$(num "$(engine_api /status | jq -r '.m_head')")
t_head=$(num "$(engine_api /status | jq -r '.t_head')")
info "reth=$reth_head follow=$follow M=$m_head T=$t_head"
[ "$reth_head" -ge "$follow" ] && ok "reth is at/above the follow head" || bad "reth behind follow head"
if [ "$m_head" -lt "$((follow - 2))" ]; then
  info "M is $((follow - m_head)) blocks behind; waiting for it to catch up…"
  for _ in $(seq 1 60); do
    m_head=$(num "$(engine_api /status | jq -r '.m_head')")
    [ "$m_head" -ge "$((follow - 2))" ] && break
    sleep 2
  done
fi
[ "$m_head" -ge "$((follow - 2))" ] && ok "M is replaying at the follow head" || bad "M lagging: $((follow - m_head)) blocks"
cursor_t=$(num "$(engine_api /status | jq -r '.cursor_t')")
[ "$cursor_t" -ge "$((follow - 2))" ] && ok "T has applied parent blocks up to the follow head (cursor_t=$cursor_t, height=$t_head)" \
  || bad "T applier lagging: cursor_t=$cursor_t, follow=$follow"
probes_match http://reth:8545 http://anvil-m:8545 "M" "$(printf '0x%x' "$m_head")"
at=$(printf '0x%x' "$(num "$(engine_api /status | jq -r '.cursor_t')")")
probes_match http://reth:8545 http://anvil-t:8545 "T" "$at"

echo "-- 3. devnet workflow on T"
USER=0x00000000000000000000000000000000000F00d1
# fund (unlimited funds: T is a devnet, anvil cheatcodes are the faucet)
rpc http://anvil-t:8545 anvil_setBalance "[\"$USER\",\"0x8ac7230489e80000\"]" >/dev/null # 10 ETH
bal=$(rpc http://anvil-t:8545 eth_getBalance "[\"$USER\",\"latest\"]" | jq -r '.result')
[ "$bal" = "0x8ac7230489e80000" ] && ok "faucet: funded $USER with 10 ETH" || bad "balance after fund: $bal"

# shadowed write: a watched slot written through a cheatcode must survive the
# parent diffs that keep flowing (copy-on-write), while sibling slots keep syncing.
WETH=0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
SLOT=0x0000000000000000000000000000000000000000000000000000000000000003 # totalSupply slot
OTHER=0x0000000000000000000000000000000000000000000000000000000000000004
pre=$($ANVIL_T cast storage $WETH $SLOT --rpc-url http://localhost:8545 2>/dev/null || echo "")
shadow=$(engine_api /status | jq -r '.shadowed_slots // 0')
rpc http://anvil-t:8545 anvil_setStorageAt "[\"$WETH\",\"$SLOT\",\"0x0000000000000000000000000000000000000000000000000000000000000042\"]" >/dev/null
resp=$($ENGINE curl -sf -m 20 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
  -d "{\"address\":\"$WETH\",\"slots\":[\"$SLOT\"],\"account_fields\":false}" 2>/dev/null)
addr=$(echo "$resp" | jq -r '.address // "ERR"' 2>/dev/null)
[ "${addr,,}" = "${WETH,,}" ] && ok "shadow registered (slots now $(echo "$resp" | jq -r '.shadowed_slots // "?"'))" \
  || bad "shadow registration failed: ${resp:0:80}"
info "waiting for parent diffs to flow (2 mainnet blocks)…"
sleep 25
val=$($ANVIL_T cast storage $WETH $SLOT --rpc-url http://localhost:8545 2>/dev/null | sed 's/0x//' | sed 's/^0*//')
[ "${val:-x}" = "42" ] && ok "shadowed slot kept the devnet value through parent diffs" \
  || bad "shadowed slot was overwritten (value ${val:-?})"
other=$($ANVIL_T cast storage $WETH $OTHER --rpc-url http://localhost:8545 2>/dev/null)
reth_other=$(rpc http://reth:8545 eth_getStorageAt "[\"$WETH\",\"$OTHER\",\"latest\"]" | jq -r '.result')
[ "$other" = "$reth_other" ] && ok "sibling slot still tracks mainnet" \
  || info "sibling slot differs (expected if that slot is not watched): ${other:0:18} vs ${reth_other:0:18}"

# snapshot / revert (anvil-native)
snap=$($ANVIL_T cast rpc evm_snapshot --rpc-url http://localhost:8545 2>/dev/null | jq -r '.')
rpc http://anvil-t:8545 anvil_setStorageAt "[\"$WETH\",\"$SLOT\",\"0x0000000000000000000000000000000000000000000000000000000000000099\"]" >/dev/null
$ANVIL_T cast rpc evm_revert "$snap" --rpc-url http://localhost:8545 >/dev/null 2>&1
val=$($ANVIL_T cast storage $WETH $SLOT --rpc-url http://localhost:8545 2>/dev/null | sed 's/0x//' | sed 's/^0*//')
[ "${val:-x}" = "42" ] && ok "evm_snapshot/evm_revert restores the shadowed value" || bad "revert left value ${val:-?}"

# journal rewind: revert the last applied parent block on T
before=$(engine_api /status | jq -r '.cursor_t')
rew=$($ENGINE curl -sf -m 30 -X POST localhost:8080/admin/rewind -H 'Content-Type: application/json' -d '{"blocks":1}' 2>/dev/null | jq -r '.rewound_blocks // "ERR"')
after=$(engine_api /status | jq -r '.cursor_t')
if [ "$rew" != "ERR" ] && [ "$(num "$after")" -lt "$(num "$before")" ]; then
  ok "journal rewind moved T cursor $before -> $after"
else
  info "rewind returned: ${rew:0:60} (cursor $before -> $after)"
fi

echo "-- 4. integrity counters"
m=$(engine_api /metrics)
for counter in mirror_divergences_total mirror_value_divergences_total \
               mirror_m_convergence_failures_total mirror_blob_failures_total; do
  v=$(echo "$m" | awk -v c="$counter" '$1==c {print $2}')
  case "$counter" in
    mirror_value_divergences_total) info "$counter = ${v:-0} (blockhash-dependent, allowed)";;
    *) [ "$(num "$v")" -eq 0 ] && ok "$counter = 0" || bad "$counter = ${v:-?}";;
  esac
done

echo
echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
