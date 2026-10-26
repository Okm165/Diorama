#!/usr/bin/env bash
# The crosschain unified-margin cycle: one actor's funds
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
#                           impact lane), the identity stays exact, a reduce-only
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

NS="${NS:-diorama-venue}"
RELAY="${RELAY:-}"                                             # the bridge's wallet (the cycle's actor)
TRADER="${TRADER:-0x00000000000000000000000000000000000000e5}" # the V-side trader
UNIFIED="${UNIFIED:-0x00000000000000000000000000000000000000ea}" # a unified-mode account
AMOUNT="${AMOUNT:-5000000}"                                    # 5 USDC (6-dec)
COIN="${COIN:-ETH}"
SIZE="${SIZE:-0.01}"
BUDGET="${BUDGET:-180}"
CAST="${CAST:-cast}"
USDC_T="${USDC_T:-0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48}"
USDC_V="${USDC_V:-0xb88339CB7199b77E23DB6E890353E22632Ba630f}"
CORE_WRITER=0x3333333333333333333333333333333333333333

source "$(dirname "${BASH_SOURCE[0]}")/lib-cctp.sh"
require_chain_urls

# The actor is the bridge's own wallet: section A mints to it and the burn
# sends from it. Local mode uses the harness's known wallet; cluster mode
# derives it from the venue secret, so the default can never mispoint (the
# local address has no funds on the cluster's T).
if [ -z "$RELAY" ]; then
  if [ -n "${BRIDGE_CTRL_URL:-}" ]; then
    RELAY=0x70997970C51812dc3A010C7d01b50e0d17dc79C8
  else
    key=$(kubectl -n "$NS" get secret cctp-relay-key -o jsonpath='{.data.relay-key}' 2>/dev/null | base64 -d 2>/dev/null | tr -d '\r')
    RELAY=$("${CAST:-cast}" wallet address --private-key "$key" 2>/dev/null | tr -d '\r')
    [ -n "$RELAY" ] || { echo "RELAY unset and the cluster secret cctp-relay-key is unreadable" >&2; exit 1; }
    echo "  · the bridge's wallet derived from the cluster secret: $RELAY"
  fi
fi

# The CoreWriter calldata for a raw action stream (docs: the `sendRawAction`
# selector + offset + length + padded stream) — one home for every action the
# cycle sends.
core_writer_calldata() { # core_writer_calldata <stream_hex>
  python3 - "$1" <<'PY'
import sys
stream = bytes.fromhex(sys.argv[1])
arg = len(stream).to_bytes(32, "big") + stream
arg += b"\x00" * ((32 - len(stream) % 32) % 32)
print("0x17938e13" + (32).to_bytes(32, "big").hex() + arg.hex())
PY
}

echo "== crosschain unified-margin cycle (T -> CCTP -> venue -> trade -> CCTP -> T) =="

echo "-- preconditions"
[ -n "$(cget /status "$SRC_ENGINE" | json 'd.get("pin")')" ] && ok "the T engine answers" || bad "T engine silent"
[ -n "$(cget /status "$DST_ENGINE" | json 'd.get("pin")')" ] && ok "the V engine answers" || bad "V engine silent"
[ -n "$(bget /health | json 'd.get("ok")')" ] && ok "the bridge answers" || bad "bridge silent"
[ -n "$(sget /readyz || true)" ] && ok "the simulator is ready" || bad "sim not ready"
idx=$(sipost /info '{"type":"meta"}' | python3 -c "import json,sys; print([a['name'] for a in json.load(sys.stdin)['universe']].index('$COIN'))")
[ -n "$idx" ] && ok "$COIN is in the live meta (index $idx)" || bad "no $COIN in the live meta"
# The trader's leftovers from a previous run: flatten the perp, clear the book
# scenario; the spot leg's deal is a delta below.
spost /control/set_book "{\"coin\":\"$COIN\"}" > /dev/null
flatten_coin "$TRADER" "$COIN"
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
print("01000007" + f"{ntl:064x}" + f"{1:064x}")
PY
)
tx=$(send_tx "$TRADER" "$CORE_WRITER" "$(core_writer_calldata "$sendraw")")
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
[ "$(echo "$state" | identity_ok)" = "ok" ] && ok "the identity holds with the position on" || bad "identity: $(echo "$state" | identity_ok)"
close=$(spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"$SIZE\",\"reduce_only\":true}")
csz=$(echo "$close" | json 'd.get("totalSz")')
[ "$csz" = "$SIZE" ] && ok "the reduce-only close filled ($csz)" || bad "close: $close"
state=$(sim_account "$TRADER")
[ "$(echo "$state" | json 'd["assetPositions"][0]["position"]["szi"] if d.get("assetPositions") else "0"')" = "0" ] \
  && ok "the position is flat" || bad "not flat: $state"
[ "$(echo "$state" | identity_ok)" = "ok" ] && ok "the identity holds flat" || bad "identity: $(echo "$state" | identity_ok)"

echo "-- D2. the CoreWriter order door (action 1: the HyperEVM path into the lane)"
# (asset 1 = ETH, isBuy, limitPx 10^8 x 2500, sz 10^8 x 0.02, not reduce-only,
# tif 3 = Ioc, cloid 0) — the bridge decodes the action and fills through the
# same lane as /exchange and /control/order.
stream=$(python3 - <<'PY'
word = lambda v: f"{v:064x}"
print("01000001" + word(1) + word(1) + word(2500 * 10**8) + word(int(0.02 * 10**8)) + word(0) + word(3) + word(0))
PY
)
before=$(sim_account "$TRADER" | json 'd["assetPositions"][0]["position"]["szi"] if d.get("assetPositions") else "0"')
tx=$(send_tx "$TRADER" "$CORE_WRITER" "$(core_writer_calldata "$stream")")
[ -n "$tx" ] && [ "$tx" != "None" ] && ok "the CoreWriter limit order mined ($tx)" || bad "CoreWriter order tx failed"
after="$before"
for _ in $(seq 1 20); do
  after=$(sim_account "$TRADER" | json 'd["assetPositions"][0]["position"]["szi"] if d.get("assetPositions") else "0"')
  [ "$after" != "$before" ] && break
  sleep 1
done
[ "$after" != "$before" ] \
  && ok "the CoreWriter order filled through the lane ($before -> $after)" \
  || bad "the CoreWriter order did not fill ($before -> $after)"
# Close the extra size through the control door (both doors, one lane).
extra=$(python3 -c "from decimal import Decimal; print(Decimal('$after') - Decimal('$before'))")
spost /control/order "{\"address\":\"$TRADER\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"$extra\",\"reduce_only\":true}" > /dev/null
state=$(sim_account "$TRADER")
[ "$(echo "$state" | json 'd["assetPositions"][0]["position"]["szi"] if d.get("assetPositions") else "0"')" = "0" ] \
  && ok "the CoreWriter size closed through /control/order (flat)" || bad "not flat: $state"

echo "-- D3. unified collateral (spot USDC backs the perp side; the docs' single balance)"
# The account must exist on HyperCore before its actions apply (the bridge's
# measured eligibility rule, docs: interaction timings): the spot deal creates
# it first, then the real abstraction path runs — CoreWriter action 16
# (16 = 0x10), abstraction 2.
spost /control/deal "{\"address\":\"$UNIFIED\",\"class\":\"spot\",\"amount\":\"250\"}" > /dev/null
stream=$(python3 - "$UNIFIED" <<'PY'
import sys
user = int(sys.argv[1], 16)
print("01000010" + f"{user:064x}" + f"{2:064x}")
PY
)
rpcv "$DST_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setBalance\",\"params\":[\"$UNIFIED\",\"0x8ac7230489e80000\"]}" > /dev/null
tx=$(send_tx "$UNIFIED" "$CORE_WRITER" "$(core_writer_calldata "$stream")")
[ -n "$tx" ] && [ "$tx" != "None" ] && ok "the unified abstraction set through CoreWriter 16" || bad "action 16 tx failed"
abs=""
for _ in $(seq 1 20); do
  # The venue serves the abstraction as a bare JSON string (not an object).
  abs=$(sipost /info "{\"type\":\"userAbstraction\",\"user\":\"$UNIFIED\"}" | json 'd if isinstance(d, str) else d.get("abstraction")')
  [ "$abs" = "unifiedAccount" ] && break
  sleep 1
done
[ "$abs" = "unifiedAccount" ] && ok "the sim serves unifiedAccount (the real path, not a fixture)" \
  || bad "abstraction=$abs"
ufill=$(spost /control/order "{\"address\":\"$UNIFIED\",\"coin\":\"$COIN\",\"is_buy\":true,\"size\":\"0.01\"}")
usz=$(echo "$ufill" | json 'd.get("totalSz")')
[ "$usz" = "0.01" ] && ok "a spot-only unified account fills a perp order (collateral backs margin)" \
  || bad "unified fill: $ufill"
ustate=$(sim_account "$UNIFIED")
[ "$(echo "$ustate" | identity_ok)" = "ok" ] && ok "the unified identity holds (collateral inside totalRawUsd)" \
  || bad "unified identity: $(echo "$ustate" | identity_ok)"
spost /control/order "{\"address\":\"$UNIFIED\",\"coin\":\"$COIN\",\"is_buy\":false,\"size\":\"0.01\",\"reduce_only\":true}" > /dev/null

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
owned=$(smetrics | awk '$1 == "venue_sim_owned_levels" {print $2}')
[ "$owned" = "0" ] && ok "consumptions handed back (owned_levels 0)" || info "owned_levels=$owned (a later read will prune)"

echo "== cctp margin cycle: $pass passed, $fail failed"
[ "$fail" = "0" ]
