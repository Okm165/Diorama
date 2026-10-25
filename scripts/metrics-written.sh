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
    # The hand-rolled homes: `pub <field>: AtomicU64` sets, no registry macro.
    # The simulator's is the largest (agent-found stale path: this used to
    # point at `sim/mod.rs`, which carries no such field — the gate silently
    # checked zero fields).
    pathlib.Path("crates/venue/src/sim/metrics.rs"): "manual",
    pathlib.Path("crates/cctp/src/metrics.rs"): "manual",
    pathlib.Path("crates/cctp/src/watcher.rs"): "manual",
    pathlib.Path("crates/devnet/src/plane.rs"): "manual",
}
# (The push-style homes — `feed/metrics.rs`, `bridge/metrics.rs`,
# `feed/metrics.rs` — declare private counters and push their literal
# series from their own methods; `docs-metrics` checks those names both ways
# and the AST gate sees the methods' call sites.)
dead, total = [], 0
counts = []
for path, kind in homes.items():
    s = path.read_text()
    if kind == "registry":
        body = re.search(r"registry! \{(.*?)\n\}", s, re.S)
        fields = [m.group(1) for m in re.finditer(
            r"^\s*(?:gauge|counter) ([a-z0-9_]+) = ", body.group(1), re.M)]
    else:
        fields = [m.group(1) for m in re.finditer(
            r"^\s*pub ([a-z0-9_]+): (?:std::sync::atomic::)?AtomicU64", s, re.M)]
    counts.append((path, len(fields)))
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
for path, n in counts:
    print(f"  {path}: {n} fields")
for path, f, n in dead:
    print(f"  UNWRITTEN (declared, only ever rendered): {path}: {f} (uses={n})")
sys.exit(1 if dead else 0)
PY
