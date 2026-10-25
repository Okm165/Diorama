#!/usr/bin/env bash
# mainnet-workshop-local: a financial workshop on live mainnet state, natively
# (no reth, no cluster). The counterpart of the venue workshop
# (scripts/venue-workshop-local.sh): the same acts — the market on live data,
# minted liquidity and a position, the impact measured — run against the
# *real* protocols there and the real book here. The whole arc a margin
# product runs: minted liquidity, a real levered position, live 1:1
# measurement, a crash and a liquidation.
#
# A source anvil forks Ethereum mainnet at a public RPC and mines on top of
# the pin (its blocks carry amsterdam block access lists, so the composer's
# native rung answers every block — no traces needed). T forks the same RPC
# at the same pin, so its base is the source's base by construction and its
# state reads do not hop through the source. The engine keeps T fresh from
# the source's BALs; then:
#
#   act 1  the trader   /admin/deal mints WETH and USDC (slot discovery — the
#                       real USDC's packed slot included) -> supply WETH to
#                       the real Aave pool -> borrow USDC -> swap on the real
#                       Uniswap pool -> re-supply the swap. The swap's impact
#                       is measured the way the venue measures book
#                       consumption: the real pool's price moves, and the
#                       engine owns it (the pool lands in the shadowed set)
#   act 2  live data    the same market views sampled across several *source
#                       blocks* all match the source at the applied cursor
#                       (the engine's own probe contract), and the engine's
#                       configured probes converge
#   act 3  the bot      the oracle's source is swapped to the USDC/USD feed
#                       (the crash; a devnet write the COW keeps on T), the
#                       health factor falls under 1, and a bot liquidates the
#                       position and takes the collateral bonus
#
# Requires: anvil (ANVIL=/path/to/anvil), cast (CAST=/path/to/cast), the engine
# binary built (`make mainnet-build`), python3, curl, and a public mainnet RPC
# that serves state reads (MAINNET_RPC; drpc is the deployed stack's endpoint,
# nodies and tenderly's public gateway also pass). Ports: 19960/19962/19980
# (override with SRC_PORT/T_PORT/HTTP). Never touches the cluster.
set -euo pipefail
cd "$(dirname "$0")/.."

ANVIL="${ANVIL:-anvil}"
CAST="${CAST:-cast}"
ENGINE="${ENGINE:-target/debug/mainnet-engine}"
RPC="${MAINNET_RPC:-https://eth.drpc.org}"
SRC_PORT="${SRC_PORT:-19960}"; T_PORT="${T_PORT:-19962}"; HTTP="${HTTP:-19980}"
DIR="${DIR:-/tmp/opencode/mainnet-workshop}"
SRC="http://127.0.0.1:$SRC_PORT"; T="http://127.0.0.1:$T_PORT"
USDC=0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
WETH=0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
POOL=0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2
ORACLE=0x54586bE62E3c3580375aE3723C145253060Ca0C2
PROVIDER=0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e
A_WETH=0x4d5F47FA6A74757f35C14fD3a6Ef8E3C9BC514E8
UNIV3=0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640
ROUTER=0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45
CL_WETH_USD=0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419
CL_USDC_USD=0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6

cleanup() { for pid in ${PIDS:-}; do kill "$pid" 2>/dev/null || true; done; }
trap cleanup EXIT
rm -rf "$DIR"; mkdir -p "$DIR/state/journal"

metric() { curl -s "http://127.0.0.1:$HTTP/metrics" | awk -v k="$1" '$1 == k {print $2}'; }
source "$(dirname "${BASH_SOURCE[0]}")/lib-common.sh"
# Public endpoints rate-limit transiently; every read retries.
retry() { # retry <attempts> <cmd...>
  local n=$1 out=""; shift
  for _ in $(seq 1 "$n"); do
    out=$("$@" 2>/dev/null) && { printf '%s' "$out"; return 0; }
    sleep 3
  done
  return 1
}

# The pin read retries: public endpoints rate-limit transiently and may answer
# non-JSON bodies under load.
head_of() {
  local out
  for _ in $(seq 1 6); do
    out=$(curl -s -m 30 -X POST -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' "$1" \
      | python3 -c 'import json,sys
try:
    print(int(json.load(sys.stdin)["result"], 16))
except Exception:
    sys.exit(1)' 2>/dev/null || true)
    [ -n "$out" ] && { printf '%s' "$out"; return 0; }
    sleep 3
  done
  return 1
}
HEAD=$(head_of "$RPC") || { echo "the public RPC did not answer ($RPC)"; exit 1; }
PIN=$((HEAD - 8))
echo "== mainnet workshop (rpc $RPC, pin $PIN)"

"$ANVIL" --port "$SRC_PORT" --chain-id 1 --fork-url "$RPC" --fork-block-number "$PIN" \
  --hardfork amsterdam --block-time 1 --silent > "$DIR/source.log" 2>&1 &
PIDS="$!"
# The source's fork fetch can be throttled when two forks race the same
# endpoint: wait for it to answer, and retry when it died on the fetch.
up=false
for attempt in 1 2 3; do
  for _ in $(seq 1 30); do
    curl -sf -m 2 -X POST -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' "$SRC" >/dev/null 2>&1 \
      && { up=true; break; }
    sleep 1
  done
  [ "$up" = true ] && break
  echo "the source anvil did not answer (attempt $attempt); retrying"
  sleep 3
  "$ANVIL" --port "$SRC_PORT" --chain-id 1 --fork-url "$RPC" --fork-block-number "$PIN" \
    --hardfork amsterdam --block-time 1 --silent > "$DIR/source.log" 2>&1 &
  PIDS="$PIDS $!"
done
[ "$up" = true ] || { echo "the source anvil did not come up"; tail -5 "$DIR/source.log"; exit 1; }
# T forks the *public* RPC directly at the same pin as the source: its base is
# the source's base by construction, and the workshop's protocol calls do not
# hop through the source. The engine's source stays the local anvil (its
# amsterdam blocks carry the BALs the applier lands).
"$ANVIL" --port "$T_PORT" --chain-id 1 --fork-url "$RPC" --fork-block-number "$PIN" \
  --hardfork amsterdam --auto-impersonate --no-mining --silent > "$DIR/t.log" 2>&1 &
PIDS="$PIDS $!"
up=false
for _ in $(seq 1 30); do
  curl -sf -m 2 -X POST -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' "$T" >/dev/null 2>&1 \
    && { up=true; break; }
  sleep 1
done
[ "$up" = true ] || { echo "the T anvil did not come up"; tail -5 "$DIR/t.log"; exit 1; }

# The workshop's scope is the deployed stack's audited watchlist (the cluster
# instance) with the market views as probes: the same closure and the same
# measurements the mainnet-mirror namespace carries.
WATCH=$(python3 - <<'PY'
import re
s = open("config/instances/mainnet-mirror/instance.toml").read()
addrs = re.findall(r'\[\[watchlist\]\]\naddress = "(\S+)"', s)
print("\n".join(f'[[watchlist]]\naddress = "{a}"\nnote = "workshop scope"' for a in addrs))
PY
)
cat > "$DIR/instance.toml" <<EOF
[instance]
name = "mainnet-workshop"
namespace = "mainnet-workshop"
profile = "mainnet"

[chains.l1]
kind = "ethereum"
source_url = "$SRC"
follow = "latest"
fork_offset = 128

$WATCH

[services.engine]
state_dir = "$DIR/state"
fixture_addresses = []

[services.engine.anvil]
target_url = "$T"

[services.engine.journal]
dir = "$DIR/state/journal"

[services.engine.verifier]
probes = [
  { name = "chainlink_weth_usd", to = "$CL_WETH_USD", data = "0xfeaf968c" },
  { name = "aave_price_weth", to = "$ORACLE", data = "0xb3596f07000000000000000000000000C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2" },
  { name = "univ3_weth_usdc_slot0", to = "$UNIV3", data = "0x3850c7bd" },
  { name = "usdc_total_supply", to = "$USDC", data = "0x18160ddd" },
  { name = "weth_total_supply", to = "$WETH", data = "0x18160ddd" },
]

[services.engine.http]
addr = "127.0.0.1"
port = $HTTP
EOF

"$ENGINE" run --config "$DIR/instance.toml" > "$DIR/engine.log" 2>&1 &
PIDS="$PIDS $!"
for _ in $(seq 1 60); do curl -sf "http://127.0.0.1:$HTTP/livez" >/dev/null 2>&1 && break; sleep 0.5; done
curl -sf "http://127.0.0.1:$HTTP/livez" >/dev/null || { echo "engine down"; tail -20 "$DIR/engine.log"; exit 1; }
cursor() { curl -s "http://127.0.0.1:$HTTP/status" | python3 -c 'import json,sys
try:
    print(json.load(sys.stdin).get("cursor_t") or 0)
except Exception:
    print(0)' 2>/dev/null || echo 0; }
for _ in $(seq 1 60); do
  c=$(cursor)
  [ "${c:-0}" -ge $((PIN + 2)) ] && break; sleep 1
done
applied=$(metric mirror_blocks_applied_total)
[ "${applied:-0}" -ge 1 ] && ok "the engine applied $applied parent block(s) onto T (live from the source)" || bad "no applies"

echo "-- act 1. the trader builds a levered position (deal -> Aave -> Uniswap -> Aave)"
TRADER=0x$(printf '%040x' $(( ($(date +%s) % 100000000) * 100000 + 21 )))
BOT=0x$(printf '%040x' $(( ($(date +%s) % 100000000) * 100000 + 22 )))
# An explicit gas limit: gas *estimation* would execute the call against T's
# forked state (a slow public-RPC fetch per touched slot); the impersonated
# account pays the limit, and the fork's block gas limit is 60M.
send() { # send <from> <to> <sig> [args...]
  local from=$1 to=$2; shift 2
  retry 4 "$CAST" send --rpc-url "$T" --unlocked --from "$from" --gas-limit 3000000 "$to" "$@" >/dev/null
}
try_send() { # try_send <desc> <from> <to> <sig> [args...]
  local desc=$1 from=$2 to=$3; shift 3
  if send "$from" "$to" "$@"; then ok "$desc"; else bad "$desc (send failed)"; fi
}
call_t() { retry 4 "$CAST" call --rpc-url "$T" "$@" | awk 'NR==1 {print $1}'; }
acct() { # acct <addr> -> "collateral debt hf"
  retry 4 "$CAST" call --rpc-url "$T" "$POOL" \
    "getUserAccountData(address)(uint256,uint256,uint256,uint256,uint256,uint256)" "$1" \
    | awk 'NR==1{c=$1} NR==2{d=$1} NR==6{h=$1} END {print c, d, h}'
}
for a in "$TRADER" "$BOT"; do
  rpc "$T" anvil_setBalance "[\"$a\",\"0x21e19e0c9bab2400000\"]" >/dev/null
  rpc "$T" anvil_impersonateAccount "[\"$a\"]" >/dev/null
done
deal() { # deal <token> <to> <amount>: the probe reads the fork's state — retry
  local out=""
  for _ in $(seq 1 4); do
    out=$(curl -s -m 60 -X POST "http://127.0.0.1:$HTTP/admin/deal" -H 'content-type: application/json' \
      -d "{\"token\":\"$1\",\"to\":\"$2\",\"amount\":\"$3\"}")
    printf '%s' "$out" | python3 -c 'import json,sys
try:
    sys.exit(0 if json.load(sys.stdin).get("ok") else 1)
except Exception:
    sys.exit(1)' && { printf '%s' "$out"; return 0; }
    sleep 3
  done
  printf '%s' "$out"
}
deal_ok() { # deal_ok <desc> <token> <to> <amount>
  local d; d=$(deal "$2" "$3" "$4")
  printf '%s' "$d" | python3 -c 'import json,sys
d = json.load(sys.stdin)
sys.exit(0 if d.get("ok") else 1)' 2>/dev/null \
    && ok "$1 (key $(printf '%s' "$d" | python3 -c 'import json,sys; print(json.load(sys.stdin)["slot"])'))" \
    || bad "$1: $d"
}
deal_ok "trader funded: 1.05 WETH via /admin/deal" "$WETH" "$TRADER" 1050000000000000000
deal_ok "bot funded: 2000 USDC via /admin/deal" "$USDC" "$BOT" 2000000000
slot0_before=$(call_t "$UNIV3" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)")

try_send "trader approved WETH" "$TRADER" "$WETH" "approve(address,uint256)" "$POOL" 1000000000000000000
try_send "trader supplied 1 WETH to Aave" "$TRADER" "$POOL" "supply(address,uint256,address,uint16)" "$WETH" 1000000000000000000 "$TRADER" 0
try_send "trader borrowed 100 USDC" "$TRADER" "$POOL" "borrow(address,uint256,uint256,uint16,address)" "$USDC" 100000000 2 0 "$TRADER"
try_send "trader approved USDC to Uniswap" "$TRADER" "$USDC" "approve(address,uint256)" "$ROUTER" 50000000
try_send "trader swapped 50 USDC on the real UniV3 pool" "$TRADER" "$ROUTER" \
  "exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))" "($USDC,$WETH,500,$TRADER,50000000,0,0)"
# The market impact, measured like the venue's book consumption: the swap is a
# real trade on the real pool (the price moved), and the engine owns it — the
# pool lands in the shadowed set, which is exactly what act 2's parity check
# scopes out while everything else stays 1:1 with the source.
slot0_after=$(call_t "$UNIV3" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)")
[ -n "$slot0_after" ] && [ "$slot0_after" != "$slot0_before" ] \
  && ok "the swap moved the real pool (sqrtPriceX96 $slot0_before -> $slot0_after)" \
  || bad "the pool did not move ($slot0_before -> $slot0_after)"
owned=false
for _ in $(seq 1 20); do
  s=$(curl -s "http://127.0.0.1:$HTTP/status" | python3 -c 'import json,sys
try:
    print(" ".join(json.load(sys.stdin).get("shadowed", [])))
except Exception:
    print("")' 2>/dev/null || true)
  case " ${s,,} " in *" ${UNIV3,,} "*) owned=true; break;; esac
  sleep 1
done
[ "$owned" = true ] && ok "the engine owns the impact (the pool is devnet-owned; act 2 scopes it out)" \
  || bad "the pool never entered the shadowed set"
weth_got=$(call_t "$WETH" "balanceOf(address)(uint256)" "$TRADER")
if [ "${weth_got:-0}" -gt 0 ]; then
  try_send "trader approved the swapped WETH" "$TRADER" "$WETH" "approve(address,uint256)" "$POOL" "$weth_got"
  try_send "trader re-supplied the swap" "$TRADER" "$POOL" "supply(address,uint256,address,uint16)" "$WETH" "$weth_got" "$TRADER" 0
fi
a1=$(call_t "$A_WETH" "balanceOf(address)(uint256)" "$TRADER")
read -r coll debt hf < <(acct "$TRADER")
[ "${a1:-0}" -ge 1000000000000000000 ] && ok "levered: 1 WETH supplied + the swap re-supplied (aEthWETH $a1)" || bad "collateral did not land ($a1)"
[ "${debt:-0}" -gt 0 ] && ok "borrowed against it (debt base $debt, HF $hf)" || bad "no debt after the borrow"

echo "-- act 2. live data across source blocks (the views vs the source at the cursor)"
VIEWS="
chainlink_weth_usd $CL_WETH_USD 0xfeaf968c
chainlink_usdc_usd $CL_USDC_USD 0xfeaf968c
aave_price_weth $ORACLE 0xb3596f07000000000000000000000000C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
univ3_weth_usdc_slot0 $UNIV3 0x3850c7bd
weth_total_supply $WETH 0x18160ddd
aave_weth_income $POOL 0xd15e0053000000000000000000000000C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
"
shadowed=$(curl -s "http://127.0.0.1:$HTTP/status" | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin).get("shadowed",[])))')
rounds=0; compared=0; matched=0; skipped=0; last=""
for _ in $(seq 1 10); do
  cur=$(cursor)
  [ "$cur" = "$last" ] && { sleep 1; continue; }
  last=$cur; rounds=$((rounds+1))
  while read -r name to data; do
    [ -n "${name:-}" ] || continue
    case " ${shadowed,,} " in *" ${to,,} "*) skipped=$((skipped+1)); continue;; esac
    v_t=$(retry 4 "$CAST" call --rpc-url "$T" "$to" "$data" | tr '\n' ' ' || true)
    v_r=$(retry 4 "$CAST" call --rpc-url "$SRC" --block "$cur" "$to" "$data" | tr '\n' ' ' || true)
    compared=$((compared+1))
    [ "$v_t" = "$v_r" ] && matched=$((matched+1)) || bad "block $cur: $name diverges (T=${v_t:0:24}… src=${v_r:0:24}…)"
  done <<< "$VIEWS"
  [ "$rounds" -ge 3 ] && break
done
[ "$rounds" -ge 3 ] && [ "$compared" -ge 9 ] && [ "$matched" = "$compared" ] \
  && ok "market data flowed live: $matched/$compared view-comparisons matched the source across $rounds source blocks ($skipped devnet-owned skipped)" \
  || bad "freshness incomplete (rounds=$rounds compared=$compared matched=$matched skipped=$skipped)"
conv=$(metric mirror_t_convergence_ok_total)
[ "${conv:-0}" -ge 1 ] && ok "the engine's own probes converged ($conv ok)" || bad "no probe convergence"

echo "-- act 3. the crash and the liquidation"
ORACLE_OWNER=$(call_t "$PROVIDER" "owner()(address)")
rpc "$T" anvil_impersonateAccount "[\"$ORACLE_OWNER\"]" >/dev/null
rpc "$T" anvil_setBalance "[\"$ORACLE_OWNER\",\"0xde0b6b3a7640000\"]" >/dev/null
send "$ORACLE_OWNER" "$ORACLE" "setAssetSources(address[],address[])" "[$WETH]" "[$CL_USDC_USD]"
crash_hf=""
for _ in $(seq 1 20); do
  sleep 1
  crash_hf=$(retry 4 "$CAST" call --rpc-url "$T" "$POOL" \
    "getUserAccountData(address)(uint256,uint256,uint256,uint256,uint256,uint256)" "$TRADER" | awk 'NR==6 {print $1}')
  [ -n "$crash_hf" ] && [ "$crash_hf" != "$hf" ] && break
done
if [ -n "$crash_hf" ] && [ "$crash_hf" -lt 1000000000000000000 ] 2>/dev/null; then
  ok "position liquidatable after the oracle crash (HF $hf -> $crash_hf)"
else
  bad "HF did not fall below 1e18 ($hf -> $crash_hf)"
fi
debt_before=$(acct "$TRADER" | awk '{print $2}')
send "$BOT" "$USDC" "approve(address,uint256)" "$POOL" 500000000
send "$BOT" "$POOL" "liquidationCall(address,address,address,uint256,bool)" "$WETH" "$USDC" "$TRADER" 50000000 false
debt_after=$(acct "$TRADER" | awk '{print $2}')
[ "${debt_after:-1}" -lt "${debt_before:-0}" ] \
  && ok "the bot liquidated the position (debt base $debt_before -> $debt_after)" \
  || bad "debt did not clear ($debt_before -> $debt_after)"
bot_weth=$(call_t "$WETH" "balanceOf(address)(uint256)" "$BOT")
[ "${bot_weth:-0}" -gt 0 ] && ok "the liquidator took the collateral bonus (WETH $bot_weth)" || bad "no bonus (WETH $bot_weth)"

summary "mainnet-workshop-local" "$DIR"
