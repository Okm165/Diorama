#!/usr/bin/env bash
# The venue epoch drill (plan §7.8, the counterpart of `make scenario`):
# proves the epoch rule end-to-end — a deal survives a same-pin reset (the
# COW restore), and a new-pin epoch clears it (fresh base = fresh epoch).
#
# Run from the repo root:
#   make venue-drill                 # against the deployed venue-mirror
#   ENGINE_URL=http://127.0.0.1:18681 V_URL=http://127.0.0.1:18610 \
#     ./scripts/venue-drill.sh       # direct mode: a scratch stack, no kubectl
set -euo pipefail

NS="${NS:-venue-mirror}"
USDC="${USDC:-0xb88339CB7199b77E23DB6E890353E22632Ba630f}"   # HyperEVM native USDC
ACCT="${ACCT:-0x00000000000000000000000000000000000000d1}"   # a scratch account
AMOUNT="${AMOUNT:-100000000}"                                 # 100 USDC (6-dec ERC20)
CAST="${CAST:-cast}"

pass=0; fail=0
ok()   { echo "  ✔ $*"; pass=$((pass+1)); }
bad()  { echo "  ✘ $*"; fail=$((fail+1)); }
info() { echo "  · $*"; }

# One transport per mode; every call goes through these two.
if [ -n "${ENGINE_URL:-}" ]; then
  # Direct mode: the scratch stacks (no kubectl, no cluster).
  eget()  { curl -sf -m 60 "$ENGINE_URL$1"; }
  epost() { curl -sf -m 60 -X POST "$ENGINE_URL$1" -H 'Content-Type: application/json' -d "$2"; }
  rpcv()  { curl -sf -m 30 -X POST "$V_URL" -H 'Content-Type: application/json' -d "$1"; }
  balance() { "$CAST" call "$USDC" 'balanceOf(address)(uint256)' "$ACCT" --rpc-url "$V_URL" 2>/dev/null | awk '{print $1}'; }
else
  # Cluster mode: every call execs from the venue engine pod (cluster-internal
  # service names — the same transport the engine itself uses).
  exec_pod() { kubectl -n "$NS" exec -i deploy/engine -- "$@"; }
  eget()  { exec_pod curl -sf -m 60 "localhost:8080$1"; }
  epost() { exec_pod curl -sf -m 60 -X POST "localhost:8080$1" -H 'Content-Type: application/json' -d "$2"; }
  rpcv()  { exec_pod curl -sf -m 30 -X POST http://anvil-v:8545 -H 'Content-Type: application/json' -d "$1"; }
  balance() {
    exec_pod curl -sf -m 30 -X POST http://anvil-v:8545 -H 'Content-Type: application/json' \
      -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$USDC\",\"data\":\"0x70a08231000000000000000000000000${ACCT:2}\"},\"latest\"]}" \
      | python3 -c "import json,sys; r=json.load(sys.stdin).get('result','0x0'); print(int(r,16))"
  }
fi

json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
status_now() { eget /status; }

echo "== venue epoch drill (namespace $NS) =="

echo "-- preconditions"
status=$(status_now)
pin=$(echo "$status" | json 'd["fork_pin"]')
slots_before=$(echo "$status" | json 'd["shadowed_slots"]')
source_url="${SOURCE_URL:-$(echo "$status" | json 'd["source"]')}"
[ -n "$pin" ] && [ "$pin" != "None" ] && ok "V is pinned (pin=$pin, source=$source_url)" || bad "no fork pin: $status"

echo "-- 1. a devnet write (deal), then a same-pin reset: the COW restore must keep it"
epost /admin/deal "{\"token\":\"$USDC\",\"to\":\"$ACCT\",\"amount\":\"$AMOUNT\"}" | json 'd["ok"]' | grep -q True \
  && ok "deal accepted" || bad "deal refused"
before=$(balance); info "balance after deal: $before"
[ "$before" = "$AMOUNT" ] && ok "the dealt balance landed" || bad "deal balance $before != $AMOUNT"

rpcv "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_reset\",\"params\":[{\"forking\":{\"jsonRpcUrl\":\"$source_url\",\"blockNumber\":$pin}}]}" > /dev/null \
  && ok "same-pin reset issued (explicit forking — no-params drops the fork)" || bad "anvil_reset failed"
info "waiting for the driver's same-pin restore (canary -> epoch rule)..."
restored=0
for _ in $(seq 1 30); do
  sleep 2
  s=$(status_now)
  now_pin=$(echo "$s" | json 'd["fork_pin"]')
  now_slots=$(echo "$s" | json 'd["shadowed_slots"]')
  if [ "$now_pin" = "$pin" ] && [ "$now_slots" -ge "$slots_before" ] 2>/dev/null; then
    b=$(balance)
    if [ "$b" = "$AMOUNT" ]; then restored=1; break; fi
  fi
done
[ "$restored" = "1" ] && ok "same-pin reset: the devnet's write survived (restore re-applied it, balance=$AMOUNT)" \
  || bad "same-pin restore did not bring the dealt balance back (balance=$(balance))"
now_pin=$(status_now | json 'd["fork_pin"]')
[ "$now_pin" = "$pin" ] && ok "the pin is unchanged (same-pin path, not an epoch)" || bad "pin moved to $now_pin"

echo "-- 2. a new-pin epoch: the base moves, the epoch rule must clear the devnet's state"
epost /admin/epoch '{}' > /dev/null && ok "re-pin accepted (POST /admin/epoch)" || bad "re-pin refused"
new_pin=""
for _ in $(seq 1 60); do
  sleep 3
  new_pin=$(status_now | json 'd["fork_pin"]')
  [ -n "$new_pin" ] && [ "$new_pin" != "None" ] && [ "$new_pin" != "$pin" ] && break
done
[ "$new_pin" != "$pin" ] && ok "V re-pinned to a new base ($pin -> $new_pin)" || bad "the pin did not move ($new_pin)"

cleared=0
for _ in $(seq 1 30); do
  sleep 2
  s=$(status_now)
  slots=$(echo "$s" | json 'd["shadowed_slots"]')
  ep=$(echo "$s" | json 'd["epoch_pin"]')
  if [ "$slots" = "0" ] && [ "$ep" = "$new_pin" ]; then cleared=1; break; fi
done
[ "$cleared" = "1" ] && ok "new epoch: marks and values cleared, epoch_pin == fork_pin ($new_pin)" \
  || bad "the epoch rule did not clear (shadowed_slots=$(status_now | json 'd["shadowed_slots"]'))"
b=$(balance)
[ "$b" = "0" ] && ok "the old base's write is gone with its epoch (balance=0)" || bad "stale write survived the epoch (balance=$b)"

echo "-- 3. the driver stays live after the drill"
rpcv '{"jsonrpc":"2.0","id":1,"method":"evm_mine","params":[]}' > /dev/null
live=0; cur=""; h=""
for _ in $(seq 1 15); do
  sleep 2
  s=$(status_now)
  cur=$(echo "$s" | json 'd["cursor"]')
  h=$(echo "$s" | json 'd["head"]')
  [ "$cur" = "$h" ] && live=1 && break
done
[ "$live" = "1" ] && ok "the driver scanned to the head after the drill (cursor=head=$h)" \
  || bad "the driver is not tracking the head (cursor=$cur head=$h)"

echo
echo "== $pass passed, $fail failed =="
[ "$fail" = "0" ]
