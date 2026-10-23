#!/usr/bin/env bash
# The venue capability proof (plan §7.8, the counterpart of
# `make capability-proof`): the four things the venue stack must do —
#   A. mint on V       - `/admin/deal` lands an ERC20 balance (silent COW write)
#   B. sim deal        - the simulator funds an account and the served identity
#                        holds (accountValue == totalRawUsd + signed PnL)
#   C. crash ⇒ liquidation - a seeded underwater account liquidates on the
#                        automaton; a crash draws equity down and hands back
#   D. bridge round-trip - EVM→Core credits the sender's Core spot balance; a
#                        `sendAsset` completes Core→EVM through the system
#                        address (the counters move, the balances round-trip)
#   E. block production - a tx left in the pool (no manual mine) is mined by
#                        the engine on its next pass (`venue_mined_blocks_total`)
#
# Run from the repo root:
#   make venue-proof                    # against the deployed venue-mirror
#   ENGINE_URL=http://127.0.0.1:18681 V_URL=http://127.0.0.1:18610 \
#     SIM_INFO=http://127.0.0.1:18790 SIM_CTRL=http://127.0.0.1:18791 \
#     ./scripts/venue-proof.sh          # direct mode: a scratch stack
set -euo pipefail

NS="${NS:-venue-mirror}"
USDC="${USDC:-0xb88339CB7199b77E23DB6E890353E22632Ba630f}"    # HyperEVM native USDC
# The bridge leg uses PURR (spot index 1): Circle's USDC blacklists the system
# addresses (measured live: `isBlacklisted(0x2000…0000) == true`) and its
# spotMeta link (0x6b9e…) is a phantom that cannot emit Transfer logs — so the
# faithful EVM→Core path needs a token whose link is a real ERC-20 that
# permits the system address (PURR: 18-dec ERC-20, core wei 5).
PURR="${PURR:-0x9b498c3c8a0b8cd8ba1d9851d40d186f1872b44e}"
PURR_INDEX=1
PURR_SYSTEM=0x2000000000000000000000000000000000000001        # 0x20 ‖ index 1 (BE)
PURR_WEI_DECIMALS=5
PURR_AMOUNT=100000000000000000000                            # 100 PURR (18-dec ERC-20)
PURR_SEND=5000000000000000000                                # 5 PURR to Core
PURR_CORE_WEI=300000                                         # 3 PURR back (5-dec wei)
CORE_WRITER=0x3333333333333333333333333333333333333333
CAST="${CAST:-cast}"
ACCT="${ACCT:-0x00000000000000000000000000000000000000e1}"    # the V-side actor
SIM_ACCT="${SIM_ACCT:-0x00000000000000000000000000000000000000e2}"
UNDER="${UNDER:-0x00000000000000000000000000000000000000e3}"
MINT="${MINT:-100000000}"                                     # 100 USDC (6-dec ERC20)

pass=0; fail=0
ok()   { echo "  ✔ $*"; pass=$((pass+1)); }
bad()  { echo "  ✘ $*"; fail=$((fail+1)); }
info() { echo "  · $*"; }

# One transport per mode; every call goes through these.
if [ -n "${ENGINE_URL:-}" ]; then
  eget()   { curl -sf -m 60 "$ENGINE_URL$1"; }
  epost()  { curl -sf -m 60 -X POST "$ENGINE_URL$1" -H 'Content-Type: application/json' -d "$2"; }
  rpcv()   { curl -sf -m 30 -X POST "$V_URL" -H 'Content-Type: application/json' -d "$1"; }
  sget()   { curl -sf -m 30 "$SIM_INFO$1"; }
  sipost() { curl -sf -m 30 -X POST "$SIM_INFO$1" -H 'Content-Type: application/json' -d "$2"; }
  spost()  { curl -sf -m 30 -X POST "$SIM_CTRL$1" -H 'Content-Type: application/json' -d "$2"; }
  smetrics() { curl -sf -m 30 "$SIM_CTRL/metrics"; }
  balance() { "$CAST" call "$1" 'balanceOf(address)(uint256)' "$2" --rpc-url "$V_URL" 2>/dev/null | awk '{print $1}'; }
else
  exec_pod() { kubectl -n "$NS" exec -i deploy/engine -- "$@"; }
  eget()   { exec_pod curl -sf -m 60 "localhost:8080$1"; }
  epost()  { exec_pod curl -sf -m 60 -X POST "localhost:8080$1" -H 'Content-Type: application/json' -d "$2"; }
  rpcv()   { exec_pod curl -sf -m 30 -X POST http://anvil-v:8545 -H 'Content-Type: application/json' -d "$1"; }
  sget()   { exec_pod curl -sf -m 30 "http://venue-sim:8090$1"; }
  sipost() { exec_pod curl -sf -m 30 -X POST "http://venue-sim:8090$1" -H 'Content-Type: application/json' -d "$2"; }
  spost()  { exec_pod curl -sf -m 30 -X POST "http://venue-sim:8091$1" -H 'Content-Type: application/json' -d "$2"; }
  smetrics() { exec_pod curl -sf -m 30 "http://venue-sim:8091/metrics"; }
  balance() {
    exec_pod curl -sf -m 30 -X POST http://anvil-v:8545 -H 'Content-Type: application/json' \
      -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$1\",\"data\":\"0x70a08231000000000000000000000000${2:2}\"},\"latest\"]}" \
      | python3 -c "import json,sys; r=json.load(sys.stdin).get('result','0x0'); print(int(r,16))"
  }
fi

json()   { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
sim_account() { sipost /info "{\"type\":\"clearinghouseState\",\"user\":\"$1\"}"; }
metric() { smetrics | awk -v k="$1" '$1 == k || $1 ~ "^"k"\\{" {s+=$2; found=1} END {print (found ? s : 0)}'; }
mine()   { rpcv '{"jsonrpc":"2.0","id":1,"method":"evm_mine","params":[]}' > /dev/null; }
# eth_sendTransaction works because V runs auto-impersonate (the parity flag):
# no key material is needed for the proof's actor. V is no-mining, so the
# caller mines the block the tx lands in.
send_tx() { # send_tx <from> <to> <data>
  local tx
  tx=$(rpcv "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$1\",\"to\":\"$2\",\"data\":\"$3\"}]}" | json 'd["result"]')
  mine
  printf '%s' "$tx"
}

echo "== venue capability proof (namespace $NS) =="

echo "-- preconditions"
status=$(eget /status)
pin=$(echo "$status" | json 'd["fork_pin"]')
[ -n "$pin" ] && [ "$pin" != "None" ] && ok "V is pinned (pin=$pin)" || bad "no fork pin: $status"
ready=$(sget /readyz || true)
[ -n "$ready" ] && ok "the simulator is ready (market plane fresh)" || bad "simulator /readyz is not ready"
chain=$(rpcv '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' | json 'd["result"]')
[ "$chain" = "0x3e7" ] && ok "V answers chain 999" || bad "V chain id $chain"
automine=$(rpcv '{"jsonrpc":"2.0","id":1,"method":"anvil_getAutomine","params":[]}' | json 'd["result"]')
[ "$automine" = "False" ] && ok "automine off (txs land on our call)" || bad "automine=$automine"

echo "-- A. mint on V (the silent COW write) + gas for the actor"
rpcv "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setBalance\",\"params\":[\"$ACCT\",\"0x8ac7230489e80000\"]}" > /dev/null
g0=$(rpcv "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBalance\",\"params\":[\"$ACCT\",\"latest\"]}" | json 'd["result"]')
[ "$g0" = "0x8ac7230489e80000" ] && ok "actor gas minted (10 ETH via anvil_setBalance)" || bad "gas fund: $g0"
epost /admin/deal "{\"token\":\"$USDC\",\"to\":\"$ACCT\",\"amount\":\"$MINT\"}" | json 'd["ok"]' | grep -q True \
  && ok "deal accepted" || bad "deal refused"
b0=$(balance "$USDC" "$ACCT")
[ "$b0" = "$MINT" ] && ok "the dealt balance landed ($b0)" || bad "deal balance $b0 != $MINT"

echo "-- B. sim deal + the served identity"
# The deal response carries the affected balance; the seed's marginSummary and
# that balance are different surfaces, so the check is a two-deal delta.
d1=$(spost /control/deal "{\"address\":\"$SIM_ACCT\",\"class\":\"perp\",\"amount\":\"1000\"}" | json 'd["balance_after"]')
d2=$(spost /control/deal "{\"address\":\"$SIM_ACCT\",\"class\":\"perp\",\"amount\":\"1000\"}" | json 'd["balance_after"]')
grew=$(python3 -c "from decimal import Decimal; print('yes' if Decimal('$d2') - Decimal('$d1') == 1000 else 'no')")
[ "$grew" = "yes" ] && ok "perp funds dealt (+1000 twice, $d1 -> $d2)" || bad "deal delta $d1 -> $d2"
ident=$(sim_account "$SIM_ACCT" | python3 -c "
import json,sys
from decimal import Decimal
d=json.load(sys.stdin)
v=Decimal(d['marginSummary']['accountValue']); raw=Decimal(d['marginSummary']['totalRawUsd'])
pnl=sum(Decimal(p['position']['positionValue']) * (-1 if p['position']['szi'].startswith('-') else 1) for p in d['assetPositions'])
print('ok' if abs(v - (raw + pnl)) < Decimal('0.000001') else f'mismatch v={v} raw={raw} pnl={pnl}')")
[ "$ident" = "ok" ] && ok "served identity holds (accountValue == totalRawUsd + signed PnL)" || bad "identity: $ident"

echo "-- C. crash ⇒ liquidation (the venue-side drawdown, not an oracle crash)"
seed='{"marginSummary":{"accountValue":"0","totalNtlPos":"0","totalRawUsd":"-90000","totalMarginUsed":"1000"},"assetPositions":[{"type":"oneWay","position":{"coin":"BTC","entryPx":"100000","leverage":{"type":"cross","value":10},"marginUsed":"1000","maxLeverage":40,"positionValue":"0","returnOnEquity":"0","szi":"1","unrealizedPnl":"0","cumFunding":{"allTime":"0","sinceOpen":"0","sinceChange":"0"}}}]}'
spost /control/set_account "{\"address\":\"$UNDER\",\"state\":$seed}" > /dev/null \
  && ok "underwater account seeded" || bad "set_account refused"
liqs=$(spost /control/advance '{"seconds":2}' | json 'len(d["liquidations"])')
sleep 3
after=$(sim_account "$UNDER")
flat=$(echo "$after" | json 'len(d["assetPositions"])')
[ "$liqs" -ge 1 ] && [ "$flat" = "0" ] \
  && ok "the underwater position liquidated on the advance (equity=$(echo "$after" | json 'd["marginSummary"]["accountValue"]'))" \
  || bad "no liquidation (report=$liqs, positions=$flat)"
# A crash draws a positioned account's equity down and the hand-back restores
# the live mark (the daloc indexer's arc).
eth_seed='{"marginSummary":{"accountValue":"1000","totalNtlPos":"0","totalRawUsd":"1000","totalMarginUsed":"50"},"assetPositions":[{"type":"oneWay","position":{"coin":"ETH","entryPx":"2500","leverage":{"type":"cross","value":20},"marginUsed":"50","maxLeverage":25,"positionValue":"0","returnOnEquity":"0","szi":"0.4","unrealizedPnl":"0","cumFunding":{"allTime":"0","sinceOpen":"0","sinceChange":"0"}}}]}'
spost /control/set_account "{\"address\":\"$SIM_ACCT\",\"state\":$eth_seed}" > /dev/null
base=$(sim_account "$SIM_ACCT" | json 'd["marginSummary"]["accountValue"]')
spost /control/crash '{"coin":"ETH","pct":"30"}' | json 'd["owned"]' | grep -q True \
  && ok "crash accepted (the mark is owned)" || bad "crash refused"
drawn=$(sim_account "$SIM_ACCT" | json 'd["marginSummary"]["accountValue"]')
spost /control/crash '{"coin":"ETH"}' | json 'd["owned"]' | grep -q False \
  && ok "crash handed back" || bad "hand-back refused"
recovered=$(sim_account "$SIM_ACCT" | json 'd["marginSummary"]["accountValue"]')
[ "$drawn" != "$base" ] && [ "$recovered" != "$drawn" ] \
  && ok "crash draws the equity and the hand-back recovers it ($base -> $drawn -> $recovered)" \
  || bad "the crash arc did not move the equity ($base -> $drawn -> $recovered)"

echo "-- D. bridge round-trip (EVM→Core credit, Core→EVM completion)"
credits0=$(metric venue_bridge_credits_total); completions0=$(metric venue_bridge_completions_total)
epost /admin/deal "{\"token\":\"$PURR\",\"to\":\"$ACCT\",\"amount\":\"$PURR_AMOUNT\"}" | json 'd["ok"]' | grep -q True \
  && ok "PURR dealt for the bridge leg" || bad "PURR deal refused"
t0=$(balance "$PURR" "$ACCT")
[ "$t0" = "$PURR_AMOUNT" ] && ok "PURR balance landed ($t0)" || bad "PURR balance $t0 != $PURR_AMOUNT"
# EVM→Core: an ERC20 transfer to the token's system address credits the
# sender's Core spot balance (5 PURR, 18-dec ERC-20 → 5 core wei at 5-dec).
# (The calldata is built by cast: a 40-hex address overflows bash arithmetic.)
data=$("$CAST" calldata "transfer(address,uint256)" "$PURR_SYSTEM" "$PURR_SEND")
tx=$(send_tx "$ACCT" "$PURR" "$data")
[ -n "$tx" ] && [ "$tx" != "None" ] && ok "transfer to the system address mined ($tx)" || bad "system-address transfer failed"
sleep 3   # the bridge pass (1 s) + the sim's atomic save
credits1=$(metric venue_bridge_credits_total)
[ "$credits1" -gt "$credits0" ] 2>/dev/null && ok "EVM→Core credited ($credits0 -> $credits1)" \
  || bad "no credit recorded ($credits0 -> $credits1)"
# Core→EVM: a sendAsset naming the system address completes through it. The
# action stream: version 1, id 13 (u24 BE), then destination, subAccount,
# sourceDex, destinationDex (spot = u32::MAX), token (PURR's spot index), wei.
sendraw=$(python3 - "$PURR_SYSTEM" "$PURR_INDEX" "$PURR_CORE_WEI" <<'PY'
import sys
system = int(sys.argv[1], 16)
index = int(sys.argv[2])
wei = int(sys.argv[3])
stream = bytes.fromhex(
    "0100000d"
    + f"{system:064x}"
    + "00" * 32                                   # subAccount
    + "ff" * 4 + "00" * 28                        # sourceDex = u32::MAX
    + "ff" * 4 + "00" * 28                        # destinationDex = u32::MAX
    + f"{index:064x}"                             # token = PURR's spot index
    + f"{wei:064x}"                               # wei (core, 5-dec)
)
arg = len(stream).to_bytes(32, "big") + stream
arg += b"\x00" * ((32 - len(stream) % 32) % 32)
print("0x17938e13" + (32).to_bytes(32, "big").hex() + arg.hex())
PY
)
tx=$(send_tx "$ACCT" "$CORE_WRITER" "$sendraw")
[ -n "$tx" ] && [ "$tx" != "None" ] && ok "sendAsset (CoreWriter 0x3333) mined ($tx)" || bad "CoreWriter action failed"
# The sim's watcher decodes the action on its own cadence (seconds), then
# submits the completion tx; the engine mines it on its next pass. Poll rather
# than sleep a fixed window (measured: the decode latency varies ~4-9 s).
completions1="$completions0"
for _ in $(seq 1 20); do
  completions1=$(metric venue_bridge_completions_total)
  [ "$completions1" -gt "$completions0" ] 2>/dev/null && break
  sleep 1
done
[ "$completions1" -gt "$completions0" ] 2>/dev/null && ok "Core→EVM completed ($completions0 -> $completions1)" \
  || bad "no completion recorded ($completions0 -> $completions1)"
# The completion tx must land for the balance to move: the engine mines it
# (within one poll); this explicit mine is the harness-style fallback driver.
mine
b1=""
for _ in $(seq 1 20); do
  b1=$(balance "$PURR" "$ACCT")
  [ "$b1" = "98000000000000000000" ] && break
  sleep 1
done
# 100 - 5 sent + 3 completed = 98 PURR
[ "$b1" = "98000000000000000000" ] && ok "the round trip is exact (100 - 5 + 3 = 98 PURR on V)" \
  || bad "round-trip balance $b1 != 98000000000000000000"

echo "-- E. the engine is V's block producer (a pooled tx, no manual mine)"
m0=$(eget /metrics | awk '$1 == "venue_mined_blocks_total" {print $2}')
tx=$(rpcv "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$ACCT\",\"to\":\"$ACCT\",\"value\":\"0x1\"}]}" | json 'd["result"]')
# Deliberately no `mine`: the driver mines a non-empty pool on its next pass
# (T's counterpart is the applier, one block per applied parent block).
receipt=""
for _ in $(seq 1 20); do
  receipt=$(rpcv "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$tx\"]}" | json 'd["result"]')
  [ "$receipt" != "None" ] && break
  sleep 1
done
m1=$(eget /metrics | awk '$1 == "venue_mined_blocks_total" {print $2}')
[ "$receipt" != "None" ] && ok "the pooled tx mined without a manual mine ($tx)" \
  || bad "tx $tx stayed in the pool: no block producer"
[ "${m1:-0}" -gt "${m0:-0}" ] 2>/dev/null && ok "venue_mined_blocks_total moved ($m0 -> $m1)" \
  || bad "venue_mined_blocks_total did not move ($m0 -> $m1)"

echo "== venue capability proof: $pass passed, $fail failed"
[ "$fail" = "0" ]
