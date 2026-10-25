#!/usr/bin/env bash
# venue-product-sim: the daloc use-case arc against the venue stack — the
# counterpart of `mainnet-product-sim` (the entry point; the arc itself is
# scripts/daloc-sim.py, the plan §17 harness). Endpoints come from the
# environment exactly as daloc-sim.py documents:
#   ENGINE_URL=http://127.0.0.1:18681 SIM_INFO=http://127.0.0.1:18790 \
#     SIM_CTRL=http://127.0.0.1:18791 ./scripts/venue-product-sim.sh
# `make daloc-local` boots that stack locally; `make venue-product-sim` runs
# this against the deployed one.
set -euo pipefail
exec python3 "$(dirname "$0")/daloc-sim.py" "$@"
