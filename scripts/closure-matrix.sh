#!/usr/bin/env bash
# Fitness function: closure-matrix (plan §11.3).
#
# Every internal dependency edge must be one the plan's layering table
# declares; anything else is an accidental edge (a project cross-dep, a
# bin reaching sideways, a cycle). The table is the open/closed declaration:
#   config     open, no internal deps
#   anvil      open, -> config
#   devnet     closed between anvil and the projects, -> anvil, config
#   projects   siblings (no cross edges), -> devnet, anvil, config
#   cctp       cross-cutting element: bypasses devnet by design, -> anvil, config
#   bins/*     composition roots, above all
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

allowed() {
  case "$1" in
    mirror-config)      echo "" ;;
    mirror-anvil)       echo "mirror-config" ;;
    mirror-devnet)      echo "mirror-anvil mirror-config" ;;
    mirror-mainnet)     echo "mirror-devnet mirror-anvil mirror-config" ;;
    mirror-venue)       echo "mirror-devnet mirror-anvil mirror-config" ;;
    mirror-cctp)        echo "mirror-anvil mirror-config" ;;
    mainnet-engine)     echo "mirror-mainnet mirror-devnet mirror-anvil mirror-config" ;;
    venue-engine)       echo "mirror-venue mirror-devnet mirror-anvil mirror-config" ;;
    venue-sim)          echo "mirror-venue mirror-devnet mirror-anvil mirror-config" ;;
    cctp-bridge)        echo "mirror-cctp mirror-anvil mirror-config" ;;
    ast-scope)          echo "" ;;
    *)                  echo "UNKNOWN-PACKAGE" ;;
  esac
}

metadata=$(cargo metadata --no-deps --format-version 1)
bad=0
count=0
for pkg in $(echo "$metadata" | jq -r '.packages[].name' | sort); do
  count=$((count + 1))
  ok=$(allowed "$pkg")
  if [ "$ok" = "UNKNOWN-PACKAGE" ]; then
    echo "VIOLATION: package $pkg is not in the closure matrix (add it to the table and this script)"
    bad=1
    continue
  fi
  deps=$(echo "$metadata" | jq -r --arg p "$pkg" \
    '.packages[] | select(.name==$p) | .dependencies[] | select(.name|startswith("mirror-")) | .name' | sort -u)
  for d in $deps; do
    case " $ok " in
      *" $d "*) ;;
      *) echo "VIOLATION: $pkg -> $d (edge not declared in the closure matrix)"; bad=1 ;;
    esac
  done
done

if [ "$bad" -eq 0 ]; then
  echo "closure-matrix: OK ($count packages, every internal edge declared)"
fi
exit $bad
