#!/usr/bin/env bash
# spine-purity: the spine's *code* never names a mirror. `crates/devnet` is
# the shared pipeline; its doc comments cite the mirrors as examples (the
# seam list does), but identifiers, strings and types must not — otherwise a
# new mirror kind would need edits inside the spine, which is exactly what
# the seam design exists to prevent. Line comments are stripped; the
# remainder is the check.
set -euo pipefail
cd "$(dirname "$0")/.."

hits=$(python3 -c '
import re, pathlib
strip = re.compile(r"//[^\n]*")
leak = re.compile(r"\b(mainnet|venue)", re.I)
for p in sorted(pathlib.Path("crates/devnet/src").rglob("*.rs")):
    for i, line in enumerate(p.read_text().splitlines(), 1):
        code = strip.sub("", line)
        if leak.search(code):
            print(f"{p}:{i}: {code.strip()}")
')
if [ -n "$hits" ]; then
  echo "SPINE-PURITY: the spine's code names a mirror:"
  echo "$hits"
  exit 1
fi
echo "spine-purity: crates/devnet's code names no mirror"
