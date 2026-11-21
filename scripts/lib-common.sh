#!/usr/bin/env bash
# lib-common: the small shared furniture for the live substrate scripts
# (`smoke.sh`, `substrate-attest.sh`). A helper used by only one script
# belongs in that script, not here.
pass=0; fail=0
ok()   { echo "  ✔ $*"; pass=$((pass + 1)); }
bad()  { echo "  ✘ $*"; fail=$((fail + 1)); }
info() { echo "  · $*"; }

# Parse a JSON body and print one expression. An empty or unparsable body is a
# *named* failure with the body echoed to stderr (a curl that suppressed its
# body must not surface as a Python traceback: the caller's own check reports
# the miss).
json() {
  python3 -c "
import json, sys
raw = sys.stdin.read()
try:
    d = json.loads(raw)
except Exception:
    sys.stderr.write('json: unparsable body: ' + raw.strip()[:200] + '\n')
    print('None')
    raise SystemExit
print($1)
"
}
