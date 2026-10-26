#!/usr/bin/env bash
# Watchlist closure audit: for every market-data view the product reads, trace
# its read path on reth and report each account whose *storage* is read but
# which is not in the watchlist. An unwatched storage dependency is a staleness
# waiting to happen - the view keeps working until that account changes on
# mainnet, then freezes while every probe stays green.
#
# Measured finds: Aave's WBTC source is composite (two proxy/aggregator pairs
# missing), the USDC source is a CAPO wrapper with ratio feeds missing, and the
# wstETH source reads stETH's share rate (updated daily) which was missing.
#
# Code-only accounts are not dependencies: a delegatecall target (the pool's
# implementation) appears in the trace but code is immutable, so only accounts
# with storage reads are flagged.
#
# Usage: NS=diorama-mainnet ./scripts/mainnet-closure-check.sh
set -uo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib-mainnet.sh"

VIEWS="
aave_price_weth   0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f07000000000000000000000000C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
aave_price_usdc   0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f07000000000000000000000000A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
aave_price_usdt   0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f07000000000000000000000000dAC17F958D2ee523a2206206994597C13D831ec7
aave_price_wbtc   0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f070000000000000000000000002260FAC5E5542a773Aa44fBCfeDf7C193bc2C599
aave_price_wsteth 0x54586bE62E3c3580375aE3723C145253060Ca0C2 0xb3596f070000000000000000000000007f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0
chainlink_weth_usd 0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419 0xfeaf968c
chainlink_wbtc_usd 0xF4030086522a5bEEa4988F8cA5B36dbC97BeE88c 0xfeaf968c
chainlink_steth_usd 0xCfE54B5cD566aB89272946F602D76Ea879CAb4a8 0xfeaf968c
chainlink_usdc_usd 0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6 0xfeaf968c
chainlink_usdt_usd 0x3E7d1eAB13ad0104d2750B8863b489D65364e32D 0xfeaf968c
univ3_slot0       0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640 0x3850c7bd
aave_income_usdc  0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2 0xd15e0053000000000000000000000000A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
aave_income_weth  0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2 0xd15e0053000000000000000000000000C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
"
# The watchlist's single home is the instance file the engine loads (measured:
# the old k8s/base/engine-config.yaml path went stale with the config move and
# made every view look unwatched). Only the [[watchlist]] blocks count: the
# file's other 0x addresses (fixtures, contracts) are not watched accounts.
INSTANCE_TOML="${INSTANCE_TOML:-config/instances/diorama-mainnet/instance.toml}"
WATCH=$(awk '/^\[\[watchlist\]\]/{f=1;next} /^\[/{f=0} f' "$INSTANCE_TOML" \
  | grep -oE '0x[0-9a-fA-F]{40}' | tr 'A-F' 'a-f' | sort -u)

echo "== watchlist closure audit ($(echo "$WATCH" | wc -l) watched accounts, $(echo "$VIEWS" | grep -c .) views) =="
gaps=0
while read -r name to data; do
  [ -n "${name:-}" ] || continue
  READS=$(rpc http://reth:8545 debug_traceCall \
    "[{\"to\":\"$to\",\"data\":\"$data\"},\"latest\",{\"tracer\":\"prestateTracer\"}]" \
    | jq -r '.result | to_entries[] | select((.value.storage // {}) | length > 0) | .key' 2>/dev/null)
  missing=""
  for a in $READS; do
    echo "$WATCH" | grep -qx "$a" || missing="$missing $a"
  done
  if [ -n "$missing" ]; then
    bad "$name reads unwatched storage:"
    for m in $missing; do echo "      $m"; done
    gaps=$((gaps + 1))
  else
    ok "$name: every storage dependency is watched"
  fi
done <<EOF
$VIEWS
EOF

echo
[ "$gaps" -eq 0 ] \
  && ok "closure: all views' storage dependencies are watched" \
  || bad "closure: $gaps views read unwatched storage - add the addresses to $INSTANCE_TOML's watchlist"
[ "$fail" -eq 0 ]
