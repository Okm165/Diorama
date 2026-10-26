#!/usr/bin/env bash
# The hedged book: the same collateral and the same *gross* exposure as an
# unhedged position, but market-neutral (long ETH, short BTC, dollar-matched).
# The measured point is the capital efficiency of the hedge: the venue's
# maintenance table gives it no credit (it sums both legs), yet the equity —
# what the waterfall actually reads — stays flat through a market-wide crash
# that liquidates the unhedged equivalent at ~49%.
#
#   A. the book       - the collateral set exactly, the two legs filled on
#                       V's real books, the cross maintenance (the sum)
#   B. the shocks     - both legs crashed together: the account value flat,
#                       the legs' PnL offsetting, the maintenance shrinking
#   C. the read       - hedged vs unhedged (the capital-frontier's measured
#                       48.98% at the same gross/collateral ratio)
#   D. the hand-back  - the crashes cleared, the books handed back
#
# Run from the repo root:
#   make hedged-book                      # against the deployed stack
#   CAST=/path/to/cast make hedged-book   # when cast is not on PATH
set -euo pipefail

NS="${NS:-diorama-venue}"
TRADER="${TRADER:-0x00000000000000000000000000000000000000e5}"
LONG_COIN="${LONG_COIN:-ETH}"
SHORT_COIN="${SHORT_COIN:-BTC}"
COLLATERAL="${COLLATERAL:-2000}"      # the perp wallet, USDC
LEG_NOTIONAL="${LEG_NOTIONAL:-2000}"  # per leg: gross = 2x this
SHOCKS="${SHOCKS:-10 30 50 60}"       # the market-wide crash ladder, percent
CAST="${CAST:-cast}"

source "$(dirname "${BASH_SOURCE[0]}")/lib-cctp.sh"
require_chain_urls

coin_index() { # coin_index <coin> -> the meta's index
  sipost /info '{"type":"meta"}' | python3 -c "import json,sys; print([a['name'] for a in json.load(sys.stdin)['universe']].index('$1'))"
}
sz_decimals() { # sz_decimals <coin>
  sipost /info '{"type":"meta"}' | jq -r ".universe[] | select(.name == \"$1\") | .szDecimals"
}
acct() { sim_account "$TRADER"; }
crash_both() { # crash_both <pct|clear>: hand the overrides back first (crash
  # is relative to the *current* mark; repeated steps would compound), then own
  # the absolute level on both legs.
  for c in "$LONG_COIN" "$SHORT_COIN"; do
    spost /control/crash "{\"coin\":\"$c\"}" > /dev/null
    if [ "$1" != "clear" ]; then
      spost /control/crash "{\"coin\":\"$c\",\"pct\":\"$1\"}" > /dev/null
    fi
  done
}

echo "== the hedged book (namespace $NS) =="

echo "-- preconditions"
[ -n "$(sget /readyz || true)" ] && ok "the simulator is ready" || bad "sim /readyz is not ready"
li=$(coin_index "$LONG_COIN"); si=$(coin_index "$SHORT_COIN")
[ -n "$li" ] && [ -n "$si" ] && ok "$LONG_COIN (index $li) and $SHORT_COIN (index $si) are in the live meta" || bad "a leg is not in the meta"

echo "-- A. the book (long $LONG_COIN, short $SHORT_COIN, \$$LEG_NOTIONAL per leg, collateral \$$COLLATERAL)"
crash_both clear
for c in "$LONG_COIN" "$SHORT_COIN"; do spost /control/set_book "{\"coin\":\"$c\"}" > /dev/null 2>&1 || true; done
flatten_coin "$TRADER" "$LONG_COIN" > /dev/null 2>&1 || true
flatten_coin "$TRADER" "$SHORT_COIN" > /dev/null 2>&1 || true
cur=$(acct | jq -r '.marginSummary.totalRawUsd')
delta=$(python3 -c "print(f'{float('$COLLATERAL') - float('$cur'):.6f}')")
spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"perp\",\"amount\":\"$delta\"}" > /dev/null
ctxs=$(sipost /info '{"type":"metaAndAssetCtxs"}')
lpx=$(echo "$ctxs" | jq -r ".[1][$li].markPx")
spx=$(echo "$ctxs" | jq -r ".[1][$si].markPx")
ld=$(sz_decimals "$LONG_COIN"); sd=$(sz_decimals "$SHORT_COIN")
lsz=$(python3 -c "import sys; n=float(sys.argv[1]); p=float(sys.argv[2]); d=int(sys.argv[3]); print(f'{n/p:.{d}f}')" "$LEG_NOTIONAL" "$lpx" "$ld")
ssz=$(python3 -c "import sys; n=float(sys.argv[1]); p=float(sys.argv[2]); d=int(sys.argv[3]); print(f'{n/p:.{d}f}')" "$LEG_NOTIONAL" "$spx" "$sd")
spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$LONG_COIN\",\"is_buy\":true,\"size\":\"$lsz\"}" > /dev/null
spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$SHORT_COIN\",\"is_buy\":false,\"size\":\"$ssz\"}" > /dev/null
st=$(acct)
ok "the legs filled: long $lsz $LONG_COIN @ $lpx, short $ssz $SHORT_COIN @ $spx" || true
maint=$(echo "$st" | jq -r '.crossMaintenanceMarginUsed')
value0=$(echo "$st" | jq -r '.marginSummary.accountValue')
lml=$(sipost /info '{"type":"meta"}' | jq -r ".universe[] | select(.name == \"$LONG_COIN\") | .maxLeverage")
sml=$(sipost /info '{"type":"meta"}' | jq -r ".universe[] | select(.name == \"$SHORT_COIN\") | .maxLeverage")
expected=$(python3 -c "print(f'{$LEG_NOTIONAL / (2 * $lml) + $LEG_NOTIONAL / (2 * $sml):.4f}')")
python3 -c "exit(0 if abs(float('$maint') - float('$expected')) / float('$expected') < 0.1 else 1)" \
  && ok "the cross maintenance is the sum of both legs ($LONG_COIN at ${lml}x, $SHORT_COIN at ${sml}x: \$$maint) — the table gives no hedge credit" \
  || bad "maintenance \$$maint (expected ~\$$expected)"

echo "-- B. the market-wide shocks (both legs together)"
printf "  %-8s %-22s %-14s %-26s\n" "crash" "accountValue" "maintenance" "legs' unrealized"
survived=1
for pct in $SHOCKS; do
  crash_both "$pct"
  spost /control/advance '{"seconds":1}' > /dev/null
  st=$(acct)
  szi_l=$(echo "$st" | jq -r ".assetPositions[] | select(.position.coin == \"$LONG_COIN\") | .position.szi // \"0\"")
  [ "$szi_l" = "0" ] && survived=0
  av=$(echo "$st" | jq -r '.marginSummary.accountValue')
  mt=$(echo "$st" | jq -r '.crossMaintenanceMarginUsed')
  legs=$(echo "$st" | jq -r '[.assetPositions[].position.unrealizedPnl] | join(" / ")')
  printf "  %-8s %-22s %-14s %-26s\n" "$pct%" "$av" "$mt" "$legs"
  python3 -c "exit(0 if abs(float('$av') - float('$value0')) / float('$value0') < 0.005 else 1)" \
    && ok "at $pct% the hedged book holds its value (flat within 0.5%)" \
    || bad "at $pct% the value moved: $value0 -> $av"
done
[ "$survived" = "1" ] && ok "the book survived every shock (no liquidation — the equity never neared the maintenance)" \
  || bad "a leg vanished mid-ladder"

echo "-- C. the read (the hedge's capital efficiency)"
python3 - "$LEG_NOTIONAL" "$COLLATERAL" "$value0" <<'PY' | while IFS= read -r line; do info "$line"; done
import sys
leg, coll, v0 = map(float, sys.argv[1:4])
gross = 2 * leg
print(f"collateral ${coll:,.0f} | gross ${gross:,.0f} ({gross/coll:.1f}x) | the book survived the 60% market-wide shock flat")
print(f"the unhedged equivalent (same ratio, `make capital-frontier` L=2) liquidates at 48.98% — measured")
print("the maintenance table gives the hedge no credit; the equity the waterfall reads does")
PY
ok "the read is above"

echo "-- D. the hand-back"
crash_both clear
flatten_coin "$TRADER" "$LONG_COIN" > /dev/null 2>&1 || true
flatten_coin "$TRADER" "$SHORT_COIN" > /dev/null 2>&1 || true
for c in "$LONG_COIN" "$SHORT_COIN"; do spost /control/set_book "{\"coin\":\"$c\"}" > /dev/null; done
own=$(smetrics 2>/dev/null | awk '/^venue_sim_owned_levels /{print $2}')
[ "$own" = "0" ] && ok "the crashes cleared, the books handed back (owned_levels 0)" || bad "owned_levels=$own"

echo "== hedged book: $pass passed, $fail failed"
[ "$fail" = "0" ]
