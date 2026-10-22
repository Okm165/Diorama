#!/usr/bin/env bash
# Tenderly-core capability proof: the three things a fork must do.
#   A. live fork     - T tracks live mainnet blocks, watched state matches reth
#   B. mint funds    - ETH, WETH (slot 3), USDC (packed slot 9) via cheatcodes
#   C. live markets  - Aave supply/borrow/repay/withdraw, Uniswap V3 swap,
#                      Chainlink price, debug traces, state overrides
# Ends by resetting the venues it touched back to mainnet truth (unshadow).
# Usage: NS=mainnet-mirror ./scripts/tenderly-core.sh
set -uo pipefail

NS=${NS:-mainnet-mirror}
K="kubectl -n $NS"
ENGINE="$K exec deploy/engine --"
ANVIL_T="$K exec deploy/anvil-t --"
pass=0; fail=0
ok()  { echo "  ✔ $*"; pass=$((pass+1)); }
bad() { echo "  ✘ $*"; fail=$((fail+1)); }
info(){ echo "  · $*"; }
num() { printf '%d' "${1:-0}" 2>/dev/null || echo 0; }

rpc() { # rpc <url> <method> [params-json]
  local url=$1 method=$2 params=${3:-[]}
  $ENGINE curl -sf -m 25 "$url" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" 2>/dev/null
}
# offline cast (calldata/index) must NOT get --rpc-url: cast rejects the flag for
# those subcommands, the error goes to /dev/null and the tx ships empty calldata
# (measured: a 21000-gas "supply" that reverted - a plain transfer).
castoff() { $ANVIL_T cast "$@" 2>/dev/null | tr -d '\r'; }
castT() { $ANVIL_T cast "$@" --rpc-url http://localhost:8545 2>/dev/null | tr -d '\r'; }
castR() { $ANVIL_T cast "$@" --rpc-url http://reth:8545 2>/dev/null | tr -d '\r'; }
send_tx() { # send_tx <from> <to> <calldata> -> hash | error text
  rpc http://anvil-t:8545 eth_sendTransaction \
    "[{\"from\":\"$1\",\"to\":\"$2\",\"data\":\"$3\",\"gasPrice\":\"0x174876e800\"}]" | jq -r '.result // .error.message'
}
wait_tx() { # wait_tx <hash> [tries] -> status
  local tries=${2:-45} st
  for _ in $(seq 1 "$tries"); do
    st=$(rpc http://anvil-t:8545 eth_getTransactionReceipt "[\"$1\"]" | jq -r '.result.status // "pending"' 2>/dev/null)
    [ "$st" != "pending" ] && { echo "$st"; return 0; }
    sleep 4
  done
  echo pending
}
sim() { # sim <from> <to> <calldata> -> "ok" or the revert reason (Tenderly UX)
  rpc http://anvil-t:8545 eth_call "[{\"from\":\"$1\",\"to\":\"$2\",\"data\":\"$3\"},\"latest\"]" \
    | jq -r '.error.message // "ok"'
}
send_ok() { # send_ok <from> <to> <calldata> -> hash, or empty on reject/revert
  local h st r
  r=$(sim "$1" "$2" "$3")
  [ "$r" != "ok" ] && info "simulate first: $r" >&2
  h=$(send_tx "$1" "$2" "$3")
  case "$h" in 0x*) : ;; *) info "send rejected: $h" >&2; return 1 ;; esac
  st=$(wait_tx "$h" 45)
  [ "$st" = "0x1" ] || { info "send reverted ($st): $h" >&2; return 1; }
  echo "$h"
}

USDC=0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
WETH=0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
POOL=0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2
A_WETH=0x4d5F47FA6A74757f35C14fD3a6Ef8E3C9BC514E8
ORACLE=0x54586bE62E3c3580375aE3723C145253060Ca0C2
ROUTER=0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45
UNIV3=0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640
MKT=0x00000000000000000000000000000000000F00d2

echo "== Tenderly-core proof (namespace $NS)"
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

echo "-- D. Tenderly-class debug surface"
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
for addr in $MKT $WETH $USDC $A_WETH $UNIV3; do
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
