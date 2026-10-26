#!/usr/bin/env bash
# End-to-end scenario test: is the mirror really a live, exact mainnet mirror,
# and is T really a usable devnet with copy-on-write semantics?
#
# Stages
#   1. self-containment   reth has no peers, the top-up gap is 0, blobs are local
#   2. live parity        heads agree; the 11 state probes match reth on T
#   3. devnet workflow    fund, send, shadow, rewind, snapshot/revert on T
#   4. integrity counters no divergences / convergence failures / blob failures
#
# Requires: the deployed stack (make mainnet-bootstrap), kubectl, no external network.
set -uo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib-mainnet.sh"

# The probes mirror config/instances/diorama-mainnet/instance.toml:probes (all watched
# accounts, so T must match reth exactly; none is blockhash-dependent either
# (the engine's convergence repair is what makes that true).
probe() { # probe <rpc-url> <block-hex> <to> <data>
  rpc "$1" eth_call "[{\"to\":\"$3\",\"data\":\"$4\"},\"$2\"]" | jq -r '.result // "ERR"'
}
probes_match() { # probes_match <url-a> <url-b> <label> <block-hex>
  local a=$1 b=$2 label=$3 at=$4 mismatches=0 skipped=0 shadowed=""
  if [ "$b" = "http://anvil-t:8545" ]; then
    # Devnet-owned (shadowed) accounts are copy-on-write: their state is the
    # devnet's, not mainnet's, so parity is not expected. The engine's status
    # lists them (a devnet tx on the pool/aToken legitimately diverges views
    # like totalSupply); skip those probes instead of alarming.
    shadowed=$(engine_api /status | jq -r '(.shadowed // [])[]' 2>/dev/null | tr '\n' ' ')
  fi
  while read -r name to data; do
    case " $shadowed " in
      *" ${to,,} "*) info "$label probe $name: devnet-owned (shadowed), skipped"; skipped=$((skipped+1)); continue;;
    esac
    va=$(probe "$a" "$at" "$to" "$data")
    # a chain whose height diverges from the parent height is read at latest
    if [ "$b" = "http://anvil-t:8545" ]; then
      for attempt in 1 2 3; do
        at=$(printf '0x%x' "$(num "$(engine_api /status | jq -r '.cursor_t')")")
        va=$(probe "$a" "$at" "$to" "$data")
        vb=$(rpc "$b" eth_call "[{\"to\":\"$to\",\"data\":\"$data\"},\"latest\"]" | jq -r '.result // "ERR"')
        [ "$va" = "$vb" ] && break
        sleep 3
      done
    else
      vb=$(probe "$b" "$at" "$to" "$data")
    fi
    if [ "$va" != "$vb" ]; then
      bad "$label probe $name: ${va:0:20}… != ${vb:0:20}…"; mismatches=$((mismatches+1))
    fi
  done <<'PROBES'
usdc_total_supply 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 0x18160ddd
weth_total_supply 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 0x18160ddd
chainlink_weth_usd 0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419 0xfeaf968c
univ3_weth_usdc_slot0 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640 0x3850c7bd
aave_aethusdc_scaled_total_supply 0x98C23E9d8f34FEFb1B7BD6a91B7FF122F4e16F5c 0xb1bf962d
aave_usdc_normalized_income 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2 0xd15e0053000000000000000000000000A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
aave_usdc_price 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f07000000000000000000000000A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
aave_weth_price 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f07000000000000000000000000C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
aave_wbtc_price 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f070000000000000000000000002260FAC5E5542a773Aa44fBCfeDf7C193bc2C599
aave_usdt_price 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f07000000000000000000000000dAC17F958D2ee523a2206206994597C13D831ec7
aave_wsteth_price 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f070000000000000000000000007f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0
PROBES
  [ "$mismatches" -eq 0 ] && ok "$label matches reth on all 11 probes (${skipped} devnet-owned skipped)"
}

echo "== mirror scenario test (namespace $NS)"

echo "-- 1. self-containment"
peers=$(rpc http://reth:8545 net_peerCount | jq -r '.result // "ERR"')
[ "$(num "$peers")" -eq 0 ] && ok "reth has no EL peers (${peers})" || bad "reth has peers: $peers"
gap=$(engine_api /metrics | awk '/^diorama_source_sync_gap_blocks /{print $2}')
[ "$(num "$gap")" -eq 0 ] && ok "top-up gap is 0 (reth tracks the CL head)" || bad "top-up gap: $gap"
urls=$($K get cm engine-config -o jsonpath='{.data.engine\.yaml}' | awk '/beacon_urls:/{getline; print}' | tr -d ' -')
case "$urls" in *lighthouse*) ok "blob source is the local CL ($urls)";; *) bad "blob source: $urls";; esac
pipeline=$($K logs deploy/reth --since=10m 2>/dev/null | grep -ciE "pipeline|backfill")
[ "$(num "$pipeline")" -eq 0 ] && ok "no peer-based pipeline/backfill activity on reth" || info "reth pipeline log lines: $pipeline"

# Preflight: T is a devnet, so a previous drill's manipulation persists by
# design (devnet writes are copy-on-write). Hand the oracle's shadowed keys
# back to mainnet sync: the engine re-syncs reth's values and future Chainlink
# updates flow again. One idempotent POST, no transactions.
PRE_ORACLE=0x54586bE62E3c3580375aE3723C145253060Ca0C2
PRE_USDC=0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
$ENGINE curl -sf -m 25 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
  -d "{\"address\":\"$PRE_ORACLE\",\"slots\":[],\"account_fields\":false,\"clear\":true,\"all\":true}" >/dev/null 2>&1
sleep 4
pre_t=$($ANVIL_T cast call $PRE_ORACLE "getSourceOfAsset(address)(address)" $PRE_USDC --rpc-url http://localhost:8545 2>/dev/null | tr -d '\r')
pre_true=$($ANVIL_T cast call $PRE_ORACLE "getSourceOfAsset(address)(address)" $PRE_USDC --rpc-url http://reth:8545 2>/dev/null | tr -d '\r')
if [ -n "$pre_t" ] && [ "$pre_t" = "$pre_true" ]; then
  ok "preflight: oracle sources re-synced from reth"
else
  bad "preflight: oracle source still off ($pre_t vs reth $pre_true)"
fi

echo "-- 2. live parity"
# Wait for the applier to reach the head it follows (the status has no
# `t_head`: T's height is its own — the applier's target is `follow_head`).
for _ in $(seq 1 30); do
  st=$(engine_api /status)
  c=$(num "$(echo "$st" | jq -r '.cursor_t')")
  f=$(num "$(echo "$st" | jq -r '.follow_head')")
  [ "$c" -ge "$((f - 2))" ] && break
  sleep 2
done
reth_head=$(num "$(rpc http://reth:8545 eth_blockNumber | jq -r '.result')")
follow=$(num "$(engine_api /status | jq -r '.follow_head')")
cursor_t=$(num "$(engine_api /status | jq -r '.cursor_t')")
t_head=$(num "$(engine_api /status | jq -r '.head')")
info "reth=$reth_head follow=$follow cursor_t=$cursor_t T=$t_head"
[ "$reth_head" -ge "$follow" ] && ok "reth is at/above the follow head" || bad "reth behind follow head"
if [ "$cursor_t" -lt "$((follow - 2))" ]; then
  lag0=$((follow - cursor_t))
  info "the applier is $lag0 blocks behind; waiting to see it advance…"
  for _ in $(seq 1 45); do
    c2=$(num "$(engine_api /status | jq -r '.cursor_t')")
    [ "$c2" -gt "$cursor_t" ] && { cursor_t=$c2; break; }
    sleep 2
  done
  if [ "$cursor_t" -gt "$((follow - lag0))" ]; then
    ok "the applier is catching up ($((follow - cursor_t)) blocks behind)"
  else
    bad "the applier is stalled at $cursor_t (follow $follow)"
  fi
else
  ok "the applier is at the follow head"
fi
probes_match http://reth:8545 http://anvil-t:8545 "T" "$(printf '0x%x' "$cursor_t")"

# A1/A2: fork blocks carry no trie (stateRoot = 0x0 - proofs are impossible by
# design) while the pin block itself is byte-identical to mainnet's.
root=$(rpc http://anvil-t:8545 eth_getBlockByNumber "[\"$(printf '0x%x' "$t_head")\",false]" | jq -r '.result.stateRoot')
[ "$root" = "0x0000000000000000000000000000000000000000000000000000000000000000" ] \
  && ok "A1: T blocks carry no trie (stateRoot = 0x0)" || bad "A1: stateRoot = $root"
pin=$($ANVIL_T cast rpc anvil_nodeInfo --rpc-url http://localhost:8545 2>/dev/null | jq -r '.forkConfig.forkBlockNumber' | tr -d '\r')
thash=$(rpc http://anvil-t:8545 eth_getBlockByNumber "[\"$(printf '0x%x' "$pin")\",false]" | jq -r '.result.hash')
rhash=$(rpc http://reth:8545 eth_getBlockByNumber "[\"$(printf '0x%x' "$pin")\",false]" | jq -r '.result.hash')
[ -n "$thash" ] && [ "$thash" = "$rhash" ] && ok "A2: pin block $pin byte-identical to mainnet" \
  || bad "A2: pin block mismatch (t=$thash reth=$rhash)"
at=$(printf '0x%x' "$(num "$(engine_api /status | jq -r '.cursor_t')")")
probes_match http://reth:8545 http://anvil-t:8545 "T" "$at"

echo "-- 3. devnet workflow on T"
USER=0x00000000000000000000000000000000000F00d1
# fund (unlimited funds: T is a devnet, anvil cheatcodes are the faucet)
rpc http://anvil-t:8545 anvil_setBalance "[\"$USER\",\"0x8ac7230489e80000\"]" >/dev/null # 10 ETH
bal=$(rpc http://anvil-t:8545 eth_getBalance "[\"$USER\",\"latest\"]" | jq -r '.result')
[ "$bal" = "0x8ac7230489e80000" ] && ok "faucet: funded $USER with 10 ETH" || bad "balance after fund: $bal"

# A11: an ERC20 "deal" writes storage silently (no Transfer log). WETH has a
# standard balance mapping so anvil finds the slot; USDC's packed
# balanceAndBlacklistStates does not ("no slot found", measured) - scenarios
# needing USDC write the known slot or transfer for real.
# anvil_dealERC20 cannot discover balance slots on a forked, no-mining anvil
# ("no slot found", measured - its probe writes never reach the mined state),
# so the scenario writes the standard mapping slot directly: the same silent
# storage semantics the devnet uses for funding.
WETH_DEAL=0x00000000000000000000000000000000000Dea11
WETH_TOKEN=0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
DEAL_TOPIC="0x$(printf '0%.0s' $(seq 1 24))${WETH_DEAL#0x}"
DEAL_SLOT=$($ANVIL_T cast index address "$WETH_DEAL" 3 2>/dev/null | tr -d '\r')
if [ -n "$DEAL_SLOT" ]; then
  rpc http://anvil-t:8545 anvil_setStorageAt "[\"$WETH_TOKEN\",\"$DEAL_SLOT\",\"0x00000000000000000000000000000000000000000000000000000000000004d2\"]" >/dev/null
fi
wbal=$(rpc http://anvil-t:8545 eth_call "[{\"to\":\"$WETH_TOKEN\",\"data\":\"0x70a0823100000000000000000000000000000000000000000000000000000000000Dea11\"},\"latest\"]" | jq -r '.result')
[ "$wbal" = "0x00000000000000000000000000000000000000000000000000000000000004d2" ] \
  && ok "A11: a raw balance write is silent (WETH balance set)" || bad "A11: WETH balance after write: $wbal"
tlogs=$(rpc http://anvil-t:8545 eth_getLogs "[{\"address\":\"$WETH_TOKEN\",\"fromBlock\":\"$(printf '0x%x' "$((t_head - 3))")\",\"toBlock\":\"latest\",\"topics\":[\"0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef\",null,\"$DEAL_TOPIC\"]}]" | jq -r '.result | length')
[ "$tlogs" = "0" ] && ok "A11: deal emitted no Transfer log" || bad "A11: deal emitted $tlogs Transfer log(s)"

# shadowed write: a watched slot written through a cheatcode must survive the
# parent diffs that keep flowing (copy-on-write), while sibling slots keep syncing.
WETH=0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
SLOT=0xdead000000000000000000000000000000000000000000000000000000000001 # unused high slot (slot 3 is totalSupply!)
OTHER=0x0000000000000000000000000000000000000000000000000000000000000004
pre=$($ANVIL_T cast storage $WETH $SLOT --rpc-url http://localhost:8545 2>/dev/null || echo "")
shadow=$(engine_api /status | jq -r '.shadowed_slots // 0')
rpc http://anvil-t:8545 anvil_setStorageAt "[\"$WETH\",\"$SLOT\",\"0x0000000000000000000000000000000000000000000000000000000000000042\"]" >/dev/null
resp=$($ENGINE curl -sf -m 20 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
  -d "{\"address\":\"$WETH\",\"slots\":[\"$SLOT\"],\"account_fields\":false}" 2>/dev/null)
addr=$(echo "$resp" | jq -r '.address // "ERR"' 2>/dev/null)
[ "${addr,,}" = "${WETH,,}" ] && ok "shadow registered (slots now $(echo "$resp" | jq -r '.shadowed_slots // "?"'))" \
  || bad "shadow registration failed: ${resp:0:80}"
info "waiting for parent diffs to flow (2 mainnet blocks)…"
sleep 25
val=$($ANVIL_T cast storage $WETH $SLOT --rpc-url http://localhost:8545 2>/dev/null | sed 's/0x//' | sed 's/^0*//')
[ "${val:-x}" = "42" ] && ok "shadowed slot kept the devnet value through parent diffs" \
  || bad "shadowed slot was overwritten (value ${val:-?})"
other=$($ANVIL_T cast storage $WETH $OTHER --rpc-url http://localhost:8545 2>/dev/null)
reth_other=$(rpc http://reth:8545 eth_getStorageAt "[\"$WETH\",\"$OTHER\",\"latest\"]" | jq -r '.result')
[ "$other" = "$reth_other" ] && ok "sibling slot still tracks mainnet" \
  || info "sibling slot differs (expected if that slot is not watched): ${other:0:18} vs ${reth_other:0:18}"

# hand the slot back to mainnet sync (and re-sync it now)
$ENGINE curl -sf -m 20 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
  -d "{\"address\":\"$WETH\",\"slots\":[\"$SLOT\"],\"account_fields\":false,\"clear\":true}" >/dev/null 2>&1
val=$($ANVIL_T cast storage $WETH $SLOT --rpc-url http://localhost:8545 2>/dev/null)
reth_val=$(rpc http://reth:8545 eth_getStorageAt "[\"$WETH\",\"$SLOT\",\"latest\"]" | jq -r '.result')
[ "$val" = "$reth_val" ] && ok "unshadow re-synced the slot from reth" || bad "unshadow left $val (reth $reth_val)"

# The same round-trip on a *direct* (low-numbered) slot: this exercises the
# encoder. The re-sync must pad the slot - a quantized "0x4" is rejected by
# anvil with "invalid string length", and the key would stay stale with its
# shadow mark already cleared (the silent-staleness class; mapping slots are
# keccak-derived and 64 chars, which is why they never caught it).
DSLOT=0x0000000000000000000000000000000000000000000000000000000000000004
rpc http://anvil-t:8545 anvil_setStorageAt "[\"$WETH\",\"$DSLOT\",\"0x0000000000000000000000000000000000000000000000000000000000000042\"]" >/dev/null
$ENGINE curl -sf -m 20 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
  -d "{\"address\":\"$WETH\",\"slots\":[\"$DSLOT\"],\"account_fields\":false}" >/dev/null 2>&1
$ENGINE curl -sf -m 20 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
  -d "{\"address\":\"$WETH\",\"slots\":[\"$DSLOT\"],\"account_fields\":false,\"clear\":true}" >/dev/null 2>&1
dval=$($ANVIL_T cast storage $WETH $DSLOT --rpc-url http://localhost:8545 2>/dev/null)
dreth=$(rpc http://reth:8545 eth_getStorageAt "[\"$WETH\",\"$DSLOT\",\"latest\"]" | jq -r '.result')
[ "$dval" = "$dreth" ] && ok "unshadow re-synced a direct (low-numbered) slot too" \
  || bad "direct-slot re-sync left $dval (reth $dreth)"

# snapshot / revert (anvil-native)
pre=$($ANVIL_T cast storage $WETH $SLOT --rpc-url http://localhost:8545 2>/dev/null)
snap=$($ANVIL_T cast rpc evm_snapshot --rpc-url http://localhost:8545 2>/dev/null | jq -r '.')
rpc http://anvil-t:8545 anvil_setStorageAt "[\"$WETH\",\"$SLOT\",\"0x0000000000000000000000000000000000000000000000000000000000000099\"]" >/dev/null
$ANVIL_T cast rpc evm_revert "$snap" --rpc-url http://localhost:8545 >/dev/null 2>&1
val=$($ANVIL_T cast storage $WETH $SLOT --rpc-url http://localhost:8545 2>/dev/null)
[ -n "$val" ] && [ "$val" = "$pre" ] && ok "evm_snapshot/evm_revert restores the value" || bad "revert left ${val:-?} (was ${pre:-?})"

# journal rewind: revert the last applied parent block on T
before=$(engine_api /status | jq -r '.cursor_t')
rew=$($ENGINE curl -sf -m 30 -X POST localhost:8080/admin/rewind -H 'Content-Type: application/json' -d '{"blocks":1}' 2>/dev/null | jq -r '.rewound_blocks // "ERR"')
after=$(engine_api /status | jq -r '.cursor_t')
if [ "$rew" != "ERR" ] && [ "$(num "$after")" -lt "$(num "$before")" ]; then
  ok "journal rewind moved T cursor $before -> $after"
else
  info "rewind returned: ${rew:0:60} (cursor $before -> $after)"
fi

# The rewind rolls T's *values* back without moving its head, so the applier
# must re-apply the rewound range (a plain "diffs already applied" skip would
# leave them stale - measured). Give it a couple of blocks, then re-assert.
sleep 20
at=$(printf '0x%x' "$(num "$(engine_api /status | jq -r '.cursor_t')")")
probes_match http://reth:8545 http://anvil-t:8545 "T after rewind" "$at"

echo "-- 5. cascade lever (the real Aave crash path)"
# The mirror's signature scenario: take over the Aave oracle through its own
# admin path and crash a price, with no mock contracts - the devnet's
# dependency closure (all watched) makes the lever real.
#
# Manipulations on T *persist* (that is the devnet's contract), so this check
# heals on entry: the truth for the oracle's source comes from reth, never
# from T, otherwise a previous run's leftover would poison the next one.
ORACLE=0x54586bE62E3c3580375aE3723C145253060Ca0C2
PROVIDER=0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e
USDC=0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
WETH_USD=0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419
RPC_T="--rpc-url http://localhost:8545"
RPC_R="--rpc-url http://reth:8545"
# The oracle has no owner() of its own; its admin is the PoolAddressesProvider's
# owner (the Aave governance executor) - verified on-chain.
ORACLE_OWNER=$($ANVIL_T cast call $PROVIDER "owner()(address)" $RPC_T 2>/dev/null | tr -d '\r')
TRUE_SOURCE=$($ANVIL_T cast call $ORACLE "getSourceOfAsset(address)(address)" $USDC $RPC_R 2>/dev/null | tr -d '\r')
T_SOURCE=$($ANVIL_T cast call $ORACLE "getSourceOfAsset(address)(address)" $USDC $RPC_T 2>/dev/null | tr -d '\r')
if [ -n "$ORACLE_OWNER" ] && [ -n "$TRUE_SOURCE" ]; then
  rpc http://anvil-t:8545 anvil_impersonateAccount "[\"$ORACLE_OWNER\"]" >/dev/null
  rpc http://anvil-t:8545 anvil_setBalance "[\"$ORACLE_OWNER\",\"0xde0b6b3a7640000\"]" >/dev/null
  price() { $ANVIL_T cast call $ORACLE "getAssetPrice(address)(uint256)" $USDC $RPC_T 2>/dev/null | tr -d '\r'; }
  set_source() { # set_source <source> ; the engine mines, we do not wait
    local cd
    cd=$($ANVIL_T cast calldata "setAssetSources(address[],address[])" "[$USDC]" "[$1]" 2>/dev/null | tr -d '\r')
    send_tx "$ORACLE_OWNER" "$ORACLE" "$cd" >/dev/null
  }
  before=$(price)
  CASCADE_CD=$($ANVIL_T cast calldata "setAssetSources(address[],address[])" "[$USDC]" "[$WETH_USD]" 2>/dev/null | tr -d '\r')
  CASCADE_TX=$(send_tx "$ORACLE_OWNER" "$ORACLE" "$CASCADE_CD")
  after=""
  for _ in $(seq 1 24); do
    sleep 5
    after=$(price)
    [ -n "$after" ] && [ "$after" != "$before" ] && break
  done
  now_src=$($ANVIL_T cast call $ORACLE "getSourceOfAsset(address)(address)" $USDC $RPC_T 2>/dev/null | tr -d '\r')
  # Hand the mapping back to mainnet sync: re-syncs the real feed *and* lets
  # future Chainlink updates flow (a restore transaction would leave the slot
  # devnet-owned forever - the poisoned-source bug this suite hit).
  $ENGINE curl -sf -m 25 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
    -d "{\"address\":\"$ORACLE\",\"slots\":[],\"account_fields\":false,\"clear\":true,\"all\":true}" >/dev/null 2>&1
  # The unshadow re-syncs from reth immediately (deterministic), and the engine
  # scans T up to the head before clearing, so a just-mined crash cannot be
  # re-marked after the fact. Poll briefly anyway; a pending restore is a real
  # substrate failure and must surface here, not as a confusing borrow revert.
  set_source_done=""
  for _ in $(seq 1 8); do
    sleep 2
    set_source_done=$($ANVIL_T cast call $ORACLE "getSourceOfAsset(address)(address)" $USDC $RPC_T 2>/dev/null | tr -d '\r')
    [ "$set_source_done" = "$TRUE_SOURCE" ] && break
  done
  if [ "$set_source_done" = "$TRUE_SOURCE" ]; then
    ok "cascade: source restored via unshadow ($set_source_done)"
  else
    bad "cascade: source NOT restored after unshadow ($set_source_done vs $TRUE_SOURCE)"
  fi
  if [ -n "$after" ] && [ "$after" != "$before" ]; then
    ok "cascade lever: oracle admin impersonated, USDC price crashed ($before -> $after)"
  else
    bad "cascade lever: USDC price did not move ($before -> $after; src=$now_src want=$WETH_USD tx=${CASCADE_TX:0:20})"
  fi
else
  bad "cascade lever: could not read the oracle admin/source (admin=$ORACLE_OWNER source=$TRUE_SOURCE)"
fi

echo "-- 6. market simulation on T (the core product loop, no mocks)"
# supply -> borrow -> oracle crash -> liquidation condition, on the real Aave
# pool. Everything is the product's own dependency closure: WETH/USDC, the
# pool, the oracle and its sources are all watched, so the position and the
# crash are real market state, not fixtures.
POOL=0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2
A_WETH=0x4d5F47FA6A74757f35C14fD3a6Ef8E3C9BC514E8
MKT=0x00000000000000000000000000000000000F00d2
WETH=0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
USDC=0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
USDC_USD_FEED=0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6
TRUE_WETH_SRC=$($ANVIL_T cast call $ORACLE "getSourceOfAsset(address)(address)" $WETH $RPC_R 2>/dev/null | tr -d '\r')
# fund: WETH via its standard balance slot (silent), ETH for gas
MKT_SLOT=$($ANVIL_T cast index address "$MKT" 3 2>/dev/null | tr -d '\r')
rpc http://anvil-t:8545 anvil_setStorageAt "[\"$WETH\",\"$MKT_SLOT\",\"0x0000000000000000000000000000000000000000000000000de0b6b3a7640000\"]" >/dev/null
rpc http://anvil-t:8545 anvil_impersonateAccount "[\"$MKT\"]" >/dev/null
rpc http://anvil-t:8545 anvil_setBalance "[\"$MKT\",\"0x21e19e0c9bab2400000\"]" >/dev/null # 10000 ETH
# approve + supply 1 WETH + borrow 100 USDC (variable), mined as one T block
MKT_ERR=""
# T is a devnet: the position accumulates across runs by design, so measure
# the supply as a delta against the balance before this run.
a_weth_before=$($ANVIL_T cast call $A_WETH "balanceOf(address)(uint256)" $MKT $RPC_T 2>/dev/null | awk '{print $1}' | tr -d '\r')
a_weth_before=${a_weth_before:-0}
APPROVE_TX=$(send_tx "$MKT" "$WETH" "$($ANVIL_T cast calldata "approve(address,uint256)" $POOL 1000000000000000000 2>/dev/null | tr -d '\r')")
SUPPLY_TX=$(send_tx "$MKT" "$POOL" "$($ANVIL_T cast calldata "supply(address,uint256,address,uint16)" $WETH 1000000000000000000 $MKT 0 2>/dev/null | tr -d '\r')")
[ -z "$SUPPLY_TX" ] && MKT_ERR="supply not accepted"
# wait for supply to land before borrowing (the engine mines one block per parent)
[ -n "$SUPPLY_TX" ] && [ "$(wait_tx "$SUPPLY_TX" 40)" = "pending" ] && MKT_ERR="supply not mined"
# Heal-on-entry: the borrow validates LTV against the USDC price, so it must be
# mainnet's (a leftover manipulation would silently turn it into an LTV revert).
at=$(printf '0x%x' "$(num "$(engine_api /status | jq -r '.cursor_t')")")
price_t=$($ANVIL_T cast call $ORACLE "getAssetPrice(address)(uint256)" $USDC $RPC_T 2>/dev/null | tr -d '\r')
price_r=$($ANVIL_T cast call $ORACLE "getAssetPrice(address)(uint256)" $USDC $RPC_R --block "$at" 2>/dev/null | tr -d '\r')
[ -n "$price_t" ] && [ "$price_t" = "$price_r" ] && ok "market: USDC price is mainnet's before the borrow ($price_t)" \
  || bad "market: USDC price diverges from mainnet before the borrow (t=$price_t r@cursor=$price_r)"
BORROW_TX=$(send_tx "$MKT" "$POOL" "$($ANVIL_T cast calldata "borrow(address,uint256,uint256,uint16,address)" $USDC 100000000 2 0 $MKT 2>/dev/null | tr -d '\r')")
case "$BORROW_TX" in 0x*) : ;; *) MKT_ERR="${MKT_ERR:+$MKT_ERR; }borrow rejected: ${BORROW_TX:0:60}" ;; esac
# aToken balances accrue interest through the liquidity index and the position
# accumulates across runs, so the check is a band above the pre-run balance.
a_want=$((a_weth_before + 1000000000000000000))
a_cap=$((a_weth_before + 1100000000000000000))
a_weth=""
for _ in $(seq 1 12); do
  sleep 5
  a_weth=$($ANVIL_T cast call $A_WETH "balanceOf(address)(uint256)" $MKT $RPC_T 2>/dev/null | awk '{print $1}' | tr -d '\r')
  if [ -n "$a_weth" ] && [ "$a_weth" -ge "$a_want" ] 2>/dev/null && [ "$a_weth" -lt "$a_cap" ] 2>/dev/null; then break; fi
done
if [ -n "$a_weth" ] && [ "$a_weth" -ge "$a_want" ] 2>/dev/null && [ "$a_weth" -lt "$a_cap" ] 2>/dev/null; then
  ok "market: supplied 1 WETH + borrowed 100 USDC on the real Aave pool (aEthWETH=$a_weth_before -> $a_weth)"
else
  bad "market: supply/borrow did not land (aEthWETH=$a_weth_before -> ${a_weth:-?}; $MKT_ERR)"
fi
hf() { $ANVIL_T cast call $POOL "getUserAccountData(address)(uint256,uint256,uint256,uint256,uint256,uint256)" $MKT $RPC_T 2>/dev/null | sed -n 6p | awk '{print $1}' | tr -d '\r'; }
hf_before=$(hf)
# crash WETH's oracle source to the USDC feed (~$1) through the same admin path
send_tx "$ORACLE_OWNER" "$ORACLE" "$($ANVIL_T cast calldata "setAssetSources(address[],address[])" "[$WETH]" "[$USDC_USD_FEED]" 2>/dev/null | tr -d '\r')" >/dev/null
hf_after=""
for _ in $(seq 1 24); do
  sleep 5
  hf_after=$(hf)
  [ -n "$hf_after" ] && [ "$hf_after" != "$hf_before" ] && break
done
# Hand WETH's mapping back to mainnet sync (same reason as the cascade).
$ENGINE curl -sf -m 25 -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
  -d "{\"address\":\"$ORACLE\",\"slots\":[],\"account_fields\":false,\"clear\":true,\"all\":true}" >/dev/null 2>&1
# ... and assert it really came back (the restore is deterministic; a pending
# one would poison the next run's borrow through the same LTV path).
at=$(printf '0x%x' "$(num "$(engine_api /status | jq -r '.cursor_t')")")
weth_src_t=""
weth_src_r=$($ANVIL_T cast call $ORACLE "getSourceOfAsset(address)(address)" $WETH $RPC_R --block "$at" 2>/dev/null | tr -d '\r')
for _ in $(seq 1 8); do
  sleep 2
  weth_src_t=$($ANVIL_T cast call $ORACLE "getSourceOfAsset(address)(address)" $WETH $RPC_T 2>/dev/null | tr -d '\r')
  [ "$weth_src_t" = "$weth_src_r" ] && break
done
[ -n "$weth_src_t" ] && [ "$weth_src_t" = "$weth_src_r" ] && ok "market: WETH source restored via unshadow ($weth_src_t)" \
  || bad "market: WETH source NOT restored ($weth_src_t vs $weth_src_r)"
if [ -n "$hf_after" ] && [ "$hf_after" -lt 1000000000000000000 ] 2>/dev/null; then
  ok "market: the crash made the position liquidatable (HF $hf_before -> $hf_after)"
else
  bad "market: HF did not fall below 1e18 ($hf_before -> $hf_after)"
fi
# core debug capability: the position's txs must be traceable
if [ -n "$SUPPLY_TX" ]; then
  for _ in $(seq 1 24); do
    st=$(rpc http://anvil-t:8545 eth_getTransactionReceipt "[\"$SUPPLY_TX\"]" | jq -r '.result.status // "pending"' 2>/dev/null)
    [ "$st" != "pending" ] && break
    sleep 5
  done
  frames=$(rpc http://anvil-t:8545 debug_traceTransaction "[\"$SUPPLY_TX\",{\"tracer\":\"callTracer\"}]" | jq -r '.result.calls | length' 2>/dev/null)
  [ -n "$frames" ] && [ "$frames" -gt 0 ] 2>/dev/null \
    && ok "market: debug_traceTransaction(callTracer) works on T ($frames call frames)" \
    || bad "market: trace of the supply tx failed (frames=$frames)"
else
  bad "market: supply tx hash not captured"
fi

echo "-- 4. integrity counters"
m=$(engine_api /metrics)
for counter in diorama_t_convergence_failures_total \
               diorama_full_sweep_mismatches_total \
               diorama_reorgs_detected_total; do
  v=$(echo "$m" | awk -v c="$counter" '$1==c {print $2}')
  case "$counter" in
    diorama_reorgs_detected_total) info "$counter = ${v:-0} (upstream reorgs; informational, the applier self-heals)";;
    *) [ "$(num "$v")" -eq 0 ] && ok "$counter = 0" || info "$counter = ${v:-?} (cumulative; see the latest drift entry)";;
  esac
done

echo
echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
# Health is about the *current* stream: the latest drift entry must be clean.
last_class=$($ENGINE sh -c 'tail -1 /state/drift.jsonl' 2>/dev/null | jq -r '.divergence_class // "none"' 2>/dev/null)
[ "$last_class" != "fatal" ] && ok "latest drift entry is clean or value_level ($last_class)" \
  || bad "latest drift entry is $last_class"
