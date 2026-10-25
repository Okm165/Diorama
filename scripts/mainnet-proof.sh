#!/usr/bin/env bash
# Capability proof: the three things a fork must do.
#   A. live fork     - T tracks live mainnet blocks, watched state matches reth
#   B. mint funds    - ETH, WETH (slot 3), USDC (packed slot 9) via cheatcodes
#   C. live markets  - Aave supply/borrow/repay/withdraw, Uniswap V3 swap,
#                      Chainlink price, debug traces, state overrides
# Ends by resetting the venues it touched back to mainnet truth (unshadow).
# Usage: NS=mainnet-mirror ./scripts/mainnet-proof.sh
set -uo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib-mainnet.sh"

USDC=0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
WETH=0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
POOL=0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2
A_WETH=0x4d5F47FA6A74757f35C14fD3a6Ef8E3C9BC514E8
# The debt tokens hold the position's debt; leaving them out of the reset leaves
# a phantom debt that makes the next run's borrow fail with LtvValidationFailed
# (measured: $315 of leftover debt against zero collateral).
V_DEBT_USDC=0x72E95b8931767C79bA4EeE721354d6E99a61D004
V_DEBT_WETH=0xeA51d7853EEFb32b6ee06b1C12E6dcCA88Be0fFE
# Chainlink USDC/USD proxy: crashing WETH's source onto it prices WETH at ~$1.
USDC_USD_FEED=0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6
ORACLE=0x54586bE62E3c3580375aE3723C145253060Ca0C2
ROUTER=0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45
UNIV3=0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640
# A fresh position address per run: the proof is self-contained, and reusing one
# address accumulates Aave debt (Aave V3.3 keeps the user's debt pool-side, so a
# slot-level reset cannot clear it) and risks the same-nonce/same-hash trap.
# The address only has to be stable *within* a run.
MKT=0x$(printf '%040x' $(( ( $(date +%s) % 100000000 ) * 100000 + 1 )))

echo "== capability proof (namespace $NS)"
# T's state is "as of" the applied parent cursor; reth's head is ahead. Every
# parity read must compare T@latest against reth@cursor_t (measured: comparing
# against reth@latest reports the ~50-block safe-head lag as a false mismatch).
CUR=$(num "$($ENGINE curl -sf localhost:8080/status | jq -r .cursor_t)")
CURHEX=$(printf '0x%x' "$CUR")

echo "-- A. live fork (T is at parent cursor $CUR)"
head0=$(num "$(rpc http://anvil-t:8545 eth_blockNumber | jq -r .result)")
reth0=$(num "$(rpc http://reth:8545 eth_blockNumber | jq -r .result)")
follow0=$(num "$(rpc http://reth:8545 eth_getBlockByNumber '["safe",false]' | jq -r .result.number)")
info "T=$head0 reth=$reth0 safe=$follow0"
[ "$head0" -ge "$CUR" ] && ok "T's head is at or ahead of the applied parent cursor (T=$head0 cursor=$CUR)" \
  || bad "T head $head0 < cursor $CUR (T fell behind the parent stream)"
[ "$follow0" -le "$reth0" ] && ok "reth's safe head is $((reth0 - follow0)) blocks behind its head (reorg-safe follow)" \
  || bad "safe head $follow0 > head $reth0"
for a in $USDC $WETH; do
  pt=$(castT call $ORACLE "getAssetPrice(address)(uint256)" $a | awk '{print $1}')
  pr=$(castR call $ORACLE "getAssetPrice(address)(uint256)" $a --block "$CURHEX" | awk '{print $1}')
  [ -n "$pt" ] && [ "$pt" = "$pr" ] && ok "live oracle price $(echo $a | cut -c1-10)… = $pt (== reth@$CUR)" \
    || bad "price mismatch $(echo $a | cut -c1-10)…: T=$pt reth@$CUR=$pr"
done

echo "-- B. mint funds (unlimited, silent storage writes)"
rpc http://anvil-t:8545 anvil_setBalance "[\"$MKT\",\"0x21e19e0c9bab2400000\"]" >/dev/null
b=$(rpc http://anvil-t:8545 eth_getBalance "[\"$MKT\",\"latest\"]" | jq -r .result)
[ "$b" = "0x21e19e0c9bab2400000" ] && ok "minted 10000 ETH (anvil_setBalance)" || bad "ETH mint: $b"
rpc http://anvil-t:8545 anvil_setStorageAt "[\"$WETH\",\"$(castoff index address $MKT 3)\",\"0x000000000000000000000000000000000000000000000000016345785d8a0000\"]" >/dev/null
w=$(castT call $WETH "balanceOf(address)(uint256)" $MKT | awk '{print $1}')
[ "$w" = "100000000000000000" ] && ok "minted 0.1 WETH (slot 3 storage write)" || bad "WETH mint: $w"
rpc http://anvil-t:8545 anvil_setStorageAt "[\"$USDC\",\"$(castoff index address $MKT 9)\",\"0x0000000000000000000000000000000000000000000000000000000005f5e100\"]" >/dev/null
u=$(castT call $USDC "balanceOf(address)(uint256)" $MKT | awk '{print $1}')
[ "$u" = "100000000" ] && ok "minted 100 USDC (packed slot 9 storage write)" || bad "USDC mint: $u"

echo "-- C. live markets (real Aave pool + real Uniswap pool, no mocks)"
rpc http://anvil-t:8545 anvil_impersonateAccount "[\"$MKT\"]" >/dev/null
h=$(send_ok "$MKT" "$WETH" "$(castoff calldata 'approve(address,uint256)' $POOL 100000000000000000)") \
  && info "approve WETH->pool: $h" || bad "approve WETH failed"
a0=$(num "$(castT call $A_WETH "balanceOf(address)(uint256)" $MKT | awk '{print $1}')")
SUPPLY=$(send_ok "$MKT" "$POOL" "$(castoff calldata 'supply(address,uint256,address,uint16)' $WETH 100000000000000000 $MKT 0)") \
  || bad "Aave supply failed"
a1=$(num "$(castT call $A_WETH "balanceOf(address)(uint256)" $MKT | awk '{print $1}')")
[ "$((a1 - a0))" -ge 99900000000000000 ] && ok "Aave supply: aEthWETH +$((a1 - a0)) wei (tx $SUPPLY)" \
  || bad "Aave supply did not land: aEthWETH $a0 -> $a1"
h=$(send_ok "$MKT" "$POOL" "$(castoff calldata 'borrow(address,uint256,uint256,uint16,address)' $USDC 10000000 2 0 $MKT)") \
  && ok "Aave borrow: 10 USDC variable (tx $h)" || bad "Aave borrow failed"
acct=$(castT call $POOL "getUserAccountData(address)(uint256,uint256,uint256,uint256,uint256,uint256)" $MKT | awk '{print $1}' | tr '\n' ' ')
debt=$(echo "$acct" | awk '{print $2}'); hf=$(echo "$acct" | awk '{print $6}')
[ "$(num "$debt")" -gt 0 ] && ok "Aave accounting: debt=$debt HF=$hf collateral=$(echo "$acct" | awk '{print $1}')" \
  || bad "Aave debt did not land: $acct"
h=$(send_ok "$MKT" "$USDC" "$(castoff calldata 'approve(address,uint256)' $POOL 5000000)") && info "approve USDC->pool: $h" || bad "approve USDC failed"
h=$(send_ok "$MKT" "$POOL" "$(castoff calldata 'repay(address,uint256,uint256,address)' $USDC 5000000 2 $MKT)") \
  && ok "Aave repay: 5 USDC (tx $h)" || bad "Aave repay failed"
debt2=$(num "$(castT call $POOL "getUserAccountData(address)(uint256,uint256,uint256,uint256,uint256,uint256)" $MKT | sed -n 2p | awk '{print $1}')")
[ "$debt2" -lt "$(num "$debt")" ] && ok "Aave repay landed: debt $debt -> $debt2" || bad "repay: debt $debt -> $debt2"
h=$(send_ok "$MKT" "$POOL" "$(castoff calldata 'withdraw(address,uint256,address)' $WETH 50000000000000000 $MKT)") \
  && ok "Aave withdraw: 0.05 WETH (tx $h)" || bad "Aave withdraw failed"
a2=$(num "$(castT call $A_WETH "balanceOf(address)(uint256)" $MKT | awk '{print $1}')")
[ "$a2" -lt "$a1" ] && ok "Aave withdraw landed: aEthWETH $a1 -> $a2" || bad "withdraw: aEthWETH $a1 -> $a2"
u0=$(num "$(castT call $USDC "balanceOf(address)(uint256)" $MKT | awk '{print $1}')")
h=$(send_ok "$MKT" "$WETH" "$(castoff calldata 'approve(address,uint256)' $ROUTER 100000000000000000)") \
  && info "approve WETH->router: $h" || bad "approve router failed"
SWAP=$(send_ok "$MKT" "$ROUTER" "$(castoff calldata 'exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))' "($WETH,$USDC,500,$MKT,50000000000000000,0,0)")") \
  || bad "Uniswap swap failed"
u1=$(num "$(castT call $USDC "balanceOf(address)(uint256)" $MKT | awk '{print $1}')")
got=$(awk -v d="$((u1 - u0))" 'BEGIN { printf "%.6f", d / 1000000 }')
[ "$((u1 - u0))" -gt 0 ] && ok "Uniswap V3 swap: 0.05 WETH -> $got USDC (tx $SWAP)" \
  || bad "swap did not land: USDC $u0 -> $u1"

echo "-- C2. the financial core: crash the oracle, then liquidate for real"
# The position from C has collateral and debt. Crash WETH's oracle source (the
# same admin path a governance action takes), then have a *fresh* liquidator -
# funded through the engine's ERC20 faucet - actually seize the collateral.
# Everything here is the live Aave pool: no mocks, no special-casing.
LIQ=0x$(printf '%040x' $(( ( $(date +%s) % 100000000 ) * 100000 + 7 )))
rpc http://anvil-t:8545 anvil_setBalance "[\"$LIQ\",\"0x21e19e0c9bab2400000\"]" >/dev/null
rpc http://anvil-t:8545 anvil_impersonateAccount "[\"$LIQ\"]" >/dev/null
# 1000 USDC from the faucet (slot discovery via state-override probing).
fund=$($ENGINE curl -sf -m 25 -X POST localhost:8080/admin/deal -H 'Content-Type: application/json' \
  -d "{\"token\":\"$USDC\",\"to\":\"$LIQ\",\"amount\":\"1000000000\"}" | jq -r '.ok')
[ "$fund" = "true" ] && ok "faucet: minted 1000 USDC to the liquidator via /admin/deal (slot discovery)" \
  || bad "faucet: /admin/deal failed"
# Crash WETH: point its oracle source at the USDC feed (~$1).
OWNER=$(castT call 0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e "owner()(address)" | tr -d '\r')
rpc http://anvil-t:8545 anvil_impersonateAccount "[\"$OWNER\"]" >/dev/null
rpc http://anvil-t:8545 anvil_setBalance "[\"$OWNER\",\"0xde0b6b3a7640000\"]" >/dev/null
WETH_SRC_TRUE=$(castR call $ORACLE "getSourceOfAsset(address)(address)" $WETH | tr -d '\r')
send_tx "$OWNER" "$ORACLE" "$($ANVIL_T cast calldata "setAssetSources(address[],address[])" "[$WETH]" "[$USDC_USD_FEED]" 2>/dev/null | tr -d '\r')" >/dev/null
hf_liq=""
for _ in $(seq 1 24); do
  sleep 5
  hf_liq=$(castT call $POOL "getUserAccountData(address)(uint256,uint256,uint256,uint256,uint256,uint256)" $MKT | awk 'NR==6{print $1}')
  [ -n "$hf_liq" ] && [ "$hf_liq" -lt 1000000000000000000 ] 2>/dev/null && break
done
[ "$(num "$hf_liq")" -lt 1000000000000000000 ] 2>/dev/null && ok "position liquidatable: HF=$hf_liq (< 1e18)" \
  || bad "position not liquidatable: HF=$hf_liq"
# The liquidator: approve the pool, then liquidateCall with receiveAToken=false.
h=$(send_ok "$LIQ" "$USDC" "$(castoff calldata 'approve(address,uint256)' $POOL 1000000000)") || bad "liquidator approve failed"
debt_before=$(castT call $V_DEBT_USDC "balanceOf(address)(uint256)" $MKT | awk '{print $1}')
weth_before=$(castT call $WETH "balanceOf(address)(uint256)" $LIQ | awk '{print $1}')
h=$(send_ok "$LIQ" "$POOL" "$(castoff calldata 'liquidationCall(address,address,address,uint256,bool)' $WETH $USDC $MKT 5000000 false)") \
  && ok "liquidationCall executed (tx $h)" || bad "liquidationCall failed"
debt_after=$(castT call $V_DEBT_USDC "balanceOf(address)(uint256)" $MKT | awk '{print $1}')
weth_after=$(castT call $WETH "balanceOf(address)(uint256)" $LIQ | awk '{print $1}')
[ "$(num "$debt_after")" -lt "$(num "$debt_before")" ] && ok "debt seized: $debt_before -> $debt_after" \
  || bad "debt unchanged: $debt_before -> $debt_after"
[ "$(num "$weth_after")" -gt "$(num "$weth_before")" ] && ok "collateral seized by the liquidator: +$((weth_after - weth_before)) wei WETH (incl. bonus)" \
  || bad "liquidator received no collateral: $weth_before -> $weth_after"
# Restore the oracle: hand the source back to mainnet sync.
$ENGINE curl -sf -m 25 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
  -d "{\"address\":\"$ORACLE\",\"slots\":[],\"account_fields\":false,\"clear\":true,\"all\":true}" >/dev/null 2>&1
sleep 4
src_now=$(castT call $ORACLE "getSourceOfAsset(address)(address)" $WETH | tr -d '\r')
[ "$src_now" = "$WETH_SRC_TRUE" ] && ok "oracle restored via unshadow ($src_now)" \
  || bad "oracle restore failed: $src_now vs $WETH_SRC_TRUE"

echo "-- D2. the event bridge (plan A18: delivery is event-driven)"
# Subscribe to WETH Transfer logs over WS inside the anvil pod, then make one
# happen and assert the subscription delivered it. A fresh address funded
# through the faucet does the transfer (the deal itself is silent by design).
EV=0x$(printf '%040x' $(( ( $(date +%s) % 100000000 ) * 100000 + 9 )))
rpc http://anvil-t:8545 anvil_setBalance "[\"$EV\",\"0x21e19e0c9bab2400000\"]" >/dev/null
rpc http://anvil-t:8545 anvil_impersonateAccount "[\"$EV\"]" >/dev/null
fund=$($ENGINE curl -sf -m 25 -X POST localhost:8080/admin/deal -H 'Content-Type: application/json' \
  -d "{\"token\":\"$WETH\",\"to\":\"$EV\",\"amount\":\"1000000000000000\"}" | jq -r '.ok')
# anvil serves WS on its HTTP port (there is no --ws flag), and cast
# block-buffers stdout when redirected to a file - without stdbuf -oL a
# perfectly working subscription looks dead (this masked delivery for a while).
$ANVIL_T sh -c 'rm -f /tmp/ws.log; nohup stdbuf -oL timeout 40 cast logs --subscribe --rpc-url ws://localhost:8545 --address 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 > /tmp/ws.log 2>&1 &' >/dev/null 2>&1
sleep 4
h=$(send_ok "$EV" "$WETH" "$(castoff calldata 'transfer(address,uint256)' 0x000000000000000000000000000000000000dEaD 1)") \
  && info "event-bridge transfer: $h" || bad "event-bridge transfer failed"
# The subscription must deliver the log *while still connected* - that is the
# path a liquidation bot or a position monitor takes.
got=0
for _ in $(seq 1 12); do
  sleep 2
  got=$($ANVIL_T sh -c 'grep -c 0xddf252ad /tmp/ws.log 2>/dev/null || true' 2>/dev/null | tr -d '\r')
  [ "${got:-0}" -gt 0 ] 2>/dev/null && break
done
[ "${got:-0}" -gt 0 ] 2>/dev/null \
  && ok "WS eth_subscribe delivered the Transfer log live ($got line(s), engine-mined block)" \
  || bad "no log delivered over WS (subscription, filter or buffering failed)"
if [ -n "$h" ]; then
  got=$(rpc http://anvil-t:8545 eth_getLogs "[{\"address\":\"$WETH\",\"fromBlock\":\"0x$(printf %x $(( $(num "$(rpc http://anvil-t:8545 eth_blockNumber | jq -r .result)") - 20 )))\",\"toBlock\":\"latest\",\"topics\":[\"0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef\"]}]" | jq -r '.result | length')
  [ "${got:-0}" -gt 0 ] 2>/dev/null && ok "eth_getLogs returns the same Transfer (the polling fallback): $got log(s)" \
    || bad "eth_getLogs returned no logs for the transfer"
fi

echo "-- C3. financial time travel (the core of a margin product)"
# A margin book is a function of time: interest accrual, expiry, funding. A
# static fork gets this from evm_increaseTime; on a live mirror the next parent
# block would overwrite the clock. The engine's clock offset survives it.
# A fresh position: C2 liquidated the previous one.
TT=0x$(printf '%040x' $(( ( $(date +%s) % 100000000 ) * 100000 + 11 )))
rpc http://anvil-t:8545 anvil_setBalance "[\"$TT\",\"0x21e19e0c9bab2400000\"]" >/dev/null
rpc http://anvil-t:8545 anvil_impersonateAccount "[\"$TT\"]" >/dev/null
for spec in "$WETH 100000000000000000" "$USDC 1000000000"; do
  set -- $spec
  $ENGINE curl -sf -m 25 -X POST localhost:8080/admin/deal -H 'Content-Type: application/json' \
    -d "{\"token\":\"$1\",\"to\":\"$TT\",\"amount\":\"$2\"}" >/dev/null 2>&1
done
send_ok "$TT" "$WETH" "$(castoff calldata 'approve(address,uint256)' $POOL 100000000000000000)" >/dev/null || bad "time-travel approve failed"
send_ok "$TT" "$POOL" "$(castoff calldata 'supply(address,uint256,address,uint16)' $WETH 100000000000000000 $TT 0)" >/dev/null || bad "time-travel supply failed"
send_ok "$TT" "$POOL" "$(castoff calldata 'borrow(address,uint256,uint256,uint16,address)' $USDC 10000000 2 0 $TT)" >/dev/null || bad "time-travel borrow failed"
acct_before=$(castT call $POOL "getUserAccountData(address)(uint256,uint256,uint256,uint256,uint256,uint256)" $TT | awk '{print $1}' | tr '\n' ' ')
debt_before_ts=$(echo "$acct_before" | awk '{print $2}')
income_before=$(castT call $POOL "getReserveNormalizedIncome(address)(uint256)" $USDC | awk '{print $1}')
J=2592000 # 30 days
$ENGINE curl -sf -m 20 -X POST localhost:8080/admin/time -H 'Content-Type: application/json' \
  -d "{\"offset_seconds\":$J}" | jq -r '.offset_seconds' >/dev/null 2>&1
# let T apply a couple of parent blocks with the offset
sleep 30
t_ts_after=$($ANVIL_T cast block latest --field timestamp 2>/dev/null | tr -d '\r')
parent_ts=$($ANVIL_T cast block $($ENGINE curl -sf localhost:8080/status | jq -r .cursor_t) --field timestamp --rpc-url http://reth:8545 2>/dev/null | tr -d '\r')
debt_after=$(castT call $POOL "getUserAccountData(address)(uint256,uint256,uint256,uint256,uint256,uint256)" $TT | awk '{print $1}' | tr '\n' ' ')
debt_after_ts=$(echo "$debt_after" | awk '{print $2}')
income_after=$(castT call $POOL "getReserveNormalizedIncome(address)(uint256)" $USDC | awk '{print $1}')
[ -n "$t_ts_after" ] && [ -n "$parent_ts" ] && [ "$((t_ts_after - parent_ts))" -gt 2000000 ] 2>/dev/null \
  && ok "devnet clock is $(( (t_ts_after - parent_ts) / 86400 )) days ahead of mainnet (and still following it)" \
  || bad "clock offset did not stick: T=$t_ts_after parent=$parent_ts"
# The indexes are 27-digit: bash arithmetic overflows, awk does not.
grew=$(awk -v a="$income_before" -v b="$income_after" 'BEGIN { print (b > a) ? 1 : 0 }')
[ "$grew" = "1" ] \
  && ok "30 days of interest accrued on the live USDC reserve (income index $income_before -> $income_after)" \
  || bad "no interest accrued (index $income_before -> $income_after)"
grew=$(awk -v a="$debt_before_ts" -v b="$debt_after_ts" 'BEGIN { print (b > a) ? 1 : 0 }')
[ "$grew" = "1" ] \
  && ok "the position's debt grew with the clock ($debt_before_ts -> $debt_after_ts in base currency)" \
  || bad "debt did not grow ($debt_before_ts -> $debt_after_ts)"
$ENGINE curl -sf -m 20 -X POST localhost:8080/admin/time -H 'Content-Type: application/json' -d '{"offset_seconds":0}' >/dev/null 2>&1
info "clock offset reset to 0 (timestamps are monotonic: the clock converges, it does not jump back)"

echo "-- D. debug surface"
if [ -n "$SUPPLY" ]; then
  frames=$(rpc http://anvil-t:8545 debug_traceTransaction "[\"$SUPPLY\",{\"tracer\":\"callTracer\"}]" | jq -r '.result.calls | length' 2>/dev/null)
  [ "$(num "$frames")" -gt 0 ] && ok "debug_traceTransaction(callTracer): $frames call frames on the supply" \
    || bad "callTracer frames=$frames"
  logs=$(rpc http://anvil-t:8545 eth_getTransactionReceipt "[\"$SUPPLY\"]" | jq -r '.result.logs | length')
  [ "$(num "$logs")" -gt 0 ] && ok "receipt logs: $logs events (real Aave events on T)" || bad "supply receipt logs=$logs"
fi
DEAD=0x000000000000000000000000000000000000dEaD
ov=$(rpc http://anvil-t:8545 eth_call "[{\"to\":\"$WETH\",\"data\":\"0x70a08231000000000000000000000000000000000000000000000000000000000000dEaD\"},\"latest\",{\"$WETH\":{\"stateDiff\":{\"$(castoff index address $DEAD 3)\":\"0x00000000000000000000000000000000000000000000000000000000000000ff\"}}}]" | jq -r '.result')
[ "$ov" = "0x00000000000000000000000000000000000000000000000000000000000000ff" ] \
  && ok "eth_call stateDiff override: balanceOf(0x…dEaD)=0xff with no state change" \
  || bad "state override returned: $ov"

echo "-- E. reset the touched venues to mainnet truth (unshadow)"
# MKT is deliberately *not* reset: re-syncing its account fields would push its
# nonce back to mainnet's, and the next run would then re-send byte-identical
# txs - same nonce, same data, same gas price means the same hash, and anvil
# answers with the *old* receipt while the state (reset in between) no longer
# holds the effects. Measured: "successful" approves that changed nothing, then
# a supply reverting with a bare revert() and a repay failing on allowance.
for addr in $WETH $USDC $A_WETH $A_WETH $V_DEBT_USDC $V_DEBT_WETH $UNIV3; do
  $ENGINE curl -sf -m 25 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
    -d "{\"address\":\"$addr\",\"slots\":[],\"account_fields\":true,\"clear\":true,\"all\":true}" >/dev/null 2>&1
done
sleep 8
CUR2=$(num "$($ENGINE curl -sf localhost:8080/status | jq -r .cursor_t)")
s0=$(castT call $UNIV3 "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | tr '\n' ' ')
r0=$(castR call $UNIV3 "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" --block "$(printf '0x%x' "$CUR2")" | tr '\n' ' ')
[ "$s0" = "$r0" ] && ok "venue reset: Uniswap slot0 back to mainnet truth (reth@$CUR2)" || bad "venue reset left slot0=$s0 (reth@$CUR2=$r0)"

echo
echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
