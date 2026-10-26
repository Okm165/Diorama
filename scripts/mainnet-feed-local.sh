#!/usr/bin/env bash
# mainnet-feed-local: the feed bridge, proven natively (two anvils; no reth,
# no public RPC, no cluster) — the local counterpart of the deployed stack's
# bridge, which `mainnet-feed-bridge` drives on the cluster against the real
# Chainlink feeds.
#
# The source anvil (chain 1, `--hardfork amsterdam`, mining every second)
# carries hand-assembled Chainlink-shaped stubs at the feeds' *source*
# addresses, so the bridge reads live answers from its parent; T forks the
# source at a pin, and the engine's applier mines one T block per applied
# source block, so the bridge's own `updateAnswer` txs land. Then:
#
#   A. the mock aggregators (the stub stands in for the fixture deploy)
#   B. the bridge publishes the parent's answers (decimals matched at boot)
#   C. the scenario discipline: a write to a target pauses the feed, its value
#      is preserved (the bridge never overwrites it), and the hand-back
#      republishes the parent's value
#
# Requires: anvil (ANVIL=/path/to/anvil), the engine binary built
# (`make mainnet-build`), cast (CAST=/path/to/cast), python3, curl.
# Ports: 8695/8696 + 18096. Never touches the cluster.
set -euo pipefail
cd "$(dirname "$0")/.."

ANVIL="${ANVIL:-anvil}"
ENGINE="${ENGINE:-target/debug/mainnet-engine}"
CAST="${CAST:-cast}"
SRC_PORT="${SRC_PORT:-8696}"
T_PORT="${T_PORT:-8695}"
HTTP_PORT="${HTTP_PORT:-18096}"
DIR="${DIR:-/tmp/mainnet-feed-local}"
ANVIL_T_URL="http://127.0.0.1:$T_PORT"
SRC_URL="http://127.0.0.1:$SRC_PORT"
ENGINE_T_URL="http://127.0.0.1:$HTTP_PORT"
# The bridge's writer (a dedicated account: sharing one with a concurrent
# sender races nonces) and the scenario's writer.
WRITER=0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC # anvil account 2
SCENARIO_KEY=0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d # account 1
# The parent's aggregators (stubs on the source) and T's mocks (stubs here).
SOURCE1=0x0000000000000000000000000000000000005e01
SOURCE2=0x0000000000000000000000000000000000005e02
TARGET1=0x000000000000000000000000000000000000fe01
TARGET2=0x000000000000000000000000000000000000fe02

# The aggregator stub: decimals() -> 8, latestRoundData() -> (1, slot0, now,
# now, 1), updateAnswer(int256) -> slot0 = the word. Hand-assembled (no solc
# in the image); the interface is Chainlink's — the same one the mocks speak.
STUB="0x60003560e01c8063313ce56714610028578063feaf968c14610033578063a87a20ce1461005157005b600860005260206000f35b60016000526000546020524260405242606052600160805260a06000f35b60043560005500"

source "$(dirname "${BASH_SOURCE[0]}")/lib-common.sh"

command -v "$ANVIL" >/dev/null || { echo "anvil not found (set ANVIL=)"; exit 2; }
[ -x "$ENGINE" ] || { echo "$ENGINE not built (make mainnet-build)"; exit 2; }

cleanup() {
  [ -n "${ENG_PID:-}" ] && kill "$ENG_PID" 2>/dev/null || true
  [ -n "${SRC_PID:-}" ] && kill "$SRC_PID" 2>/dev/null || true
  [ -n "${T_PID:-}" ] && kill "$T_PID" 2>/dev/null || true
}
trap cleanup EXIT
rm -rf "$DIR"; mkdir -p "$DIR/state/journal"


field() { # field <feed-name> <key>  (reads /admin/feed JSON on stdin)
  python3 -c "
import json, sys
name, key = sys.argv[1], sys.argv[2]
for f in json.load(sys.stdin):
    if f['name'] == name:
        v = f.get(key)
        print('' if v is None else v)
        break
" "$1" "$2"
}

answer() { # answer <target>: the aggregator's current answer (raw)
  $CAST call "$1" "latestRoundData()(uint80,int256,uint256,uint256,uint80)" \
    --rpc-url "$ANVIL_T_URL" 2>/dev/null | sed -n 2p | awk '{print $1}'
}

# ---- the scratch stack ----------------------------------------------------
echo "== local feed-bridge proof (source $SRC_URL, T $ANVIL_T_URL)"

"$ANVIL" --port "$SRC_PORT" --chain-id 1 --block-time 1 --silent --hardfork amsterdam \
  > "$DIR/source.log" 2>&1 &
SRC_PID=$!
sleep 1.5
rpc "$SRC_URL" anvil_setCode "[\"$SOURCE1\",\"$STUB\"]" >/dev/null
rpc "$SRC_URL" anvil_setCode "[\"$SOURCE2\",\"$STUB\"]" >/dev/null
# The parent's live answers (each tx in its own block: the bridge polls the
# follow head).
$CAST send "$SOURCE1" "updateAnswer(int256)" 300000000000 \
  --rpc-url "$SRC_URL" --private-key "$SCENARIO_KEY" >/dev/null 2>&1
$CAST send "$SOURCE2" "updateAnswer(int256)" 200000000000 \
  --rpc-url "$SRC_URL" --private-key "$SCENARIO_KEY" >/dev/null 2>&1
sleep 1.5

PIN=$(rpc "$SRC_URL" eth_blockNumber '[]' \
  | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"], 16))')
PIN=$((PIN - 1))
"$ANVIL" --port "$T_PORT" --chain-id 1 --fork-url "$SRC_URL" --fork-block-number "$PIN" \
  --no-mining --auto-impersonate --silent > "$DIR/t.log" 2>&1 &
T_PID=$!
sleep 2
# The bridge's writer pays its own gas (auto-impersonate); the scenario writer
# mines through the applier.
rpc "$ANVIL_T_URL" anvil_setBalance "[\"$WRITER\",\"0x21e19e0c9bab2400000\"]" >/dev/null
rpc "$ANVIL_T_URL" anvil_setBalance "[\"0x70997970C51812dc3A010C7d01b50e0d17dc79C8\",\"0x21e19e0c9bab2400000\"]" >/dev/null

cat > "$DIR/instance.toml" <<EOF
[instance]
name = "mainnet-feed-local"
namespace = "mainnet-feed-local"
profile = "mainnet"

[chains.l1]
kind = "ethereum"
source_url = "$SRC_URL"
follow = "latest"
fork_offset = 128

[[watchlist]]
address = "$TARGET1"
note = "the mock aggregator"
[[watchlist]]
address = "$TARGET2"
note = "the mock aggregator"
[[watchlist]]
address = "$WRITER"
note = "the bridge's writer"

[services.engine]
state_dir = "$DIR/state"
fixture_addresses = []

[services.engine.anvil]
target_url = "$ANVIL_T_URL"

[services.engine.journal]
dir = "$DIR/state/journal"

[services.engine.verifier]
probes = []

[services.engine.feed_bridge]
poll_interval_ms = 200
confirm_window_ms = 3000
writer = "$WRITER"
feeds = [
  { name = "ETH/USD", target = "$TARGET1", source = "$SOURCE1" },
  { name = "BTC/USD", target = "$TARGET2", source = "$SOURCE2" },
]

[services.engine.http]
addr = "127.0.0.1"
port = $HTTP_PORT
EOF

"$ENGINE" run --config "$DIR/instance.toml" > "$DIR/engine.log" 2>&1 &
ENG_PID=$!
for _ in $(seq 1 60); do curl -sf "$ENGINE_T_URL/livez" >/dev/null 2>&1 && break; sleep 0.5; done
curl -sf "$ENGINE_T_URL/livez" >/dev/null || {
  echo "the engine did not come up"; tail -20 "$DIR/engine.log"; exit 1;
}

feeds=$(curl -sf -m 10 "$ENGINE_T_URL/admin/feed") || {
  echo "no /admin/feed — is the engine up with [services.engine.feed_bridge] configured?"
  exit 1
}
count=$(echo "$feeds" | python3 -c "import json,sys; print(len(json.load(sys.stdin)))")
[ "$count" -gt 0 ] || { echo "the feed bridge has no feeds configured"; exit 1; }

echo "-- A. the mock aggregators (the stub stands in for the fixture deploy)"
installed=0
for target in $(echo "$feeds" | python3 -c "import json,sys; print(' '.join(f['target'] for f in json.load(sys.stdin)))"); do
  code=$($CAST code "$target" --rpc-url "$ANVIL_T_URL" 2>/dev/null)
  if [ -z "$code" ] || [ "$code" = "0x" ]; then
    $CAST rpc --rpc-url "$ANVIL_T_URL" anvil_setCode "$target" "$STUB" > /dev/null \
      && installed=$((installed + 1))
  fi
done
[ "$installed" -gt 0 ] \
  && ok "the aggregator stub installed at $installed target(s) ($count feeds configured)" \
  || echo "  (stubs already installed)"

echo "-- B. the bridge publishes the parent's answers"
# A publish's tx mines on the next applied parent block (T follows the
# source), so allow a full block plus the confirm window.
missing=""
for _ in $(seq 1 30); do
  feeds=$(curl -sf -m 10 "$ENGINE_T_URL/admin/feed")
  missing=$(echo "$feeds" | python3 -c "
import json, sys
print(' '.join(f['name'] for f in json.load(sys.stdin) if not f.get('confirmed_answer')))")
  [ -z "$missing" ] && break
  sleep 2
done
[ -z "$missing" ] \
  && ok "every feed holds a live parent-chain answer (decimals matched at boot)" \
  || bad "feeds without a value: $missing"
got=$(curl -sf -m 10 "$ENGINE_T_URL/metrics" | awk '/^feed_bridge_updates_total/{s+=$2} END{print s+0}')
[ "$got" -ge "$count" ] \
  && ok "every feed published ($got answers for $count feeds)" \
  || bad "updates=$got < $count"

echo "-- C. the scenario discipline (a write pauses the feed; hand-back resumes)"
first=$(echo "$feeds" | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['name'])")
ftarget=$(echo "$feeds" | field "$first" target)
confirmed=$(answer "$ftarget")
[ -n "$confirmed" ] && [ "$confirmed" != "0" ] \
  || { echo "no live value on $ftarget to diverge from"; exit 1; }
sentinel=$((confirmed + 1))
$CAST send "$ftarget" "updateAnswer(int256)" "$sentinel" \
  --rpc-url "$ANVIL_T_URL" --private-key "$SCENARIO_KEY" > /dev/null 2>&1
owner=""
for _ in $(seq 1 30); do
  owner=$(curl -sf -m 10 "$ENGINE_T_URL/admin/feed" | field "$first" owner)
  [ "$owner" = "scenario" ] && break
  sleep 1
done
[ "$owner" = "scenario" ] \
  && ok "the scenario's write paused the feed ($first)" \
  || bad "owner=$owner (the bridge kept writing over the scenario)"
held=$(answer "$ftarget")
[ "$held" = "$sentinel" ] \
  && ok "the scenario's value is preserved (the bridge did not overwrite)" \
  || bad "target holds $held, the scenario wrote $sentinel"
pauses=$(curl -sf -m 10 "$ENGINE_T_URL/metrics" \
  | awk -v f="$first" '$0 ~ "^feed_bridge_pauses_total\\{feed=\"" f "\"" {print $2}')
[ "${pauses:-0}" -ge 1 ] && ok "the pause counter moved ($first)" || bad "pauses=$pauses"
curl -sf -m 10 -X POST "$ENGINE_T_URL/admin/feed/resume" \
  -H 'Content-Type: application/json' -d "{\"name\":\"$first\"}" > /dev/null
restored=""
for _ in $(seq 1 30); do
  now=$(answer "$ftarget")
  if [ -n "$now" ] && [ "$now" != "$sentinel" ] && [ "$now" != "0" ]; then
    restored=$now
    break
  fi
  sleep 1
done
[ -n "$restored" ] \
  && ok "the hand-back restored the live value ($first: $restored)" \
  || bad "the target still holds the scenario's $sentinel"

echo "== local feed-bridge proof: $pass passed, $fail failed"
[ "$fail" = "0" ]
