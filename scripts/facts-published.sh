#!/usr/bin/env bash
# facts-published: every `Fact` variant the event vocabulary declares is
# published by real code. The enum is the contract the engines and the
# consumers share; a variant nobody constructs is an event that never
# happens — drift the `/events` consumers would otherwise discover the hard
# way (found: `DealApplied`, declared and never published). The bus's own
# tests are not publishers, so facts.rs is excluded.
set -euo pipefail
cd "$(dirname "$0")/.."

variants=$(python3 -c '
import re
s = open("crates/devnet/src/facts.rs").read()
body = re.search(r"pub enum Fact \{(.*?)\n\}", s, re.S).group(1)
for m in re.finditer(r"^\s{4}([A-Z][A-Za-z0-9_]*)", body, re.M):
    print(m.group(1))
')

fail=0
count=0
for v in $variants; do
  count=$((count + 1))
  if ! grep -rqE "publish\([A-Za-z_:$]*Fact::$v\b" crates bins --include="*.rs" --exclude=facts.rs; then
    echo "UNPUBLISHED (the Fact enum declares it, no code publishes it): $v"
    fail=1
  fi
done
echo "facts-published: $count variants checked"
exit $fail
