#!/usr/bin/env bash
# The portfolio simulation: a delta-neutral basis book across the two mirrors,
# with the carry and a shock measured — the prime-brokerage capital-efficiency
# read the daloc benchmarks build on.
#
#   A. the live context   - V's perps (marks, fundings) + T's live ETH/USD feed
#   B. the basis book     - the long leg (WETH dealt on T, registered COW), the
#                           short leg (the ETH perp filled on V's real book)
#   C. the delta          - coin and dollar deltas (the hedge's residual)
#   D. the carry          - a scripted hourly funding, one hour advanced, the
#                           collected carry measured against the capital
#   E. the shock          - a crash on V; the hedged PnL (the T leg under the
#                           same shock, stated — one market, two venues)
#   F. capital efficiency - the table: notional, capital, effective leverage,
#                           annualized carry, shock PnL on capital
#   G. the hand-back      - flatten, clear overrides, done
#
# Run from the repo root:
#   make portfolio-sim                      # against the deployed stack
#   CAST=/path/to/cast make portfolio-sim   # when cast is not on PATH
set -euo pipefail

NS="${NS:-diorama-venue}"
TRADER="${TRADER:-0x00000000000000000000000000000000000000e5}"
COIN="${COIN:-ETH}"
WETH_AMOUNT="${WETH_AMOUNT:-4}"        # the long leg, in WETH
FUNDS="${FUNDS:-20000}"                # the perp margin dealt on V
RATE="${RATE:-0.00002}"                # the scripted hourly funding rate
SHOCK="${SHOCK:-15}"                   # the crash's drop, percent
WETH="${WETH:-0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2}"
ETH_FEED="${ETH_FEED:-0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419}"
CAST="${CAST:-cast}"

source "$(dirname "${BASH_SOURCE[0]}")/lib-cctp.sh"
require_chain_urls

echo "== portfolio simulation (namespace $NS) =="

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

echo "-- A. the live context"
ctxs=$(sipost /info '{"type":"metaAndAssetCtxs"}')
mark=$(echo "$ctxs" | jq -r ".[1][$idx].markPx // empty")
mid=$(echo "$ctxs" | jq -r ".[1][$idx].midPx // .[1][$idx].markPx // empty")
oracle=$(echo "$ctxs" | jq -r ".[1][$idx].oraclePx // empty")
live_rate=$(echo "$ctxs" | jq -r ".[1][$idx].funding // empty")
[ -n "$mark" ] && ok "V's $COIN perp is live: mark=$mark oracle=$oracle funding=$live_rate/h" || bad "no $COIN context in the live meta"
# T's live ETH/USD: the Chainlink feed the scenario watches (8-dec answer).
feed=$(rpcv "$SRC_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$ETH_FEED\",\"data\":\"0xfeaf968c\"},\"latest\"]}" | jq -r '.result // empty')
t_price=$(python3 -c "
r='''$feed'''
print(int(r[66:130],16)/1e8 if r.startswith('0x') and len(r) >= 130 else '')
" 2>/dev/null || true)
[ -n "$t_price" ] && ok "T's live ETH/USD feed: \$$t_price (Chainlink, the real market)" || bad "T feed read: $feed"

echo "-- B. the basis book (long WETH on T, short $COIN perp on V)"
# The long leg: the devnet faucet's silent slot write, registered with the
# engine (the COW mark) — the scenario's exact pattern (WETH's balance
# mapping is standard, slot 3).
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
# The short leg: the perp margin dealt on V, then the fill at the real book.
spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"perp\",\"amount\":\"$FUNDS\"}" > /dev/null
fill=$(spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"$WETH_AMOUNT\"}")
filled=$(echo "$fill" | jq -r '.totalSz // "ERR"')
avgpx=$(echo "$fill" | jq -r '.avgPx // "ERR"')
[ "$filled" = "$WETH_AMOUNT" ] && ok "the short leg filled at the real book: $filled $COIN @ $avgpx" || bad "fill: $fill"
impact=$(python3 -c "print(f\"{(float('$avgpx') - float('$mid')) / float('$mid') * 100:+.3f}%\")" 2>/dev/null || echo "?")
info "the fill's impact vs the mid: $impact (a sell below the mid — the book's cost)"

echo "-- C. the delta (the hedge's residual)"
st=$(sim_account "$TRADER")
sz=$(echo "$st" | jq -r ".assetPositions[0].position.szi // empty" | python3 -c "import sys; print(sys.stdin.read().strip() or '0')")
net_coin=$(python3 -c "print(f\"{float('$WETH_AMOUNT') + float('$sz'):+.6f}\")")
net_usd=$(python3 -c "print(f\"{float('$WETH_AMOUNT') * float('$t_price') + float('$sz') * float('$mark'):+,.2f}\")")
info "the net delta: $net_coin $COIN / \$$net_usd (spot long + perp short)"
python3 -c "exit(0 if abs(float('$net_coin')) <= 0.01 else 1)" \
  && ok "the book is delta-neutral in coin terms (|net| <= 0.01 $COIN)" \
  || bad "the delta is off: $net_coin"

echo "-- D. the carry (funding on the short, one hour)"
eq0=$(echo "$st" | jq -r '.marginSummary.totalRawUsd')
spost /control/set_funding "{\"coin\":\"$COIN\",\"rate\":\"$RATE\"}" > /dev/null
spost /control/advance '{"seconds":3600}' > /dev/null
st2=$(sim_account "$TRADER")
eq1=$(echo "$st2" | jq -r '.marginSummary.totalRawUsd')
carry=$(python3 -c "print(f\"{float('$eq1') - float('$eq0'):+.4f}\")")
carry_f=$(python3 -c "print(float('$eq1') - float('$eq0'))")
python3 -c "exit(0 if float('$carry') > 0 else 1)" \
  && ok "the short collected the hour's funding: \$$carry (rate $RATE/h on \$$(python3 -c "print(f\"{abs(float('$sz')) * float('$oracle'):,.0f}\")") notional)" \
  || bad "the carry did not land: $carry (eq $eq0 -> $eq1)"

echo "-- E. the shock (a crash on V; the T leg under the same shock, stated)"
pre_shock=$(echo "$st2" | jq -r '.marginSummary.accountValue')
spost /control/crash "{\"coin\":\"$COIN\",\"pct\":\"$SHOCK\"}" > /dev/null
st3=$(sim_account "$TRADER")
post_shock=$(echo "$st3" | jq -r '.marginSummary.accountValue')
perp_f=$(python3 -c "print(float('$post_shock') - float('$pre_shock'))")
spot_f=$(python3 -c "print(-float('$WETH_AMOUNT') * float('$t_price') * float('$SHOCK') / 100)")
hedged_f=$(python3 -c "print(float('$perp_f') + float('$spot_f'))")
perp_pnl=$(python3 -c "print(f\"{float('$perp_f'):+,.2f}\")")
spot_pnl=$(python3 -c "print(f\"{float('$spot_f'):+,.2f}\")")
hedged=$(python3 -c "print(f\"{float('$hedged_f'):+,.2f}\")")
python3 -c "exit(0 if float('$perp_f') > 0 else 1)" \
  && ok "the short gained on the crash: \$$perp_pnl (the mark fell $SHOCK%)" \
  || bad "the short leg did not gain: $perp_pnl"
info "the hedged book: perp $perp_pnl + spot $spot_pnl (same shock on T, stated) = \$$hedged"
spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null
ok "the crash handed back (the mark returns to live)"

echo "-- F. capital efficiency (the prime-brokerage read)"
margin=$(echo "$st3" | jq -r '.marginSummary.totalMarginUsed')
python3 - "$WETH_AMOUNT" "$t_price" "$mark" "$margin" "$carry_f" "$hedged_f" "$RATE" <<'PY' | while IFS= read -r line; do info "$line"; done
import sys
weth, tpx, mark, margin, carry, hedged, rate = map(float, sys.argv[1:8])
notional = weth * tpx
capital = notional + margin          # the spot leg's cost + the perp's margin
lev = notional / capital if capital else 0.0
carry_yr = carry * 24 * 365 / capital * 100 if capital else 0.0
shock_pct = hedged / capital * 100 if capital else 0.0
print(f"notional ${notional:,.0f} | capital ${capital:,.0f} (spot ${notional:,.0f} + margin ${margin:,.0f})")
print(f"effective leverage {lev:.2f}x | annualized carry {carry_yr:.2f}% on capital | shock PnL {shock_pct:+.2f}% on capital")
print(f"the hedge's residual under the {sys.argv[0]} shock is the delta's mark-feed basis, not a market loss")
PY
ok "the capital-efficiency read is above"

echo "-- G. the hand-back"
spost /control/set_funding "{\"coin\":\"$COIN\"}" > /dev/null
# Flatten first: the close itself consumes a level. Then the book hand-back
# (absent levels = hand back; the order-proof's shape) clears the fill
# consumptions and the owned-levels gauge returns to 0.
flatten_coin "$TRADER" "$COIN" > /dev/null 2>&1 || true
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
own=$(smetrics 2>/dev/null | awk '/^venue_sim_owned_levels /{print $2}')
[ "$own" = "0" ] && ok "the overrides cleared, the book handed back (owned_levels 0), the trader flattened" \
  || bad "the hand-back left owned_levels=$own"

echo "== portfolio simulation: $pass passed, $fail failed"
[ "$fail" = "0" ]
