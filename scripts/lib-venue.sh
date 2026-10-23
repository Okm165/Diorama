#!/usr/bin/env bash
# Shared helpers for the venue-mirror scripts that run in both modes — the
# epoch drill and the capability proof. Source it, never run it:
#   source "$(dirname "$0")/lib-venue.sh"
#
# One transport per mode, chosen once (the same convention in every venue
# script that supports it):
#   direct  (ENGINE_URL set): the scratch stack's URLs — ENGINE_URL, V_URL,
#           SIM_INFO, SIM_CTRL — no kubectl;
#   cluster (default): the venue-mirror namespace, every call exec'd from the
#           venue engine pod (cluster-internal service names — the same
#           transport the engine itself uses).
#
# Conventions (measured, plan §7.6): V reads/writes go through eth_call /
# eth_sendTransaction; the engine's admin reads are /status + /admin/*; the
# simulator's info plane (8090) serves daloc's shapes and its control plane
# (8091) the /control/* verbs.
NS="${NS:-venue-mirror}"
CAST="${CAST:-cast}"

pass=0; fail=0
ok()   { echo "  ✔ $*"; pass=$((pass+1)); }
bad()  { echo "  ✘ $*"; fail=$((fail+1)); }
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

if [ -n "${ENGINE_URL:-}" ]; then
  eget()   { curl -s --fail-with-body -m 60 "$ENGINE_URL$1"; }
  epost()  { curl -s --fail-with-body -m 60 -X POST "$ENGINE_URL$1" -H 'Content-Type: application/json' -d "$2"; }
  rpcv()   { curl -s --fail-with-body -m 30 -X POST "$V_URL" -H 'Content-Type: application/json' -d "$1"; }
  sget()   { curl -s --fail-with-body -m 30 "$SIM_INFO$1"; }
  sipost() { curl -s --fail-with-body -m 30 -X POST "$SIM_INFO$1" -H 'Content-Type: application/json' -d "$2"; }
  spost()  { curl -s --fail-with-body -m 30 -X POST "$SIM_CTRL$1" -H 'Content-Type: application/json' -d "$2"; }
  smetrics() { curl -s --fail-with-body -m 30 "$SIM_CTRL/metrics"; }
  balance() { # balance <token> <address> -> decimal units
    "$CAST" call "$1" 'balanceOf(address)(uint256)' "$2" --rpc-url "$V_URL" 2>/dev/null | awk '{print $1}'
  }
else
  exec_pod() { kubectl -n "$NS" exec -i deploy/engine -- "$@"; }
  eget()   { exec_pod curl -sf -m 60 "localhost:8080$1"; }
  epost()  { exec_pod curl -sf -m 60 -X POST "localhost:8080$1" -H 'Content-Type: application/json' -d "$2"; }
  rpcv()   { exec_pod curl -sf -m 30 -X POST http://anvil-v:8545 -H 'Content-Type: application/json' -d "$1"; }
  sget()   { exec_pod curl -sf -m 30 "http://venue-sim:8090$1"; }
  sipost() { exec_pod curl -sf -m 30 -X POST "http://venue-sim:8090$1" -H 'Content-Type: application/json' -d "$2"; }
  spost()  { exec_pod curl -sf -m 30 -X POST "http://venue-sim:8091$1" -H 'Content-Type: application/json' -d "$2"; }
  smetrics() { exec_pod curl -sf -m 30 "http://venue-sim:8091/metrics"; }
  balance() { # balance <token> <address> -> decimal units
    exec_pod curl -sf -m 30 -X POST http://anvil-v:8545 -H 'Content-Type: application/json' \
      -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$1\",\"data\":\"0x70a08231000000000000000000000000${2:2}\"},\"latest\"]}" \
      | python3 -c "import json,sys; r=json.load(sys.stdin).get('result','0x0'); print(int(r,16))"
  }
fi
