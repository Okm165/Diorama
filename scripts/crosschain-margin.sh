#!/usr/bin/env bash
# The crosschain margin book: collateral bridged T -> V, a perp position held
# across scripted funding hours and a mark move — the costs and the unrealized
# PnL read the way daloc's prime-brokerage benchmarks will.
#
#   A. the live context   - V's ETH perp (mark/oracle/funding) + T's live feed
#   B. the crosschain leg - T mint -> CCTP burn/relay/mint (exact) -> the
#                           venue spot ledger -> CoreWriter spot->perp (a real
#                           action through the bridge's decoder)
#   C. the position       - a long filled on V's real book; the impact named
#   D. the costs          - N scripted funding hours advanced; the funding
#                           paid read from the account and cumFunding, in bps
#   E. the unrealized     - a mark move; the served unrealizedPnl/accountValue
#                           and the margin state read; the identity checked
#   F. the economics      - the collateral vs the notional vs the costs: the
#                           leverage, the funding in bps of notional, the
#                           unrealized coverage of the collateral
#   G. the loop closed    - close, the CoreWriter perp->spot reverse, the CCTP
#                           V->T return exact, the hand-back
#
# Run from the repo root:
#   make crosschain-margin                      # against the deployed stack
#   CAST=/path/to/cast make crosschain-margin   # when cast is not on PATH
set -euo pipefail

NS="${NS:-venue-mirror}"
TRADER="${TRADER:-0x00000000000000000000000000000000000000e5}"
COIN="${COIN:-ETH}"
AMOUNT="${AMOUNT:-20000000}"          # the crosschain collateral: 20 USDC (6-dec)
SIZE="${SIZE:-0.04}"                  # the perp position, in coin (~4.7x)
HOURS="${HOURS:-8}"                   # the scripted funding hours
RATE="${RATE:-0.0005}"                # the hourly funding rate (a long pays)
MOVE="${MOVE:-10}"                    # the mark move, percent down
CAST="${CAST:-cast}"
USDC_T="${USDC_T:-0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48}"
USDC_V="${USDC_V:-0xb88339CB7199b77E23DB6E890353E22632Ba630f}"
CORE_WRITER=0x3333333333333333333333333333333333333333
ETH_FEED="${ETH_FEED:-0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419}"

source "$(dirname "${BASH_SOURCE[0]}")/lib-cctp.sh"
require_chain_urls

# The actor is the bridge's own wallet (the burn sends from it): local mode
# uses the harness's known wallet; cluster mode derives it from the venue
# secret, so the default can never mispoint.
RELAY="${RELAY:-}"
if [ -z "$RELAY" ]; then
  if [ -n "${BRIDGE_CTRL_URL:-}" ]; then
    RELAY=0x70997970C51812dc3A010C7d01b50e0d17dc79C8
  else
    key=$(kubectl -n "$NS" get secret cctp-relay-key -o jsonpath='{.data.relay-key}' 2>/dev/null | base64 -d 2>/dev/null | tr -d '\r')
    RELAY=$("${CAST:-cast}" wallet address --private-key "$key" 2>/dev/null | tr -d '\r')
    [ -n "$RELAY" ] || { echo "RELAY unset and the cluster secret cctp-relay-key is unreadable" >&2; exit 1; }
  fi
fi

# The CoreWriter calldata for a raw action stream (the margin cycle's shape):
# the `sendRawAction` selector + offset + length + the padded stream.
core_writer_calldata() { # core_writer_calldata <stream_hex>
  python3 - "$1" <<'PY'
import sys
stream = bytes.fromhex(sys.argv[1])
arg = len(stream).to_bytes(32, "big") + stream
arg += b"\x00" * ((32 - len(stream) % 32) % 32)
print("0x17938e13" + (32).to_bytes(32, "big").hex() + arg.hex())
PY
}

# The USD-class transfer stream: version 1, action id 7 (u24 BE),
# (uint64 ntl in core wei, bool toPerp). 6-dec USDC -> 8-dec core wei is x100.
usd_class_stream() { # usd_class_stream <ntl_core_wei> <toPerp: 1|0>
  python3 - "$1" "$2" <<'PY'
import sys
print("01000007" + f"{int(sys.argv[1]):064x}" + f"{int(sys.argv[2]):064x}")
PY
}

acct() { # acct <addr> -> the served account JSON
  sim_account "$1"
}
field() { # field <json> <jq-path>
  echo "$1" | jq -r "$2"
}

echo "== crosschain margin: costs and unrealized PnL (namespace $NS) =="

echo "-- preconditions"
[ -n "$(sget /readyz || true)" ] && ok "the simulator is ready" || bad "sim /readyz is not ready"
meta=$(sipost /info '{"type":"meta"}' || true)
idx=$(echo "$meta" | python3 -c "import json,sys; print([a['name'] for a in json.load(sys.stdin)['universe']].index('$COIN'))" 2>/dev/null || true)
[ -n "$idx" ] && ok "$COIN is in the live meta (index $idx)" || bad "no $COIN in the live meta"
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true
spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true
spost /control/set_funding "{\"coin\":\"$COIN\"}" > /dev/null 2>&1 || true
flatten_coin "$TRADER" "$COIN" > /dev/null 2>&1 || true
rpcv "$DST_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setBalance\",\"params\":[\"$TRADER\",\"0x8ac7230489e80000\"]}" > /dev/null
info "the coin's overrides cleared, the trader flattened, V gas set"

echo "-- A. the live context"
ctxs=$(sipost /info '{"type":"metaAndAssetCtxs"}')
mark=$(echo "$ctxs" | jq -r ".[1][$idx].markPx // empty")
mid=$(echo "$ctxs" | jq -r ".[1][$idx].midPx // .[1][$idx].markPx // empty")
oracle=$(echo "$ctxs" | jq -r ".[1][$idx].oraclePx // empty")
live_rate=$(echo "$ctxs" | jq -r ".[1][$idx].funding // empty")
[ -n "$mark" ] && ok "V's $COIN perp is live: mark=$mark oracle=$oracle funding=$live_rate/h" || bad "no $COIN context"
feed=$(rpcv "$SRC_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$ETH_FEED\",\"data\":\"0xfeaf968c\"},\"latest\"]}" | jq -r '.result // empty')
t_price=$(python3 -c "
r='''$feed'''
print(int(r[66:130],16)/1e8 if r.startswith('0x') and len(r) >= 130 else '')
" 2>/dev/null || true)
[ -n "$t_price" ] && ok "T's live ETH/USD feed: \$$t_price (Chainlink, the real market)" || bad "T feed read: $feed"

echo "-- B. the crosschain leg (T -> CCTP -> V -> the venue ledger -> perp)"
t0=$(balance "$USDC_T" "$RELAY" "$SRC_URL")
cpost /admin/deal "$SRC_ENGINE" "{\"token\":\"$USDC_T\",\"to\":\"$RELAY\",\"amount\":\"$AMOUNT\"}" > /dev/null
t1=$(balance "$USDC_T" "$RELAY" "$SRC_URL")
# The faucet sets the balance (a slot write), not adds: the post-mint reading
# is the exact amount, and the return leg closes against it.
[ "$t1" = "$AMOUNT" ] && ok "T minted to the bridge's wallet ($t0 -> $t1, the faucet sets)" || bad "T mint: $t0 -> $t1 (wanted $AMOUNT)"
burn_resp=$(burn 0 19 "$TRADER")
burn_tx=$(echo "$burn_resp" | json 'd.get("burn_tx","")')
[ -n "$burn_tx" ] && ok "burn submitted ($burn_tx)" || bad "no burn_tx: $burn_resp"
state=$(await_relay "$burn_tx" || true)
case "$state" in
  relayed) ok "the record relayed" ;;
  *) bad "not relayed: $state" ;;
esac
v_bal=$(balance "$USDC_V" "$TRADER" "$DST_URL")
[ "$v_bal" = "$AMOUNT" ] && ok "the V mint landed exactly ($AMOUNT to the trader)" || bad "V mint: $v_bal (wanted $AMOUNT)"
spot0=$(sim_spot "$TRADER")
spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"spot\",\"amount\":\"$((AMOUNT / 1000000))\"}" > /dev/null
spot1=$(sim_spot "$TRADER")
[ "$(python3 -c "from decimal import Decimal; print('ok' if Decimal('$spot1') - Decimal('$spot0') == Decimal('$((AMOUNT / 1000000))') else 'bad')")" = "ok" ] \
  && ok "the venue spot ledger credited (+$((AMOUNT / 1000000)) USDC, the mint analogue)" \
  || bad "spot delta $spot0 -> $spot1"
perp0=$(field "$(acct "$TRADER")" '.marginSummary.totalRawUsd')
tx=$(send_tx "$TRADER" "$CORE_WRITER" "$(core_writer_calldata "$(usd_class_stream "$((AMOUNT * 100))" 1)")")
[ -n "$tx" ] && [ "$tx" != "None" ] && ok "CoreWriter spot->perp mined ($tx)" || bad "CoreWriter tx failed"
perp1=""
for _ in $(seq 1 20); do
  perp1=$(field "$(acct "$TRADER")" '.marginSummary.totalRawUsd')
  [ "$(python3 -c "from decimal import Decimal; print('ok' if Decimal('$perp1') - Decimal('$perp0') == Decimal('$((AMOUNT / 1000000))') else 'bad')")" = "ok" ] && break
  sleep 1
done
[ "$(python3 -c "from decimal import Decimal; print('ok' if Decimal('$perp1') - Decimal('$perp0') == Decimal('$((AMOUNT / 1000000))') else 'bad')")" = "ok" ] \
  && ok "the perp wallet credited exactly (+$((AMOUNT / 1000000)) USDC through the bridge's decoder)" \
  || bad "perp delta $perp0 -> $perp1"

echo "-- C. the position (a long on V's real book)"
fill=$(spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"$SIZE\"}")
fsz=$(echo "$fill" | json 'd.get("totalSz")')
avg=$(echo "$fill" | json 'd.get("avgPx")')
[ "$fsz" = "$SIZE" ] && ok "the long filled ($fsz $COIN @ $avg)" || bad "fill: $fill"
imp=$(python3 -c "print(f'{(float('$avg') - float('$mid')) / float('$mid') * 100:+.4f}')" 2>/dev/null || echo "?")
info "the fill's impact vs the mid: $imp% (a buy above the mid — the book's cost)"
st=$(acct "$TRADER")
szi=$(field "$st" '.assetPositions[0].position.szi // "0"')
notional=$(python3 -c "print(abs(float('$szi')) * float('$mark'))")
[ -n "$szi" ] && [ "$szi" != "0" ] && ok "the position: $szi $COIN, notional \$$notional" || bad "no position: $st"
[ "$(echo "$st" | identity_ok)" = "ok" ] && ok "the served identity holds (accountValue = raw + positions)" || bad "identity: $(echo "$st" | identity_ok)"

echo "-- D. the costs ($HOURS scripted funding hours on the long)"
eq0=$(field "$st" '.marginSummary.totalRawUsd')
spost /control/set_funding "{\"coin\":\"$COIN\",\"rate\":\"$RATE\"}" > /dev/null
spost /control/advance "{\"seconds\": $((HOURS * 3600))}" > /dev/null
st2=$(acct "$TRADER")
eq1=$(field "$st2" '.marginSummary.totalRawUsd')
funding=$(python3 -c "print(float('$eq0') - float('$eq1'))")
cum=$(field "$st2" '.assetPositions[0].position.cumFunding.sinceOpen // "n/a"')
python3 -c "exit(0 if float('$funding') > 0 else 1)" \
  && ok "the long paid the hours' funding: \$$(python3 -c "print(f'{float('$funding'):.4f}')") (rate $RATE/h on \$$(python3 -c "print(f'{float('$notional'):,.0f}')") notional)" \
  || bad "no funding cost landed: $funding (eq $eq0 -> $eq1)"
[ "$cum" != "n/a" ] && python3 -c "exit(0 if float('$cum') < 0 else 1)" \
  && ok "the venue reports the cost itself: cumFunding.sinceOpen = $cum" \
  || bad "cumFunding: $cum"

echo "-- E. the unrealized (a $MOVE% mark move)"
pre=$(field "$st2" '.marginSummary.accountValue')
spost /control/crash "{\"coin\":\"$COIN\",\"pct\":\"$MOVE\"}" > /dev/null
st3=$(acct "$TRADER")
post=$(field "$st3" '.marginSummary.accountValue')
unreal=$(field "$st3" '.assetPositions[0].position.unrealizedPnl // "0"')
delta=$(python3 -c "print(float('$post') - float('$pre'))")
python3 -c "exit(0 if float('$unreal') < 0 else 1)" \
  && ok "the served unrealizedPnl: \$$unreal (the long against the $MOVE% crash)" \
  || bad "unrealized: $unreal"
python3 -c "exit(0 if abs(float('$delta') - float('$unreal')) < 1 else 1)" \
  && ok "the account moved with it: accountValue $pre -> $post" \
  || bad "account delta $delta vs unrealized $unreal"
[ "$(echo "$st3" | identity_ok)" = "ok" ] && ok "the identity still holds under the crash" || bad "identity: $(echo "$st3" | identity_ok)"
used=$(field "$st3" '.marginSummary.totalMarginUsed')
python3 -c "exit(0 if float('$post') > float('$used') else 1)" \
  && ok "the margin survives: accountValue \$$post > marginUsed \$$used (no liquidation)" \
  || bad "the position is at the edge: \$$post vs margin \$$used"
spost /control/crash "{\"coin\":\"$COIN\"}" > /dev/null
ok "the crash handed back (the mark returns to live)"

echo "-- F. the economics (the crosschain margin read)"
python3 - "$AMOUNT" "$notional" "$funding" "$unreal" "$HOURS" "$RATE" <<'PY' | while IFS= read -r line; do info "$line"; done
import sys
amt, notional, funding, unreal, hours, rate = float(sys.argv[1]) / 1e6, float(sys.argv[2]), float(sys.argv[3]), float(sys.argv[4]), float(sys.argv[5]), float(sys.argv[6])
lev = notional / amt
bps = funding / notional * 1e4
apr = rate * 24 * 365 * 100
cover = unreal / amt * 100
print(f"collateral ${amt:,.2f} (bridged) | notional ${notional:,.2f} | effective leverage {lev:.1f}x")
print(f"funding ${funding:.4f} over {hours:.0f}h = {bps:.1f} bps of notional (scripted {apr:.0f}%/yr)")
print(f"unrealized ${unreal:,.2f} = {cover:+.1f}% of the collateral; the bridge leg was exact (fee 0 on the devnet)")
PY
ok "the economics read is above"

echo "-- G. the loop closed"
flatten_coin "$TRADER" "$COIN" > /dev/null
szi=$(field "$(acct "$TRADER")" '.assetPositions[0].position.szi // "0"')
[ "$szi" = "0" ] && ok "the position closed" || bad "still open: $szi"
# The reverse: CoreWriter perp -> spot (the same action with toPerp=false).
# The whole perp balance moves — the measured costs left less than the
# original collateral, and moving more than the balance would be refused.
spot2=$(sim_spot "$TRADER")
perp2=$(field "$(acct "$TRADER")" '.marginSummary.totalRawUsd')
move_wei=$(python3 -c "from decimal import Decimal; print(int(Decimal('$perp2') * Decimal(10)**8))")
move_usd=$(python3 -c "from decimal import Decimal; print(Decimal($move_wei) / Decimal(10)**8)")
tx=$(send_tx "$TRADER" "$CORE_WRITER" "$(core_writer_calldata "$(usd_class_stream "$move_wei" 0)")")
[ -n "$tx" ] && [ "$tx" != "None" ] && ok "CoreWriter perp->spot mined ($tx)" || bad "reverse CoreWriter failed"
okv=""
for _ in $(seq 1 20); do
  spot3=$(sim_spot "$TRADER")
  okv=$(python3 -c "from decimal import Decimal; print('ok' if Decimal('$spot3') - Decimal('$spot2') == Decimal('$move_usd') else 'bad')" 2>/dev/null || echo bad)
  [ "$okv" = "ok" ] && break
  sleep 1
done
[ "$okv" = "ok" ] && ok "the spot ledger credited back (+$move_usd = the whole perp balance at 8-dec — the reverse direction proven)" || bad "spot $spot2 -> $spot3"
perp3=$(field "$(acct "$TRADER")" '.marginSummary.totalRawUsd')
[ "$(python3 -c "from decimal import Decimal; print('ok' if Decimal('$perp2') - Decimal('$perp3') == Decimal('$move_usd') else 'bad')")" = "ok" ] \
  && ok "the perp side was debited (the transfer is not a mint)" || bad "perp $perp2 -> $perp3"
# The CCTP return: the trader returns the V USDC to the bridge's wallet, then
# the burn closes the loop exactly on T.
data=$("$CAST" calldata "transfer(address,uint256)" "$RELAY" "$AMOUNT")
tx=$(send_tx "$TRADER" "$USDC_V" "$data")
status=$(receipt_status "$tx")
[ "$status" = "0x1" ] && ok "the trader returned the V USDC to the bridge's wallet" || bad "return transfer failed (status=$status)"
r0=$(balance "$USDC_V" "$RELAY" "$DST_URL")
[ "$r0" -ge "$AMOUNT" ] && ok "the bridge's wallet holds the return ($r0)" || bad "bridge wallet short: $r0"
burn_tx=$(burn 19 0 "$RELAY" | json 'd["burn_tx"]')
[ -n "$burn_tx" ] && [ "$burn_tx" != "None" ] && ok "V burn submitted ($burn_tx)" || bad "no V burn_tx"
state=$(await_relay "$burn_tx" || true)
case "$state" in
  relayed) ok "the return record relayed" ;;
  *) bad "return not relayed: $state" ;;
esac
t2=$(balance "$USDC_T" "$RELAY" "$SRC_URL")
[ "$t2" = "$t1" ] && ok "the loop closed exactly on T ($t1 -> $t2)" || bad "T delta: $t1 -> $t2"

echo "-- hand-back"
spost /control/set_funding "{\"coin\":\"$COIN\"}" > /dev/null
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
flatten_coin "$TRADER" "$COIN" > /dev/null 2>&1 || true
own=$(smetrics 2>/dev/null | awk '/^venue_sim_owned_levels /{print $2}')
[ "$own" = "0" ] && ok "the overrides cleared, the book handed back (owned_levels 0)" || bad "owned_levels=$own"

echo "== crosschain margin: $pass passed, $fail failed"
[ "$fail" = "0" ]
