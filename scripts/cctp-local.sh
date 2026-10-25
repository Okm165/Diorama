#!/usr/bin/env bash
# cctp-local: the cross-mirror lane, proven locally — the prime broker's
# margin cycle. The counterpart of the deployed-stack `cctp-*` suites, with
# no cluster: T is an anvil forked from a *public mainnet RPC* (the real CCTP
# contracts, the real USDC), V is an anvil forked from the *real HyperEVM*
# (the real CCTP V2 contracts), the two engines keep them live (the T
# engine's source is a local fork of the same pin — the workshop's
# arrangement; the V engine's source is a static fork of the same pin, so
# V's at-pin truth stays readable), the venue simulator serves the account
# plane from live Hyperliquid data, and the bridge binary runs against both
# chains — then the scripted suites run in direct mode:
#
#   cctp-roundtrip    - burn on T → the bridge attests → the mint lands on V,
#                       the destination delta exactly the amount
#   cctp-margin-cycle - the unified-margin cycle end to end: the deal mints
#                       USDC on T to the bridge's wallet, CCTP moves it to V,
#                       the venue collateral is credited, a real CoreWriter
#                       USD-class transfer moves spot → perp, a perp order
#                       fills against the live book, and the return leg lands
#                       back on T (the cycle closed)
#
# Requires: anvil (ANVIL=), cast (CAST=), the four binaries built
# (`make mainnet-build venue-build cctp-build`), python3, curl, outbound
# HTTPS to the public mainnet RPC (MAINNET_RPC, default https://eth.drpc.org)
# and to the HyperEVM (HL_RPC, default https://rpc.hyperliquid.xyz/evm).
# Ports: 8690-8693 + 18090-18098 (override with the *_PORT variables).
# KEEP_UP=1 leaves the stack up (PIDs in $DIR/pids). Never touches the cluster.
set -euo pipefail
cd "$(dirname "$0")/.."

ANVIL="${ANVIL:-anvil}"
CAST="${CAST:-cast}"
MAINNET_RPC="${MAINNET_RPC:-https://eth.drpc.org}"
HL_RPC="${HL_RPC:-https://rpc.hyperliquid.xyz/evm}"
SRC_T_PORT="${SRC_T_PORT:-8690}"
T_PORT="${T_PORT:-8691}"
T_ADMIN_PORT="${T_ADMIN_PORT:-18090}"
SRC_V_PORT="${SRC_V_PORT:-8692}"
V_PORT="${V_PORT:-8693}"
V_ADMIN_PORT="${V_ADMIN_PORT:-18091}"
SIM_INFO_PORT="${SIM_INFO_PORT:-18092}"
SIM_CTRL_PORT="${SIM_CTRL_PORT:-18093}"
BRIDGE_IRIS_PORT="${BRIDGE_IRIS_PORT:-18097}"
BRIDGE_CTRL_PORT="${BRIDGE_CTRL_PORT:-18098}"
DIR="${DIR:-/tmp/cctp-local}"
RELAY=0x70997970C51812dc3A010C7d01b50e0d17dc79C8          # the bridge's wallet
RELAY_KEY=0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d
TRADER=0x00000000000000000000000000000000000000e5
UNIFIED=0x00000000000000000000000000000000000000ea

T_URL="http://127.0.0.1:$T_PORT"
V_URL="http://127.0.0.1:$V_PORT"

for bin in target/debug/mainnet-engine target/debug/venue-engine target/debug/venue-sim target/debug/cctp-bridge; do
  [ -x "$bin" ] || { echo "$bin not built (make mainnet-build venue-build cctp-build)"; exit 2; }
done
command -v "$ANVIL" >/dev/null || { echo "anvil not found (set ANVIL=)"; exit 2; }

cleanup() {
  for pid in ${PIDS:-}; do kill "$pid" 2>/dev/null || true; done
}
if [ "${KEEP_UP:-0}" = 1 ]; then
  trap 'printf "%s\n" ${PIDS:-} > "$DIR/pids"' EXIT
else
  trap cleanup EXIT
fi
rm -rf "$DIR"; mkdir -p "$DIR/t/state/journal" "$DIR/v/state/journal" "$DIR/sim" "$DIR/bridge"

head_of() { # head_of <url> — the public RPC's head, with retries
  local out
  for _ in $(seq 1 6); do
    out=$(curl -s -m 30 -X POST -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' "$1" \
      | python3 -c 'import json,sys
try:
    print(int(json.load(sys.stdin)["result"], 16))
except Exception:
    sys.exit(1)' 2>/dev/null || true)
    [ -n "$out" ] && { printf '%s' "$out"; return 0; }
    sleep 3
  done
  return 1
}
wait_up() { # wait_up <url> <seconds>
  for _ in $(seq 1 "$2"); do
    curl -sf -m 2 -X POST -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' "$1" >/dev/null 2>&1 \
      && return 0
    sleep 1
  done
  return 1
}
fork_anvil() { # fork_anvil <port> <chain-id> <rpc> <pin> <extra flags...> — retried
  local port="$1" chain="$2" rpc="$3" pin="$4"; shift 4
  local attempt up
  for attempt in 1 2 3; do
    up=false
    for _ in $(seq 1 30); do
      curl -sf -m 2 -X POST -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        "http://127.0.0.1:$port" >/dev/null 2>&1 && { up=true; break; }
      sleep 1
    done
    [ "$up" = true ] && return 0
    echo "  anvil :$port did not answer (attempt $attempt); re-forking"
    "$ANVIL" --port "$port" --chain-id "$chain" --fork-url "$rpc" --fork-block-number "$pin" \
      "$@" --silent >> "$DIR/anvil-$port.log" 2>&1 &
    PIDS="$PIDS $!"
    sleep 2
  done
  return 1
}

echo "== cctp-local: booting the cross-mirror stack"
echo "   mainnet: $MAINNET_RPC"
echo "   venue:   $HL_RPC"
HEAD_T=$(head_of "$MAINNET_RPC") || { echo "the mainnet RPC did not answer ($MAINNET_RPC)"; exit 1; }
PIN_T=$((HEAD_T - 8))
HEAD_V=$(head_of "$HL_RPC") || { echo "the HyperEVM RPC did not answer ($HL_RPC)"; exit 1; }
PIN_V=$((HEAD_V - 8))
echo "   pins: T $PIN_T, V $PIN_V"

# ---- the sources -----------------------------------------------------------
# T's source: a local fork of the same mainnet pin, advancing (empty blocks):
# the applier's amsterdam BALs come from here, and T's base matches by
# construction (the workshop's arrangement).
"$ANVIL" --port "$SRC_T_PORT" --chain-id 1 --fork-url "$MAINNET_RPC" --fork-block-number "$PIN_T" \
  --hardfork amsterdam --block-time 1 --silent > "$DIR/src-t.log" 2>&1 &
PIDS="$!"
fork_anvil "$SRC_T_PORT" 1 "$MAINNET_RPC" "$PIN_T" --hardfork amsterdam --block-time 1 \
  || { echo "the T source did not come up"; tail -5 "$DIR/src-t.log"; exit 1; }
echo "   T source up (fork of $PIN_T, advancing)"

# V's source: a static local fork of the same venue pin — V's at-pin truth
# stays readable (the venue source's pin guard holds while head == pin), and
# the at-pin probes compare V against the source at the pin.
"$ANVIL" --port "$SRC_V_PORT" --chain-id 999 --fork-url "$HL_RPC" --fork-block-number "$PIN_V" \
  --silent > "$DIR/src-v.log" 2>&1 &
PIDS="$PIDS $!"
fork_anvil "$SRC_V_PORT" 999 "$HL_RPC" "$PIN_V" \
  || { echo "the V source did not come up"; tail -5 "$DIR/src-v.log"; exit 1; }
echo "   V source up (fork of $PIN_V, static)"

# ---- T and V: forks of the real chains (the real contracts) ----------------
"$ANVIL" --port "$T_PORT" --chain-id 1 --fork-url "$MAINNET_RPC" --fork-block-number "$PIN_T" \
  --hardfork amsterdam --auto-impersonate --silent > "$DIR/t.log" 2>&1 &
PIDS="$PIDS $!"
fork_anvil "$T_PORT" 1 "$MAINNET_RPC" "$PIN_T" --hardfork amsterdam --auto-impersonate \
  || { echo "T (the mainnet fork) did not come up"; tail -5 "$DIR/t.log"; exit 1; }
echo "   T up (the real mainnet state at $PIN_T)"

"$ANVIL" --port "$V_PORT" --chain-id 999 --fork-url "$HL_RPC" --fork-block-number "$PIN_V" \
  --auto-impersonate --silent > "$DIR/v.log" 2>&1 &
PIDS="$PIDS $!"
fork_anvil "$V_PORT" 999 "$HL_RPC" "$PIN_V" --auto-impersonate \
  || { echo "V (the HyperEVM fork) did not come up"; tail -5 "$DIR/v.log"; exit 1; }
echo "   V up (the real HyperEVM state at $PIN_V)"

# The relay pays gas on both chains; the cycle's actors need gas on V.
rpc() { curl -s -m 60 -X POST -H 'content-type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}" "$1" >/dev/null; }
for chain in "$T_URL" "$V_URL"; do
  for who in "$RELAY" "$TRADER" "$UNIFIED" 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266; do
    rpc "$chain" anvil_setBalance "[\"$who\",\"0x21e19e0c9bab2400000\"]"
  done
done

# ---- the T engine (source: the local fork; target: the mainnet fork) -------
cat > "$DIR/t.toml" <<EOF
[instance]
name = "cctp-local-t"
namespace = "cctp-local-t"
profile = "mainnet"

[chains.l1]
kind = "ethereum"
source_url = "http://127.0.0.1:$SRC_T_PORT"
follow = "latest"
fork_offset = 128

[[watchlist]]
address = "$RELAY"
note = "the bridge's wallet"
[[watchlist]]
address = "$TRADER"
note = "the V-side trader"

[services.engine]
state_dir = "$DIR/t/state"
fixture_addresses = []

[services.engine.anvil]
target_url = "$T_URL"

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

# ---- the V engine + the simulator (source: the static venue fork) ----------
cat > "$DIR/v.toml" <<EOF
[instance]
name = "cctp-local-v"
namespace = "cctp-local-v"
profile = "venue"

[chains.venue]
kind = "hyperliquid"
source_url = "http://127.0.0.1:$SRC_V_PORT"
api_url = "https://api.hyperliquid.xyz"
ws_url = "wss://api.hyperliquid.xyz/ws"

[[watchlist]]
address = "$RELAY"
note = "the bridge's wallet"
[[watchlist]]
address = "$TRADER"
note = "the V-side trader"

[services.engine]
state_dir = "$DIR/v/state"

[services.engine.anvil]
target_url = "$V_URL"

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
anvil_url = "$V_URL"

[services.venue_sim.info]
addr = "127.0.0.1"
port = $SIM_INFO_PORT

[services.venue_sim.control]
addr = "127.0.0.1"
port = $SIM_CTRL_PORT

# The bridge: both chains are the forks above (the real CCTP V2 contracts,
# the real USDC); the scan pins keep the boot scans short.
[services.cctp_bridge]
state_dir = "$DIR/bridge"
t_url = "$T_URL"
v_url = "$V_URL"
relay_key = "$RELAY_KEY"
attester_seed = "mirror-cctp-devnet-attester"
poll_interval_ms = 200
finality_delay_secs = 0
expiration_blocks = 1000
scan_chunk_blocks = 2000
receipt_timeout_secs = 60
relayer = "auto"
balance_low_wei = "100000000000000000"
balance_critical_wei = "10000000000000000"
t_start_block = $PIN_T
v_start_block = $PIN_V

[services.cctp_bridge.iris]
addr = "127.0.0.1"
port = $BRIDGE_IRIS_PORT

[services.cctp_bridge.control]
addr = "127.0.0.1"
port = $BRIDGE_CTRL_PORT
EOF
target/debug/venue-engine run --config "$DIR/v.toml" > "$DIR/v-engine.log" 2>&1 &
PIDS="$PIDS $!"
target/debug/venue-sim run --config "$DIR/v.toml" > "$DIR/sim.log" 2>&1 &
PIDS="$PIDS $!"
target/debug/cctp-bridge run --config "$DIR/v.toml" > "$DIR/bridge.log" 2>&1 &
PIDS="$PIDS $!"

for _ in $(seq 1 120); do curl -sf "http://127.0.0.1:$T_ADMIN_PORT/livez" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://127.0.0.1:$T_ADMIN_PORT/livez" >/dev/null \
  || { echo "the T engine failed"; tail -20 "$DIR/t-engine.log"; exit 1; }
echo "T engine up (the boot asserts, audits and anchors at $PIN_T)"
for _ in $(seq 1 120); do curl -sf "http://127.0.0.1:$V_ADMIN_PORT/livez" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://127.0.0.1:$V_ADMIN_PORT/livez" >/dev/null \
  || { echo "the V engine failed"; tail -20 "$DIR/v-engine.log"; exit 1; }
echo "V engine up (the boot asserts, audits and anchors at $PIN_V)"
for _ in $(seq 1 180); do curl -sf "http://127.0.0.1:$SIM_INFO_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://127.0.0.1:$SIM_INFO_PORT/readyz" >/dev/null \
  || { echo "the sim not ready"; tail -20 "$DIR/sim.log"; exit 1; }
echo "sim ready (live Hyperliquid market data)"
for _ in $(seq 1 120); do curl -sf "http://127.0.0.1:$BRIDGE_CTRL_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://127.0.0.1:$BRIDGE_CTRL_PORT/health" >/dev/null \
  || { echo "the bridge not ready"; tail -20 "$DIR/bridge.log"; exit 1; }
echo "bridge ready (both chains asserted, the attester installed)"

# ---- the suites (direct mode against this stack) ---------------------------
rc=0
echo "== cctp-local: the round trip"
BRIDGE_CTRL_URL="http://127.0.0.1:$BRIDGE_CTRL_PORT" \
BRIDGE_IRIS_URL="http://127.0.0.1:$BRIDGE_IRIS_PORT" \
ENGINE_T_URL="http://127.0.0.1:$T_ADMIN_PORT" ENGINE_V_URL="http://127.0.0.1:$V_ADMIN_PORT" \
T_URL="$T_URL" V_URL="$V_URL" \
CAST="$CAST" ./scripts/cctp-roundtrip.sh || rc=1

if [ "$rc" -eq 0 ]; then
  echo "== cctp-local: the unified-margin cycle"
  BRIDGE_CTRL_URL="http://127.0.0.1:$BRIDGE_CTRL_PORT" \
  BRIDGE_IRIS_URL="http://127.0.0.1:$BRIDGE_IRIS_PORT" \
  ENGINE_T_URL="http://127.0.0.1:$T_ADMIN_PORT" ENGINE_V_URL="http://127.0.0.1:$V_ADMIN_PORT" \
  T_URL="$T_URL" V_URL="$V_URL" \
  SIM_INFO="http://127.0.0.1:$SIM_INFO_PORT" SIM_CTRL="http://127.0.0.1:$SIM_CTRL_PORT" \
  CAST="$CAST" ./scripts/cctp-margin-cycle.sh || rc=1
fi

if [ "${KEEP_UP:-0}" = 1 ]; then
  echo "cctp-local: stack kept up (PIDs in $DIR/pids)"
fi
echo "cctp-local: exit=$rc (logs in $DIR)"
exit "$rc"
