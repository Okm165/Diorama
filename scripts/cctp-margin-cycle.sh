#!/usr/bin/env bash
# The crosschain unified-margin cycle (plan §15.5 + §4.5): one actor's funds
# travel the whole substrate and trade — the axiom that both mirrors compose.
#
#   A. mint on T          - the T engine's `/admin/deal` mints real USDC to the
#                           bridge's wallet (the crosschain actor)
#   B. CCTP T → V         - burn, wait for `relayed`, the V mint lands exactly
#                           (mint recipient = the trader)
#   C. venue collateral   - the HyperCore spot ledger is credited (the
#                           venue-side mint analogue; measured: the EVM→Core
#                           USDC path is faithfully refused — Circle blacklists
#                           the system address and the USDC spotMeta link is a
#                           phantom, see TESTING "the bridge's token door"),
#                           then a **real CoreWriter USD-class transfer** on V
#                           moves it spot → perp through the bridge's decoder
#   D. trade              - a perp order fills against the real book (the
#                           §4.5 lane), the identity stays exact, a reduce-only
#                           close flattens
#   E. CCTP V → T         - the trader returns the V-side USDC to the bridge
#                           wallet, the burn relays, and T's balance lands
#                           exactly back (the cycle is closed)
#
# Run from the repo root:
#   make cctp-margin-cycle
# Direct mode (a scratch stack):
#   BRIDGE_CTRL_URL=http://127.0.0.1:8096 \
#   ENGINE_T_URL=http://127.0.0.1:18680 ENGINE_V_URL=http://127.0.0.1:18681 \
#   T_URL=http://127.0.0.1:18602 V_URL=http://127.0.0.1:18610 \
#   SIM_INFO=http://127.0.0.1:18790 SIM_CTRL=http://127.0.0.1:18791 \
#   ./scripts/cctp-margin-cycle.sh
set -euo pipefail

NS="${NS:-venue-mirror}"
RELAY="${RELAY:-0x70997970C51812dc3A010C7d01b50e0d17dc79C8}"   # the bridge's wallet
TRADER="${TRADER:-0x00000000000000000000000000000000000000e5}" # the V-side trader
AMOUNT="${AMOUNT:-5000000}"                                    # 5 USDC (6-dec)
COIN="${COIN:-ETH}"
SIZE="${SIZE:-0.01}"
BUDGET="${BUDGET:-180}"
CAST="${CAST:-cast}"
USDC_T="${USDC_T:-0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48}"
USDC_V="${USDC_V:-0xb88339CB7199b77E23DB6E890353E22632Ba630f}"
CORE_WRITER=0x3333333333333333333333333333333333333333

pass=0; fail=0
ok()   { echo "  ✔ $*"; pass=$((pass+1)); }
bad()  { echo "  ✘ $*"; fail=$((fail+1)); }
info() { echo "  · $*"; }

if [ -n "${BRIDGE_CTRL_URL:-}" ]; then
  bget()  { curl -s --fail-with-body -m 30 "$BRIDGE_CTRL_URL$1"; }
  bpost() { curl -s --fail-with-body -m 120 -X POST "$BRIDGE_CTRL_URL$1" -H 'Content-Type: application/json' -d "$2"; }
  cget()  { curl -s --fail-with-body -m 60 "$2$1"; }
  cpost() { curl -s --fail-with-body -m 60 -X POST "$2$1" -H 'Content-Type: application/json' -d "$3"; }
  balance() { "$CAST" call "$1" 'balanceOf(address)(uint256)' "$2" --rpc-url "$3" 2>/dev/null | awk '{print $1}'; }
  SRC_URL="${T_URL:?set T_URL in direct mode}"; DST_URL="${V_URL:?set V_URL in direct mode}"
  SRC_ENGINE="${ENGINE_T_URL:?set ENGINE_T_URL in direct mode}"; DST_ENGINE="${ENGINE_V_URL:?set ENGINE_V_URL in direct mode}"
  SIM_INFO="${SIM_INFO:?set SIM_INFO in direct mode}"; SIM_CTRL="${SIM_CTRL:?set SIM_CTRL in direct mode}"
  rpcv()  { curl -s --fail-with-body -m 60 -X POST "$1" -H 'Content-Type: application/json' -d "$2"; }
  sipost() { curl -s --fail-with-body -m 30 -X POST "$SIM_INFO$1" -H 'Content-Type: application/json' -d "$2"; }
  spost()  { curl -s --fail-with-body -m 30 -X POST "$SIM_CTRL$1" -H 'Content-Type: application/json' -d "$2"; }
else
  exec_pod() { kubectl -n "$NS" exec -i deploy/cctp-bridge -- "$@"; }
  bget()  { exec_pod curl -sf -m 30 "localhost:8096$1"; }
  bpost() { exec_pod curl -sf -m 120 -X POST "localhost:8096$1" -H 'Content-Type: application/json' -d "$2"; }
  cget()  { exec_pod curl -sf -m 60 "$2$1"; }
  cpost() { exec_pod curl -sf -m 60 -X POST "$2$1" -H 'Content-Type: application/json' -d "$3"; }
  balance() { exec_pod "$CAST" call "$1" 'balanceOf(address)(uint256)' "$2" --rpc-url "$3" 2>/dev/null | awk '{print $1}'; }
  SRC_URL="http://anvil-t:8545"; DST_URL="http://anvil-v:8545"
  SRC_ENGINE="http://mainnet-engine.mainnet-mirror.svc:8080"; DST_ENGINE="localhost:8080"
  SIM_INFO="http://venue-sim:8090"; SIM_CTRL="http://venue-sim:8091"
  rpcv()  { exec_pod curl -sf -m 60 -X POST "$1" -H 'Content-Type: application/json' -d "$2"; }
  sipost() { exec_pod curl -sf -m 30 -X POST "$SIM_INFO$1" -H 'Content-Type: application/json' -d "$2"; }
  spost()  { exec_pod curl -sf -m 30 -X POST "$SIM_CTRL$1" -H 'Content-Type: application/json' -d "$2"; }
fi

json() {
  python3 -c "
import json, sys
raw = sys.stdin.read()
try:
    d = json.loads(raw)
except Exception:
    sys.stderr.write('json: unparsable body: ' + raw.strip()[:200] + '\n')
    print('None')
    raise SystemExit
print($1)"
}
mine() { rpcv "$DST_URL" '{"jsonrpc":"2.0","id":1,"method":"evm_mine","params":[]}' > /dev/null; }
receipt_status() { # receipt_status <tx> -> 0x1 | 0x0 | None
  rpcv "$DST_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$1\"]}" \
    | json 'd["result"]["status"] if d.get("result") else None'
}
send_tx() { # send_tx <from> <to> <data>
  local tx
  tx=$(rpcv "$DST_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$1\",\"to\":\"$2\",\"data\":\"$3\"}]}" | json 'd["result"]')
  mine
  printf '%s' "$tx"
}
# Wait for a bridge record to reach `relayed` (the roundtrip's own pattern).
await_relay() { # await_relay <burn_tx>
  local state="" i=0 rec
  while [ "$i" -lt "$BUDGET" ]; do
    rec=$(bget /control/state | python3 -c "
import json, sys
d = json.load(sys.stdin)
for m in d.get('messages', []):
    if m.get('burn_tx', '').lower() == '$1'.lower():
        print(m.get('state', ''), '|', m.get('park_reason') or '')
        break
")
    state=${rec%% *}
    case "$state" in
      relayed) printf 'relayed'; return 0 ;;
      parked)  printf 'parked: %s' "${rec#*| }"; return 1 ;;
    esac
    i=$((i + 1))
    sleep 1
  done
  printf 'timeout (state=%s)' "${state:-none}"
  return 1
}
burn() { # burn <source> <destination> <mint_recipient>
  bpost /control/transfer "{\"source\":$1,\"destination\":$2,\"amount\":\"$AMOUNT\",\"mint_recipient\":\"$3\"}"
}
identity() {
  python3 -c "
import json, sys
from decimal import Decimal
d = json.load(sys.stdin)
v = Decimal(d['marginSummary']['accountValue'])
raw = Decimal(d['marginSummary']['totalRawUsd'])
pnl = sum(Decimal(p['position']['positionValue']) * (-1 if p['position']['szi'].startswith('-') else 1)
          for p in d['assetPositions'])
print('ok' if abs(v - (raw + pnl)) < Decimal('0.01') else f'mismatch v={v} raw={raw} pnl={pnl}')"
}
sim_account() { sipost /info "{\"type\":\"clearinghouseState\",\"user\":\"$1\"}"; }
sim_spot() {
  sipost /info "{\"type\":\"spotClearinghouseState\",\"user\":\"$1\"}" \
    | python3 -c "
import json, sys
from decimal import Decimal
d = json.load(sys.stdin)
for b in d.get('balances', []):
    if b.get('coin') == 'USDC':
        print(b.get('total', '0'))
        break
else:
    print('0')"
}

echo "== crosschain unified-margin cycle (T -> CCTP -> venue -> trade -> CCTP -> T) =="

echo "-- preconditions"
[ -n "$(cget /status "$SRC_ENGINE" | json 'd.get("pin")')" ] && ok "the T engine answers" || bad "T engine silent"
[ -n "$(cget /status "$DST_ENGINE" | json 'd.get("pin")')" ] && ok "the V engine answers" || bad "V engine silent"
[ -n "$(bget /health | json 'd.get("ok")')" ] && ok "the bridge answers" || bad "bridge silent"
[ -n "$(sipost /readyz '' 2>/dev/null || true)$(cget /readyz "$SIM_INFO" || true)" ] && ok "the simulator is ready" || bad "sim not ready"
idx=$(sipost /info '{"type":"meta"}' | python3 -c "import json,sys; print([a['name'] for a in json.load(sys.stdin)['universe']].index('$COIN'))")
[ -n "$idx" ] && ok "$COIN is in the live meta (index $idx)" || bad "no $COIN in the live meta"
# The trader's leftovers from a previous run: flatten the perp, clear the book
# scenario; the spot leg's deal is a delta below.
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
szi0=$(sim_account "$TRADER" | json 'd["assetPositions"][0]["position"]["szi"] if d.get("assetPositions") else "0"')
if [ "$szi0" != "0" ]; then
  is_buy=false; case "$szi0" in -*) is_buy=true ;; esac
  abs_szi=$(python3 -c "from decimal import Decimal; print(abs(Decimal('$szi0')))")
  spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":$is_buy,\"size\":\"$abs_szi\",\"reduce_only\":true}" > /dev/null || true
fi
rpcv "$DST_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setBalance\",\"params\":[\"$TRADER\",\"0x8ac7230489e80000\"]}" > /dev/null
ok "trader flattened, gas set (10 ETH on V)"

echo "-- A. mint on T (the crosschain actor = the bridge's wallet)"
t0=$(balance "$USDC_T" "$RELAY" "$SRC_URL")
[ -n "$t0" ] && ok "T balance read ($t0)" || bad "T balance read failed"
# The faucet *sets* the balance slot (measured; `deal`'s silent COW write), so
# it is used only when short — the roundtrip's own pattern.
if [ "$t0" -lt "$AMOUNT" ]; then
  cpost /admin/deal "$SRC_ENGINE" "{\"token\":\"$USDC_T\",\"to\":\"$RELAY\",\"amount\":\"$AMOUNT\"}" > /dev/null
  t1=$(balance "$USDC_T" "$RELAY" "$SRC_URL")
  [ "$t1" = "$AMOUNT" ] && ok "T minted to exactly $AMOUNT ($t0 -> $t1)" || bad "T mint $t0 -> $t1 (wanted $AMOUNT)"
else
  t1=$t0
  info "the T wallet already holds $t0 (>= $AMOUNT); no mint needed"
fi

echo "-- B. CCTP T -> V (mint recipient = the trader)"
v0=$(balance "$USDC_V" "$TRADER" "$DST_URL")
burn_tx=$(burn 0 19 "$TRADER" | json 'd["burn_tx"]')
[ -n "$burn_tx" ] && [ "$burn_tx" != "None" ] && ok "burn submitted ($burn_tx)" || bad "no burn_tx"
state=$(await_relay "$burn_tx" || true)
case "$state" in
  relayed) ok "the record relayed" ;;
  *) bad "not relayed: $state" ;;
esac
v1=$(balance "$USDC_V" "$TRADER" "$DST_URL")
[ "$v1" = "$((v0 + AMOUNT))" ] && ok "V minted exactly $AMOUNT to the trader ($v0 -> $v1)" \
  || bad "V mint delta $v0 -> $v1"
t_mid=$(balance "$USDC_T" "$RELAY" "$SRC_URL")
[ "$t_mid" = "$((t1 - AMOUNT))" ] && ok "T shows the burn ($t1 -> $t_mid; the funds left the source)" \
  || bad "T after burn $t1 -> $t_mid (wanted -$AMOUNT)"

echo "-- C. venue collateral: spot ledger + the CoreWriter USD-class transfer"
spot0=$(sim_spot "$TRADER")
perp0=$(sim_account "$TRADER" | json 'd["marginSummary"]["totalRawUsd"]')
spost /control/deal "{\"address\":\"$TRADER\",\"class\":\"spot\",\"amount\":\"$((AMOUNT / 1000000))\"}" > /dev/null
spot1=$(sim_spot "$TRADER")
[ "$(python3 -c "from decimal import Decimal; print('ok' if Decimal('$spot1') - Decimal('$spot0') == Decimal('$((AMOUNT / 1000000))') else 'bad')")" = "ok" ] \
  && ok "the venue-side spot ledger credited (+$((AMOUNT / 1000000)) USDC, the mint analogue)" \
  || bad "spot delta $spot0 -> $spot1"
# The real CoreWriter action: version 1, id 7 (u24 BE), (uint64 ntl, bool toPerp)
# — ntl in core wei (USDC is 8-dec on Core).
sendraw=$(python3 - "$AMOUNT" <<'PY'
import sys
ntl = int(sys.argv[1]) * 100            # 6-dec USDC -> 8-dec core wei
stream = bytes.fromhex("01000007" + f"{ntl:064x}" + f"{1:064x}")
arg = len(stream).to_bytes(32, "big") + stream
arg += b"\x00" * ((32 - len(stream) % 32) % 32)
print("0x17938e13" + (32).to_bytes(32, "big").hex() + arg.hex())
PY
)
tx=$(send_tx "$TRADER" "$CORE_WRITER" "$sendraw")
[ -n "$tx" ] && [ "$tx" != "None" ] && ok "CoreWriter USD-class transfer mined ($tx)" || bad "CoreWriter tx failed"
perp1=""
for _ in $(seq 1 20); do
  perp1=$(sim_account "$TRADER" | json 'd["marginSummary"]["totalRawUsd"]')
  [ "$(python3 -c "from decimal import Decimal; print('ok' if Decimal('$perp1') - Decimal('$perp0') == Decimal('$((AMOUNT / 1000000))') else 'bad')")" = "ok" ] && break
  sleep 1
done
[ "$(python3 -c "from decimal import Decimal; print('ok' if Decimal('$perp1') - Decimal('$perp0') == Decimal('$((AMOUNT / 1000000))') else 'bad')")" = "ok" ] \
  && ok "the perp wallet credited exactly (+$((AMOUNT / 1000000)) USDC through the bridge's decoder)" \
  || bad "perp delta $perp0 -> $perp1"
spot2=$(sim_spot "$TRADER")
[ "$(python3 -c "from decimal import Decimal; print('ok' if Decimal('$spot1') - Decimal('$spot2') == Decimal('$((AMOUNT / 1000000))') else 'bad')")" = "ok" ] \
  && ok "the spot side was debited (the transfer is not a mint)" || bad "spot $spot1 -> $spot2"

echo "-- D. trade: an order against the real book, then a reduce-only close"
fill=$(spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"$SIZE\"}")
fsz=$(echo "$fill" | json 'd.get("totalSz")')
[ "$fsz" = "$SIZE" ] && ok "the order filled ($fsz $COIN at $(echo "$fill" | json 'd.get("avgPx")'))" || bad "fill: $fill"
state=$(sim_account "$TRADER")
[ "$(echo "$state" | identity)" = "ok" ] && ok "the identity holds with the position on" || bad "identity: $(echo "$state" | identity)"
close=$(spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"$SIZE\",\"reduce_only\":true}")
csz=$(echo "$close" | json 'd.get("totalSz")')
[ "$csz" = "$SIZE" ] && ok "the reduce-only close filled ($csz)" || bad "close: $close"
state=$(sim_account "$TRADER")
[ "$(echo "$state" | json 'd["assetPositions"][0]["position"]["szi"] if d.get("assetPositions") else "0"')" = "0" ] \
  && ok "the position is flat" || bad "not flat: $state"
[ "$(echo "$state" | identity)" = "ok" ] && ok "the identity holds flat" || bad "identity: $(echo "$state" | identity)"

echo "-- E. CCTP V -> T (the cycle closes)"
# The trader returns the V-side USDC to the bridge's wallet, then it burns.
data=$("$CAST" calldata "transfer(address,uint256)" "$RELAY" "$AMOUNT")
tx=$(send_tx "$TRADER" "$USDC_V" "$data")
status=$(receipt_status "$tx")
# The receipt status is the truth — a submitted-but-reverted tx must not read
# as a successful return (measured: a wiped attester left the mint absent and
# the transfer reverted silently).
[ "$status" = "0x1" ] && ok "the trader returned the V USDC to the bridge wallet" \
  || bad "the return transfer failed (status=$status; is the trader's V balance $AMOUNT?)"
r0=$(balance "$USDC_V" "$RELAY" "$DST_URL")
if [ "$r0" -lt "$AMOUNT" ]; then
  # The V wallet funds the return burn; the venue faucet sets the slot (the
  # same pattern as T above).
  cpost /admin/deal "$DST_ENGINE" "{\"token\":\"$USDC_V\",\"to\":\"$RELAY\",\"amount\":\"$AMOUNT\"}" > /dev/null
  r0=$(balance "$USDC_V" "$RELAY" "$DST_URL")
  info "the bridge wallet was funded on V to $r0"
fi
[ "$r0" -ge "$AMOUNT" ] && ok "the bridge wallet holds the return ($r0)" || bad "bridge wallet short: $r0"
burn_tx=$(burn 19 0 "$RELAY" | json 'd["burn_tx"]')
[ -n "$burn_tx" ] && [ "$burn_tx" != "None" ] && ok "V burn submitted ($burn_tx)" || bad "no V burn_tx"
state=$(await_relay "$burn_tx" || true)
case "$state" in
  relayed) ok "the return record relayed" ;;
  *) bad "return not relayed: $state" ;;
esac
t2=$(balance "$USDC_T" "$RELAY" "$SRC_URL")
# The return mint lands to the same wallet that burned on V, so T closes
# exactly where the cycle began: -5 (the first burn) + 5 (the return mint).
[ "$t2" = "$t1" ] && ok "the cycle closed exactly on T ($t_mid -> $t2 = the pre-cycle balance)" \
  || bad "T delta $t_mid -> $t2 (wanted +$AMOUNT back to $t1)"

echo "-- hand-back"
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
owned=$(curl -s --fail-with-body -m 30 "$SIM_CTRL/metrics" | awk '$1 == "venue_sim_owned_levels" {print $2}')
[ "$owned" = "0" ] && ok "consumptions handed back (owned_levels 0)" || info "owned_levels=$owned (a later read will prune)"

echo "== cctp margin cycle: $pass passed, $fail failed"
[ "$fail" = "0" ]
