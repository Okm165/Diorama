#!/usr/bin/env bash
# venue-engine-local: the venue engine UX, proven natively (no cluster, no
# Docker): a local anvil stands in for the HL public API, V forks it, and the
# engine must (1) mirror the watched account 1:1 as the source moves, (2) keep
# a devnet write on V when the next source window lands (the COW filter), and
# (3) serve the shared admin surface — the mainnet counterpart is
# `mainnet-engine-local.sh`.
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
DIR="${DIR:-/tmp/venue-engine-local}"
SRC="http://127.0.0.1:$SRC_PORT"
V="http://127.0.0.1:$V_PORT"
HTTP="http://127.0.0.1:$HTTP_PORT"
WATCH=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 # anvil account 0 (watched)
OTHER=0x70997970C51812dc3A010C7d01b50e0d17dc79C8 # anvil account 1 (devnet tx)
TOKEN=0x00000000000000000000000000000000000000e7    # the mock ERC-20 (see below)

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
metric() { curl -s "$HTTP/metrics" | grep "^$1 " | awk '{print $2}'; }
bal() { rpc "$1" eth_getBalance "[\"$WATCH\",\"latest\"]" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))'; }
nonce() { rpc "$1" eth_getTransactionCount "[\"$WATCH\",\"latest\"]" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))'; }
send() { rpc "$1" eth_sendTransaction "[{\"from\":\"$2\",\"to\":\"$3\",\"value\":\"$4\"}]" >/dev/null; }

"$ANVIL" --port "$SRC_PORT" --chain-id 998 --block-time 1 --silent > "$DIR/source.log" 2>&1 &
SRC_PID=$!
sleep 1.5
# A mock ERC-20 on the source (so V's fork carries it): balanceOf(address) is
# SLOAD(keccak256(addr ‖ slot 0)) — the standard mapping layout the deal's
# slot discovery probes for (the real HyperEVM USDC plays this role on the
# cluster; a local source has no tokens).
rpc "$SRC" anvil_setCode "[\"$TOKEN\",\"0x600435600052600060205260406000205460005260206000f3\"]" >/dev/null
sleep 0.3
PIN=$(rpc "$SRC" eth_blockNumber '[]' | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))')

"$ANVIL" --port "$V_PORT" --chain-id 999 --fork-url "$SRC" --fork-block-number "$PIN" --silent > "$DIR/v.log" 2>&1 &
V_PID=$!
sleep 2

cat > "$DIR/instance.toml" <<EOF
[instance]
name = "venue-engine-local"
namespace = "venue-engine-local"
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
skipped=$(metric venue_follow_skipped_shadowed_total || true)
if [ "${skipped:-0}" -ge 2 ]; then
  ok "COW: skipped_shadowed_total=$skipped (balance + nonce)"
else
  bad "COW: skipped_shadowed_total=$skipped (expected >= 2)"
fi
applied=$(metric venue_follow_applied_total || true)
if [ "${applied:-0}" -ge 2 ]; then
  ok "follow applied_total=$applied (the pre-COW window's fields)"
else
  bad "follow applied_total=$applied (expected >= 2)"
fi
# The follow's measurement: every applied key must read back the composed
# value (the live parity). The pair was silently inverted once — a clean
# window reported as a failure — so the proof asserts it here.
sampled=$(metric venue_follow_sampled_total || true)
sampled_ok=$(metric venue_follow_sampled_ok_total || true)
if [ "${sampled_ok:-0}" = "${sampled:-0}" ] && [ "${sampled:-0}" -ge 1 ]; then
  ok "live parity: $sampled_ok/$sampled applied keys read back the composed value"
else
  bad "live parity: $sampled_ok/$sampled (the follow's measurement)"
fi

# Stage 3: the forensic trail. A log from a contract no declared closure
# covers cannot be followed — it must land in the shared drift log
# (`drift.jsonl`, the venue's counterpart of the applier's anomaly lines),
# beside the counter.
EMITTER=0x00000000000000000000000000000000000000e9
rpc "$SRC" anvil_setCode "[\"$EMITTER\",\"0x60006000a0\"]" >/dev/null # PUSH1 0 PUSH1 0 LOG0
send "$SRC" "$WATCH" "$EMITTER" 0
sleep 5
if grep -q '"uncovered_logs"' "$DIR/state/drift.jsonl" 2>/dev/null; then
  ok "drift.jsonl: the uncovered emitter is on the venue's forensic trail"
else
  bad "drift.jsonl missing the uncovered_logs line (see $DIR/state/drift.jsonl)"
fi

# ---- the shared admin surface (the venue side of the engine UX) ------------
# The exact verbs `make engine-symmetry` checks cluster-side, driven here
# against the one engine: /status, pause/resume effects, the shadow
# hand-back and its designed refusal, probes, deal, events, the rewind
# refusal.
TOKEN=0x00000000000000000000000000000000000000e7 # the mock ERC-20 (deployed on the source)

st=$(curl -s "$HTTP/status")
for k in head cursor halted shadowed shadowed_slots shadowed_accounts capabilities fork_pin epoch_pin probes source; do
  printf '%s' "$st" | python3 -c "import json,sys; sys.exit(0 if '$k' in json.load(sys.stdin) else 1)"     && ok "/status has $k" || bad "/status missing $k"
done

curl -s -X POST "$HTTP/admin/pause" -H 'content-type: application/json' -d '{}' >/dev/null
h=$(curl -s "$HTTP/status" | python3 -c 'import json,sys; print(json.load(sys.stdin)["halted"])')
hz=$(curl -s -o /dev/null -w '%{http_code}' "$HTTP/healthz")
g=$(metric mirror_halted || true)
if [ "$h" = "True" ] && [ "$hz" = "503" ] && [ "$g" = "1" ]; then
  ok "pause holds (halted, /healthz 503, mirror_halted 1)"
else
  bad "pause: halted=$h /healthz=$hz gauge=$g"
fi
curl -s -X POST "$HTTP/admin/resume" -H 'content-type: application/json' -d '{}' >/dev/null
h=$(curl -s "$HTTP/status" | python3 -c 'import json,sys; print(json.load(sys.stdin)["halted"])')
[ "$h" = "False" ] && ok "resume releases" || bad "resume: halted=$h"

# The bare clear is the designed refusal (a clear with nothing to hand back
# would look like success).
sc=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$HTTP/admin/shadow"   -H 'content-type: application/json' -d '{"address":"'$WATCH'","clear":true}')
[ "$sc" = "400" ] && ok "/admin/shadow bare clear: 400 (the designed refusal)" || bad "/admin/shadow bare clear: $sc"
# A selective hand-back while the source has advanced past the pin is the
# documented latest-only refusal (divergence #1): fail-closed, never a
# silently newer value.
hbresp=$(curl -s -w '\n%{http_code}' -X POST "$HTTP/admin/shadow" -H 'content-type: application/json'   -d '{"address":"'$WATCH'","account_fields":true,"clear":true}')
if printf '%s' "$hbresp" | grep -q "past the pin"; then
  ok "/admin/shadow: the latest-only refusal names the pin (source advanced)"
else
  bad "/admin/shadow: $(printf '%s' "$hbresp" | head -1)"
fi

po=$(curl -s -X POST "$HTTP/admin/probes" -H 'content-type: application/json' -d '{}')
printf '%s' "$po" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if all(k in d for k in ("ok","failed","skipped")) else 1)' \
  && ok "/admin/probes reports {ok, failed, skipped}" || bad "/admin/probes: $po"

timeout 5 curl -s -N "$HTTP/events" > "$DIR/events.out" 2>/dev/null &
for _ in $(seq 1 20); do grep -q ": connected" "$DIR/events.out" 2>/dev/null && break; sleep 0.1; done
deal=$(curl -s -X POST "$HTTP/admin/deal" -H 'content-type: application/json' \
  -d '{"token":"'$TOKEN'","to":"'$OTHER'","amount":"100000000"}')
printf '%s' "$deal" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("ok") and d.get("slot") else 1)' \
  && ok "/admin/deal: minted on V (key $(printf '%s' "$deal" | python3 -c 'import json,sys; print(json.load(sys.stdin)["slot"])'))" \
  || bad "/admin/deal: $deal"
db=$(rpc "$V" eth_call "[{\"to\":\"$TOKEN\",\"data\":\"0x70a08231000000000000000000000000$(echo "$OTHER" | cut -c3-)\"},\"latest\"]" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))')
[ "$db" = "100000000" ] && ok "mint: the recipient's token balance on V is 100000000" || bad "mint: token balance $db"
sleep 3
grep -q "deal_applied" "$DIR/events.out" && ok "/events carries deal_applied" || bad "/events missing deal_applied"
grep -q "block_applied" "$DIR/events.out" && ok "/events carries block_applied" || bad "/events missing block_applied"

rc=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$HTTP/admin/rewind" -H 'content-type: application/json' -d '{"blocks":9999999}')
[ "$rc" = "400" ] && ok "/admin/rewind beyond ring: 400 (the named refusal)" || bad "/admin/rewind beyond ring: $rc"

# A token without the standard layout writes nothing, so it must publish
# nothing: the fact is a state change, never a request receipt.
timeout 4 curl -s -N "$HTTP/events" > "$DIR/events2.out" 2>/dev/null &
for _ in $(seq 1 20); do grep -q ": connected" "$DIR/events2.out" 2>/dev/null && break; sleep 0.1; done
bad_deal=$(curl -s -X POST "$HTTP/admin/deal" -H 'content-type: application/json' \
  -d '{"token":"0x00000000000000000000000000000000000000e8","to":"'$OTHER'","amount":"1"}')
printf '%s' "$bad_deal" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("ok") is False else 1)' \
  && ok "/admin/deal: a token without the layout answers ok:false" || bad "/admin/deal bad token: $bad_deal"
sleep 3
grep -q "deal_applied" "$DIR/events2.out" \
  && bad "the no-slot deal published deal_applied (it wrote nothing)" \
  || ok "the no-slot deal publishes no deal_applied (the fact is a state change)"

# ---- the epoch rule (the mainnet script's section, V's own facts) ----------
# A one-call re-pin moves V's base: the driver detects the reset and the
# epoch rule runs — the events carry both facts, the counter moves.
timeout 20 curl -s -N "$HTTP/events" > "$DIR/events3.out" 2>/dev/null &
for _ in $(seq 1 20); do grep -q ": connected" "$DIR/events3.out" 2>/dev/null && break; sleep 0.1; done
ep=$(curl -s -X POST "$HTTP/admin/epoch")
printf '%s' "$ep" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("reset") is True and d.get("source") else 1)' \
  && ok "/admin/epoch: re-pinned ($ep)" || bad "/admin/epoch: $ep"
for _ in $(seq 1 30); do
  grep -q "epoch_changed" "$DIR/events3.out" 2>/dev/null && break
  sleep 1
done
grep -q "reset_detected" "$DIR/events3.out" && ok "/events carries reset_detected after the re-pin" \
  || bad "/events missing reset_detected (see $DIR/events3.out)"
grep -q "epoch_changed" "$DIR/events3.out" && ok "/events carries epoch_changed (the epoch rule ran)" \
  || bad "/events missing epoch_changed"
e1=$(metric venue_epochs_total)
[ "${e1:-0}" -ge 1 ] && ok "venue_epochs_total moved ($e1)" || bad "venue_epochs_total did not move ($e1)"

echo "venue-engine-local: $pass ok, $fail bad (logs in $DIR)"
[ "$fail" -eq 0 ]
