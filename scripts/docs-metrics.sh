#!/usr/bin/env bash
# docs-metrics: the docs<->metrics fitness function (plan §11.3): every
# metric-like token the docs name (`mirror_*` / `venue_*` / `feed_bridge_*` /
# `cctp_*`) must exist in the code, so a renamed metric surfaces as a build
# failure instead of drifting silently in the docs.
#
# A token ending in `_` is a prefix, not a name; crate names are not metrics.
set -euo pipefail
cd "$(dirname "$0")/.."

crate_names="mirror_devnet mirror_anvil mirror_config mirror_ethereum mirror_hyperliquid mirror_cctp"
tokens=$(grep -rhoE '(mirror|venue|feed_bridge|cctp)_[a-z0-9_]+' docs/*.md | sort -u)
fail=0
checked=0
for tok in $tokens; do
  case "$tok" in
    *_) continue ;; # a prefix (`venue_bridge_`), not a name
  esac
  if echo " $crate_names " | grep -q " $tok "; then continue; fi
  checked=$((checked + 1))
  if ! grep -rq -- "$tok" crates/ bins/; then
    echo "MISSING (docs name it, the code does not): $tok"
    fail=1
  fi
done
if [ "$fail" -ne 0 ]; then
  echo "docs-metrics: FAIL"
  exit 1
fi
echo "docs-metrics: OK ($checked metric names resolve in the code)"
