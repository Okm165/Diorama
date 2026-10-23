#!/usr/bin/env bash
# Fitness function: no-panics (the services never panic on data).
#
# A mirror that panics on an upstream answer is a crash loop, not a degradation
# — every fault path must be a typed refusal or a named, bounded degradation.
# This gate scans the *production* code (test modules stripped, brace-matched)
# for the panic family and fails on any site that is not in the allow-list
# below, where each entry is a named invariant: the panic is unreachable by
# construction or it is a loud boot refusal, and the message says which.
#
# Run from the repo root: ./scripts/no-panics.sh
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import os, re, sys

# The allow-list: (file, normalized source snippet, why it is not a data-path
# panic). A new site — or an edit to one of these — fails the gate.
ALLOWED = [
    (
        "crates/cctp/src/transfer.rs",
        'u32::from_be_bytes(raw[at..at + 4].try_into().expect("length checked"))',
        "a 4-byte slice: the caller checked the length first",
    ),
    (
        "bins/cctp-bridge/src/main.rs",
        '.expect("both served chains are configured (checked at boot)")',
        "a boot-time invariant, checked at boot: a loud refusal, never a silent skip",
    ),
]

PATTERN = re.compile(r'\.unwrap\(\)|\.expect\(|panic!\(|unreachable!\(|todo!\(|unimplemented!\(')


def production(text: str) -> str:
    """The file without its `#[cfg(test)]` blocks (brace-matched)."""
    out = text
    while True:
        i = out.find("#[cfg(test)]")
        if i == -1:
            return out
        j = out.find("{", i)
        if j == -1:
            return out[:i]
        depth, k = 0, j
        while k < len(out):
            if out[k] == "{":
                depth += 1
            elif out[k] == "}":
                depth -= 1
                if depth == 0:
                    break
            k += 1
        out = out[:i] + out[k + 1:]


def normalize(s: str) -> str:
    return re.sub(r"\s+", " ", s).strip()


allowed = {(path, normalize(snippet)) for path, snippet, _ in ALLOWED}
allowed_paths = {path for path, _, _ in ALLOWED}

found, unexpected = 0, []
for root in ("crates", "bins"):
    for crate in sorted(os.listdir(root)):
        src = os.path.join(root, crate, "src")
        if not os.path.isdir(src):
            continue
        for dirpath, _, files in os.walk(src):
            for f in sorted(files):
                if not f.endswith(".rs"):
                    continue
                path = os.path.join(dirpath, f)
                for n, line in enumerate(production(open(path).read()).splitlines(), 1):
                    if PATTERN.search(line):
                        found += 1
                        if (path, normalize(line)) not in allowed:
                            unexpected.append(f"{path}:{n}: {line.strip()[:120]}")

for line in unexpected:
    print(f"UNEXPECTED PANIC SITE: {line}")
    print("  (a data path must refuse or degrade; if this is a named invariant, add it to the allow-list with its reason)")

used = sum(1 for path, _, _ in ALLOWED)
for path, _, why in ALLOWED:
    if not any(path == p for p in allowed_paths):
        print(f"STALE ALLOW-LIST ENTRY: {path} ({why})")

print(f"no-panics: {'OK' if not unexpected else 'FAIL'} ({found} production panic-family sites, {used} allowed by name)")
sys.exit(1 if unexpected else 0)
PY
