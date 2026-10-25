#!/usr/bin/env bash
# shell-mode: the scripts' two family conventions, both measured.
#
# 1. Every script that runs standalone opens with `set -euo pipefail`: a
#    failed read aborts loudly instead of counting as a check. Two judged
#    exceptions: the four oldest deployed-stack suites run leniently so every
#    check counts (STRUCTURE's row), and the sourced libs never set the mode
#    (the caller owns the shell).
# 2. Every `source` line resolves through `${BASH_SOURCE[0]}`: a lib sourced
#    via `$0` breaks whenever the caller is not path-invoked — measured:
#    lib-cctp's `./lib-venue.sh` (worked only under the exact
#    `./scripts/x.sh` invocation).
set -euo pipefail
cd "$(dirname "$0")/.."

bad=0
libs="lib-common.sh lib-mainnet.sh lib-venue.sh lib-cctp.sh"
lenient="mainnet-proof.sh mainnet-scenario.sh mainnet-product-sim.sh mainnet-closure-check.sh"

for f in scripts/*.sh; do
  name=$(basename "$f")
  case " $libs " in *" $name "*) continue ;; esac
  if ! grep -q "^set -euo pipefail" "$f"; then
    case " $lenient " in
      *" $name "*) ;; # judged: the lenient four (STRUCTURE's row)
      *)
        echo "VIOLATION: $name runs without set -euo pipefail"
        bad=1
        ;;
    esac
  fi
done

hits=$(grep -rn 'source.*\$0' scripts/ --exclude=shell-mode.sh 2>/dev/null | grep -v BASH_SOURCE || true)
if [ -n "$hits" ]; then
  echo "$hits" | while IFS= read -r line; do echo "VIOLATION: a source line uses \$0: $line"; done
  bad=1
fi

if [ "$bad" -eq 0 ]; then
  echo "shell-mode: OK (every standalone script strict, every source line BASH_SOURCE-resolved)"
fi
exit $bad
