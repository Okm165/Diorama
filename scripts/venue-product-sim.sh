#!/usr/bin/env bash
# venue-product-sim: the daloc use-case arc against the venue stack — the
# counterpart of `mainnet-product-sim` (the entry point; the arc itself is
# scripts/daloc-sim.py, the plan §17 harness). Endpoints come from the
# environment exactly as daloc-sim.py documents:
#   T_RPC=http://127.0.0.1:18602 T_ADMIN=http://127.0.0.1:18680 \
#   V_RPC=http://127.0.0.1:18610 V_ADMIN=http://127.0.0.1:18681 \
#   SIM_INFO=http://127.0.0.1:18790 SIM_CTRL=http://127.0.0.1:18791 \
#     ./scripts/venue-product-sim.sh
# The venue script family's aliases are accepted too: ENGINE_URL is the venue
# engine's admin (V_ADMIN) and V_URL is the V fork (V_RPC) — so one
# environment drives the order proof, the capability proof and this entry.
# `make daloc-local` boots the stack locally; `make venue-product-sim` runs
# this against the deployed one.
set -euo pipefail
if [ -z "${V_ADMIN:-}" ] && [ -n "${ENGINE_URL:-}" ]; then export V_ADMIN="$ENGINE_URL"; fi
if [ -z "${V_RPC:-}" ] && [ -n "${V_URL:-}" ]; then export V_RPC="$V_URL"; fi
exec python3 "$(dirname "$0")/daloc-sim.py" "$@"
