#!/usr/bin/env bash
# The venue order-lane proof (plan §4.5 — the market-impact axiom): a devnet
# order fills against the *real* book and its consequences are preserved.
#
#   A. the fill walks the served book - a scripted base makes the walk
#      deterministic: two levels, the size-weighted average, the fully
#      consumed level leaves the book, the untouched side stays verbatim
#   B. the trader's state moves      - size and size-weighted entry, the
#      reduce-only fill's realized PnL, the measured identity exact
#   C. the impact persists           - live updates do not resurrect the
#      consumed levels while the scripted base owns the coin, and nothing is
#      pruned spuriously (the served base is what the consumption scopes to)
#   D. the /exchange door            - the compact order action fills in the
#      venue's own envelope; a priced Gtc rests (openOrders serves it, cancel
#      closes it); a non-order action answers the err envelope
#   E. the live book                 - an order against the live depth fills
#      at the touch (the scripted base handed back first)
#   F. the owned mark is never moved - with the mark owned by a crash, a fill
#      leaves it identical (I3), and the hand-back clears the consumptions
#   G. the noop action            - the venue's no-data success envelope; an
#      order without `user` refuses by name (the field is optional so the
#      SDK's noop, which carries none, parses)
#   H. the freshness gate         - a stalled live plane refuses by name and
#      answers readyz 503; the hand-back restores both (the shared window;
#      this leg sleeps past it, ~31 s)
#
# Run from the repo root:
#   make venue-order-proof
#   ENGINE_URL=http://127.0.0.1:18681 SIM_INFO=http://127.0.0.1:18790 \
#     SIM_CTRL=http://127.0.0.1:18791 ./scripts/venue-order-proof.sh
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib-venue.sh"

ORDER_ACCT="${ORDER_ACCT:-0x00000000000000000000000000000000000000e4}"
COIN="${COIN:-ETH}"
FUNDS="${FUNDS:-10000}"

metric() { smetrics | awk -v k="$1" '$1 == k || $1 ~ "^"k"\\{" {s+=$2; found=1} END {print (found ? s : 0)}'; }
book() { sipost /info "{\"type\":\"l2Book\",\"coin\":\"$COIN\"}"; }
mark() {
  sipost /info '{"type":"metaAndAssetCtxs"}' | python3 -c "
import json, sys
d = json.load(sys.stdin)
i = [a['name'] for a in d[0]['universe']].index('$COIN')
print(d[1][i]['markPx'])"
}
same() {
  python3 - "$1" "$2" <<'PY'
import sys
from decimal import Decimal, InvalidOperation
try:
    a, b = Decimal(sys.argv[1]), Decimal(sys.argv[2])
except (InvalidOperation, ValueError):
    print("bad")
    raise SystemExit
print("ok" if a == b else f"{a} != {b}")
PY
}
approx() {
  python3 - "$1" "$2" <<'PY'
import sys
from decimal import Decimal, InvalidOperation
try:
    a, b = Decimal(sys.argv[1]), Decimal(sys.argv[2])
except (InvalidOperation, ValueError):
    print("bad")
    raise SystemExit
print("ok" if abs(a - b) < Decimal("0.000000001") else f"{a} != {b}")
PY
}
echo "== venue order-lane proof =="

echo "-- preconditions"
ready=$(sget /readyz || true)
[ -n "$ready" ] && ok "the simulator is ready (market plane fresh)" || bad "sim /readyz is not ready"
ws=$(metric venue_sim_ws_connected)
[ "$ws" = "1" ] && ok "the market websocket is connected" || bad "venue_sim_ws_connected=$ws"
idx=$(sipost /info '{"type":"meta"}' | python3 -c "import json,sys; print([a['name'] for a in json.load(sys.stdin)['universe']].index('$COIN'))")
[ -n "$idx" ] && ok "$COIN is in the live meta (index $idx)" || bad "no $COIN in the live meta"
# Idempotent clear of this coin's scenario state, then funds for the actor.
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null
filled0=$(metric venue_sim_orders_filled_total)
flatten_coin "$ORDER_ACCT" "$COIN"
spost /control/deal "{\"address\":\"$ORDER_ACCT\",\"class\":\"perp\",\"amount\":\"$FUNDS\"}" > /dev/null
ok "scenario state cleared, the actor flattened; $FUNDS perp USDC dealt"

echo "-- A. the fill walks the served book (scripted base: deterministic)"
spost /control/set_book "{\"coin\":\"$COIN\",\"levels\":[[{\"px\":\"3000\",\"sz\":\"5\",\"n\":1}],[{\"px\":\"3001\",\"sz\":\"2\",\"n\":1},{\"px\":\"3002\",\"sz\":\"1\",\"n\":1}]]}" > /dev/null
pruned0=$(metric venue_sim_owned_pruned_total)
# Own the mark so the I3 check is strict: a live mark legitimately moves with
# the market between two reads — an owned one cannot.
spost /control/crash "{\"coin\":\"$COIN\",\"pct\":\"0.01\"}" > /dev/null
mark0=$(mark)
fill=$(spost /control/order "{\"address\":\"$ORDER_ACCT\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"2.5\"}")
sz=$(echo "$fill" | json 'd["totalSz"]')
px=$(echo "$fill" | json 'd["avgPx"]')
[ "$sz" = "2.5" ] && ok "filled 2.5 across the two asks" || bad "totalSz=$sz"
r=$(same "$px" "3001.20")
[ "$r" = "ok" ] && ok "the average is size-weighted across levels ($px)" || bad "avgPx $r"
cons=$(echo "$fill" | json '[(c["side"], c["price"], c["size"]) for c in d["consumed"]]')
[ "$cons" = "[(1, '3001', '2'), (1, '3002', '0.5')]" ] \
  && ok "the consumed levels are recorded exactly (the ownership write)" || bad "consumed=$cons"
served=$(book)
asks=$(echo "$served" | json '[(l["px"], l["sz"]) for l in d["levels"][1]]')
bids=$(echo "$served" | json '[(l["px"], l["sz"]) for l in d["levels"][0]]')
[ "$asks" = "[('3002', '0.5')]" ] \
  && ok "the fully consumed level left the book; the partial serves the remainder" \
  || bad "asks=$asks"
[ "$bids" = "[('3000', '5')]" ] \
  && ok "the untouched bid side stays verbatim" || bad "bids=$bids"
mark1=$(mark)
[ "$mark1" = "$mark0" ] && ok "I3: the mark never moved ($mark0)" || bad "mark moved $mark0 -> $mark1"
spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null
own=$(metric venue_sim_owned_levels)
[ "$own" = "2" ] && ok "two owned levels (the distinct consumed prices)" || bad "owned_levels=$own"
fill2=$(spost /control/order "{\"address\":\"$ORDER_ACCT\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"0.5\"}")
px2=$(echo "$fill2" | json 'd["avgPx"]')
r=$(same "$px2" "3002")
[ "$r" = "ok" ] && ok "a second order sees the first one's impact ($px2 > $px)" || bad "second fill $r"
asks2=$(book | json 'd["levels"][1]')
[ "$asks2" = "[]" ] && ok "the served ask side is now empty (all consumed)" || bad "asks=$asks2"

echo "-- B. the trader's own state moves (the identity exact)"
state=$(sim_account "$ORDER_ACCT")
r=$(echo "$state" | identity_ok)
[ "$r" = "ok" ] && ok "the measured identity holds after the fills" || bad "identity $r"
szi=$(echo "$state" | json 'd["assetPositions"][0]["position"]["szi"]')
r=$(same "$szi" "3")
[ "$r" = "ok" ] && ok "the position is 3 long" || bad "szi=$r"
entry=$(echo "$state" | json 'd["assetPositions"][0]["position"]["entryPx"]')
r=$(approx "$entry" "3001.333333333333333333333333")
[ "$r" = "ok" ] && ok "the entry is the size-weighted average of both fills ($entry)" || bad "entry $r"
red=$(spost /control/order "{\"address\":\"$ORDER_ACCT\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"1\",\"reduce_only\":true}")
pxr=$(echo "$red" | json 'd["avgPx"]')
r=$(same "$pxr" "3000")
[ "$r" = "ok" ] && ok "the reduce-only sell filled at the bid (3000)" || bad "reduce px $r"
state=$(sim_account "$ORDER_ACCT")
r=$(echo "$state" | identity_ok)
[ "$r" = "ok" ] && ok "the identity stays exact after the realized PnL" || bad "identity $r"
szi=$(echo "$state" | json 'd["assetPositions"][0]["position"]["szi"]')
r=$(same "$szi" "2")
[ "$r" = "ok" ] && ok "the position reduced to 2" || bad "szi=$r"

echo "-- C. the impact persists through live updates (scripted base owns the coin)"
sleep 4
asks3=$(book | json 'd["levels"][1]')
[ "$asks3" = "[]" ] && ok "live updates did not resurrect the consumed levels" || bad "asks=$asks3"
pruned1=$(metric venue_sim_owned_pruned_total)
[ "$pruned1" = "$pruned0" ] && ok "no spurious pruning while the scripted base owns the coin" \
  || bad "owned_pruned moved $pruned0 -> $pruned1"

echo "-- D. the /exchange door (the venue's own envelope)"
# The venue's nonce rule (plan §5): the 100 highest per signer, window
# (T − 2 d, T + 1 d) — so a fixture nonce must be a fresh millisecond stamp.
nonce=$(date +%s%3N)
x=$(sipost /exchange "{\"action\":{\"type\":\"order\",\"orders\":[{\"a\":$idx,\"b\":false,\"p\":\"3000\",\"s\":\"0.2\",\"t\":{\"limit\":{\"tif\":\"Ioc\"}}}]},\"user\":\"$ORDER_ACCT\",\"nonce\":$nonce,\"signature\":{\"r\":\"0x01\",\"s\":\"0x02\",\"v\":27}}")
st=$(echo "$x" | json 'd["response"]["data"]["statuses"][0].get("filled",{}).get("totalSz")')
[ "$st" = "0.2" ] && ok "the compact order filled through /exchange (totalSz 0.2)" || bad "exchange: $x"
# A resting tif needs its own price: the venue never walks a Gtc as a market
# order (it would fill instead of rest).
x=$(sipost /exchange "{\"action\":{\"type\":\"order\",\"orders\":[{\"a\":$idx,\"b\":true,\"s\":\"0.1\",\"t\":{\"limit\":{\"tif\":\"Gtc\"}}}]},\"user\":\"$ORDER_ACCT\"}")
err=$(echo "$x" | json 'd["response"]["data"]["statuses"][0].get("error","")')
[ "$err" = "Order has invalid price." ] && ok "a price-less Gtc refuses by name" \
  || bad "gtc: $x"
# The resting lane: a priced Gtc rests, openOrders serves it, cancel closes it,
# orderStatus reports the terminal state.
px=$(book | json 'float(d["levels"][0][0]["px"]) * 0.95')
x=$(sipost /exchange "{\"action\":{\"type\":\"order\",\"orders\":[{\"a\":$idx,\"b\":true,\"p\":\"$px\",\"s\":\"0.1\",\"t\":{\"limit\":{\"tif\":\"Gtc\"}}}]},\"user\":\"$ORDER_ACCT\",\"nonce\":$(date +%s%3N)}")
oid=$(echo "$x" | json 'd["response"]["data"]["statuses"][0].get("resting",{}).get("oid")')
[ -n "$oid" ] && [ "$oid" != "None" ] && ok "a priced Gtc rests (oid $oid)" || bad "rest: $x"
op=$(sipost /info "{\"type\":\"openOrders\",\"user\":\"$ORDER_ACCT\"}")
[ "$(echo "$op" | json 'len(d)')" = "1" ] && ok "openOrders serves the resting order" || bad "openOrders: $op"
x=$(sipost /exchange "{\"action\":{\"type\":\"cancel\",\"cancels\":[{\"a\":$idx,\"o\":$oid}]},\"user\":\"$ORDER_ACCT\",\"nonce\":$(date +%s%3N)}")
st=$(echo "$x" | json 'd["response"]["data"]["statuses"][0]')
[ "$st" = "success" ] && ok "the cancel succeeds (the venue's success token)" || bad "cancel: $x"
os=$(sipost /info "{\"type\":\"orderStatus\",\"user\":\"$ORDER_ACCT\",\"oid\":$oid}")
[ "$(echo "$os" | json 'd["order"]["status"]')" = "canceled" ] && ok "orderStatus reports canceled" || bad "orderStatus: $os"
x=$(sipost /exchange "{\"action\":{\"type\":\"cancel\",\"cancels\":[]},\"user\":\"$ORDER_ACCT\"}")
s=$(echo "$x" | json 'd["status"]')
[ "$s" = "err" ] && ok "an empty cancel batch answers the venue's err envelope" || bad "cancel: $x"

echo "-- E. the live book (real depth, handed back)"
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
fp=""
for _ in 1 2 3; do
  live_px=$(book | json 'd["levels"][1][0]["px"]')
  f=$(spost /control/order "{\"address\":\"$ORDER_ACCT\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"0.01\",\"limit_px\":\"$live_px\"}" || true)
  fp=$(echo "$f" | json 'd.get("avgPx")')
  [ "$fp" = "$live_px" ] && break
done
r=$(same "$fp" "$live_px")
[ "$r" = "ok" ] && ok "a live order fills at the touch ($fp)" || bad "live fill: read $live_px, got $fp"

echo "-- F. the owned mark is never moved by a fill (I3, strict)"
cr=$(spost /control/crash "{\"coin\":\"$COIN\",\"pct\":\"0.01\"}")
owned=$(echo "$cr" | json 'd["owned"]')
[ "$owned" = "True" ] && ok "the mark is now scenario-owned" || bad "crash: $cr"
mark_owned=$(mark)
for _ in 1 2 3; do
  live_px=$(book | json 'd["levels"][1][0]["px"]')
  f=$(spost /control/order "{\"address\":\"$ORDER_ACCT\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"0.01\",\"limit_px\":\"$live_px\"}" || true)
  [ -n "$f" ] && break
done
mark_after=$(mark)
[ "$mark_after" = "$mark_owned" ] \
  && ok "the owned mark is identical after the fill ($mark_after)" \
  || bad "owned mark moved $mark_owned -> $mark_after"
spost /control/crash "{\"coin\":\"$COIN\"}" | json 'd["owned"]' | grep -q False \
  && ok "the crash hand-back returned the mark to live" || bad "crash hand-back failed"

echo "-- hand-back clears the consumptions"
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
own=$(metric venue_sim_owned_levels)
[ "$own" = "0" ] && ok "owned_levels is 0 after the hand-back" || bad "owned_levels=$own"
filled1=$(metric venue_sim_orders_filled_total)
[ "$filled1" -gt "$filled0" ] && ok "the fill counter moved ($filled0 -> $filled1)" || bad "orders_filled=$filled1"

echo "-- G. the noop action (the venue's no-data success)"
x=$(sipost /exchange "{\"action\":{\"type\":\"noop\"},\"nonce\":$(date +%s%3N),\"signature\":{\"r\":\"0x01\",\"s\":\"0x02\",\"v\":27}}")
[ "$(echo "$x" | json 'd.get("status")')" = "ok" ] && [ "$(echo "$x" | json 'd["response"]["type"]')" = "default" ] \
  && ok "noop answers the venue's default envelope (no user field needed)" || bad "noop: $x"
x=$(sipost /exchange "{\"action\":{\"type\":\"order\",\"orders\":[{\"a\":$idx,\"b\":true,\"s\":\"0.01\"}]},\"nonce\":1705234567891}")
echo "$x" | json 'd.get("status")' | grep -q err \
  && echo "$x" | json 'd["response"]' | grep -q "must name the acting account" \
  && ok "an order without \`user\` refuses by name (the devnet has no keys)" || bad "user-less order: $x"

echo "-- H. the freshness gate (a stalled live plane refuses by name)"
x=$(spost /control/stall '{"seconds":45}')
[ "$(echo "$x" | json 'd["stalled_until_ms"]')" != "0" ] && ok "the live plane is stalled" || bad "stall: $x"
sleep 31
code=$(sreadyz)
[ "$code" = "503" ] && ok "readyz answers 503 on the stalled plane" || bad "readyz during stall: $code"
x=$(spost /control/order "{\"address\":\"$ORDER_ACCT\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"0.01\"}" 2>/dev/null || true)
echo "$x" | grep -q "stale" && ok "an order on the stale book refuses by name" || bad "stale order: $x"
spost /control/stall '{"seconds":0}' > /dev/null
code=""
for _ in $(seq 1 20); do
  code=$(sreadyz)
  [ "$code" = "200" ] && break
  sleep 1
done
[ "$code" = "200" ] && ok "the hand-back restored the live plane (readyz 200)" || bad "readyz after: $code"
f=$(spost /control/order "{\"address\":\"$ORDER_ACCT\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"0.01\"}")
[ "$(echo "$f" | json 'd.get("totalSz")')" = "0.01" ] && ok "an order fills again after the hand-back" || bad "post-stall fill: $f"
spost /control/order "{\"address\":\"$ORDER_ACCT\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"0.01\",\"reduce_only\":true}" > /dev/null || true

echo "== venue order-lane proof: $pass passed, $fail failed"
[ "$fail" = "0" ]
