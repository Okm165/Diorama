#!/usr/bin/env bash
# daloc-local: the daloc use-case arc (scripts/daloc-sim.py) against a local
# scratch stack — exactly the shape the arc's default endpoints describe:
# T (a scratch Ethereum fork) + the mainnet engine, V (a scratch HyperEVM
# fork) + the venue engine, and the venue simulator polling the *real*
# Hyperliquid API (live market data, like the cluster's venue-mirror
# namespace). No cluster, no Docker; needs outbound HTTPS/WSS to
# api.hyperliquid.xyz (the simulator's market plane).
#
# The defaults are the daloc arc's own ports (T 18602/18680, V 18610/18681,
# sim 18790/18791); override the *_PORT variables when they are taken.
set -euo pipefail
cd /home/bartosz/.local/share/opencode/worktree/cfbc0b/spine

ANVIL="${ANVIL:-/tmp/opencode/anvil-186-bin/anvil}"
DIR=/tmp/opencode/daloc-local
SRC_T_PORT="${SRC_T_PORT:-18600}"; T_PORT="${T_PORT:-18602}"; T_ADMIN_PORT="${T_ADMIN_PORT:-18680}"
SRC_V_PORT="${SRC_V_PORT:-18608}"; V_PORT="${V_PORT:-18610}"; V_ADMIN_PORT="${V_ADMIN_PORT:-18681}"
SIM_INFO_PORT="${SIM_INFO_PORT:-18790}"; SIM_CTRL_PORT="${SIM_CTRL_PORT:-18791}"
A0=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266

cleanup() {
  for pid in ${PIDS:-}; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT
rm -rf "$DIR"; mkdir -p "$DIR/t/state/journal" "$DIR/v/state/journal" "$DIR/sim"

rpc() { curl -s -X POST -H 'content-type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}" "$1"; }

# ---- the T side: a scratch Ethereum fork + the mainnet engine --------------
"$ANVIL" --port "$SRC_T_PORT" --chain-id 1 --block-time 1 --silent --hardfork amsterdam > "$DIR/src-t.log" 2>&1 &
PIDS="$!"
sleep 1.5
PIN_T=$(rpc "http://127.0.0.1:$SRC_T_PORT" eth_blockNumber '[]' | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))')
"$ANVIL" --port "$T_PORT" --chain-id 1 --fork-url "http://127.0.0.1:$SRC_T_PORT" --fork-block-number "$PIN_T" --silent > "$DIR/t.log" 2>&1 &
PIDS="$PIDS $!"
sleep 1.5

cat > "$DIR/t.toml" <<EOF
[instance]
name = "daloc-local-t"
namespace = "daloc-local-t"
profile = "mainnet"

[chains.l1]
kind = "ethereum"
source_url = "http://127.0.0.1:$SRC_T_PORT"
follow = "latest"
fork_offset = 128

[[watchlist]]
address = "$A0"
note = "the scratch sender"

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
port = $T_ADMIN_PORT
EOF

target/debug/mainnet-engine run --config "$DIR/t.toml" > "$DIR/t-engine.log" 2>&1 &
PIDS="$PIDS $!"

# ---- the V side: a scratch HyperEVM fork + the venue engine ----------------
"$ANVIL" --port "$SRC_V_PORT" --chain-id 999 --silent > "$DIR/src-v.log" 2>&1 &
PIDS="$PIDS $!"
sleep 1.5
PIN_V=$(rpc "http://127.0.0.1:$SRC_V_PORT" eth_blockNumber '[]' | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))')
"$ANVIL" --port "$V_PORT" --chain-id 999 --fork-url "http://127.0.0.1:$SRC_V_PORT" --fork-block-number "$PIN_V" --silent > "$DIR/v.log" 2>&1 &
PIDS="$PIDS $!"
sleep 1.5

cat > "$DIR/v.toml" <<EOF
[instance]
name = "daloc-local-v"
namespace = "daloc-local-v"
profile = "venue"

[chains.venue]
kind = "hyperliquid"
source_url = "http://127.0.0.1:$SRC_V_PORT"
api_url = "https://api.hyperliquid.xyz"
ws_url = "wss://api.hyperliquid.xyz/ws"

[[watchlist]]
address = "$A0"
note = "the scratch sender"

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
port = $V_ADMIN_PORT

[services.venue_sim]
state_dir = "$DIR/sim"
markets = ["BTC", "ETH", "HYPE", "SOL"]
engine_admin_url = "http://127.0.0.1:$V_ADMIN_PORT"
anvil_url = "http://127.0.0.1:$V_PORT"

[services.venue_sim.info]
addr = "127.0.0.1"
port = $SIM_INFO_PORT

[services.venue_sim.control]
addr = "127.0.0.1"
port = $SIM_CTRL_PORT
EOF

target/debug/venue-engine run --config "$DIR/v.toml" > "$DIR/v-engine.log" 2>&1 &
PIDS="$PIDS $!"
target/debug/venue-sim run --config "$DIR/v.toml" > "$DIR/sim.log" 2>&1 &
PIDS="$PIDS $!"

# ---- wait for the stack ----------------------------------------------------
for _ in $(seq 1 120); do curl -sf http://127.0.0.1:$T_ADMIN_PORT/livez >/dev/null 2>&1 && break; sleep 1; done
curl -sf http://127.0.0.1:$T_ADMIN_PORT/livez >/dev/null && echo "T engine up" || { echo "T engine failed"; tail -20 "$DIR/t-engine.log"; exit 1; }
for _ in $(seq 1 120); do curl -sf http://127.0.0.1:$V_ADMIN_PORT/livez >/dev/null 2>&1 && break; sleep 1; done
curl -sf http://127.0.0.1:$V_ADMIN_PORT/livez >/dev/null && echo "V engine up" || { echo "V engine failed"; tail -20 "$DIR/v-engine.log"; exit 1; }
for _ in $(seq 1 120); do curl -sf "http://127.0.0.1:$SIM_INFO_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://127.0.0.1:$SIM_INFO_PORT/readyz" >/dev/null && echo "sim ready (live HL market data)" || { echo "sim not ready"; tail -30 "$DIR/sim.log"; exit 1; }

# ---- the daloc arc ---------------------------------------------------------
set +e
T_RPC="http://127.0.0.1:$T_PORT" T_ADMIN="http://127.0.0.1:$T_ADMIN_PORT" \
V_RPC="http://127.0.0.1:$V_PORT" V_ADMIN="http://127.0.0.1:$V_ADMIN_PORT" \
SIM_INFO="http://127.0.0.1:$SIM_INFO_PORT" SIM_CTRL="http://127.0.0.1:$SIM_CTRL_PORT" \
CAST=/tmp/opencode/foundry-bin/cast python3 scripts/daloc-sim.py --lane all 2>&1 | tee "$DIR/daloc.out"
rc=${PIPESTATUS[0]}
set -e
echo "daloc-local: exit=$rc (logs in $DIR)"
exit "$rc"
