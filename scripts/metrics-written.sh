#!/usr/bin/env bash
# metrics-written: every series the metric registries declare is *written* by
# real code. docs-metrics checks names both ways and facts-published checks
# the event enum; this is the same class on the metric side — a declared
# series nobody produces renders a constant zero, which reads as "nothing
# happened" instead of "nobody measures it" (the DealApplied lesson).
#
# A series is written when some use of the field is not a `.load(` (the
# render reads are loads; writes go through `.fetch_add`/`.store`/`.set(&…)`
# and friends). Accesses span lines, so the check normalizes whitespace.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import re, pathlib, sys

roots = [pathlib.Path("crates"), pathlib.Path("bins")]
corpus = []
for root in roots:
    for p in sorted(root.rglob("*.rs")):
        corpus.append((p, p.read_text()))
flat = [(p, re.sub(r"\s+", "", s)) for p, s in corpus]

homes = {
    pathlib.Path("crates/devnet/src/metrics.rs"): "registry",
    pathlib.Path("crates/mainnet/src/metrics.rs"): "registry",
    pathlib.Path("crates/venue/src/metrics.rs"): "registry",
    pathlib.Path("crates/venue/src/sim/mod.rs"): "manual",
}
dead, total = [], 0
for path, kind in homes.items():
    s = path.read_text()
    if kind == "registry":
        body = re.search(r"registry! \{(.*?)\n\}", s, re.S)
        fields = [m.group(1) for m in re.finditer(
            r"^\s*(?:gauge|counter) ([a-z0-9_]+) = ", body.group(1), re.M)]
    else:
        fields = [m.group(1) for m in re.finditer(
            r"^\s*pub ([a-z0-9_]+): AtomicU64", s, re.M)]
    for f in fields:
        total += 1
        pattern = re.compile(rf"\.{re.escape(f)}(?![a-z0-9_])")
        uses = 0
        loads = 0
        for _, text in flat:
            for m in pattern.finditer(text):
                uses += 1
                tail = text[m.end():m.end() + 6]
                if tail.startswith(".load("):
                    loads += 1
        if uses == loads:
            dead.append((path, f, uses))
print(f"metrics-written: {total} series checked")
for path, f, n in dead:
    print(f"  UNWRITTEN (declared, only ever rendered): {path}: {f} (uses={n})")
sys.exit(1 if dead else 0)
PY
