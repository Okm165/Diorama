#!/usr/bin/env bash
# The basis capture: the live T-vs-V basis sampled as a series, then traded —
# long the T spot leg, short the V perp leg. The cross-venue read the daloc
# capital-efficiency benchmarks build on: one market, two venues, and the
# difference is the trade.
#
#   A. the series   - T's Chainlink ETH/USD vs V's live perp mark, sampled
#                     SAMPLES times: min/mean/max/std in bps, the live
#                     funding stated
#   B. the book     - the long leg (WETH dealt on T, COW-registered), the
#                     short leg (the perp fill on V's real book, its impact
#                     against the quoted mark)
#   C. the basis    - the book's entry basis (the fill's own, and the quoted
#                     mark's) and the delta (coin and dollar)
#   D. the capture  - one funding hour at the live rate; the basis re-sampled;
#                     the carry read from the venue's own cumFunding, the
#                     realized basis move from the samples, and the identity:
#                     the served equity delta = the capture + the carry
#   E. the read     - notional, capital, the basis in bps, the carry in bps
#   F. the hand-back - flatten, clear overrides, done
#
# Run from the repo root:
#   make basis-capture                      # against the deployed stack
#   CAST=/path/to/cast make basis-capture   # when cast is not on PATH
set -euo pipefail

NS="${NS:-venue-mirror}"
TRADER="${TRADER:-0x00000000000000000000000000000000000000e5}"
COIN="${COIN:-ETH}"
WETH_AMOUNT="${WETH_AMOUNT:-4}"        # the long leg, in WETH
FUNDS="${FUNDS:-20000}"                # the perp margin dealt on V
SAMPLES="${SAMPLES:-12}"               # the series' sample count
INTERVAL="${INTERVAL:-2}"              # seconds between samples
WETH="${WETH:-0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2}"
ETH_FEED="${ETH_FEED:-0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419}"
CAST="${CAST:-cast}"

source "$(dirname "${BASH_SOURCE[0]}")/lib-cctp.sh"
require_chain_urls

echo "== basis capture (namespace $NS) =="

# T's live ETH/USD: the Chainlink feed (8-dec answer, bytes 66..130).
t_price_now() {
  local feed
  feed=$(rpcv "$SRC_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$ETH_FEED\",\"data\":\"0xfeaf968c\"},\"latest\"]}" | jq -r '.result // empty')
  python3 -c "
r='''$feed'''
print(int(r[66:130],16)/1e8 if r.startswith('0x') and len(r) >= 130 else '')
"
}

# V's live perp mark, from the simulator's own meta context.
v_mark_now() {
  sipost /info '{"type":"metaAndAssetCtxs"}' | jq -r ".[1][$idx].markPx // empty"
}

echo "-- preconditions"
[ -n "$(sget /readyz || true)" ] && ok "the simulator is ready (market plane fresh)" || bad "sim /readyz is not ready"
meta=$(sipost /info '{"type":"meta"}' || true)
idx=$(echo "$meta" | python3 -c "import json,sys; print([a['name'] for a in json.load(sys.stdin)['universe']].index('$COIN'))" 2>/dev/null || true)
[ -n "$idx" ] && ok "$COIN is in the live meta (index $idx)" || bad "no $COIN in the live meta"
# Idempotent clear: the coin's scenario overrides + the trader's book.
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true
spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true
spost /control/set_funding "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true
flatten_coin "$TRADER" "$COIN" > /dev/null 2>&1 || true
info "the coin's overrides cleared, the trader flattened"

echo "-- A. the live basis series ($SAMPLES samples, ${INTERVAL}s apart)"
series=""
for _ in $(seq 1 "$SAMPLES"); do
  tp=$(t_price_now)
  vm=$(v_mark_now)
  [ -n "$tp" ] && [ -n "$vm" ] && series="$series $tp $vm"
  sleep "$INTERVAL"
done
read -r entry_spot entry_mark series_stats <<< "$(python3 -c "
import statistics
vals = [float(v) for v in '''$series'''.split()]
pairs = list(zip(vals[0::2], vals[1::2]))
if not pairs:
    print('0 0 0 0 0 0 0')
else:
    bps = [(m - s) / s * 1e4 for s, m in pairs]
    print(pairs[0][0], pairs[0][1],
          f'{min(bps):+.1f}', f'{statistics.fmean(bps):+.1f}', f'{max(bps):+.1f}',
          f'{statistics.pstdev(bps):.1f}', len(pairs))
")"
stats=$(echo "$series_stats" | awk '{print "min "$1" bps | mean "$2" bps | max "$3" bps | std "$4" bps | n="$5}')
[ "$(echo "$series_stats" | awk '{print $5}')" -ge $((SAMPLES - 1)) ] \
  && ok "the live series read ($stats)" \
  || bad "the series is too short: $series_stats"
ctxs=$(sipost /info '{"type":"metaAndAssetCtxs"}')
live_rate=$(echo "$ctxs" | jq -r ".[1][$idx].funding // empty")
oracle=$(echo "$ctxs" | jq -r ".[1][$idx].oraclePx // empty")
info "the live carry: funding=$live_rate/h (V's own rate; T's feed is the real market)"
python3 -c "exit(0 if float('$entry_mark') > 0 else 1)" \
  && ok "the quoted deviation: mark \$$entry_mark vs T \$$entry_spot (the perp's premium is the trade)" \
  || bad "no live deviation read: mark=$entry_mark spot=$entry_spot"

echo "-- B. the book (long WETH on T, short $COIN perp on V)"
slot=$("$CAST" index address "$TRADER" 3 2>/dev/null | tr -d '\r')
[ -n "$slot" ] && ok "the WETH balance slot resolves ($slot)" || bad "cast index failed (set CAST=)"
wad_hex=$(python3 -c "print('0x%064x' % int(round(float('$WETH_AMOUNT') * 10**18)))")
wad_dec=$(python3 -c "print(int(round(float('$WETH_AMOUNT') * 10**18)))")
rpcv "$SRC_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setStorageAt\",\"params\":[\"$WETH\",\"$slot\",\"$wad_hex\"]}" > /dev/null
cpost /admin/shadow "$SRC_ENGINE" "{\"address\":\"$WETH\",\"slots\":[\"$slot\"],\"account_fields\":false}" > /dev/null
wbal=$(rpcv "$SRC_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$WETH\",\"data\":\"0x70a08231000000000000000000000000${TRADER:2}\"},\"latest\"]}" | jq -r '.result // empty')
wbal_dec=$(python3 -c "print(int('$wbal', 16) if '$wbal'.startswith('0x') else -1)" 2>/dev/null || echo -1)
[ "$wbal_dec" = "$wad_dec" ] \
  && ok "the long leg landed on T: $WETH_AMOUNT WETH (a silent COW write, registered)" \
  || bad "the WETH deal landed wrong: $wbal"
spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"perp\",\"amount\":\"$FUNDS\"}" > /dev/null
fill=$(spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"$WETH_AMOUNT\"}")
filled=$(echo "$fill" | jq -r '.totalSz // "ERR"')
avgpx=$(echo "$fill" | jq -r '.avgPx // "ERR"')
[ "$filled" = "$WETH_AMOUNT" ] && ok "the short leg filled at the real book: $filled $COIN @ $avgpx" || bad "fill: $fill"
impact=$(python3 -c "print(f\"{(float('$avgpx') - float('$entry_mark')) / float('$entry_mark') * 100:+.3f}%\")" 2>/dev/null || echo "?")
info "the fill's impact vs the quoted mark: $impact (a sell below the mark — the book's cost)"

echo "-- C. the basis (the book's own entry basis and its delta)"
st=$(sim_account "$TRADER")
sz=$(echo "$st" | jq -r ".assetPositions[0].position.szi // empty" | python3 -c "import sys; print(sys.stdin.read().strip() or '0')")
eq0=$(echo "$st" | jq -r '.marginSummary.accountValue')
fill_basis=$(python3 -c "print(f\"{float('$avgpx') - float('$entry_spot'):+.4f}\")")
fill_basis_bps=$(python3 -c "print(f\"{(float('$avgpx') - float('$entry_spot')) / float('$entry_spot') * 1e4:+.1f}\")")
info "the entry basis: the fill's own \$$fill_basis ($fill_basis_bps bps vs T's live feed)"
net_coin=$(python3 -c "print(f\"{float('$WETH_AMOUNT') + float('$sz'):+.6f}\")")
net_usd=$(python3 -c "print(f\"{float('$WETH_AMOUNT') * float('$entry_spot') + float('$sz') * float('$entry_mark'):+,.2f}\")")
info "the net delta: $net_coin $COIN / \$$net_usd (spot long + perp short)"
python3 -c "exit(0 if abs(float('$net_coin')) <= 0.01 else 1)" \
  && ok "the book is delta-neutral in coin terms (|net| <= 0.01 $COIN)" \
  || bad "the delta is off: $net_coin"

echo "-- D. the capture (one funding hour at the live rate, the basis re-sampled)"
spost /control/set_funding "{\"coin\":\"$COIN\",\"rate\":\"$live_rate\"}" > /dev/null
spost /control/advance '{"seconds":3600}' > /dev/null
st2=$(sim_account "$TRADER")
eq1=$(echo "$st2" | jq -r '.marginSummary.accountValue')
cum=$(echo "$st2" | jq -r '.assetPositions[0].position.cumFunding.sinceOpen // "n/a"')
exit_spot=$(t_price_now)
exit_mark=$(v_mark_now)
u0=$(echo "$st" | jq -r '.assetPositions[0].position.unrealizedPnl // "0"')
u1=$(echo "$st2" | jq -r '.assetPositions[0].position.unrealizedPnl // "0"')
carry_f=$(python3 -c "print(float('$cum'))" 2>/dev/null || echo 0)
equity_f=$(python3 -c "print(float('$eq1') - float('$eq0'))")
mark_pnl=$(python3 -c "print(float('$u1') - float('$u0'))")
spot_move=$(python3 -c "print(float('$WETH_AMOUNT') * (float('$exit_spot') - float('$entry_spot')))")
capture_f=$(python3 -c "print(float('$mark_pnl') + float('$spot_move'))")
residual=$(python3 -c "print(float('$equity_f') - float('$carry_f') - float('$mark_pnl'))")
python3 -c "exit(0 if float('$carry_f') > 0 else 1)" \
  && ok "the short collected the hour's carry: \$$(python3 -c "print(f'{float('$carry_f'):+.4f}')") (the venue's own cumFunding, rate $live_rate/h)" \
  || bad "the carry did not land: cumFunding=$cum"
python3 -c "exit(0 if abs(float('$residual')) <= 0.01 else 1)" \
  && ok "the venue's identity holds: the served equity delta \$$(python3 -c "print(f'{float('$equity_f'):+,.4f}')") = the served unrealized delta \$$(python3 -c "print(f'{float('$mark_pnl'):+,.4f}')") + the carry \$$(python3 -c "print(f'{float('$carry_f'):+,.4f}')") (residual \$$(python3 -c "print(f'{float('$residual'):+.4f}')"))" \
  || bad "the venue's identity broke: equity $equity_f vs unrealized+carry $mark_pnl+$carry_f (residual $residual)"
quoted_move=$(python3 -c "print(float('$WETH_AMOUNT') * ((float('$entry_mark') - float('$entry_spot')) - (float('$exit_mark') - float('$exit_spot'))))")
info "the quoted basis move (the samples): $WETH_AMOUNT x (entry basis \$$(python3 -c "print(f\"{float('$entry_mark') - float('$entry_spot'):+.4f}\")") - exit basis \$$(python3 -c "print(f\"{float('$exit_mark') - float('$exit_spot'):+.4f}\")")) = \$$(python3 -c "print(f'{float('$quoted_move'):+,.4f}')")"
info "the served capture: the unrealized delta \$$(python3 -c "print(f'{float('$mark_pnl'):+,.2f}')") + the T leg's move \$$(python3 -c "print(f'{float('$spot_move'):+,.2f}')") = \$$(python3 -c "print(f'{float('$capture_f'):+,.2f}')") (the cross-venue half the V account cannot see)"
info "the fill's own basis \$$fill_basis ($fill_basis_bps bps) — the book's cost against the quoted deviation"

echo "-- E. the read (the cross-venue basis book)"
margin=$(echo "$st2" | jq -r '.marginSummary.totalMarginUsed')
python3 - "$WETH_AMOUNT" "$entry_spot" "$margin" "$quoted_move" "$capture_f" "$carry_f" "$fill_basis_bps" "$live_rate" <<'PY' | while IFS= read -r line; do info "$line"; done
import sys
weth, spot, margin, quoted, capture, carry, basis_bps, rate = map(float, sys.argv[1:9])
notional = weth * spot
capital = notional + margin          # the spot leg's cost + the perp's margin
lev = notional / capital if capital else 0.0
carry_bps = carry / notional * 1e4
carry_yr = carry * 24 * 365 / capital * 100 if capital else 0.0
print(f"notional ${notional:,.0f} | capital ${capital:,.0f} (spot ${notional:,.0f} + margin ${margin:,.0f}) | effective leverage {lev:.2f}x")
print(f"the entry basis {basis_bps:+.1f} bps | the quoted basis move {quoted:+,.2f} | the served capture {capture:+,.2f} | the hour's carry {carry:+,.4f} ({carry_bps:+.1f} bps of notional, {carry_yr:.2f}%/yr on capital)")
print(f"the book's residual is the mark-feed basis the delta cannot see; the carry is the venue's own rate ({rate}/h)")
PY
ok "the cross-venue read is above"

echo "-- F. the hand-back"
spost /control/set_funding "{\"coin\":\"$COIN\"}" > /dev/null
flatten_coin "$TRADER" "$COIN" > /dev/null 2>&1 || true
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
own=$(smetrics 2>/dev/null | awk '/^venue_sim_owned_levels /{print $2}')
[ "$own" = "0" ] && ok "the overrides cleared, the book handed back (owned_levels 0), the trader flattened" \
  || bad "the hand-back left owned_levels=$own"

echo "== basis capture: $pass passed, $fail failed"
[ "$fail" = "0" ]
