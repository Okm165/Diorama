#!/usr/bin/env bash
# Shared helpers for the venue-family scripts that run in both modes — the
# order-lane proof — and for any future venue-shaped gate. Source it, never
# run it:
#   source "$(dirname "${BASH_SOURCE[0]}")/lib-venue.sh"
#
# One transport per mode, chosen once (the same convention in every venue
# script that supports it):
#   direct  (SIM_INFO set): a locally booted simulator — SIM_INFO (the info
#           API, 8090-shaped), SIM_CTRL (the control API, 8091-shaped); the
#           proof script boots its own with scripts/venue-sim-stack.sh;
#   cluster (default): the diorama-venue namespace, every call exec'd from the
#           venue-sim pod (cluster-internal service names — the same
#           transport the engine itself uses).
#
# What lives here: the simulator doors and the venue-shaped client gates
# every venue-family script shares — `sim_account`, `sim_spot`, `identity_ok`
# (the served-identity check daloc gates on) and `flatten_coin` (a proof's
# rerunnable start).
#
# Conventions (measured): the info plane serves daloc's shapes (8090) and the
# control plane the /control/* verbs plus /metrics (8091).
NS="${NS:-diorama-venue}"

source "$(dirname "${BASH_SOURCE[0]}")/lib-common.sh"

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
  echo "flatten_coin: $account still holds $coin szi=$szi after 5 reduce-only orders" >&2
  return 1
}

if [ -n "${SIM_INFO:-}" ]; then
  sget()   { curl -s --fail-with-body -m 30 "$SIM_INFO$1"; }
  sipost() { curl -s --fail-with-body -m 30 -X POST "$SIM_INFO$1" -H 'Content-Type: application/json' -d "$2"; }
  spost()  { curl -s --fail-with-body -m 30 -X POST "$SIM_CTRL$1" -H 'Content-Type: application/json' -d "$2"; }
  smetrics() { curl -s --fail-with-body -m 30 "$SIM_CTRL/metrics"; }
  sreadyz() { # the sim's readiness code: 200 live, 503 on the stalled plane
    curl -s -m 5 -o /dev/null -w '%{http_code}' "$SIM_INFO/readyz"; }
else
  exec_pod() { kubectl -n "$NS" exec -i deploy/venue-sim -- "$@"; }
  sget()   { exec_pod curl -s --fail-with-body -m 30 "http://localhost:8090$1"; }
  sipost() { exec_pod curl -s --fail-with-body -m 30 -X POST "http://localhost:8090$1" -H 'Content-Type: application/json' -d "$2"; }
  spost()  { exec_pod curl -s --fail-with-body -m 30 -X POST "http://localhost:8091$1" -H 'Content-Type: application/json' -d "$2"; }
  smetrics() { exec_pod curl -s --fail-with-body -m 30 "http://localhost:8091/metrics"; }
  sreadyz() { # the sim's readiness code: 200 live, 503 on the stalled plane
    exec_pod curl -s -m 5 -o /dev/null -w '%{http_code}' "http://localhost:8090/readyz" | tr -d '\r'; }
fi
