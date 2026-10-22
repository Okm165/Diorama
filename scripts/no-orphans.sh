#!/usr/bin/env bash
# Fitness function: no-orphans (plan §11.3, Lesson 101).
#
# A crate or module named utils / common / helpers / misc is the drift this
# check catches: shared code with no owner and no layer. Shared code belongs
# to the crate whose axioms it implements.
set -euo pipefail
cd "$(dirname "$0")/.."

bad=0

# Crate directories.
for d in crates/*/; do
  name=$(basename "$d")
  case "$name" in
    utils|common|helpers|misc|shared-utils) echo "VIOLATION: crate named '$name' ($d)"; bad=1 ;;
  esac
done

# Module declarations (anywhere, including tests).
hits=$(grep -rIn --include="*.rs" -E '(^|[[:space:]])(pub[[:space:]]+)?mod[[:space:]]+(utils|common|helpers|misc)([[:space:]]|;|\{)' crates bins 2>/dev/null || true)
if [ -n "$hits" ]; then
  echo "$hits" | while IFS= read -r line; do echo "VIOLATION: $line"; done
  bad=1
fi

if [ "$bad" -eq 0 ]; then
  echo "no-orphans: OK (no utils/common/helpers crate or module)"
fi
exit $bad
