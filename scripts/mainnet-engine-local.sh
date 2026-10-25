#!/usr/bin/env bash
# mainnet-engine-local: the Mainnet Mirror's engine UX, proven natively (two
# anvils; no reth, no public RPC, no cluster) — the mainnet counterpart of
# `venue-engine-local.sh`.
#
# A source anvil (chain 1, `--hardfork amsterdam`, so its blocks carry real
# EIP-7928 BALs with header commitments) stands in for the parent; T forks it
# at a pin; the *engine binary* runs with its HTTP surface. Every verb of the
# shared mirror UX is driven against it:
#   mirror  — T follows the source 1:1 (the applier reaches the tx blocks;
#             the watched balance matches the source's)
#   modify  — a devnet tx on T is COW-marked and survives the source's next
#             diffs (T keeps the devnet value; the account lists in /status)
#   mint    — `/admin/deal` discovers the mock ERC-20's balance-mapping key
#             and writes it (the standard keccak(addr ‖ slot) layout)
#   measure — the configured probe converges (T vs the source at the cursor);
#             /metrics carries the per-block convergence counters
# plus the shared admin surface: /status (the shared + profile fields),
# /events (block_applied, deal_applied, reset_detected, epoch_changed),
# /admin/time (the persistent offset), /admin/shadow (the hand-back re-syncs
# the account from the source), /admin/rewind (the journal), /admin/epoch
# (the re-pin; the engine reconciles the epoch rule).
#
# Requires: anvil (ANVIL=/path/to/anvil), the engine binary built
# (`make mainnet-build`), python3, curl. Ports: 8698/8699 + 18094.
# Never touches the cluster.
set -euo pipefail
cd "$(dirname "$0")/.."

ANVIL="${ANVIL:-anvil}"
ENGINE="${ENGINE:-target/debug/mainnet-engine}"
SRC_PORT="${SRC_PORT:-8699}"
T_PORT="${T_PORT:-8698}"
HTTP_PORT="${HTTP_PORT:-18094}"
DIR="${DIR:-/tmp/mainnet-engine-local}"
SRC="http://127.0.0.1:$SRC_PORT"
T="http://127.0.0.1:$T_PORT"
HTTP="http://127.0.0.1:$HTTP_PORT"
A0=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 # anvil account 0 (the sender)
A1=0x70997970C51812dc3A010C7d01b50e0d17dc79C8 # anvil account 1 (the probe's subject)
A2=0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC # anvil account 2 (the deal's recipient)
CONTRACT=0x000000000000000000000000000000000000c0de # SSTORE(0,1) per call
TOKEN=0x00000000000000000000000000000000000000e7    # the mock ERC-20

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

send() { rpc "$1" eth_sendTransaction "[{\"from\":\"$2\",\"to\":\"$3\",\"value\":\"$4\"}]"; }
block_of() { rpc "$SRC" eth_getTransactionReceipt "[\"$1\"]" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"]["blockNumber"],16))'; }
bal() { rpc "$1" eth_getBalance "[\"$2\",\"latest\"]" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))'; }
status() { curl -s "$HTTP/status"; }
metric() { curl -s "$HTTP/metrics" | grep "^$1 " | awk '{print $2}'; }

"$ANVIL" --port "$SRC_PORT" --chain-id 1 --block-time 1 --silent --hardfork amsterdam > "$DIR/source.log" 2>&1 &
SRC_PID=$!
sleep 1.5

# The storage-writing contract (gives the sampled parity a storage write to
# check) and the mock ERC-20 whose balanceOf(address) is
# SLOAD(keccak256(addr ‖ slot 0)) — the standard mapping layout the deal's
# slot discovery probes for.
rpc "$SRC" anvil_setCode "[\"$CONTRACT\",\"0x60016000556000\"]" >/dev/null
rpc "$SRC" anvil_setCode "[\"$TOKEN\",\"0x600435600052600060205260406000205460005260206000f3\"]" >/dev/null
sleep 0.3

# Four source txs, each in its own block (block-time 1).
TX1=$(send "$SRC" "$A0" "$A1" 0xde0b6b3a7640000 | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"])')
sleep 1.2
send "$SRC" "$A1" "$A2" 0x1bc16d674ec80000 >/dev/null
sleep 1.2
send "$SRC" "$A2" "$A0" 0x16345785d8a0000 >/dev/null
sleep 1.2
TX4=$(send "$SRC" "$A0" "$CONTRACT" 0x0 | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"])')
sleep 1.5

B1=$(block_of "$TX1"); B4=$(block_of "$TX4"); PIN=$((B1 - 1))
echo "source tx blocks: $B1..$B4; pin: $PIN"

"$ANVIL" --port "$T_PORT" --chain-id 1 --fork-url "$SRC" --fork-block-number "$PIN" --silent > "$DIR/t.log" 2>&1 &
T_PID=$!
sleep 2

PROBE_DATA="0x70a08231000000000000000000000000$(echo "$A1" | cut -c3-)"

cat > "$DIR/instance.toml" <<EOF
[instance]
name = "mainnet-engine-local"
namespace = "mainnet-engine-local"
profile = "mainnet"

[chains.l1]
kind = "ethereum"
source_url = "$SRC"
follow = "latest"
fork_offset = 128

[[watchlist]]
address = "$A0"
note = "the sender"
[[watchlist]]
address = "$A1"
note = "the probe's subject"
[[watchlist]]
address = "$A2"
note = "the deal's recipient"
[[watchlist]]
address = "$CONTRACT"
note = "the storage-writing contract"
[[watchlist]]
address = "$TOKEN"
note = "the mock ERC-20"

[services.engine]
state_dir = "$DIR/state"
fixture_addresses = []

[services.engine.anvil]
target_url = "$T"

[services.engine.journal]
dir = "$DIR/state/journal"

[[services.engine.verifier.probes]]
name = "token.balanceOf(A1)"
to = "$TOKEN"
data = "$PROBE_DATA"

[services.engine.http]
addr = "127.0.0.1"
port = $HTTP_PORT
EOF

"$ENGINE" run --config "$DIR/instance.toml" > "$DIR/engine.log" 2>&1 &
ENG_PID=$!
for _ in $(seq 1 120); do curl -sf "$HTTP/livez" >/dev/null 2>&1 && break; sleep 0.5; done
curl -sf "$HTTP/livez" >/dev/null || { bad "engine did not come up (see $DIR/engine.log)"; exit 1; }

# ---- mirror: T follows the source 1:1 --------------------------------------
cursor=0
for _ in $(seq 1 60); do
  cursor=$(status | python3 -c 'import json,sys; print(json.load(sys.stdin).get("cursor_t") or 0)' 2>/dev/null || echo 0)
  [ "$cursor" -ge "$B4" ] && break
  sleep 1
done
if [ "$cursor" -ge "$B4" ]; then
  ok "mirror: the applier reached the source's tx blocks (cursor $cursor >= $B4)"
else
  bad "mirror: cursor stuck at $cursor (want >= $B4)"
fi
if [ "$(bal "$T" "$A1")" = "$(bal "$SRC" "$A1")" ]; then
  ok "mirror 1:1: A1's balance matches the source ($(bal "$T" "$A1"))"
else
  bad "mirror: A1 T=$(bal "$T" "$A1") src=$(bal "$SRC" "$A1")"
fi
# /status carries the mainnet's own fields beside the shared ones.
st=$(status)
for k in reth_head follow_head follow cursor_t lag watchlist time_offset_seconds cursor halted shadowed capabilities; do
  printf '%s' "$st" | python3 -c "import json,sys; sys.exit(0 if '$k' in json.load(sys.stdin) else 1)" \
    && ok "/status has $k" || bad "/status missing $k"
done

# ---- measure: the configured probe converges --------------------------------
po=$(curl -s -X POST "$HTTP/admin/probes" -H 'content-type: application/json' -d '{}')
printf '%s' "$po" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("ok",0)>=1 else 1)' \
  && ok "/admin/probes: the token probe converged ($po)" || bad "/admin/probes: $po"
ok_n=$(metric mirror_t_convergence_ok_total || true)
[ -n "$ok_n" ] && [ "$ok_n" -ge 1 ] 2>/dev/null \
  && ok "metrics: mirror_t_convergence_ok_total=$ok_n" \
  || bad "metrics: mirror_t_convergence_ok_total=$ok_n"

# ---- modify: a devnet tx on T is COW-marked and survives parent diffs -------
send "$T" "$A0" "$A2" 0x4563918244f40000 >/dev/null   # 5 ETH, devnet-owned
sleep 4
dev_a0=$(bal "$T" "$A0")
send "$SRC" "$A0" "$A1" 0xde0b6b3a7640000 >/dev/null # the source moves A0 too
sleep 6
if [ "$(bal "$T" "$A0")" = "$dev_a0" ] && [ "$dev_a0" != "$(bal "$SRC" "$A0")" ]; then
  ok "modify: the devnet write on T survived the source's next diffs (COW; T=$dev_a0)"
else
  bad "modify: T=$(bal "$T" "$A0") dev=$dev_a0 src=$(bal "$SRC" "$A0")"
fi
status | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if any("f39f" in a.lower() for a in d["shadowed"]) else 1)' \
  && ok "/status lists the devnet-owned account (shadowed)" \
  || bad "/status shadowed: $(status | python3 -c 'import json,sys; print(json.load(sys.stdin)["shadowed"])')"

# ---- mint: /admin/deal discovers the mock's key and writes it ---------------
timeout 5 curl -s -N "$HTTP/events" > "$DIR/events.out" 2>/dev/null &
for _ in $(seq 1 20); do grep -q ": connected" "$DIR/events.out" 2>/dev/null && break; sleep 0.1; done
deal=$(curl -s -X POST "$HTTP/admin/deal" -H 'content-type: application/json' \
  -d '{"token":"'$TOKEN'","to":"'$A2'","amount":"123000000"}')
printf '%s' "$deal" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("ok") and d.get("slot") else 1)' \
  && ok "/admin/deal: ok, key $(printf '%s' "$deal" | python3 -c 'import json,sys; print(json.load(sys.stdin)["slot"])')" \
  || bad "/admin/deal: $deal"
tb=$(rpc "$T" eth_call "[{\"to\":\"$TOKEN\",\"data\":\"0x70a08231000000000000000000000000$(echo "$A2" | cut -c3-)\"},\"latest\"]" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))')
[ "$tb" = "123000000" ] && ok "mint: A2's token balance on T is 123000000" || bad "mint: token balance $tb"
sleep 3
grep -q "deal_applied" "$DIR/events.out" && ok "/events carries deal_applied" || bad "/events missing deal_applied (see $DIR/events.out)"
grep -q "block_applied" "$DIR/events.out" && ok "/events carries block_applied" || bad "/events missing block_applied"

# A token without the standard layout writes nothing, so it must publish
# nothing: the fact is a state change, never a request receipt.
timeout 4 curl -s -N "$HTTP/events" > "$DIR/events_deal.out" 2>/dev/null &
for _ in $(seq 1 20); do grep -q ": connected" "$DIR/events_deal.out" 2>/dev/null && break; sleep 0.1; done
bad_deal=$(curl -s -X POST "$HTTP/admin/deal" -H 'content-type: application/json' \
  -d '{"token":"0x00000000000000000000000000000000000000e8","to":"'$A2'","amount":"1"}')
printf '%s' "$bad_deal" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("ok") is False else 1)' \
  && ok "/admin/deal: a token without the layout answers ok:false" || bad "/admin/deal bad token: $bad_deal"
sleep 3
grep -q "deal_applied" "$DIR/events_deal.out" \
  && bad "the no-slot deal published deal_applied (it wrote nothing)" \
  || ok "the no-slot deal publishes no deal_applied (the fact is a state change)"

# ---- the shared admin surface ----------------------------------------------
# time: the persistent devnet offset
curl -s -X POST "$HTTP/admin/time" -H 'content-type: application/json' -d '{"offset_seconds":3600}' >/dev/null
off=$(status | python3 -c 'import json,sys; print(json.load(sys.stdin)["time_offset_seconds"])')
[ "$off" = "3600" ] && ok "/admin/time: the offset is set and reported" || bad "/admin/time: $off"
curl -s -X POST "$HTTP/admin/time" -H 'content-type: application/json' -d '{"offset_seconds":0}' >/dev/null

# The bare clear is the designed refusal (a clear with nothing to hand back
# would look like success).
sc=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$HTTP/admin/shadow" -H 'content-type: application/json' -d '{"address":"'$A2'","clear":true}')
[ "$sc" = "400" ] && ok "/admin/shadow bare clear: 400 (the designed refusal)" || bad "/admin/shadow bare clear: $sc"

# shadow hand-back: A2's account fields return to the source's values
sleep 3
src_a2=$(bal "$SRC" "$A2")
hb=$(curl -s -X POST "$HTTP/admin/shadow" -H 'content-type: application/json' \
  -d '{"address":"'$A2'","account_fields":true,"clear":true}')
printf '%s' "$hb" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("resynced_keys",0)>=1 else 1)' \
  && ok "/admin/shadow: handed back $(printf '%s' "$hb" | python3 -c 'import json,sys; print(json.load(sys.stdin)["resynced_keys"])') keys" \
  || bad "/admin/shadow: $hb"
[ "$(bal "$T" "$A2")" = "$src_a2" ] && ok "shadow: T's A2 re-synced to the source ($(bal "$T" "$A2"))" \
  || bad "shadow: T=$(bal "$T" "$A2") src=$src_a2"

# rewind: the journaled parent before-values. Paused for the two reads — the
# applier races them otherwise (measured: a catch-up step landed between the
# reads and the check saw 23 -> 24). A paused engine still serves reads and
# writes (the documented pause contract), so the rewind itself is unaffected.
curl -s -X POST "$HTTP/admin/pause" -H 'content-type: application/json' -d '{}' >/dev/null
sleep 1.5
before=$(status | python3 -c 'import json,sys; print(json.load(sys.stdin)["cursor_t"])')
rw=$(curl -s -X POST "$HTTP/admin/rewind" -H 'content-type: application/json' -d '{"blocks":1}')
printf '%s' "$rw" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if "cursor" in d and "restored_values" in d else 1)' \
  && ok "/admin/rewind: restored $(printf '%s' "$rw" | python3 -c 'import json,sys; print(json.load(sys.stdin)["restored_values"])') values" \
  || bad "/admin/rewind: $rw"
after=$(status | python3 -c 'import json,sys; print(json.load(sys.stdin)["cursor_t"])')
curl -s -X POST "$HTTP/admin/resume" -H 'content-type: application/json' -d '{}' >/dev/null
[ "$after" -lt "$before" ] && ok "rewind: the cursor moved back ($before -> $after)" || bad "rewind: cursor $before -> $after"

# A rewind past the journal ring is the named refusal, never a silent no-op.
rc=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$HTTP/admin/rewind" -H 'content-type: application/json' -d '{"blocks":9999999}')
[ "$rc" = "400" ] && ok "/admin/rewind beyond ring: 400 (the named refusal)" || bad "/admin/rewind beyond ring: $rc"

# ---- stability: a SIGKILLed engine resumes from its state -----------------
# The venue script's stanza (the same atomic state files, the mainnet's
# cursor field): a kill -9 leaves no torn file, and the COW mark survives.
cursor_before=$(status | python3 -c 'import json,sys; print(json.load(sys.stdin)["cursor_t"])')
kill -9 "$ENG_PID" 2>/dev/null || true
sleep 1
"$ENGINE" run --config "$DIR/instance.toml" >> "$DIR/engine.log" 2>&1 &
ENG_PID=$!
for _ in $(seq 1 120); do curl -sf "$HTTP/livez" >/dev/null 2>&1 && break; sleep 0.5; done
cursor_after=$(status | python3 -c 'import json,sys; print(json.load(sys.stdin)["cursor_t"])')
[ "$cursor_after" -ge "$cursor_before" ] \
  && ok "restart: the cursor resumed from the state file ($cursor_before -> $cursor_after)" \
  || bad "restart: cursor went backwards ($cursor_before -> $cursor_after)"
[ "$(bal "$T" "$A0")" = "$dev_a0" ] \
  && ok "restart: the devnet-owned write survived (COW mark intact, $dev_a0)" \
  || bad "restart: A0 balance $(bal "$T" "$A0") != $dev_a0"

# ---- stability: a corrupt state file fails closed -------------------------
# A corrupt state must refuse at boot, never silently reset to empty (the
# store's contract: absent is a fact, corruption is a fault). The cursor
# file is truncated, the restart must exit non-zero naming the state, and
# the backup is restored.
kill -9 "$ENG_PID" 2>/dev/null || true
sleep 1
[ -f "$DIR/state/cursor.json" ] \
  && ok "corrupt state: the cursor file is there to corrupt" \
  || bad "corrupt state: no $DIR/state/cursor.json"
cp "$DIR/state/cursor.json" "$DIR/state/cursor.json.bak"
printf '{not json' > "$DIR/state/cursor.json"
if "$ENGINE" run --config "$DIR/instance.toml" > "$DIR/engine-corrupt.log" 2>&1; then
  bad "corrupt state: the engine started anyway (a silent reset would be data loss)"
else
  ok "corrupt state: the engine refused at boot (exit != 0)"
fi
grep -q "cursor state" "$DIR/engine-corrupt.log" \
  && ok "corrupt state: the fault names the state (cursor state)" \
  || bad "corrupt state: the log does not name the state (see $DIR/engine-corrupt.log)"
mv "$DIR/state/cursor.json.bak" "$DIR/state/cursor.json"
"$ENGINE" run --config "$DIR/instance.toml" >> "$DIR/engine.log" 2>&1 &
ENG_PID=$!
for _ in $(seq 1 120); do curl -sf "$HTTP/livez" >/dev/null 2>&1 && break; sleep 0.5; done
curl -sf "$HTTP/livez" >/dev/null \
  && ok "corrupt state: the restored file boots again" \
  || bad "corrupt state: the engine did not come back (see $DIR/engine.log)"

# epoch: one-call re-pin; the engine detects the reset and reconciles
timeout 20 curl -s -N "$HTTP/events" > "$DIR/events2.out" 2>/dev/null &
for _ in $(seq 1 20); do grep -q ": connected" "$DIR/events2.out" 2>/dev/null && break; sleep 0.1; done
ep=$(curl -s -X POST "$HTTP/admin/epoch")
printf '%s' "$ep" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("reset") is True and d.get("source") else 1)' \
  && ok "/admin/epoch: re-pinned ($ep)" || bad "/admin/epoch: $ep"
for _ in $(seq 1 30); do
  grep -q "reset_detected" "$DIR/events2.out" 2>/dev/null && break
  sleep 1
done
grep -q "reset_detected" "$DIR/events2.out" && ok "/events carries reset_detected after the re-pin" \
  || bad "/events missing reset_detected (see $DIR/events2.out)"
grep -q "epoch_changed" "$DIR/events2.out" && ok "/events carries epoch_changed (the epoch rule ran)" \
  || bad "/events missing epoch_changed"

summary "mainnet-engine-local" "$DIR"
