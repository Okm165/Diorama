#!/usr/bin/env bash
# trust-audit: every trusted axis names its guard (plan §11.3, doctrine rule 6).
# A `// TRUST:` marker without a guard in the next lines is a finding: a claim
# entered at a consumption point that nothing checks.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
found=0
while IFS=: read -r file line _; do
  found=$((found + 1))
  window=$(sed -n "${line},$((line + 6))p" "$file")
  if ! grep -qi "guard" <<<"$window"; then
    echo "UNGUARDED: $file:$line"
    fail=1
  fi
done < <(grep -rn "// TRUST:" crates/ bins/)
if [ "$fail" -ne 0 ]; then
  echo "trust-audit: FAIL"
  exit 1
fi
echo "trust-audit: OK ($found TRUST markers, every one names its guard)"
