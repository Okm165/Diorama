#!/usr/bin/env bash
# The crosschain margin's deeper reads (the suite's siblings):
#   A. the funding sign flip  - a negative rate: the long *collects* (the
#                               venue's own cumFunding goes positive), the
#                               equity identity holds
#   B. recoverable after a forced close - a levered position liquidated by the
#                               crash; the remaining margin still moves back
#                               (the CoreWriter perp->spot) and the bridged
#                               leg still returns (V burn -> T mint exact)
#
# Run from the repo root:
#   make crosschain-recovery                      # against the deployed stack
#   CAST=/path/to/cast make crosschain-recovery   # when cast is not on PATH
set -euo pipefail
NS="${NS:-diorama-venue}"
TRADER="${TRADER:-0x00000000000000000000000000000000000000e5}"
COIN="${COIN:-ETH}"
COLLATERAL="${COLLATERAL:-1000}"
LEVERAGE="${LEVERAGE:-10}"
RATE="${RATE:--0.0005}"               # negative: the long collects
CAST="${CAST:-cast}"
USDC_V="${USDC_V:-0xb88339CB7199b77E23DB6E890353E22632Ba630f}"
USDC_T="${USDC_T:-0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48}"
CORE_WRITER=0x3333333333333333333333333333333333333333

source scripts/lib-cctp.sh
require_chain_urls

RELAY="${RELAY:-}"
if [ -z "$RELAY" ]; then
  if [ -n "${BRIDGE_CTRL_URL:-}" ]; then
    RELAY=0x70997970C51812dc3A010C7d01b50e0d17dc79C8
  else
    key=$(kubectl -n "$NS" get secret cctp-relay-key -o jsonpath='{.data.relay-key}' 2>/dev/null | base64 -d 2>/dev/null | tr -d '\r')
    RELAY=$("${CAST:-cast}" wallet address --private-key "$key" 2>/dev/null | tr -d '\r')
  fi
fi
[ -n "$RELAY" ] || { echo "RELAY unresolved" >&2; exit 1; }

# The suite's own helpers (the CoreWriter stream shapes), copied so the probe
# drives the same wire the suite does.
core_writer_calldata() { # core_writer_calldata <stream_hex>
  python3 - "$1" <<'PY'
import sys
stream = bytes.fromhex(sys.argv[1])
arg = len(stream).to_bytes(32, "big") + stream
arg += b"\x00" * ((32 - len(stream) % 32) % 32)
print("0x17938e13" + (32).to_bytes(32, "big").hex() + arg.hex())
PY
}
usd_class_stream() { # usd_class_stream <ntl_core_wei> <toPerp: 1|0>
  python3 - "$1" "$2" <<'PY'
import sys
print("01000007" + f"{int(sys.argv[1]):064x}" + f"{int(sys.argv[2]):064x}")
PY
}
AMOUNT="${AMOUNT:-1000000}"           # the lib's burn helper reads it

acct()   { sim_account "$TRADER"; }
szi_of() { acct | jq -r '.assetPositions[0].position.szi // "0"' 2>/dev/null; }
liq_state() { [ "$(szi_of)" = "0" ] && echo liquidated || echo open; }
mark()   { sipost /info '{"type":"metaAndAssetCtxs"}' | jq -r ".[1][$idx].markPx // empty"; }
clear_move() { spost /control/crash "{\"coin\":\"$COIN\"}" >/dev/null 2>&1 || true; spost /control/rally "{\"coin\":\"$COIN\"}" >/dev/null 2>&1 || true; }
move_and_tick() { clear_move; spost /control/$1 "{\"coin\":\"$COIN\",\"pct\":\"$2\"}" >/dev/null; spost /control/advance '{"seconds":1}' >/dev/null; }

echo "== crosschain recovery: $(date -u +%H:%M:%S) =="
meta=$(sipost /info '{"type":"meta"}' || true)
idx=$(echo "$meta" | python3 -c "import json,sys; print([a['name'] for a in json.load(sys.stdin)['universe']].index('$COIN'))" 2>/dev/null || true)
[ -n "$idx" ] && ok "$COIN in the live meta (index $idx)" || bad "no $COIN"
clear_move; spost /control/set_book "{\"coin\":\"$COIN\"}" >/dev/null 2>&1 || true
spost /control/set_funding "{\"coin\":\"$COIN\"}" >/dev/null 2>&1 || true
flatten_coin "$TRADER" "$COIN" >/dev/null 2>&1 || true
# V gas (the suite's own precondition): a zero-balance sender's pending tx is
# silently dropped at mining, so every send needs the trader funded.
rpcv "$DST_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setBalance\",\"params\":[\"$TRADER\",\"0x8ac7230489e80000\"]}" > /dev/null
ok "V gas set for the trader"

echo "-- A. the funding sign flip (a negative rate: the long collects)"
cur=$(acct | jq -r '.marginSummary.totalRawUsd')
delta=$(python3 -c "print(f'{float('$COLLATERAL') - float('$cur'):.6f}')")
spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"perp\",\"amount\":\"$delta\"}" >/dev/null
spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"0.04\"}" >/dev/null
u0=$(acct | jq -r '.assetPositions[0].position.unrealizedPnl // "0"')
e0=$(acct | jq -r '.marginSummary.accountValue')
spost /control/set_funding "{\"coin\":\"$COIN\",\"rate\":\"$RATE\"}" >/dev/null
spost /control/advance '{"seconds":3600}' >/dev/null
u1=$(acct | jq -r '.assetPositions[0].position.unrealizedPnl // "0"')
e1=$(acct | jq -r '.marginSummary.accountValue')
cum=$(acct | jq -r '.assetPositions[0].position.cumFunding.sinceOpen // "n/a"')
m1=$(mark)
carry=$(python3 -c "print(float('$cum'))")
exp=$(python3 -c "print(-0.04 * float('$m1') * float('$RATE'))")
deq=$(python3 -c "print(float('$e1') - float('$e0'))")
du=$(python3 -c "print(float('$u1') - float('$u0'))")
python3 -c "exit(0 if float('$carry') > 0 else 1)" \
  && ok "the long collected at a negative rate: cumFunding.sinceOpen = +$cum (rate $RATE/h)" \
  || bad "the sign did not flip: cumFunding=$cum (rate $RATE)"
python3 -c "exit(0 if abs(float('$carry') - float('$exp')) <= abs(float('$exp')) * 0.05 + 0.001 else 1)" \
  && ok "the carry matches rate x notional: \$$carry vs the expected \$$exp" \
  || bad "carry $carry vs expected $exp"
python3 -c "exit(0 if abs(float('$deq') - (float('$du') + float('$carry'))) <= 0.01 else 1)" \
  && ok "the identity holds under the flip: equity delta \$$deq = unrealized \$$du + carry \$$carry" \
  || bad "identity broke: equity $deq vs unrealized $du + carry $carry"
spost /control/set_funding "{\"coin\":\"$COIN\"}" >/dev/null
flatten_coin "$TRADER" "$COIN" >/dev/null 2>&1 || true

echo "-- B. recoverable after a forced close (the liquidation, then the way back)"
cur=$(acct | jq -r '.marginSummary.totalRawUsd')
delta=$(python3 -c "print(f'{float('$COLLATERAL') - float('$cur'):.6f}')")
spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"perp\",\"amount\":\"$delta\"}" >/dev/null
m0=$(mark)
size=$(python3 -c "print(f'{$LEVERAGE * $COLLATERAL / float('$m0'):.4f}')")
spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"$size\"}" >/dev/null
buf=$(acct | python3 -c "
import json, sys
d = json.load(sys.stdin)
v = float(d['marginSummary']['accountValue']); m = float(d.get('crossMaintenanceMarginUsed') or 0)
p = d['assetPositions'][0]['position']; s = abs(float(p['szi'])); px = float(p['positionValue']) / s
print(f'{(v - m) / (s * px) * 100:.2f}')")
pct=$(python3 -c "print(f'{max(0.2, float('$buf') - 2):.2f}')")
n=0
while [ "$n" -lt 80 ]; do
  move_and_tick crash "$pct"
  [ "$(liq_state)" = "liquidated" ] && break
  pct=$(python3 -c "print(f'{float('$pct') + 0.25:.2f}')"); n=$((n+1))
done
[ "$(liq_state)" = "liquidated" ] \
  && ok "the levered position was liquidated by the crash (at $pct%, the served buffer was $buf%)" \
  || bad "no liquidation by $pct% (buffer $buf)"
clear_move
left=$(acct | jq -r '.marginSummary.totalRawUsd')
python3 -c "exit(0 if float('$left') > 0 else 1)" \
  && ok "the remaining margin is intact (\$$left — recoverable, not zeroed)" \
  || bad "the margin vanished: $left"
# The way back: the whole perp balance to the spot ledger (the CoreWriter).
spot0=$(sim_spot "$TRADER")
move_wei=$(python3 -c "from decimal import Decimal; print(int(Decimal('$left') * Decimal(10)**8))")
move_usd=$(python3 -c "from decimal import Decimal; print(Decimal($move_wei) / Decimal(10)**8)")
tx=$(send_tx "$TRADER" "$CORE_WRITER" "$(core_writer_calldata "$(usd_class_stream "$move_wei" 0)")")
[ -n "$tx" ] && [ "$tx" != "None" ] && ok "the remaining margin moved perp->spot (CoreWriter $tx)" || bad "CoreWriter failed"
okv=""
for _ in $(seq 1 20); do
  spot1=$(sim_spot "$TRADER")
  okv=$(python3 -c "from decimal import Decimal; print('ok' if Decimal('$spot1') - Decimal('$spot0') == Decimal('$move_usd') else 'bad')" 2>/dev/null || echo bad)
  [ "$okv" = "ok" ] && break
  sleep 1
done
[ "$okv" = "ok" ] && ok "the spot ledger credited the whole amount (+\$$move_usd)" || bad "spot $spot0 -> $spot1"
# The bridged leg returns: the trader's V USDC (the earlier hook lane's own
# credit) burns and mints exactly on T.
ret=1000000
held=$(balance "$USDC_V" "$TRADER" "$DST_URL")
if [ "${held:-0}" -lt "$ret" ]; then
  # The return leg's float: a devnet-owned deal on V (the engine's own faucet).
  cpost /admin/deal "$DST_ENGINE" "{\"token\":\"$USDC_V\",\"to\":\"$TRADER\",\"amount\":\"$ret\"}" > /dev/null
  held=$(balance "$USDC_V" "$TRADER" "$DST_URL")
  info "the trader's V USDC funded by the deal ($held)"
fi
if [ "${held:-0}" -ge "$ret" ]; then
  t1=$(balance "$USDC_T" "$RELAY" "$SRC_URL")
  data=$("$CAST" calldata "transfer(address,uint256)" "$RELAY" "$ret")
  tx=$(send_tx "$TRADER" "$USDC_V" "$data")
  [ "$(receipt_status "$tx")" = "0x1" ] && ok "the trader returned $ret base units of V USDC to the bridge" || bad "return transfer failed"
  burn_tx=$(burn 19 0 "$RELAY" | json 'd["burn_tx"]')
  state=$(await_relay "$burn_tx" || true)
  case "$state" in relayed) ok "the return record relayed" ;; *) bad "return: $state" ;; esac
  t2=$(balance "$USDC_T" "$RELAY" "$SRC_URL")
  [ "$t2" = "$((t1 + ret))" ] && ok "the bridged leg closed exactly on T ($t1 -> $t2)" || bad "T delta: $t1 -> $t2"
else
  info "the trader holds $held base units of V USDC (< $ret); the bridged return is skipped"
fi

echo "-- the hand-back"
clear_move
spost /control/set_funding "{\"coin\":\"$COIN\"}" >/dev/null
flatten_coin "$TRADER" "$COIN" >/dev/null 2>&1 || true
spost /control/set_book "{\"coin\":\"$COIN\"}" >/dev/null
own=$(smetrics 2>/dev/null | awk '/^venue_sim_owned_levels /{print $2}')
[ "$own" = "0" ] && ok "hand-back clean (owned_levels 0)" || bad "owned_levels=$own"

echo "== crosschain recovery: $pass passed, $fail failed =="
[ "$fail" = "0" ]
