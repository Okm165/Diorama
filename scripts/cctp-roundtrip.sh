#!/usr/bin/env bash
# The CCTP round trip, scripted (plan §15.5): fund the relay on the source if
# short, burn, wait for the record to reach `relayed`, and verify the
# destination balance delta is exactly the amount. A parked record fails with
# its own named reason.
#
# Direction and amount are env-driven; the default is the T→V leg:
#   make cctp-roundtrip
# Direct mode (a scratch stack):
#   BRIDGE_CTRL_URL=http://127.0.0.1:8096 \
#   ENGINE_T_URL=http://127.0.0.1:18680 ENGINE_V_URL=http://127.0.0.1:18681 \
#   T_URL=http://127.0.0.1:18602 V_URL=http://127.0.0.1:18610 \
#   ./scripts/cctp-roundtrip.sh
#
# On the cluster both engines mine, so the wait needs no miner; on a scratch
# stack mine the *destination* chain if the record stalls (the timeout names
# the miss).
set -euo pipefail

source "$(dirname "$0")/lib-cctp.sh"

SOURCE="${SOURCE:-0}"                 # 0 = Ethereum/T, 19 = HyperEVM/V
DESTINATION="${DESTINATION:-19}"
AMOUNT="${AMOUNT:-1000000}"           # raw units (1 USDC, 6-dec)
RELAY="${RELAY:-0x70997970C51812dc3A010C7d01b50e0d17dc79C8}"
USDC_T="${USDC_T:-0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48}"
USDC_V="${USDC_V:-0xb88339CB7199b77E23DB6E890353E22632Ba630f}"
BUDGET="${BUDGET:-180}"              # seconds to wait for `relayed`
require_chain_urls

if [ "$SOURCE" = "0" ]; then
  SRC_USDC="$USDC_T"; DST_USDC="$USDC_V"
else
  SRC_USDC="$USDC_V"; DST_USDC="$USDC_T"
fi

echo "== CCTP round trip: $SOURCE -> $DESTINATION, amount=$AMOUNT =="
src_before=$(balance "$SRC_USDC" "$RELAY" "$SRC_URL")
[ -n "$src_before" ] && ok "relay balance on the source read ($src_before)" || bad "source balance read failed"
if [ -n "$src_before" ] && [ "$src_before" -lt "$AMOUNT" ]; then
  info "relay short on the source; dealing $AMOUNT"
  cpost /admin/deal "$SRC_ENGINE" "{\"token\":\"$SRC_USDC\",\"to\":\"$RELAY\",\"amount\":\"$AMOUNT\"}" >/dev/null
  src_before=$(balance "$SRC_USDC" "$RELAY" "$SRC_URL")
  info "relay funded ($src_before)"
fi
dst_before=$(balance "$DST_USDC" "$RELAY" "$DST_URL")
ok "destination balance before: $dst_before"

burn_resp=$(burn "$SOURCE" "$DESTINATION" "$RELAY")
burn_tx=$(echo "$burn_resp" | python3 -c "import json,sys; print(json.load(sys.stdin).get('burn_tx',''))")
mined=$(echo "$burn_resp" | python3 -c "import json,sys; print(json.load(sys.stdin).get('mined'))")
[ -n "$burn_tx" ] && ok "burn submitted ($burn_tx, mined=$mined)" || bad "no burn_tx in the response: $burn_resp"

# An empty store entry is the normal pending case (the watcher absorbs the
# burn only after the finality window), not a terminal one — the wait keeps
# polling for it; only the budget running out is a failure.
state=$(await_relay "$burn_tx" || true)
case "$state" in
  relayed) ok "the record relayed" ;;
  parked*) bad "the record parked: ${state#parked: }" ;;
  *) bad "not relayed within ${BUDGET}s ($state; a scratch destination needs a miner)" ;;
esac

dst_after=$(balance "$DST_USDC" "$RELAY" "$DST_URL")
[ -n "$dst_before" ] && [ "$dst_after" = "$((dst_before + AMOUNT))" ] \
  && ok "destination minted exactly the amount ($dst_before -> $dst_after)" \
  || bad "destination delta is not the amount ($dst_before -> $dst_after, wanted +$AMOUNT)"

echo "== cctp roundtrip: $pass passed, $fail failed"
[ "$fail" = "0" ]
