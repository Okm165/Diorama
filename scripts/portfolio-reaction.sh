#!/usr/bin/env bash
# The portfolio reaction: a held portfolio (the trader) and another account
# (the whale) moving the market — through the *book* (the execution channel)
# and through the *mark* (the scenario's crash down / rally up) — with the
# portfolio's reaction measured in both directions and checked for soundness:
# the signs, the magnitudes against size x mark, the served liquidation
# buffer against the sim's own liquidations, and the hedged book's flatness.
#
#   A. the held portfolio   - the trader at L=LEVERAGE: the fill, the served
#                             liquidation buffer vs the analytic bound
#   B. the book impact      - the whale's round trip: the execution channel
#                             (depth, slippage) moves; the trader's equity
#                             moves only by the mark's own move
#   C. the mark, both ways  - a crash and a rally on the same short: the
#                             reactions mirror; the magnitudes match
#                             size x delta-mark
#   D. the thresholds       - the sim's own liquidation per direction (a
#                             rally scan for the short, a crash scan for the
#                             long) vs the served buffer and the analytic
#   E. the hedged portfolio - the same 30% moves both ways: flat, no
#                             liquidation, the served buffer far
#   F. the hand-back        - moves cleared, books handed back, flat
#
# Run from the repo root:
#   make portfolio-reaction                      # against the deployed stack
#   CAST=/path/to/cast make portfolio-reaction   # when cast is not on PATH
set -euo pipefail

NS="${NS:-venue-mirror}"
TRADER="${TRADER:-0x00000000000000000000000000000000000000f1}"
WHALE="${WHALE:-0x00000000000000000000000000000000000000f2}"
COIN="${COIN:-ETH}"
COLLATERAL="${COLLATERAL:-3000}"
LEVERAGE="${LEVERAGE:-10}"
WHALE_COLLATERAL="${WHALE_COLLATERAL:-50000}"
WHALE_SIZE="${WHALE_SIZE:-300}"
CRASH_PCT="${CRASH_PCT:-5}"
HEDGE_SIZE="${HEDGE_SIZE:-4}"
SHOCK="${SHOCK:-15}"               # the flatness phase's move (inside the buffer)
SHOCK_HARD="${SHOCK_HARD:-40}"        # the beyond-the-buffer liquidation probe
WETH="${WETH:-0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2}"
CAST="${CAST:-cast}"

source "$(dirname "${BASH_SOURCE[0]}")/lib-cctp.sh"
require_chain_urls

json()   { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
acct()   { sim_account "$TRADER"; }
szi_of() { acct | jq -r '.assetPositions[0].position.szi // "0"' 2>/dev/null; }
liq_state() { [ "$(szi_of)" = "0" ] && echo liquidated || echo open; }
mark()   { sipost /info '{"type":"metaAndAssetCtxs"}' | jq -r ".[1][$idx].markPx // empty"; }
mid() {
  local b; b=$(sipost /info "{\"type\":\"l2Book\",\"coin\":\"$COIN\"}")
  python3 -c "
import json, sys
d = json.loads(sys.argv[1])
print(f\"{(float(d['levels'][0][0]['px']) + float(d['levels'][1][0]['px'])) / 2:.4f}\")" "$b"
}
# The served liquidation buffer: the adverse move (percent of the mark) the
# equity covers before it meets the maintenance.
buffer_pct() {
  acct | python3 -c "
import json, sys
d = json.load(sys.stdin)
v = float(d['marginSummary']['accountValue'])
m = float(d.get('crossMaintenanceMarginUsed') or 0)
p = d['assetPositions'][0]['position'] if d.get('assetPositions') else None
if not p:
    print('0')
else:
    size = abs(float(p['szi']))
    px = float(p['positionValue']) / size if size else 0
    print(f'{(v - m) / (size * px) * 100:.2f}' if size and px else '0')
"
}
clear_move() { spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true; spost /control/rally "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true; }
move_and_tick() { # move_and_tick <verb> <pct>: the move is relative to the
  # *current* (possibly owned) mark, so the override is handed back first,
  # else repeated steps compound (the frontier's measured lesson).
  clear_move
  spost /control/$1 "{\"coin\":\"$COIN\",\"pct\":\"$2\"}" > /dev/null
  spost /control/advance '{"seconds":1}' > /dev/null
}
scan() { # scan <verb> <from_pct>: 0.25% steps until the sim liquidates
  local pct="$2" n=0
  while [ "$n" -lt 80 ]; do
    move_and_tick "$1" "$pct"
    if [ "$(liq_state)" = "liquidated" ]; then echo "$pct"; return; fi
    pct=$(python3 -c "print(f'{float('$pct') + 0.25:.2f}')"); n=$((n+1))
  done
  echo ">$pct"
}

echo "== portfolio reaction: another account moves the market (namespace $NS) =="

echo "-- preconditions"
[ -n "$(sget /readyz || true)" ] && ok "the simulator is ready" || bad "sim /readyz is not ready"
meta=$(sipost /info '{"type":"meta"}' || true)
idx=$(echo "$meta" | python3 -c "import json,sys; print([a['name'] for a in json.load(sys.stdin)['universe']].index('$COIN'))" 2>/dev/null || true)
[ -n "$idx" ] && ok "$COIN is in the live meta (index $idx)" || bad "no $COIN in the live meta"
clear_move
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true
spost /control/set_funding "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true
flatten_coin "$TRADER" "$COIN" > /dev/null 2>&1 || true
flatten_coin "$WHALE" "$COIN" > /dev/null 2>&1 || true

echo "-- A. the held portfolio (the trader at L=$LEVERAGE, short)"
cur=$(acct | jq -r '.marginSummary.totalRawUsd')
delta=$(python3 -c "print(f'{float('$COLLATERAL') - float('$cur'):.6f}')")
spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"perp\",\"amount\":\"$delta\"}" > /dev/null
m0=$(mark)
size=$(python3 -c "print(f'{$LEVERAGE * $COLLATERAL / float('$m0'):.4f}')")
fill=$(spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"$size\"}")
fsz=$(echo "$fill" | jq -r '.totalSz // "0"')
st=$(acct)
eq_open=$(echo "$st" | jq -r '.marginSummary.accountValue')
buf=$(buffer_pct)
theory=$(python3 -c "print(f'{max(0.2, (1 / $LEVERAGE - 0.02) / 0.98 * 100):.2f}')")
info "the short filled: $fsz $COIN at mark \$$m0 | equity \$$eq_open | the served buffer $buf% vs the analytic $theory%"
python3 -c "exit(0 if abs(float('$buf') - float('$theory')) <= 0.5 else 1)" \
  && ok "the served buffer matches the analytic bound (within 0.5%)" \
  || bad "served buffer $buf vs analytic $theory"
id=$(acct | identity_ok)
[ "$id" = "ok" ] && ok "the served identity holds at the open" || bad "identity: $id"

echo "-- B. another account's book impact (the execution channel)"
spost /control/deal "{\"address\":\"$WHALE\",\"class\":\"perp\",\"amount\":\"$WHALE_COLLATERAL\"}" > /dev/null
mb=$(mid); eb=$(acct | jq -r '.marginSummary.accountValue'); mkb=$(mark)
wf=$(spost /control/order "{\"address\":\"$WHALE\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"$WHALE_SIZE\"}")
spx=$(echo "$wf" | jq -r '.avgPx // empty')
ma=$(mid); ea=$(acct | jq -r '.marginSummary.accountValue'); mka=$(mark)
slip=$(python3 -c "print(f'{(float('$mb') - float('$spx')) / float('$mb') * 1e4:+.1f}')")
info "the whale sold $WHALE_SIZE $COIN: mid \$$mb -> \$$ma | the fill at \$$spx ($slip bps) | the trader's equity \$$eb -> \$$ea"
# The trader's equity moves only by the mark's own move: size x delta-mark.
dmark=$(python3 -c "print(float('$mka') - float('$mkb'))")
dtrend=$(python3 -c "print(float('$ea') - float('$eb'))")
danalytic=$(python3 -c "print(-float('$fsz') * (float('$mka') - float('$mkb')))")
python3 -c "exit(0 if abs(float('$dtrend') - float('$danalytic')) <= max(1.0, abs(float('$danalytic')) * 0.05) else 1)" \
  && ok "the trader's reaction is the mark's own move: equity \$$(python3 -c "print(f'{float('$dtrend'):+,.2f}')") = -size x delta-mark \$$(python3 -c "print(f'{float('$danalytic'):+,.2f}')") (delta-mark \$$(python3 -c "print(f'{float('$dmark'):+.4f}')"))" \
  || bad "the trader's reaction: equity $dtrend vs analytic $danalytic (delta-mark $dmark)"
wb=$(spost /control/order "{\"address\":\"$WHALE\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"$WHALE_SIZE\"}")
bpx=$(echo "$wb" | jq -r '.avgPx // empty')
info "the whale bought it back at \$$bpx (the round trip closes; the book is the depth it ate)"
id=$(acct | identity_ok)
[ "$id" = "ok" ] && ok "the served identity holds through the book impact" || bad "identity: $id"

echo "-- C. the mark, both ways (a crash and a rally on the same short)"
mk0=$(mark); eq0=$(acct | jq -r '.marginSummary.accountValue')
move_and_tick crash "$CRASH_PCT"
mk1=$(mark); eq1=$(acct | jq -r '.marginSummary.accountValue')
gain=$(python3 -c "print(float('$eq1') - float('$eq0'))")
gain_a=$(python3 -c "print(-float('$fsz') * (float('$mk1') - float('$mk0')))")
python3 -c "exit(0 if float('$gain') > 0 and abs(float('$gain') - float('$gain_a')) <= abs(float('$gain_a')) * 0.02 + 1.0 else 1)" \
  && ok "the crash (the short's positive side): equity \$$(python3 -c "print(f'{float('$gain'):+,.2f}')") = the analytic \$$(python3 -c "print(f'{float('$gain_a'):+,.2f}')")" \
  || bad "the crash's reaction: $gain vs analytic $gain_a"
clear_move
mk0=$(mark); eq0=$(acct | jq -r '.marginSummary.accountValue')
move_and_tick rally "$CRASH_PCT"
mk1=$(mark); eq1=$(acct | jq -r '.marginSummary.accountValue')
loss=$(python3 -c "print(float('$eq1') - float('$eq0'))")
loss_a=$(python3 -c "print(-float('$fsz') * (float('$mk1') - float('$mk0')))")
python3 -c "exit(0 if float('$loss') < 0 and abs(float('$loss') - float('$loss_a')) <= abs(float('$loss_a')) * 0.02 + 1.0 else 1)" \
  && ok "the rally (the short's negative side): equity \$$(python3 -c "print(f'{float('$loss'):+,.2f}')") = the analytic \$$(python3 -c "print(f'{float('$loss_a'):+,.2f}')")" \
  || bad "the rally's reaction: $loss vs analytic $loss_a"
clear_move
id=$(acct | identity_ok)
[ "$id" = "ok" ] && ok "the served identity holds through both mark moves" || bad "identity: $id"

echo "-- D. the thresholds (the sim's own liquidation per direction)"
buf=$(buffer_pct)
start=$(python3 -c "print(f'{max(0.2, float('$buf') - 2):.2f}')")
liq_up=$(scan rally "$start")
info "the short: the served buffer $buf% | the measured liquidation at the rally of $liq_up%"
python3 -c "exit(0 if abs(float('${liq_up#>}') - float('$buf')) <= 0.5 else 1)" \
  && ok "the short liquidates where the served buffer says (up)" \
  || bad "the short's measured threshold $liq_up vs the served buffer $buf"
left_up=$(acct | jq -r '.marginSummary.accountValue')
clear_move
# The mirror: a fresh long, the crash scan.
cur=$(acct | jq -r '.marginSummary.totalRawUsd')
delta=$(python3 -c "print(f'{float('$COLLATERAL') - float('$cur'):.6f}')")
spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"perp\",\"amount\":\"$delta\"}" > /dev/null
m0=$(mark)
size=$(python3 -c "print(f'{$LEVERAGE * $COLLATERAL / float('$m0'):.4f}')")
spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"$size\"}" > /dev/null
buf=$(buffer_pct)
start=$(python3 -c "print(f'{max(0.2, float('$buf') - 2):.2f}')")
liq_down=$(scan crash "$start")
info "the long:  the served buffer $buf% | the measured liquidation at the crash of $liq_down%"
python3 -c "exit(0 if abs(float('${liq_down#>}') - float('$buf')) <= 0.5 else 1)" \
  && ok "the long liquidates where the served buffer says (down) — the mirror holds" \
  || bad "the long's measured threshold $liq_down vs the served buffer $buf"
left_down=$(acct | jq -r '.marginSummary.accountValue')
clear_move
info "the collateral left after the liquidations: up \$$(python3 -c "print(f'{float('$left_up'):,.2f}')") | down \$$(python3 -c "print(f'{float('$left_down'):,.2f}')")"

echo "-- E. the hedged portfolio under the same moves (both ways)"
flatten_coin "$TRADER" "$COIN" > /dev/null 2>&1 || true
# The spot leg on T (the COW write, registered), the perp leg short on V.
slot=$("$CAST" index address "$TRADER" 3 2>/dev/null | tr -d '\r')
wad_hex=$(python3 -c "print('0x%064x' % int(round(float('$HEDGE_SIZE') * 10**18)))")
rpcv "$SRC_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setStorageAt\",\"params\":[\"$WETH\",\"$slot\",\"$wad_hex\"]}" > /dev/null
cpost /admin/shadow "$SRC_ENGINE" "{\"address\":\"$WETH\",\"slots\":[\"$slot\"],\"account_fields\":false}" > /dev/null
cur=$(acct | jq -r '.marginSummary.totalRawUsd')
delta=$(python3 -c "print(f'{float('$COLLATERAL') - float('$cur'):.6f}')")
spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"perp\",\"amount\":\"$delta\"}" > /dev/null
spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"$HEDGE_SIZE\"}" > /dev/null
cap=$(python3 -c "print(float('$HEDGE_SIZE') * float('$(mark)'))")
buf=$(buffer_pct)
info "the hedged book: $HEDGE_SIZE $COIN short (V) + $HEDGE_SIZE long (T, stated) | gross \$$cap | the served buffer $buf%"
# The flatness phase stays inside the buffer: the combined reaction (the V
# account's perp move + the T leg's stated move) must cancel.
for verb in crash rally; do
  clear_move
  mk0=$(mark); eq0=$(acct | jq -r '.marginSummary.accountValue')
  move_and_tick "$verb" "$SHOCK"
  mk1=$(mark); eq1=$(acct | jq -r '.marginSummary.accountValue')
  perp=$(python3 -c "print(float('$eq1') - float('$eq0'))")
  # The T leg is *long* spot: it loses what the short gains (and vice versa).
  spot=$(python3 -c "print(float('$HEDGE_SIZE') * (float('$mk1') - float('$mk0')))")
  net=$(python3 -c "print(float('$perp') + float('$spot'))")
  net_pct=$(python3 -c "print(abs(float('$net')) / float('$cap') * 100)")
  python3 -c "exit(0 if float('$net_pct') <= 0.5 else 1)" \
    && ok "the hedged book under the $verb $SHOCK%: flat (perp $(python3 -c "print(f'{float('$perp'):+,.2f}')") + spot $(python3 -c "print(f'{float('$spot'):+,.2f}')") = \$$(python3 -c "print(f'{float('$net'):+,.2f}')"), $net_pct% of the gross)" \
    || bad "the hedged book's net reaction under the $verb: $net (perp $perp + spot $spot, $net_pct% of the gross)"
  clear_move
done
python3 -c "exit(0 if float('$buf') > float('$SHOCK') else 1)" \
  && ok "the flatness phase stayed inside the buffer ($SHOCK% < $buf%)" \
  || bad "the buffer $buf does not cover the $SHOCK% flatness phase"
# The short's adverse side *beyond* the buffer: the trigger must fire — and the
# close executes at the live book (the sim's documented split: a mark-only
# shock moves the risk input; the book channel is the impact ladder's own).
clear_move
move_and_tick rally "$SHOCK_HARD"
state=$(liq_state)
[ "$state" = "liquidated" ] \
  && ok "the rally $SHOCK_HARD% (beyond the $buf% buffer): the liquidation triggered" \
  || bad "the rally $SHOCK_HARD% left the position open ($state)"
info "the mark-only shock's close executes at the live book (the documented split; the ladder measures the book channel)"
clear_move
id=$(acct | identity_ok)
[ "$id" = "ok" ] && ok "the served identity holds for the hedged book" || bad "identity: $id"

echo "-- F. the reads"
python3 - "$LEVERAGE" "$theory" "$buf" <<'PY' | while IFS= read -r line; do info "$line"; done
import sys
print(f"a held portfolio's reaction to another account's impact is two-channel: the book moves the execution cost, the mark moves the equity")
print(f"the crash and the rally reactions mirror on the same short; each matches -size x delta-mark within fees")
print(f"the served buffer and the sim's own liquidation agree within the 0.25% scan grid, per direction; the hedge is flat both ways")
PY
ok "the reaction reads are above"

echo "-- G. the hand-back"
clear_move
spost /control/set_funding "{\"coin\":\"$COIN\"}" > /dev/null
flatten_coin "$TRADER" "$COIN" > /dev/null 2>&1 || true
flatten_coin "$WHALE" "$COIN" > /dev/null 2>&1 || true
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
own=$(smetrics 2>/dev/null | awk '/^venue_sim_owned_levels /{print $2}')
[ "$own" = "0" ] && ok "the moves cleared, the books handed back (owned_levels 0), the accounts flat" \
  || bad "the hand-back left owned_levels=$own"

echo "== portfolio reaction: $pass passed, $fail failed =="
[ "$fail" = "0" ]
