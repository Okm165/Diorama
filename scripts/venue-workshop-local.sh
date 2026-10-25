#!/usr/bin/env bash
# venue-workshop-local: a financial workshop on live venue state, natively —
# the counterpart of `mainnet-workshop-local` (there a strategy meets the real
# Aave/Uniswap state; here it meets the real Hyperliquid book). The same acts,
# the venue's mechanisms:
#
#   act 1. the market on live data  - the daloc arc: the venue reads and the
#                                     client gates, the served identity, the
#                                     crash/drawdown arc, the venue-side
#                                     liquidation, and the epoch on both
#                                     mirrors
#   act 2. minted liquidity + fill  - a devnet deal mints the trader's
#                                     collateral, then an order fills against
#                                     the *real* book (the §4.5 lane)
#   act 3. the impact measured      - the consumed depth does not resurrect,
#                                     the trader's state moves, and the owned
#                                     mark is never moved by a fill
#
# The stack is daloc-local's (a scratch T + mainnet engine, a scratch V + venue
# engine, and the simulator polling the live Hyperliquid API). The arc runs
# first: the order lane then lands on a warm, exercised stack — the venue's
# mechanics, the mainnet workshop's acts.
#
# Requires: the same as daloc-local (anvil, both engine binaries built —
# `make mainnet-build venue-build` — python3, curl, outbound HTTPS/WSS to
# api.hyperliquid.xyz). Ports: daloc-local's defaults; override the *_PORT
# variables when they are taken. Never touches the cluster.
set -euo pipefail
cd "$(dirname "$0")/.."

DIR=/tmp/opencode/daloc-local
# Our own capture dir: daloc-local resets its `$DIR` at boot (a `tee` into it
# would write to an unlinked inode), and the summaries below are cosmetic —
# the suites' exit codes are the verdict (`set -euo pipefail`).
LOGS=/tmp/opencode/venue-workshop
mkdir -p "$LOGS"
SRC_T_PORT="${SRC_T_PORT:-18600}"; T_PORT="${T_PORT:-18602}"; T_ADMIN_PORT="${T_ADMIN_PORT:-18680}"
SRC_V_PORT="${SRC_V_PORT:-18608}"; V_PORT="${V_PORT:-18610}"; V_ADMIN_PORT="${V_ADMIN_PORT:-18681}"
SIM_INFO_PORT="${SIM_INFO_PORT:-18790}"; SIM_CTRL_PORT="${SIM_CTRL_PORT:-18791}"

teardown() {
  [ -f "$DIR/pids" ] || return 0
  for pid in $(cat "$DIR/pids"); do kill "$pid" 2>/dev/null || true; done
  rm -f "$DIR/pids"
}
trap teardown EXIT

echo "== venue workshop (stack: daloc-local; live HL market data)"
echo "-- act 1. the market on live data (the arc: reads, identity, crash, liquidation)"
KEEP_UP=1 SRC_T_PORT="$SRC_T_PORT" T_PORT="$T_PORT" T_ADMIN_PORT="$T_ADMIN_PORT" \
  SRC_V_PORT="$SRC_V_PORT" V_PORT="$V_PORT" V_ADMIN_PORT="$V_ADMIN_PORT" \
  SIM_INFO_PORT="$SIM_INFO_PORT" SIM_CTRL_PORT="$SIM_CTRL_PORT" \
  ./scripts/daloc-local.sh 2>&1 | tee "$LOGS/arc.out"
arc=$(grep -oE '[0-9]+/[0-9]+ checks passed' "$LOGS/arc.out" | tail -1 || true)

echo "-- act 2+3. minted liquidity, a fill vs the real book, the impact measured"
ENGINE_URL="http://127.0.0.1:$V_ADMIN_PORT" V_URL="http://127.0.0.1:$V_PORT" \
  SIM_INFO="http://127.0.0.1:$SIM_INFO_PORT" SIM_CTRL="http://127.0.0.1:$SIM_CTRL_PORT" \
  ./scripts/venue-order-proof.sh 2>&1 | tee "$LOGS/order.out"
order=$(grep -oE '[0-9]+ passed, [0-9]+ failed' "$LOGS/order.out" | tail -1 || true)

echo "venue-workshop-local: the arc (${arc:-checks}) and the order lane (${order:-checks}) passed (captures in $LOGS, stack logs in $DIR)"
