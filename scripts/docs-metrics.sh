#!/usr/bin/env bash
# docs-metrics: the docs<->metrics fitness function (plan §11.3), both ways:
# every metric-like token the docs name (`mirror_*` / `venue_*` /
# `feed_bridge_*` / `cctp_*`) must exist in the code, and every quoted series
# the code defines must be named in the docs. A renamed metric surfaces as a
# build failure instead of drifting silently, and a new metric cannot ship
# undocumented. The prefixes are the metric namespace: any quoted string using
# them is part of the surface.
#
# A token ending in `_` is a prefix, not a name; crate and service-section
# names are not metrics.
set -euo pipefail
cd "$(dirname "$0")/.."

crate_names="mirror_devnet mirror_anvil mirror_config mirror_ethereum mirror_hyperliquid mirror_cctp venue_sim cctp_bridge"
tokens=$(grep -rhoE '(mirror|venue|feed_bridge|cctp)_[a-z0-9_]+' docs/*.md | sort -u)
fail=0
checked=0
code_checked=0
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
# The inverse: every series the code defines is named in the docs.
code_tokens=$(grep -rhoE '"(mirror|venue|feed_bridge|cctp)_[a-z0-9_]+' crates/ bins/ | tr -d '"' | sort -u)
for tok in $code_tokens; do
  case "$tok" in
    *_) continue ;;
  esac
  if echo " $crate_names " | grep -q " $tok "; then continue; fi
  code_checked=$((code_checked + 1))
  if ! grep -rq -- "$tok" docs/*.md; then
    echo "MISSING (the code emits it, the docs do not name it): $tok"
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "docs-metrics: FAIL"
  exit 1
fi
echo "docs-metrics: OK ($checked doc-named metrics resolve in the code; $code_checked code series are documented)"
