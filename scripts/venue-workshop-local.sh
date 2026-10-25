#!/usr/bin/env bash
# venue-workshop-local: the venue order lane — the market-impact axiom (plan
# §4.5) — proven locally: a devnet order fills against the *real* Hyperliquid
# book and its consequences persist (the consumed depth does not resurrect,
# the trader's state moves, the mark is never moved by a fill). The
# counterpart of the mainnet workshop (scripts/mainnet-workshop-local.sh):
# there a strategy meets the real Aave/Uniswap state; here it meets the real
# book.
#
# The stack is daloc-local's (a scratch T + mainnet engine, a scratch V +
# venue engine, and the simulator polling the live Hyperliquid API). This
# script boots it with KEEP_UP=1 — which runs the daloc arc first, so the
# order lane lands on a warm, exercised stack — then runs venue-order-proof
# in direct mode against the same endpoints, and tears everything down.
#
# Requires: the same as daloc-local (anvil, both engine binaries built —
# `make mainnet-build venue-build` — python3, curl, outbound HTTPS/WSS to
# api.hyperliquid.xyz). Ports: daloc-local's defaults; override the *_PORT
# variables when they are taken. Never touches the cluster.
set -euo pipefail
cd "$(dirname "$0")/.."

DIR=/tmp/opencode/daloc-local
SRC_T_PORT="${SRC_T_PORT:-18600}"; T_PORT="${T_PORT:-18602}"; T_ADMIN_PORT="${T_ADMIN_PORT:-18680}"
SRC_V_PORT="${SRC_V_PORT:-18608}"; V_PORT="${V_PORT:-18610}"; V_ADMIN_PORT="${V_ADMIN_PORT:-18681}"
SIM_INFO_PORT="${SIM_INFO_PORT:-18790}"; SIM_CTRL_PORT="${SIM_CTRL_PORT:-18791}"

teardown() {
  [ -f "$DIR/pids" ] || return 0
  for pid in $(cat "$DIR/pids"); do kill "$pid" 2>/dev/null || true; done
  rm -f "$DIR/pids"
}
trap teardown EXIT

echo "== venue order lane: booting the daloc-local stack (live HL market data)"
KEEP_UP=1 SRC_T_PORT="$SRC_T_PORT" T_PORT="$T_PORT" T_ADMIN_PORT="$T_ADMIN_PORT" \
  SRC_V_PORT="$SRC_V_PORT" V_PORT="$V_PORT" V_ADMIN_PORT="$V_ADMIN_PORT" \
  SIM_INFO_PORT="$SIM_INFO_PORT" SIM_CTRL_PORT="$SIM_CTRL_PORT" \
  ./scripts/daloc-local.sh

echo "== venue order lane: the proof (fills vs the real book, §4.5 impact)"
ENGINE_URL="http://127.0.0.1:$V_ADMIN_PORT" V_URL="http://127.0.0.1:$V_PORT" \
  SIM_INFO="http://127.0.0.1:$SIM_INFO_PORT" SIM_CTRL="http://127.0.0.1:$SIM_CTRL_PORT" \
  ./scripts/venue-order-proof.sh
