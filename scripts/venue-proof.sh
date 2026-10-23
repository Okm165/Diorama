#!/usr/bin/env bash
# The venue capability proof (plan §7.8, the counterpart of
# `make mainnet-proof`): the four things the venue stack must do —
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
#   F. scenario controls - the scripted book and the funding override are served
#                        exactly, and their hand-backs restore live data
#   G. chain-time follow - after a forward jump of V's clock (their harness's
#                        shape), a touched account's served `time` follows V's
#                        head while an untouched account keeps the venue's own
#                        time (the COW overlay's split, measured)
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

source "$(dirname "$0")/lib-venue.sh"

sim_account() { sipost /info "{\"type\":\"clearinghouseState\",\"user\":\"$1\"}"; }
metric() { smetrics | awk -v k="$1" '$1 == k || $1 ~ "^"k"\\{" {s+=$2; found=1} END {print (found ? s : 0)}'; }
# Poll a metric to a threshold (bounded): the bridge's and the sim's passes are
# 1 Hz, but a cold market feed defers a bridge pass (the block is re-scanned),
# so a fixed sleep flakes — a poll outlasts it, and a real breakage still
# fails at the budget's end. Prints the last value seen.
wait_metric() { # wait_metric <name> <min> [budget_secs=20]
  local name=$1 min=$2 budget=${3:-20} i=0 v=0
  while [ "$i" -lt "$budget" ]; do
    v=$(metric "$name")
    [ "$v" -ge "$min" ] 2>/dev/null && break
    sleep 1
    i=$((i + 1))
  done
  printf '%s' "$v"
}
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
# The §14.8 evaluation (accrual + liquidation check against current marks)
# runs on every account-scoped read as well as on the 1 Hz tick and on
# `advance` — so the seeding call itself closes an underwater account. The
# assertion is the metric, not which caller fired the evaluation.
liq0=$(metric venue_sim_liquidations_total)
spost /control/set_account "{\"address\":\"$UNDER\",\"state\":$seed}" > /dev/null \
  && ok "underwater account seeded" || bad "set_account refused"
spost /control/advance '{"seconds":2}' > /dev/null
liq1=$(wait_metric venue_sim_liquidations_total $((liq0 + 1)))
after=$(sim_account "$UNDER")
flat=$(echo "$after" | json 'len(d["assetPositions"])')
[ "$liq1" -gt "$liq0" ] && [ "$flat" = "0" ] \
  && ok "the underwater position liquidated (equity=$(echo "$after" | json 'd["marginSummary"]["accountValue"]'))" \
  || bad "no liquidation (metric $liq0 -> $liq1, positions=$flat)"
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
credits1=$(wait_metric venue_bridge_credits_total $((credits0 + 1)))
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
    + "00" * 28 + "ff" * 4                        # sourceDex = u32::MAX (ABI word)
    + "00" * 28 + "ff" * 4                        # destinationDex = u32::MAX (ABI word)
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

echo "-- F. scenario controls (scripted book + funding override, and their hand-backs)"
spost /control/set_book '{"coin":"BTC","levels":[[{"px":"100000","sz":"1.5","n":3}],[{"px":"100100","sz":"2","n":5}]]}' > /dev/null
book=$(sipost /info '{"type":"l2Book","coin":"BTC"}')
px=$(echo "$book" | json 'd["levels"][0][0]["px"]')
sz=$(echo "$book" | json 'd["levels"][1][0]["sz"]')
n=$(echo "$book" | json 'd["levels"][0][0]["n"]')
[ "$px" = "100000" ] && [ "$sz" = "2" ] && [ "$n" = "3" ] \
  && ok "the scripted book is served exactly (px/sz/n)" \
  || bad "scripted book not served: px=$px sz=$sz n=$n"
spost /control/set_book '{"coin":"BTC"}' > /dev/null
levels=$(sipost /info '{"type":"l2Book","coin":"BTC"}' | json 'len(d["levels"][0])')
[ "$levels" = "20" ] && ok "the hand-back restored the live depth ($levels levels/side)" \
  || bad "live depth not restored: $levels levels"
spost /control/set_funding '{"coin":"BTC","rate":"0.0001"}' > /dev/null
funding=$(sipost /info '{"type":"metaAndAssetCtxs"}' | python3 -c "
import sys,json
d=json.load(sys.stdin)
i=[k for k,u in enumerate(d[0]['universe']) if u['name']=='BTC'][0]
print(d[1][i]['funding'])")
[ "$funding" = "0.0001" ] && ok "the funding override is served ($funding)" \
  || bad "funding override not served: $funding"
spost /control/set_funding '{"coin":"BTC"}' > /dev/null
live=$(sipost /info '{"type":"metaAndAssetCtxs"}' | python3 -c "
import sys,json
d=json.load(sys.stdin)
i=[k for k,u in enumerate(d[0]['universe']) if u['name']=='BTC'][0]
print(d[1][i]['funding'])")
[ "$live" != "0.0001" ] && ok "the hand-back returned funding to live ($live)" \
  || bad "funding still overridden: $live"

echo "-- G. the chain-time invariant on V after a jump"
# Their harness jumps every chain forward and then reads the venue; a served
# `time` that lags V afterwards fails their freshness gate. The bridge reads
# V's head timestamp once per new head and the sim serves
# `max(wall, sim clock, chain time)`. Measured 2026-10-23: a touched account
# followed a +3600 s jump exactly (served == head ts) while an untouched
# account kept the real venue's own time (pass-through, by design). The jump
# below is permanent until the next re-pin (anvil_reset re-forks at a source
# block, whose timestamp is wall) — keep it small and run it last.
UNTOUCHED=0x00000000000000000000000000000000000000f9
head_ts=$(rpcv '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' | json 'int(d["result"]["timestamp"],16)')
served0=$(sim_account "$SIM_ACCT" | json 'd["time"]//1000')
jump=$((head_ts + 120))
rpcv "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setTime\",\"params\":[$jump]}" > /dev/null && mine
served1=$served0
for _ in $(seq 1 10); do
  served1=$(sim_account "$SIM_ACCT" | json 'd["time"]//1000')
  [ "$served1" -ge "$jump" ] && break
  sleep 1
done
[ "$served1" -ge "$jump" ] \
  && ok "a touched account's served time follows V (served=$served1 >= jumped head=$jump)" \
  || bad "served time lagged V after the jump: served=$served1 head=$jump"
[ "$served1" -ge "$served0" ] \
  && ok "the served clock never moved backwards ($served0 -> $served1)" \
  || bad "served clock regressed: $served0 -> $served1"
raw_time=$(sim_account "$UNTOUCHED" | json 'd["time"]//1000')
[ "$raw_time" -lt "$jump" ] \
  && ok "an untouched account still serves the venue's own time (pass-through)" \
  || bad "untouched account followed the jump: time=$raw_time jump=$jump"

echo "== venue capability proof: $pass passed, $fail failed"
[ "$fail" = "0" ]
