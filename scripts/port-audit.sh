#!/usr/bin/env bash
# port-audit: the proof scripts' default ports must be unique across the tree.
# Two scripts defaulting to the same port collide when run in parallel —
# measured: engine-symmetry-local's T admin (19980) vs the workshop's HTTP
# (19980); the workshop's engine lost the bind ("http server exited ...
# binding the http api") and its checks ran against the foreign engine. Every
# script can still override its ports through the env; the defaults must not
# fight.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import pathlib
import re
import sys

# A port-valued default: the name says PORT, or it is one of the admin/HTTP
# spellings (`T_ADMIN`, `HTTP`). Amounts and budgets (`MINT`, `BUDGET`) share
# the assignment shape but are not ports.
def is_port_name(name):
    return "PORT" in name or name.endswith("_ADMIN") or name == "HTTP"


seen = {}
shared = {}
bad = 0
pat = re.compile(r'([A-Z_]+)="\$\{([A-Z_]+):-(\d+)\}"')
for path in sorted(pathlib.Path("scripts").glob("*.sh")):
    text = path.read_text()
    # `# ports-shared-with: <script>` declares an intentional sharing (the
    # workshop runs on the daloc stack's ports, say).
    shared[path.name] = set(re.findall(r"ports-shared-with:\s*([\w.-]+)", text))
    for name, _, port in pat.findall(text):
        if is_port_name(name):
            seen.setdefault(port, []).append((path.name, name))
# The convention's other half: a port-valued assignment must be the
# overridable form. A fixed one is invisible to the duplicate scan (and to
# the operator). Comments are stripped (the instrument's own notes name the
# measured case); the value decides per assignment.
fixed_pat = re.compile(r'([A-Z_]+)=(\$\{[A-Z_]+:-[0-9]+\}|[0-9]{2,5})(?![0-9])')
for path in sorted(pathlib.Path("scripts").glob("*.sh")):
    for line in path.read_text().splitlines():
        code = line.split("#", 1)[0]
        for name, value in fixed_pat.findall(code):
            if is_port_name(name) and not value.startswith("$"):
                print(f"VIOLATION: {path.name}: {name} is a fixed port (use ${{NAME:-PORT}} so it can be overridden)")
                bad = 1

for port, sites in sorted(seen.items()):
    if len(sites) <= 1:
        continue
    names = [s[0] for s in sites]
    declared = all(
        other in shared.get(a, set())
        for a in names
        for other in names
        if other != a
    )
    if declared:
        continue
    print(
        f"VIOLATION: port {port} is a default in {len(sites)} scripts: "
        + ", ".join(f"{a}:{n}" for a, n in sites)
    )
    bad = 1
if bad == 0:
    print(f"port-audit: OK ({len(seen)} distinct default ports across the scripts)")
sys.exit(bad)
PY
