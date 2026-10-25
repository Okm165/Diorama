#!/usr/bin/env bash
# engine-symmetry-local: both mirrors, the same four verbs, side by side (the
# local counterpart of the cluster's `engine-symmetry`). Boots one minimal
# stack per mirror (a source anvil + a devnet anvil + the engine), then drives
# — with the same code path for both — mirror 1:1, modify (copy-on-write),
# mint (`/admin/deal` on the same mock ERC-20), measure (`/status`, probes,
# the parity counters, the epoch), and asserts the two engines answer
# symmetrically: the UX is one product, two parents.
#
# Requires: anvil (ANVIL=), both engine binaries built
# (`make mainnet-build venue-build`), python3, curl.
# Ports: 19900/19902/19980 (T) and 19908/19910/19981 (V) — override below.
set -euo pipefail
cd "$(dirname "$0")/.."

ANVIL="${ANVIL:-/tmp/opencode/anvil-186-bin/anvil}"
DIR="${DIR:-/tmp/engine-symmetry-local}"
ST_PORT="${ST_PORT:-19900}"; T_PORT="${T_PORT:-19902}"; T_ADMIN="${T_ADMIN:-19980}"
SV_PORT="${SV_PORT:-19908}"; V_PORT="${V_PORT:-19910}"; V_ADMIN="${V_ADMIN:-19981}"
A0=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
TOKEN=0x00000000000000000000000000000000000000e7
# balanceOf(address) = SLOAD(keccak256(addr ‖ slot 0)): the deal's probe target.
TOKEN_CODE=0x600435600052600060205260406000205460005260206000f3

cleanup() { for pid in ${PIDS:-}; do kill "$pid" 2>/dev/null || true; done; }
trap cleanup EXIT
rm -rf "$DIR"; mkdir -p "$DIR/t/state/journal" "$DIR/v/state/journal"

rpc() { curl -s -X POST -H 'content-type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}" "$1"; }
jq() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
metric() { curl -s "$1/metrics" | awk -v k="$2" '$1 == k {print $2}'; }
status() { curl -s "$1/status"; }

pass=0; fail=0
ok()  { echo "  ✔ $*"; pass=$((pass+1)); }
bad() { echo "  ✘ $*"; fail=$((fail+1)); }

# ---- the two stacks (one shape, two parents) ------------------------------
"$ANVIL" --port "$ST_PORT" --chain-id 1 --block-time 1 --silent --hardfork amsterdam > "$DIR/src-t.log" 2>&1 &
PIDS="$!"
"$ANVIL" --port "$SV_PORT" --chain-id 999 --block-time 1 --silent > "$DIR/src-v.log" 2>&1 &
PIDS="$PIDS $!"
sleep 1.5
rpc "http://127.0.0.1:$ST_PORT" anvil_setCode "[\"$TOKEN\",\"$TOKEN_CODE\"]" >/dev/null
rpc "http://127.0.0.1:$SV_PORT" anvil_setCode "[\"$TOKEN\",\"$TOKEN_CODE\"]" >/dev/null
sleep 0.5
PIN_T=$(rpc "http://127.0.0.1:$ST_PORT" eth_blockNumber '[]' | jq 'int(d["result"],16)')
PIN_V=$(rpc "http://127.0.0.1:$SV_PORT" eth_blockNumber '[]' | jq 'int(d["result"],16)')
"$ANVIL" --port "$T_PORT" --chain-id 1 --fork-url "http://127.0.0.1:$ST_PORT" --fork-block-number "$PIN_T" --silent > "$DIR/t.log" 2>&1 &
PIDS="$PIDS $!"
"$ANVIL" --port "$V_PORT" --chain-id 999 --fork-url "http://127.0.0.1:$SV_PORT" --fork-block-number "$PIN_V" --silent > "$DIR/v.log" 2>&1 &
PIDS="$PIDS $!"
sleep 1.5

cat > "$DIR/t.toml" <<EOF
[instance]
name = "ux-t"
namespace = "ux-t"
profile = "mainnet"
[chains.l1]
kind = "ethereum"
source_url = "http://127.0.0.1:$ST_PORT"
follow = "latest"
fork_offset = 128
[[watchlist]]
address = "$A0"
note = "the shared sender"
[services.engine]
state_dir = "$DIR/t/state"
fixture_addresses = []
[services.engine.anvil]
target_url = "http://127.0.0.1:$T_PORT"
[services.engine.journal]
dir = "$DIR/t/state/journal"
[services.engine.verifier]
probes = []
[services.engine.http]
addr = "127.0.0.1"
port = $T_ADMIN
EOF

cat > "$DIR/v.toml" <<EOF
[instance]
name = "ux-v"
namespace = "ux-v"
profile = "venue"
[chains.venue]
kind = "hyperliquid"
source_url = "http://127.0.0.1:$SV_PORT"
api_url = "http://127.0.0.1:$SV_PORT"
ws_url = "ws://127.0.0.1:$SV_PORT"
[[watchlist]]
address = "$A0"
note = "the shared sender"
[services.engine]
state_dir = "$DIR/v/state"
[services.engine.anvil]
target_url = "http://127.0.0.1:$V_PORT"
[services.engine.journal]
dir = "$DIR/v/state/journal"
[services.engine.verifier]
probes = []
[services.engine.http]
addr = "127.0.0.1"
port = $V_ADMIN
EOF

target/debug/mainnet-engine run --config "$DIR/t.toml" > "$DIR/t-engine.log" 2>&1 &
PIDS="$PIDS $!"
target/debug/venue-engine run --config "$DIR/v.toml" > "$DIR/v-engine.log" 2>&1 &
PIDS="$PIDS $!"
for _ in $(seq 1 60); do curl -sf "http://127.0.0.1:$T_ADMIN/livez" >/dev/null 2>&1 && break; sleep 0.5; done
for _ in $(seq 1 60); do curl -sf "http://127.0.0.1:$V_ADMIN/livez" >/dev/null 2>&1 && break; sleep 0.5; done
curl -sf "http://127.0.0.1:$T_ADMIN/livez" >/dev/null && curl -sf "http://127.0.0.1:$V_ADMIN/livez" >/dev/null \
  || { echo "engines did not come up"; tail -5 "$DIR/t-engine.log" "$DIR/v-engine.log"; exit 1; }

echo "== verb 1: follow the parent 1:1 =="
rpc "http://127.0.0.1:$ST_PORT" eth_sendTransaction "[{\"from\":\"$A0\",\"to\":\"$TOKEN\",\"value\":\"0x0\"}]" >/dev/null
rpc "http://127.0.0.1:$SV_PORT" eth_sendTransaction "[{\"from\":\"$A0\",\"to\":\"$TOKEN\",\"value\":\"0x0\"}]" >/dev/null
sleep 6
applied_t=$(metric "http://127.0.0.1:$T_ADMIN" mirror_blocks_applied_total)
applied_v=$(metric "http://127.0.0.1:$V_ADMIN" venue_follow_applied_total)
[ "${applied_t:-0}" -ge 1 ] && ok "T applied $applied_t parent block(s)" || bad "T applied nothing"
[ "${applied_v:-0}" -ge 1 ] && ok "V followed $applied_v key(s) from the source" || bad "V followed nothing"
for k in head cursor halted shadowed shadowed_slots shadowed_accounts capabilities; do
  printf '%s' "$(status "http://127.0.0.1:$T_ADMIN")" | python3 -c "import json,sys; sys.exit(0 if '$k' in json.load(sys.stdin) else 1)" \
    && printf '%s' "$(status "http://127.0.0.1:$V_ADMIN")" | python3 -c "import json,sys; sys.exit(0 if '$k' in json.load(sys.stdin) else 1)" \
    && ok "/status has $k on both" || bad "/status missing $k on one side"
done

echo "== verb 2: accept your writes (copy-on-write) =="
rpc "http://127.0.0.1:$T_PORT" eth_sendTransaction "[{\"from\":\"$A0\",\"to\":\"0x000000000000000000000000000000000000dEaD\",\"value\":\"0x1000\"}]" >/dev/null
rpc "http://127.0.0.1:$V_PORT" eth_sendTransaction "[{\"from\":\"$A0\",\"to\":\"0x000000000000000000000000000000000000dEaD\",\"value\":\"0x1000\"}]" >/dev/null
sleep 5
rpc "http://127.0.0.1:$ST_PORT" eth_sendTransaction "[{\"from\":\"$A0\",\"to\":\"$TOKEN\",\"value\":\"0x0\"}]" >/dev/null
rpc "http://127.0.0.1:$SV_PORT" eth_sendTransaction "[{\"from\":\"$A0\",\"to\":\"$TOKEN\",\"value\":\"0x0\"}]" >/dev/null
sleep 6
bal_t=$(rpc "http://127.0.0.1:$T_PORT" eth_getBalance "[\"$A0\",\"latest\"]" | jq 'int(d["result"],16)')
bal_v=$(rpc "http://127.0.0.1:$V_PORT" eth_getBalance "[\"$A0\",\"latest\"]" | jq 'int(d["result"],16)')
[ "$bal_t" != "0" ] && ok "T kept the devnet write through the next parent diffs ($bal_t)" || bad "T lost the devnet write"
[ "$bal_v" != "0" ] && ok "V kept the devnet write through the next source window ($bal_v)" || bad "V lost the devnet write"
sh_t=$(status "http://127.0.0.1:$T_ADMIN" | jq 'd["shadowed_accounts"]')
sh_v=$(status "http://127.0.0.1:$V_ADMIN" | jq 'd["shadowed_accounts"]')
[ "$sh_t" -ge 1 ] && [ "$sh_v" -ge 1 ] && ok "both engines mark devnet-owned accounts (T=$sh_t V=$sh_v)" \
  || bad "shadow marks: T=$sh_t V=$sh_v"

echo "== verb 3: mint liquidity =="
deal_t=$(curl -s -X POST "http://127.0.0.1:$T_ADMIN/admin/deal" -H 'content-type: application/json' \
  -d "{\"token\":\"$TOKEN\",\"to\":\"$A0\",\"amount\":\"777\"}")
deal_v=$(curl -s -X POST "http://127.0.0.1:$V_ADMIN/admin/deal" -H 'content-type: application/json' \
  -d "{\"token\":\"$TOKEN\",\"to\":\"$A0\",\"amount\":\"777\"}")
printf '%s' "$deal_t" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("ok") and d.get("slot") else 1)' \
  && ok "T minted (key $(printf '%s' "$deal_t" | jq 'd["slot"]'))" || bad "T deal: $deal_t"
printf '%s' "$deal_v" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("ok") and d.get("slot") else 1)' \
  && ok "V minted (key $(printf '%s' "$deal_v" | jq 'd["slot"]'))" || bad "V deal: $deal_v"
tb=$(rpc "http://127.0.0.1:$T_PORT" eth_call "[{\"to\":\"$TOKEN\",\"data\":\"0x70a08231000000000000000000000000$(echo "$A0" | cut -c3-)\"},\"latest\"]" | jq 'int(d["result"],16)')
vb=$(rpc "http://127.0.0.1:$V_PORT" eth_call "[{\"to\":\"$TOKEN\",\"data\":\"0x70a08231000000000000000000000000$(echo "$A0" | cut -c3-)\"},\"latest\"]" | jq 'int(d["result"],16)')
[ "$tb" = "777" ] && [ "$vb" = "777" ] && ok "the same mint reads 777 on both devnets" || bad "mint reads T=$tb V=$vb"

echo "== verb 4: measure the divergence =="
po_t=$(curl -s -X POST "http://127.0.0.1:$T_ADMIN/admin/probes" -H 'content-type: application/json' -d '{}')
po_v=$(curl -s -X POST "http://127.0.0.1:$V_ADMIN/admin/probes" -H 'content-type: application/json' -d '{}')
for side in T V; do
  body=$([ "$side" = "T" ] && printf '%s' "$po_t" || printf '%s' "$po_v")
  printf '%s' "$body" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if all(k in d for k in ("ok","failed","skipped")) else 1)' \
    && ok "/admin/probes reports {ok, failed, skipped} on $side" || bad "/admin/probes on $side: $body"
done
# The measurement counters each side owns (the parity of every applied unit).
[ "$(metric "http://127.0.0.1:$T_ADMIN" mirror_t_convergence_ok_total)" != "" ] \
  && ok "T's per-block convergence counters are exposed" || bad "T has no convergence counters"
[ "$(metric "http://127.0.0.1:$V_ADMIN" venue_follow_sampled_ok_total)" != "" ] \
  && ok "V's live-parity counters are exposed" || bad "V has no parity counters"
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$T_ADMIN/admin/epoch")" = "200" ] \
  && [ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$V_ADMIN/admin/epoch")" = "200" ] \
  && ok "/admin/epoch re-pins on both" || bad "/admin/epoch differs"

echo "engine-symmetry-local: $pass ok, $fail bad (logs in $DIR)"
[ "$fail" -eq 0 ]
