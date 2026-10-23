#!/usr/bin/env bash
# Fitness function: clock-labels (plan §11.3, doctrine `block-numbering`).
#
# Two clocks are legitimate — every block-like label a service *logs* must say
# which clock it is, from one place per crate. The ledger is the crate's
# `//! Clocks:` block in lib.rs; this gate extracts every top-level clock label
# used as a tracing field and requires the ledger to name it. A new label
# without a clock statement fails here instead of drifting into the logs.
#
# Run from the repo root: ./scripts/clock-labels.sh
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import os, re, sys

clock_words = ("block", "cursor", "head", "height", "pin", "epoch", "age")
label_re = re.compile(r'\b([a-z_][a-z0-9_]*)\s*=')

def labels_in(text):
    found = {}
    for m in re.finditer(r'tracing::(?:info|warn|error|debug|trace)!\(', text):
        i, j, depth = m.end(), m.end(), 1
        while j < len(text) and depth > 0:
            c = text[j]
            if c in "([{": depth += 1
            elif c in ")]}": depth -= 1
            j += 1
        body, depth, k = text[i:j - 1], 0, 0
        while k < len(body):
            c = body[k]
            if c in "([{": depth += 1
            elif c in ")]}": depth -= 1
            elif depth == 0:
                lm = label_re.match(body, k)
                if lm:
                    name = lm.group(1)
                    if any(w in name for w in clock_words):
                        found[name] = found.get(name, 0) + 1
                    k = lm.end() - 1
            k += 1
    return found

def ledger(root):
    """The crate's `//! Clocks:` block (contiguous module-doc lines)."""
    for name in ("lib.rs", "main.rs"):
        path = os.path.join(root, name)
        if not os.path.exists(path):
            continue
        lines = open(path).read().splitlines()
        for i, line in enumerate(lines):
            if line.startswith("//! Clocks:"):
                block = [line]
                for nxt in lines[i + 1:]:
                    if nxt.startswith("//!"):
                        block.append(nxt)
                    else:
                        break
                return " ".join(block)
    return None

fail = 0
checked = 0
for parent in ("crates", "bins"):
    for crate in sorted(os.listdir(parent)):
        src = os.path.join(parent, crate, "src")
        if not os.path.isdir(src):
            continue
        labels = {}
        for dirpath, _, files in os.walk(src):
            for f in files:
                if f.endswith(".rs"):
                    for name, count in labels_in(open(os.path.join(dirpath, f)).read()).items():
                        labels[name] = labels.get(name, 0) + count
        if not labels:
            continue
        checked += 1
        text = ledger(src)
        if text is None:
            print(f"MISSING LEDGER: {crate} logs clock labels {sorted(labels)} but has no `//! Clocks:` block in lib.rs")
            fail = 1
            continue
        words = set(re.findall(r'[A-Za-z_][A-Za-z0-9_]*', text))
        missing = sorted(name for name in labels if name not in words)
        if missing:
            print(f"MISSING LABEL: {crate} logs {missing} without naming the clock in its `//! Clocks:` ledger")
            fail = 1

print(f"clock-labels: {'OK' if fail == 0 else 'FAIL'} ({checked} crates log clock labels)")
sys.exit(fail)
PY
