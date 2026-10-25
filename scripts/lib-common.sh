#!/usr/bin/env bash
# lib-common: the proof scripts' shared furniture — the pass/fail ledger, the
# summary line (with the failure exit the runners read), the JSON reader
# (`json`: one expression, a named failure on an unparsable body), and the
# plain JSON-RPC call. The cluster libs (lib-mainnet/lib-venue/lib-cctp) source this
# and override `rpc` with their in-pod variant; the standalone local proofs
# source it directly.
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

# The summary every proof prints, with its failure exit as the last command.
summary() { echo "$1: $pass ok, $fail bad (logs in $2)"; [ "$fail" = "0" ]; }

rpc() { # rpc <url> <method> [params-json]
  curl -s -X POST -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":${3:-[]}}" "$1"
}
