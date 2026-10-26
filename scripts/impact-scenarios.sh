#!/usr/bin/env bash
# The impact ladder: another account moves the market — through the *book*
# (the fill walk) and through the *mark* (the crash primitive) — and the
# portfolio's reaction must be sound: monotone, mirrored across sides,
# bounded by the maintenance buffer, and never leaking value past a
# liquidation.
#
#   A. the whale's round trip         - the execution impact: a size walked
#                                       through the live book and back; the
#                                       cost printed in bps
#   B. the exit under a consumed book - the portfolio flattens into the depth
#                                       the whale just ate: baseline vs
#                                       stressed slippage (contagion via depth)
#   C. the mark shock, both sides     - a long and a short on the same
#                                       notional take the same crash: the
#                                       reactions mirror
#   D. the liquidation ladder         - the measured adverse move that
#                                       liquidates, per size, against the
#                                       analytic buffer (equity - maintenance)
#   E. the cascade                    - two near-threshold accounts crash
#                                       together, once into a fresh book and
#                                       once into the book the whale ate: the
#                                       consumed depth makes the second
#                                       liquidation worse
#   F. the funding drift              - a slow negative: the carry erodes the
#                                       equity by ~rate x notional x time
#   G. the sweep                      - the served identity holds on every
#                                       account, every book handed back, flat
#
# Run from the repo root:
#   make impact-scenarios
#   CAST=/path/to/cast make impact-scenarios
set -euo pipefail

NS="${NS:-diorama-venue}"
PORTFOLIO="${PORTFOLIO:-0x00000000000000000000000000000000000000f1}"
WHALE="${WHALE:-0x00000000000000000000000000000000000000f2}"
RIVAL="${RIVAL:-0x00000000000000000000000000000000000000f3}"
COIN="${COIN:-ETH}"
COLLATERAL="${COLLATERAL:-3000}"     # the portfolio's and the rival's perp wallet, USDC
WHALE_COLLATERAL="${WHALE_COLLATERAL:-50000}"
WHALE_SIZE="${WHALE_SIZE:-300}"      # the impact size, coin units (the live book is deep)
PORT_SIZE="${PORT_SIZE:-1}"
CAST="${CAST:-cast}"

source "$(dirname "${BASH_SOURCE[0]}")/lib-cctp.sh"
require_chain_urls

pass=0; fail=0
ok()    { echo "  ok   $1"; pass=$((pass+1)); }
bad()   { echo "  FAIL $1"; fail=$((fail+1)); }
check() { if [ "$2" = "1" ]; then ok "$1"; else bad "$1"; fi; }
json()  { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
gt()    { python3 -c "import sys; sys.exit(0 if float('$1') > float('$2') else 1)" && echo 1 || echo 0; }
lt()    { python3 -c "import sys; sys.exit(0 if float('$1') < float('$2') else 1)" && echo 1 || echo 0; }

idx=$(sipost /info '{"type":"meta"}' | json "[a['name'] for a in d['universe']].index('$COIN')")

mid() { # the live book's mid for COIN
  local b; b=$(sipost /info "{\"type\":\"l2Book\",\"coin\":\"$COIN\"}")
  python3 -c "
import json, sys
d = json.loads(sys.argv[1])
print(f\"{(float(d['levels'][0][0]['px']) + float(d['levels'][1][0]['px'])) / 2:.4f}\")" "$b"
}
acct()   { sim_account "$1"; }
raw()    { acct "$1" | json "d['marginSummary']['totalRawUsd']"; }
equity() { acct "$1" | json "d['marginSummary']['accountValue']"; }
maint()  { acct "$1" | json "d['marginSummary']['totalMarginUsed']"; }
szi()    { acct "$1" | json "d['assetPositions'][0]['position']['szi'] if d.get('assetPositions') else '0'"; }
liqs()   { smetrics | python3 -c "import sys,re; m=re.search(r'^venue_sim_liquidations_total (\d+)', sys.stdin.read(), re.M); print(m.group(1) if m else '0')"; }
fund_to() { # fund_to <account> <target raw USDC>
  local delta; delta=$(python3 -c "print(f'{float('$2') - float(\"$(raw "$1")\"):.6f}')")
  if python3 -c "import sys; sys.exit(0 if abs(float('$delta')) > 0.001 else 1)"; then
    spost /control/deal "{\"address\":\"$1\",\"class\":\"perp\",\"amount\":\"$delta\"}" > /dev/null
  fi
}
order() { # order <account> <is_buy> <size> [reduce_only]
  local extra=""; [ -n "${4:-}" ] && extra=",\"reduce_only\":$4"
  spost /control/order "{\"address\":\"$1\",\"coin\":\"$COIN\",\"is_buy\":$2,\"size\":\"$3\"$extra}"
}
clear_crash() { spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null; spost /control/advance '{"seconds":1}' > /dev/null; }

echo "== setup: fund the three accounts, flatten any leftovers =="
fund_to "$PORTFOLIO" "$COLLATERAL"
fund_to "$RIVAL" "$COLLATERAL"
fund_to "$WHALE" "$WHALE_COLLATERAL"
flatten_coin "$PORTFOLIO" "$COIN"
flatten_coin "$RIVAL" "$COIN"
flatten_coin "$WHALE" "$COIN"
echo "  portfolio ${COLLATERAL} | rival ${COLLATERAL} | whale ${WHALE_COLLATERAL} USDC | ${COIN} at $(mid)"

echo
echo "== A. the whale's round trip (the execution impact) =="
m0=$(mid)
sell=$(order "$WHALE" false "$WHALE_SIZE")
spx=$(echo "$sell" | json "d['avgPx']"); ssz=$(echo "$sell" | json "d['totalSz']")
buy=$(order "$WHALE" true "$ssz")
bpx=$(echo "$buy" | json "d['avgPx']")
python3 -c "
m0, spx, bpx, sz = float('$m0'), float('$spx'), float('$bpx'), float('$ssz')
print(f'  mid {m0:,.2f} | sell {spx:,.2f} ({(m0-spx)/m0*1e4:+.1f} bps) | buy-back {bpx:,.2f} ({(bpx-m0)/m0*1e4:+.1f} bps)')
print(f'  the round trip cost the whale { (bpx-spx)*sz:,.2f} USDC moving {sz:,.4f} ${COIN} through the live book')"
check "the whale's sell walks below the mid" "$(gt "$m0" "$spx")"
check "the buy-back walks above the mid" "$(gt "$bpx" "$m0")"
check "the round trip is a net cost" "$(gt "$bpx" "$spx")"

echo
echo "== B. the exit under a consumed book (contagion via depth) =="
o1=$(order "$PORTFOLIO" true "$PORT_SIZE")
e1=$(echo "$o1" | json "d['avgPx']"); s1=$(echo "$o1" | json "d['totalSz']")
x1=$(order "$PORTFOLIO" false "$s1" true | json "d['avgPx']")
base_slip=$(python3 -c "print(f'{(float('$e1')-float('$x1'))/float('$e1')*1e4:.2f}')")
o2=$(order "$PORTFOLIO" true "$PORT_SIZE")
e2=$(echo "$o2" | json "d['avgPx']"); s2=$(echo "$o2" | json "d['totalSz']")
order "$WHALE" false "$WHALE_SIZE" > /dev/null   # the whale eats the bids
x2=$(order "$PORTFOLIO" false "$s2" true | json "d['avgPx']")
stress_slip=$(python3 -c "print(f'{(float('$e2')-float('$x2'))/float('$e2')*1e4:.2f}')")
echo "  exit slippage: baseline ${base_slip} bps | after the whale's ${WHALE_SIZE} ${COIN} dump ${stress_slip} bps"
check "the stressed exit is worse than the baseline" "$(gt "$stress_slip" "$base_slip")"

echo
echo "== C. the mark shock (long and short, the same event) =="
flatten_coin "$PORTFOLIO" "$COIN"; flatten_coin "$RIVAL" "$COIN"
o=$(order "$PORTFOLIO" true "$PORT_SIZE"); entry=$(echo "$o" | json "d['avgPx']")
eq0=$(equity "$PORTFOLIO"); mm0=$(maint "$PORTFOLIO")
order "$RIVAL" false "$PORT_SIZE" > /dev/null
eqr0=$(equity "$RIVAL"); mmr0=$(maint "$RIVAL")
spost /control/crash "{\"coin\":\"$COIN\",\"pct\":\"10\"}" > /dev/null
spost /control/advance '{"seconds":1}' > /dev/null
eq1=$(equity "$PORTFOLIO"); mm1=$(maint "$PORTFOLIO")
eqr1=$(equity "$RIVAL"); mmr1=$(maint "$RIVAL")
notional=$(python3 -c "print(f'{float('$PORT_SIZE')*float('$entry'):.2f}')")
python3 -c "
eq0, eq1, eqr0, eqr1, n = float('$eq0'), float('$eq1'), float('$eqr0'), float('$eqr1'), float('$notional')
print(f'  the long:  equity {eq0:,.2f} -> {eq1:,.2f} ({eq1-eq0:+,.2f}) | maintenance {float(\"$mm0\"):,.2f} -> {float(\"$mm1\"):,.2f}')
print(f'  the short: equity {eqr0:,.2f} -> {eqr1:,.2f} ({eqr1-eqr0:+,.2f}) | maintenance {float(\"$mmr0\"):,.2f} -> {float(\"$mmr1\"):,.2f}')
print(f'  a 10% crash on {n:,.0f} notional: the long pays ~{-n*0.10:,.2f}, the short earns ~{n*0.10:,.2f}')"
check "the long's equity falls with the crash" "$(lt "$eq1" "$eq0")"
check "the short's equity rises with the crash" "$(gt "$eqr1" "$eqr0")"
check "the two reactions mirror within 0.5% of the notional" \
  "$(python3 -c "import sys; a=float('$eq1')-float('$eq0'); b=float('$eqr1')-float('$eqr0'); sys.exit(0 if abs(a+b) <= float('$notional')*0.005 + 1 else 1)" && echo 1 || echo 0)"
clear_crash
flatten_coin "$PORTFOLIO" "$COIN"; flatten_coin "$RIVAL" "$COIN"

echo
echo "== D. the liquidation ladder (the measured threshold per size) =="
declare -a sizes=(1 2 4 6)
declare -a thresholds=()
for sz in "${sizes[@]}"; do
  flatten_coin "$PORTFOLIO" "$COIN"
  o=$(order "$PORTFOLIO" true "$sz"); entry=$(echo "$o" | json "d['avgPx']")
  eq=$(equity "$PORTFOLIO"); mm=$(maint "$PORTFOLIO")
  notional=$(python3 -c "print(f'{float('$sz')*float('$entry'):.4f}')")
  # The liquidation condition with the maintenance shrinking as the mark
  # falls: equity - n*p = n*(1-p)*mmr, so p = (eq - n*mmr) / (n*(1-mmr)).
  predicted=$(python3 -c "
n = float('$notional'); eq = float('$eq'); mm = float('$mm')
mmr = mm / n
p = (eq - n * mmr) / (n * (1 - mmr))
print(f'{p*100:.2f}')")
  before=$(liqs); measured=""
  for step in 2 5 10 15 20 25 30 40 50; do
    spost /control/crash "{\"coin\":\"$COIN\",\"pct\":\"$step\"}" > /dev/null
    spost /control/advance '{"seconds":1}' > /dev/null
    if [ "$(liqs)" -gt "$before" ]; then measured=$step; break; fi
    spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null   # hand back before the next step
    spost /control/advance '{"seconds":1}' > /dev/null
  done
  clear_crash
  if [ -n "$measured" ]; then
    thresholds+=("$measured")
    ok "size ${sz} ${COIN} (notional ${notional}) liquidated at ~${measured}% adverse; predicted ${predicted}% from the buffer"
    check "the measured threshold brackets the prediction (step granularity)" \
      "$(python3 -c "import sys; m=float('$measured'); p=float('$predicted'); sys.exit(0 if p - 2 <= m <= p + 12 else 1)" && echo 1 || echo 0)"
  elif python3 -c "import sys; sys.exit(0 if float('$predicted') >= 50 else 1)"; then
    ok "size ${sz} ${COIN} (notional ${notional}) cannot liquidate within the 50% ladder (predicted ${predicted}%: under-leveraged - sound)"
  else
    bad "size ${sz} ${COIN} never liquidated within the 50% ladder (predicted ${predicted}%)"
  fi
  flatten_coin "$PORTFOLIO" "$COIN"
done
if [ "${#thresholds[@]}" -ge 2 ]; then
  monotone=1
  for ((i=1; i<${#thresholds[@]}; i++)); do
    [ "${thresholds[$i]}" -le "${thresholds[$((i-1))]}" ] || monotone=0
  done
  check "bigger sizes need a smaller adverse move (monotone thresholds: ${thresholds[*]})" "$monotone"
fi

echo
echo "== E. the cascade (two near-threshold accounts, fresh book vs eaten book) =="
cascade_round() { # cascade_round <label>; prints the sum of the residuals
  flatten_coin "$PORTFOLIO" "$COIN"; flatten_coin "$RIVAL" "$COIN"
  order "$PORTFOLIO" true 6 > /dev/null
  order "$RIVAL" true 6 > /dev/null
  if [ "$1" = "eaten" ]; then order "$WHALE" false "$WHALE_SIZE" > /dev/null; fi
  spost /control/crash "{\"coin\":\"$COIN\",\"pct\":\"15\"}" > /dev/null
  spost /control/advance '{"seconds":1}' > /dev/null
  python3 -c "print(f'{float(\"$(equity "$PORTFOLIO")\")+float(\"$(equity "$RIVAL")\"):.2f}')"
}
r_fresh=$(cascade_round fresh)
clear_crash
r_eaten=$(cascade_round eaten)
clear_crash
flatten_coin "$PORTFOLIO" "$COIN"; flatten_coin "$RIVAL" "$COIN"
echo "  residuals after the 15% crash: fresh book ${r_fresh} USDC | book the whale ate ${r_eaten} USDC"
check "the liquidations leave no negative residual (fresh)" "$( [ "$(python3 -c "print(1 if float('$r_fresh') >= -0.01 else 0)")" = "1" ] && echo 1 || echo 0 )"
check "the liquidations leave no negative residual (eaten)" "$( [ "$(python3 -c "print(1 if float('$r_eaten') >= -0.01 else 0)")" = "1" ] && echo 1 || echo 0 )"
check "the consumed depth makes the cascade no better (eaten <= fresh)" "$(python3 -c "import sys; sys.exit(0 if float('$r_eaten') <= float('$r_fresh') + 0.01 else 1)" && echo 1 || echo 0)"

echo
echo "== F. the funding drift (the carry erodes the equity) =="
flatten_coin "$PORTFOLIO" "$COIN"
o=$(order "$PORTFOLIO" true "$PORT_SIZE"); entry=$(echo "$o" | json "d['avgPx']")
eq0=$(equity "$PORTFOLIO")
notional=$(python3 -c "print(f'{float('$PORT_SIZE')*float('$entry'):.4f}')")
rate="0.001"   # owned: 0.1%/hour
spost /control/set_funding "{\"coin\":\"$COIN\",\"rate\":\"$rate\"}" > /dev/null
spost /control/advance '{"seconds":3600}' > /dev/null
eq1=$(equity "$PORTFOLIO")
spost /control/set_funding "{\"coin\":\"$COIN\"}" > /dev/null
python3 -c "
eq0, eq1, n, r = float('$eq0'), float('$eq1'), float('$notional'), float('$rate')
print(f'  one hour at {r*100:.2f}%/h on {n:,.0f} notional: equity {eq0:,.2f} -> {eq1:,.2f} (a drift of {eq1-eq0:+,.2f}; rate x notional = {-n*r:+,.2f})')"
check "the carry moves the equity" "$(lt "$eq1" "$eq0")"
check "the drift matches rate x notional within 25%" \
  "$(python3 -c "import sys; d=float('$eq1')-float('$eq0'); exp=-float('$notional')*float('$rate'); sys.exit(0 if abs(d-exp) <= abs(exp)*0.25 + 0.5 else 1)" && echo 1 || echo 0)"
flatten_coin "$PORTFOLIO" "$COIN"

echo
echo "== G. the sweep (identity, handed back, flat) =="
for a in "$PORTFOLIO" "$RIVAL" "$WHALE"; do
  check "the served identity holds for ${a:0:10}..." "$( [ "$(acct "$a" | identity_ok)" = "ok" ] && echo 1 || echo 0 )"
done
check "the portfolio is flat" "$( [ "$(szi "$PORTFOLIO")" = "0" ] && echo 1 || echo 0 )"
check "the rival is flat" "$( [ "$(szi "$RIVAL")" = "0" ] && echo 1 || echo 0 )"
w=$(szi "$WHALE")
if [ "$w" != "0" ]; then
  case "$w" in
    -*) order "$WHALE" true "$(python3 -c "print(abs(float('$w')))")" > /dev/null ;;
    *)  order "$WHALE" false "$w" > /dev/null ;;
  esac
fi
check "the whale is flat" "$( [ "$(szi "$WHALE")" = "0" ] && echo 1 || echo 0 )"

echo
echo "  the impact ladder: ${pass} ok, ${fail} failed"
[ "$fail" -eq 0 ]
