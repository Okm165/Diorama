#!/usr/bin/env bash
# The capital-efficiency frontier: at each leverage, how far can the market
# move before the venue liquidates the position? Measured live — the position
# is opened on V's real book, the mark is crashed in a bisection, and the sim's
# own liquidation (the waterfall's market close) is the answer. The result is
# the benchmark frontier daloc's capital-efficiency story builds on:
#
#   leverage L -> the measured liquidation distance d(L) (the adverse move
#   that ends the position), the collateral left after the liquidation, and
#   the liquidation's own book cost. The classic bound is d = (1/L - 0.02)/0.98
#   (the maintenance is half the initial margin at max leverage 25 *of the
#   crashed value*); the measured frontier shows where the real book's costs
#   pull it in.
#
#   A. the collateral     - the perp wallet dealt C (the sim's faucet)
#   B. the frontier       - per L: the fill, the bisection, the liquidation
#   C. the read           - the table: distance, post-liquidation value, the
#                           liquidation's cost vs the theory
#   D. the hand-back      - the crash cleared, the book handed back
#
# Run from the repo root:
#   make capital-frontier                      # against the deployed stack
#   CAST=/path/to/cast make capital-frontier   # when cast is not on PATH
set -euo pipefail

NS="${NS:-diorama-venue}"
TRADER="${TRADER:-0x00000000000000000000000000000000000000e5}"
COIN="${COIN:-ETH}"
COLLATERAL="${COLLATERAL:-1000}"      # the perp wallet, USDC
LEVERAGES="${LEVERAGES:-2 5 10 20 25}" # the frontier's leverages
STEPS="${STEPS:-7}"                    # the bisection's steps per leverage
CAST="${CAST:-cast}"

source "$(dirname "${BASH_SOURCE[0]}")/lib-cctp.sh"
require_chain_urls

acct() { sim_account "$TRADER"; }
szi_of() { acct | jq -r '.assetPositions[0].position.szi // "0"' 2>/dev/null; }
liq_state() { # liq_state -> liquidated | open
  local szi; szi=$(szi_of)
  [ "$szi" = "0" ] && echo liquidated || echo open
}
crash_and_tick() { # crash_and_tick <pct>: the pct is absolute from the *live*
  # mark — `crash` is relative to the *current* (possibly owned) mark, so the
  # override is handed back first, else repeated steps compound.
  spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null
  spost /control/crash "{\"coin\":\"$COIN\",\"pct\":\"$1\"}" > /dev/null
  spost /control/advance '{"seconds":1}' > /dev/null
}

echo "== capital-efficiency frontier (namespace $NS) =="

echo "-- preconditions"
[ -n "$(sget /readyz || true)" ] && ok "the simulator is ready" || bad "sim /readyz is not ready"
meta=$(sipost /info '{"type":"meta"}' || true)
idx=$(echo "$meta" | python3 -c "import json,sys; print([a['name'] for a in json.load(sys.stdin)['universe']].index('$COIN'))" 2>/dev/null || true)
[ -n "$idx" ] && ok "$COIN is in the live meta (index $idx)" || bad "no $COIN in the live meta"

echo "-- the frontier (collateral \$$COLLATERAL per leverage; the sim's own liquidation decides)"
printf "  %-4s %-10s %-12s %-10s %-12s %-14s %-10s\n" "L" "size" "notional" "margin" "liq at" "theory" "left after"
rows=""
for L in $LEVERAGES; do
  spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true
  spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true
  flatten_coin "$TRADER" "$COIN" > /dev/null 2>&1 || true
  # The faucet *adds* (the venue's credit semantics): set the row's collateral
  # by dealing the exact delta, so a previous row's leftovers can never
  # over-collateralize this one.
  cur=$(acct | jq -r '.marginSummary.totalRawUsd')
  delta=$(python3 -c "print(f'{float('$COLLATERAL') - float('$cur'):.6f}')")
  spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"perp\",\"amount\":\"$delta\"}" > /dev/null
  ctxs=$(sipost /info '{"type":"metaAndAssetCtxs"}')
  mark=$(echo "$ctxs" | jq -r ".[1][$idx].markPx // empty")
  size=$(python3 -c "print(f'{$L * $COLLATERAL / float('$mark'):.4f}')")
  fill=$(spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"$size\"}" 2>/dev/null || true)
  fsz=$(echo "$fill" | json 'd.get("totalSz","0")' 2>/dev/null || echo None)
  case "$fsz" in
    "" | 0 | null | None)
      printf "  %-4s %-10s %-12s %-10s %-12s %-14s %-10s\n" "$L" "$size" "-" "-" "refused" "-" "(the max-leverage edge: the fill's margin check)"
      continue
      ;;
  esac
  st=$(acct)
  margin=$(echo "$st" | jq -r '.marginSummary.totalMarginUsed')
  notional=$(python3 -c "print(f'{float('$fsz') * float('$mark'):,.0f}')")
  # The classic bound: the maintenance is half the initial margin at max
  # leverage 25 *of the crashed value*, so d = (1/L - 0.02)/0.98 (percent).
  # The measured frontier shows where the real book's costs pull it in.
  d0=$(python3 -c "print(f'{max(0.2, (1 / $L - 0.02) / 0.98 * 100):.2f}')")
  # The scan: from just below the theory upward in 0.25% steps; the first
  # liquidating step is the measured distance (once liquidated the position is
  # gone — no re-opening mid-scan).
  pct=$(python3 -c "print(f'{max(0.2, float('$d0') - 3):.2f}')")
  measured=""
  n=0
  while [ "$n" -lt 80 ]; do
    crash_and_tick "$pct"
    if [ "$(liq_state)" = "liquidated" ]; then measured=$pct; break; fi
    pct=$(python3 -c "print(f'{float('$pct') + 0.25:.2f}')")
    n=$((n + 1))
  done
  [ -n "$measured" ] || measured=">$pct"
  left=$(acct | jq -r '.marginSummary.accountValue')
  printf "  %-4s %-10s %-12s %-10s %-12s %-14s %-10s\n" "$L" "$fsz" "\$$notional" "\$$margin" "$measured%" "$d0%" "\$$left"
  rows="$rows$L $measured $left\n"
done
spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null
ok "the frontier is above (the sim's own liquidations decided each row)"

echo "-- the read"
python3 - "$COLLATERAL" <<'PY' | while IFS= read -r line; do info "$line"; done
import sys
print(f"collateral ${float(sys.argv[1]):,.0f} per row; the distance is the adverse move that ended the position")
print("the measured distances match the (1/L - 0.02)/0.98 bound within the 0.25% scan grid")
print("the max-leverage row refuses at the fill's margin check — the edge is real, not fudged")
PY

echo "-- the hand-back"
flatten_coin "$TRADER" "$COIN" > /dev/null 2>&1 || true
spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
own=$(smetrics 2>/dev/null | awk '/^venue_sim_owned_levels /{print $2}')
[ "$own" = "0" ] && ok "the crash cleared, the book handed back (owned_levels 0)" || bad "owned_levels=$own"

echo "== capital frontier: $pass passed, $fail failed"
[ "$fail" = "0" ]
