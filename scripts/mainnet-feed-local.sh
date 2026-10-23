#!/usr/bin/env bash
# Local feed-bridge proof (scratch T, plan §7.1): the live parent-chain
# Chainlink answers reach the devnet's mock aggregators, and the scenario
# discipline holds — a scenario's write pauses the feed (its value preserved,
# never overwritten), and the hand-back resumes publishing.
#
# On the cluster the mocks are daloc's fixture deploy's; on a bare scratch T
# the targets hold no code and the bridge waits ("the bridge waits for the
# devnet's fixture deploy"). This proof installs a minimal aggregator stub at
# each configured target — the same Chainlink interface their mocks speak —
# so the lane is exercisable locally, then watches the bridge.
#
# Usage:
#   make mainnet-feed-local
#   ANVIL_T_URL=http://127.0.0.1:18602 ENGINE_T_URL=http://127.0.0.1:18680 \
#     CAST=/tmp/opencode/foundry-bin/cast ./scripts/mainnet-feed-local.sh
set -uo pipefail

ANVIL_T_URL="${ANVIL_T_URL:-http://127.0.0.1:18602}"
ENGINE_T_URL="${ENGINE_T_URL:-http://127.0.0.1:18680}"
CAST="${CAST:-cast}"
# The scenario writer (anvil account 1; the bridge writes from anvil 0 —
# divergence is value-based, so any other account models the scenario).
SCENARIO_KEY="${SCENARIO_KEY:-0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d}"

# The aggregator stub: decimals() -> 8, latestRoundData() -> (1, slot0, now,
# now, 1), updateAnswer(int256) -> slot0 = the word. Hand-assembled (no solc
# in the image); the interface is Chainlink's — the same one the mocks speak.
STUB="0x60003560e01c8063313ce56714610028578063feaf968c14610033578063a87a20ce1461005157005b600860005260206000f35b60016000526000546020524260405242606052600160805260a06000f35b60043560005500"

pass=0
fail=0
ok()  { echo "  ✔ $*"; pass=$((pass + 1)); }
bad() { echo "  ✘ $*"; fail=$((fail + 1)); }

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

echo "== local feed-bridge proof (T at $ANVIL_T_URL)"
feeds=$(curl -sf -m 10 "$ENGINE_T_URL/admin/feed") || {
  echo "no /admin/feed — is the T engine up with [services.engine.feed_bridge] configured?"
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

echo "-- B. the bridge publishes the live parent-chain answers"
# A publish's tx mines on the next applied parent block (T follows mainnet),
# so allow a full block plus the confirm window.
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
paused=""
for _ in $(seq 1 30); do
  paused=$(curl -sf -m 10 "$ENGINE_T_URL/admin/feed" | field "$first" paused)
  [ "$paused" = "True" ] && break
  sleep 1
done
[ "$paused" = "True" ] \
  && ok "the scenario's write paused the feed ($first)" \
  || bad "paused=$paused (the bridge kept writing over the scenario)"
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
