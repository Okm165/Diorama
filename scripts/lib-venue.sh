#!/usr/bin/env bash
# Shared helpers for the venue-family scripts that run in both modes — the
# epoch scenario, the capability proof, the order-lane proof — and for the cctp
# family (through `lib-cctp.sh`, which sources this one for the venue-shaped
# gates). Source it, never run it:
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
# What lives here: the counters (`ok`/`bad`/`info`), the JSON reader, the
# engine + simulator doors, and the venue-shaped client gates every family
# shares — `sim_account`, `sim_spot`, `identity_ok` (the served-identity
# check daloc gates on) and `flatten_coin` (a proof's rerunnable start).
#
# Conventions (measured, plan §7.6): V reads/writes go through eth_call /
# eth_sendTransaction; the engine's admin reads are /status + /admin/*; the
# simulator's info plane (8090) serves daloc's shapes and its control plane
# (8091) the /control/* verbs.
NS="${NS:-venue-mirror}"
CAST="${CAST:-cast}"

source "$(dirname "${BASH_SOURCE[0]}")/lib-common.sh"
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

# The venue-shaped client gates (transport-agnostic — they read stdin or the
# sim doors above, so every venue-family script shares one definition):
# The served USDC spot balance (the unified-margin reads).
sim_spot() { # sim_spot <address>
  sipost /info "{\"type\":\"spotClearinghouseState\",\"user\":\"$1\"}" \
    | python3 -c "
import json, sys
d = json.load(sys.stdin)
for b in d.get('balances', []):
    if b.get('coin') == 'USDC':
        print(b.get('total', '0'))
        break
else:
    print('0')"
}

sim_account() { sipost /info "{\"type\":\"clearinghouseState\",\"user\":\"$1\"}"; }

# The identity daloc gates on: accountValue == totalRawUsd + Σ signed
# positionValue, exact within the served positionValue's 2dp rounding.
# Prints `ok` or the mismatch.
identity_ok() {
  python3 -c "
import json, sys
from decimal import Decimal
d = json.load(sys.stdin)
v = Decimal(d['marginSummary']['accountValue'])
raw = Decimal(d['marginSummary']['totalRawUsd'])
pnl = sum(Decimal(p['position']['positionValue']) * (-1 if p['position']['szi'].startswith('-') else 1)
          for p in d['assetPositions'])
print('ok' if abs(v - (raw + pnl)) < Decimal('0.01') else f'mismatch v={v} raw={raw} pnl={pnl}')"
}

# Flatten an account's coin position against the live book (a previous run's
# leftovers), so a proof's legs are absolute and rerunnable: a reduce-only
# order for the whole size, up to five attempts (the live book moves).
flatten_coin() { # flatten_coin <account> <coin>
  local account=$1 coin=$2 szi is_buy abs_szi
  szi=$(sim_account "$account" | json 'd["assetPositions"][0]["position"]["szi"] if d.get("assetPositions") else "0"')
  [ "$szi" = "0" ] && return 0
  is_buy=false
  case "$szi" in -*) is_buy=true ;; esac
  abs_szi=$(python3 -c "from decimal import Decimal; print(abs(Decimal('$szi')))")
  for _ in 1 2 3 4 5; do
    spost /control/order "{\"address\":\"$account\",\"coin\":\"$coin\",\"is_buy\":$is_buy,\"size\":\"$abs_szi\",\"reduce_only\":true}" > /dev/null || true
    szi=$(sim_account "$account" | json 'd["assetPositions"][0]["position"]["szi"] if d.get("assetPositions") else "0"')
    [ "$szi" = "0" ] && return 0
    abs_szi=$(python3 -c "from decimal import Decimal; print(abs(Decimal('$szi')))")
  done
  return 0
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
