#!/usr/bin/env bash
# Product simulation: a leveraged position on live mainnet state - built,
# monitored across real parent blocks, and liquidated by a bot. This is the
# workflow a margin product runs on the substrate, end to end:
#
#   act 1  the trader   faucet funds -> supply WETH -> borrow USDC -> swap on
#                       the real Uniswap pool -> supply the swap back
#   act 2  live data    the same views sampled across several *parent blocks*
#                       all match reth at the applied cursor - freshness is
#                       continuous, not a one-shot check
#   act 3  the bot      a WS subscription sees the trader's Borrow event; after
#                       the oracle crash the bot polls health, liquidates the
#                       position and takes the bonus
#
# Everything runs against the real Aave pool/oracle and the real Uniswap pool;
# the venues touched are handed back to mainnet sync at the end.
# Usage: NS=diorama-mainnet ./scripts/mainnet-product-sim.sh
set -uo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib-mainnet.sh"

USDC=0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
WETH=0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
POOL=0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2
ORACLE=0x54586bE62E3c3580375aE3723C145253060Ca0C2
PROVIDER=0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e
A_WETH=0x4d5F47FA6A74757f35C14fD3a6Ef8E3C9BC514E8
UNIV3=0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640
ROUTER=0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45
CHAINLINK_WETH_USD=0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419
USDC_USD_FEED=0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6

acct() { # acct <address> -> "collateral debt hf" (USD base, 8 decimals)
  castT call $POOL "getUserAccountData(address)(uint256,uint256,uint256,uint256,uint256,uint256)" "$1" \
    | awk 'NR==1 {c=$1} NR==2 {d=$1} NR==6 {h=$1} END {print c, d, h}'
}
fund() { # fund <token> <to> <amount> -> "true" on success
  $ENGINE curl -sf -m 25 -X POST localhost:8080/admin/deal -H 'Content-Type: application/json' \
    -d "{\"token\":\"$1\",\"to\":\"$2\",\"amount\":\"$3\"}" | jq -r '.ok'
}

echo "== product simulation (namespace $NS)"

echo "-- act 1. the trader builds a levered position (faucet -> Aave -> Uniswap -> Aave)"
TRADER=0x$(printf '%040x' $(( ( $(date +%s) % 100000000 ) * 100000 + 21 )))
BOT=0x$(printf '%040x' $(( ( $(date +%s) % 100000000 ) * 100000 + 22 )))
for a in "$TRADER" "$BOT"; do
  rpc http://anvil-t:8545 anvil_setBalance "[\"$a\",\"0x21e19e0c9bab2400000\"]" >/dev/null
  rpc http://anvil-t:8545 anvil_impersonateAccount "[\"$a\"]" >/dev/null
done
[ "$(fund $WETH $TRADER 1050000000000000000)" = "true" ] \
  && info "trader funded: 1.05 WETH via /admin/deal (slot discovery)" || bad "trader WETH funding failed"
[ "$(fund $USDC $BOT 2000000000)" = "true" ] \
  && info "bot funded: 2000 USDC via /admin/deal" || bad "bot USDC funding failed"

a0=$(num "$(castT call $A_WETH "balanceOf(address)(uint256)" $TRADER | awk '{print $1}')")
send_ok "$TRADER" "$WETH" "$(castoff calldata 'approve(address,uint256)' $POOL 1000000000000000000)" >/dev/null \
  || bad "trader approve failed"
send_ok "$TRADER" "$POOL" "$(castoff calldata 'supply(address,uint256,address,uint16)' $WETH 1000000000000000000 $TRADER 0)" >/dev/null \
  || bad "trader supply failed"
send_ok "$TRADER" "$POOL" "$(castoff calldata 'borrow(address,uint256,uint256,uint16,address)' $USDC 100000000 2 0 $TRADER)" >/dev/null \
  || bad "trader borrow failed"
send_ok "$TRADER" "$USDC" "$(castoff calldata 'approve(address,uint256)' $ROUTER 50000000)" >/dev/null \
  || bad "trader USDC approve failed"
send_ok "$TRADER" "$ROUTER" \
  "$(castoff calldata 'exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))' "($USDC,$WETH,500,$TRADER,50000000,0,0)")" >/dev/null \
  || bad "trader swap failed"
weth_got=$(num "$(castT call $WETH "balanceOf(address)(uint256)" $TRADER | awk '{print $1}')")
if [ "$weth_got" -gt 0 ]; then
  send_ok "$TRADER" "$WETH" "$(castoff calldata 'approve(address,uint256)' $POOL "$weth_got")" >/dev/null \
    || bad "re-supply approve failed"
  send_ok "$TRADER" "$POOL" "$(castoff calldata 'supply(address,uint256,address,uint16)' $WETH "$weth_got" $TRADER 0)" >/dev/null \
    || bad "re-supply failed"
fi
a1=$(num "$(castT call $A_WETH "balanceOf(address)(uint256)" $TRADER | awk '{print $1}')")
read -r coll debt hf <<EOF
$(acct "$TRADER")
EOF
[ "$((a1 - a0))" -ge 1000000000000000000 ] && ok "levered: 1 WETH supplied + the swap re-supplied (aEthWETH +$((a1 - a0)))" \
  || bad "collateral did not land (aEthWETH $a0 -> $a1)"
[ "$(num "$debt")" -gt 0 ] && ok "borrowed against it (debt base $debt, HF $hf)" \
  || bad "no debt after the borrow"
V_DEBT_USDC=0x72E95b8931767C79bA4EeE721354d6E99a61D004
debt_token_before=$(num "$(castT call $V_DEBT_USDC "balanceOf(address)(uint256)" $TRADER | awk '{print $1}')")
scaled_before=$(num "$(castT call $V_DEBT_USDC "scaledBalanceOf(address)(uint256)" $TRADER | awk '{print $1}')")
info "debt token: balanceOf=$debt_token_before scaledBalanceOf=$scaled_before"

echo "-- act 2. market data flows from live mainnet blocks"
# The principle, tested broadly: the whole market-data closure the product reads
# - five Chainlink rounds, the Aave oracle for five assets, the UniV3 pool, three
# token supplies, two Aave income indexes - sampled once per *parent block* and
# required to equal reth at the applied cursor. A view that lags (stale, not
# manipulated) fails here. Views whose account the devnet owns are skipped by
# design (copy-on-write: that state is the devnet's, not mainnet's) and counted.
VIEWS="
chainlink_weth_usd 0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419 0xfeaf968c
chainlink_usdc_usd 0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6 0xfeaf968c
chainlink_usdt_usd 0x3E7d1eAB13ad0104d2750B8863b489D65364e32D 0xfeaf968c
chainlink_wbtc_usd 0xF4030086522a5bEEa4988F8cA5B36dbC97BeE88c 0xfeaf968c
chainlink_steth_usd 0xCfE54B5cD566aB89272946F602D76Ea879CAb4a8 0xfeaf968c
aave_price_weth 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f07000000000000000000000000C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
aave_price_usdc 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f07000000000000000000000000A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
aave_price_usdt 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f07000000000000000000000000dAC17F958D2ee523a2206206994597C13D831ec7
aave_price_wbtc 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f070000000000000000000000002260FAC5E5542a773Aa44fBCfeDf7C193bc2C599
aave_price_wsteth 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f070000000000000000000000007f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0
univ3_weth_usdc_slot0 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640 0x3850c7bd
weth_total_supply 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 0x18160ddd
usdc_total_supply 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 0x18160ddd
wbtc_total_supply 0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599 0x18160ddd
aave_usdc_income 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2 0xd15e0053000000000000000000000000A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
aave_weth_income 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2 0xd15e0053000000000000000000000000C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
"
shadowed=$(engine_api /status | jq -r '.shadowed[]' 2>/dev/null | tr '\n' ' ')
rounds=0; compared=0; matched=0; skipped=0; last=""
for _ in $(seq 1 16); do
  cur=$(num "$(engine_api /status | jq -r '.cursor_t')")
  [ "$cur" = "$last" ] && { sleep 6; continue; }
  last=$cur; rounds=$((rounds + 1))
  at=$(printf '0x%x' "$cur")
  while read -r name to data; do
    [ -n "${name:-}" ] || continue
    case " $shadowed " in *" ${to,,} "*) skipped=$((skipped + 1)); continue;; esac
    v_t=$(castT call "$to" "$data" | tr '\n' ' ')
    v_r=$(castR call "$to" "$data" --block "$at" | tr '\n' ' ')
    compared=$((compared + 1))
    if [ "$v_t" = "$v_r" ]; then matched=$((matched + 1)); else
      bad "block $cur: $name diverges (T=${v_t:0:26}… reth=${v_r:0:26}…)"
    fi
  done <<EOF
$VIEWS
EOF
done
[ "$rounds" -ge 5 ] && [ "$compared" -ge 24 ] && [ "$matched" = "$compared" ] \
  && ok "market data flowed live: $matched/$compared view-comparisons matched reth across $rounds parent blocks ($skipped devnet-owned skipped)" \
  || bad "freshness incomplete (rounds=$rounds compared=$compared matched=$matched skipped=$skipped)"

echo "-- act 3. the bot: WS subscription -> crash -> detect -> liquidate"
# A crash to ~$1 makes the position *underwater*: the collateral is worth less
# than the debt. Aave's liquidation then hits its bad-debt path: the collateral
# is consumed entirely and the *whole remaining debt* is burned
# (LiquidationLogic: `burnAmount = hasNoCollateralLeft ? borrowerReserveDebt :
# actualDebtToLiquidate`). The assertions below check that signature directly -
# the debt token goes to zero while the liquidator only paid what the collateral
# covered - because it is the hardest path a margin product can hit. (Measured:
# this deployment's getReserveData returns 15 words, i.e. the pre-`deficit`
# struct, so the loss is socialised through the aToken liquidity rather than
# booked in a deficit field.)
BORROW_TOPIC=$(castoff keccak "Borrow(address,address,address,uint256,uint8,uint256,uint16)" | tr -d '\r')
# The bot subscribes to the pool's events over WS inside the anvil pod (cast is
# there; stdbuf -oL because cast block-buffers a redirected stdout).
$ANVIL_T sh -c 'rm -f /tmp/bot-ws.log; nohup stdbuf -oL timeout 120 cast logs --subscribe --rpc-url ws://localhost:8545 --address 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2 > /tmp/bot-ws.log 2>&1 &' >/dev/null 2>&1
sleep 3
send_ok "$TRADER" "$POOL" "$(castoff calldata 'borrow(address,uint256,uint256,uint16,address)' $USDC 10000000 2 0 $TRADER)" >/dev/null \
  || bad "second borrow failed"
seen=0
for _ in $(seq 1 10); do
  sleep 3
  seen=$($ANVIL_T sh -c "grep -c ${BORROW_TOPIC:0:20} /tmp/bot-ws.log 2>/dev/null || true" 2>/dev/null | tr -d '\r')
  [ "${seen:-0}" -gt 0 ] 2>/dev/null && break
done
[ "${seen:-0}" -gt 0 ] 2>/dev/null \
  && ok "the bot's WS subscription received the trader's Borrow event (live, engine-mined block)" \
  || bad "the Borrow event never reached the subscription"

ORACLE_OWNER=$(castT call $PROVIDER "owner()(address)" | tr -d '\r')
TRUE_SOURCE=$(castR call $ORACLE "getSourceOfAsset(address)(address)" $WETH | tr -d '\r')
rpc http://anvil-t:8545 anvil_impersonateAccount "[\"$ORACLE_OWNER\"]" >/dev/null
rpc http://anvil-t:8545 anvil_setBalance "[\"$ORACLE_OWNER\",\"0xde0b6b3a7640000\"]" >/dev/null
send_ok "$ORACLE_OWNER" "$ORACLE" \
  "$(castoff calldata 'setAssetSources(address[],address[])' "[$WETH]" "[$USDC_USD_FEED]")" >/dev/null \
  || bad "oracle crash failed"
crash_hf=""
for _ in $(seq 1 24); do
  sleep 5
  read -r _ _ crash_hf <<EOF
$(acct "$TRADER")
EOF
  [ -n "$crash_hf" ] && [ "$crash_hf" != "$hf" ] && break
done
if [ -n "$crash_hf" ] && [ "$(num "$crash_hf")" -lt 1000000000000000000 ] 2>/dev/null; then
  ok "position liquidatable after the crash (HF $hf -> $crash_hf)"
else
  bad "HF did not fall below 1e18 ($hf -> $crash_hf)"
fi
debt_before=$(num "$(acct "$TRADER" | awk '{print $2}')")
bot_weth_before=$(num "$(castT call $WETH "balanceOf(address)(uint256)" $BOT | awk '{print $1}')")
bot_usdc_before=$(num "$(castT call $USDC "balanceOf(address)(uint256)" $BOT | awk '{print $1}')")
send_ok "$BOT" "$USDC" "$(castoff calldata 'approve(address,uint256)' $POOL 500000000)" >/dev/null \
  || bad "bot approve failed"
LIQ=$(send_ok "$BOT" "$POOL" \
  "$(castoff calldata 'liquidationCall(address,address,address,uint256,bool)' $WETH $USDC $TRADER 50000000 false)") \
  || bad "liquidationCall failed"
debt_after=$(num "$(acct "$TRADER" | awk '{print $2}')")
debt_token_after=$(num "$(castT call $V_DEBT_USDC "balanceOf(address)(uint256)" $TRADER | awk '{print $1}')")
scaled_after=$(num "$(castT call $V_DEBT_USDC "scaledBalanceOf(address)(uint256)" $TRADER | awk '{print $1}')")
bot_weth_after=$(num "$(castT call $WETH "balanceOf(address)(uint256)" $BOT | awk '{print $1}')")
[ -n "$LIQ" ] && ok "the bot liquidated the position (tx $LIQ)" || true
info "debt token after: balanceOf=$debt_token_after scaledBalanceOf=$scaled_after; account debt base: $debt_before -> $debt_after"
[ "$debt_after" -lt "$debt_before" ] \
  && ok "trader's debt cleared by the bot (base $debt_before -> $debt_after)" \
  || bad "debt did not move ($debt_before -> $debt_after)"
bot_usdc_after=$(num "$(castT call $USDC "balanceOf(address)(uint256)" $BOT | awk '{print $1}')")
repaid=$((bot_usdc_before - bot_usdc_after))
info "the bot paid $repaid USDC for $((bot_weth_after - bot_weth_before)) wei WETH"
if [ "$debt_token_after" -eq 0 ] && [ "$repaid" -gt 0 ] && [ "$repaid" -lt "$((debt_token_before / 2))" ]; then
  ok "underwater position hit Aave's bad-debt path: the whole debt token balance was burned ($debt_token_before -> 0) while the bot repaid only $repaid USDC (what the collateral covered)"
else
  bad "expected the bad-debt signature (debt token $debt_token_before -> $debt_token_after, repaid $repaid)"
fi
[ "$((bot_weth_after - bot_weth_before))" -gt 0 ] \
  && ok "bot received the collateral with the bonus (+$((bot_weth_after - bot_weth_before)) wei WETH)" \
  || bad "bot received no collateral"

# Hand the oracle back to mainnet sync (deterministic: the engine scans T up to
# its head, clears the marks and pushes reth's values at the cursor).
$ENGINE curl -sf -m 25 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
  -d "{\"address\":\"$ORACLE\",\"slots\":[],\"account_fields\":false,\"clear\":true,\"all\":true}" >/dev/null 2>&1
restored=""
for _ in $(seq 1 8); do
  sleep 2
  restored=$(castT call $ORACLE "getSourceOfAsset(address)(address)" $WETH | tr -d '\r')
  [ "$restored" = "$TRUE_SOURCE" ] && break
done
[ "$restored" = "$TRUE_SOURCE" ] && ok "oracle restored via unshadow ($restored)" \
  || bad "oracle NOT restored ($restored vs $TRUE_SOURCE)"

echo
echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
