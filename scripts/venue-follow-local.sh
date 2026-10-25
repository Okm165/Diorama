#!/usr/bin/env bash
# venue-follow-local: the venue follow, proven natively (no cluster, no Docker):
# a local anvil stands in for the HL public API, V forks it, and the engine
# must (1) mirror the watched account 1:1 as the source moves, and (2) keep a
# devnet write on V when the next source window lands (the COW filter).
#
# The production source is the HL public RPC; this script's source is an anvil
# so the proof is hermetic. The composer's reads are the same either way
# (`eth_getBlockByNumber`, `eth_getBlockReceipts`, `eth_getSystemTxsByBlockNumber`
# — a chain without the last one answers an empty list by design).
#
# Requires: anvil (ANVIL=/path/to/anvil), the venue engine binary built
# (`make venue-build`), python3, curl. Ports: 8798/8799 + 18082 (override with
# PORTS below). Never touches the cluster.
set -euo pipefail
cd "$(dirname "$0")/.."

ANVIL="${ANVIL:-anvil}"
ENGINE="${ENGINE:-target/debug/venue-engine}"
SRC_PORT="${SRC_PORT:-8799}"
V_PORT="${V_PORT:-8798}"
HTTP_PORT="${HTTP_PORT:-18082}"
DIR="${DIR:-/tmp/venue-follow-local}"
SRC="http://127.0.0.1:$SRC_PORT"
V="http://127.0.0.1:$V_PORT"
HTTP="http://127.0.0.1:$HTTP_PORT"
WATCH=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 # anvil account 0 (watched)
OTHER=0x70997970C51812dc3A010C7d01b50e0d17dc79C8 # anvil account 1 (devnet tx)

pass=0; fail=0
ok()  { echo "  ✔ $*"; pass=$((pass + 1)); }
bad() { echo "  ✘ $*"; fail=$((fail + 1)); }

command -v "$ANVIL" >/dev/null || { echo "anvil not found (set ANVIL=)"; exit 2; }
[ -x "$ENGINE" ] || { echo "$ENGINE not built (make venue-build)"; exit 2; }

cleanup() {
  [ -n "${ENG_PID:-}" ] && kill "$ENG_PID" 2>/dev/null || true
  [ -n "${SRC_PID:-}" ] && kill "$SRC_PID" 2>/dev/null || true
  [ -n "${V_PID:-}" ] && kill "$V_PID" 2>/dev/null || true
}
trap cleanup EXIT
rm -rf "$DIR"; mkdir -p "$DIR/state/journal"

rpc() { curl -s -X POST -H 'content-type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}" "$1"; }
bal() { rpc "$1" eth_getBalance "[\"$WATCH\",\"latest\"]" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))'; }
nonce() { rpc "$1" eth_getTransactionCount "[\"$WATCH\",\"latest\"]" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))'; }
metric() { curl -s "$HTTP/metrics" | grep "^$1 " | awk '{print $2}'; }
send() { rpc "$1" eth_sendTransaction "[{\"from\":\"$2\",\"to\":\"$3\",\"value\":\"$4\"}]" >/dev/null; }

"$ANVIL" --port "$SRC_PORT" --chain-id 998 --block-time 1 --silent > "$DIR/source.log" 2>&1 &
SRC_PID=$!
sleep 1.5
PIN=$(rpc "$SRC" eth_blockNumber '[]' | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))')

"$ANVIL" --port "$V_PORT" --chain-id 999 --fork-url "$SRC" --fork-block-number "$PIN" --silent > "$DIR/v.log" 2>&1 &
V_PID=$!
sleep 2

cat > "$DIR/instance.toml" <<EOF
[instance]
name = "venue-follow-local"
namespace = "venue-follow-local"
profile = "venue"

[chains.venue]
kind = "hyperliquid"
source_url = "$SRC"
api_url = "$SRC"
ws_url = "ws://127.0.0.1:$SRC_PORT"

[[watchlist]]
address = "$WATCH"
note = "anvil account 0: the proof's watched sender"

[services.engine]
state_dir = "$DIR/state"

[services.engine.anvil]
target_url = "$V"

[services.engine.journal]
dir = "$DIR/state/journal"

[services.engine.verifier]
probes = []

[services.engine.http]
addr = "127.0.0.1"
port = $HTTP_PORT
EOF

"$ENGINE" run --config "$DIR/instance.toml" > "$DIR/engine.log" 2>&1 &
ENG_PID=$!
for _ in $(seq 1 40); do curl -sf "$HTTP/livez" >/dev/null 2>&1 && break; sleep 0.5; done
curl -sf "$HTTP/livez" >/dev/null || { bad "engine did not come up (see $DIR/engine.log)"; exit 1; }

# Stage 1: the source moves; the follow must land the watched fields on V 1:1.
send "$SRC" "$WATCH" 0x000000000000000000000000000000000000dEaD 0xde0b6b3a7640000
sleep 5
if [ "$(bal "$SRC")" = "$(bal "$V")" ] && [ "$(nonce "$SRC")" = "$(nonce "$V")" ]; then
  ok "follow 1:1: V's balance and nonce match the source after its tx"
else
  bad "follow mismatch: source $(bal "$SRC")/$(nonce "$SRC") vs V $(bal "$V")/$(nonce "$V")"
fi
base_v=$(bal "$V")

# Stage 2: a devnet write on V, then another source move: COW must skip it.
send "$V" "$OTHER" "$WATCH" 0x4563918244f40000
sleep 4
expect=$(python3 -c "print($base_v + 5 * 10**18)")
if [ "$(bal "$V")" = "$expect" ]; then
  ok "devnet write on V landed (balance + 5 ETH)"
else
  bad "devnet write did not land ($(bal "$V") != $expect)"
fi

send "$SRC" "$WATCH" 0x000000000000000000000000000000000000dEaD 0xde0b6b3a7640000
sleep 5
if [ "$(bal "$V")" = "$expect" ] && [ "$(bal "$SRC")" != "$expect" ]; then
  ok "COW: the devnet-owned balance survived the next source window"
else
  bad "COW: V $(bal "$V") vs expected $expect (source $(bal "$SRC"))"
fi
skipped=$(metric venue_follow_skipped_shadowed_total)
if [ "${skipped:-0}" -ge 2 ]; then
  ok "COW: skipped_shadowed_total=$skipped (balance + nonce)"
else
  bad "COW: skipped_shadowed_total=$skipped (expected >= 2)"
fi
applied=$(metric venue_follow_applied_total)
if [ "${applied:-0}" -ge 2 ]; then
  ok "follow applied_total=$applied (the pre-COW window's fields)"
else
  bad "follow applied_total=$applied (expected >= 2)"
fi

echo "venue-follow-local: $pass ok, $fail bad (logs in $DIR)"
[ "$fail" -eq 0 ]
