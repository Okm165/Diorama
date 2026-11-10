#!/usr/bin/env bash
# substrate-attest — the read-only "is the research substrate actually
# trustworthy right now?" pass. It asserts the invariants the risk work
# depends on, in the order a failure becomes a wrong financial answer:
#
#   1. both mirrors' follow cursors are within their observation bounds;
#   2. the venue simulator's market plane is fresh;
#   3. the chain-1 and chain-999 Daloc deployments have code at the
#      configured addresses (a re-fork that moved the pin past the deployment
#      epoch leaves every address codeless — the 2026-11-02 and 2026-11-07
#      failure);
#   4. the CCTP bridge is readable and its parked set is named;
#   5. Postgres answers and is not near its connection ceiling;
#   6. the host has memory and CPU headroom.
#
# It never writes, restarts, deploys, or sends a transaction. Exit non-zero
# when any hard invariant fails; WARNs are printed but do not fail the pass.
#
# Usage: scripts/substrate-attest.sh
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib-common.sh"

K="${KUBECTL:-kubectl}"
NS_MAINNET="${NS_MAINNET:-diorama-mainnet}"
NS_VENUE="${NS_VENUE:-diorama-venue}"
NS_CCTP="${NS_CCTP:-diorama-cctp}"
NS_DALOC="${NS_DALOC:-daloc}"
PGUSER="${PGUSER:-daloc}"
PGDATABASE="${PGDATABASE:-daloc}"
# Conservative decision bounds. Override per deployment only with a recorded
# reason: these are the values the research conclusions rely on.
MAINNET_LAG_BOUND="${MAINNET_LAG_BOUND:-60}"
VENUE_LAG_BOUND="${VENUE_LAG_BOUND:-50}"
PIN_AGE_BOUND="${PIN_AGE_BOUND:-2000}"
VENUE_SOURCE_PIN_AGE_BOUND="${VENUE_SOURCE_PIN_AGE_BOUND:-1000}"

warn=0
warning() { echo "  ! $*"; warn=$((warn + 1)); }

metric() { # metric <namespace> <deployment> <metric-name>
  "$K" -n "$1" exec "deploy/$2" -- curl -sf localhost:8080/metrics 2>/dev/null \
    | awk -v m="$3" '$1 == m { print $2; exit }'
}

status() { # status <namespace> <deployment> <key>
  "$K" -n "$1" exec "deploy/$2" -- curl -sf localhost:8080/status 2>/dev/null \
    | json "d.get(\"$3\")"
}

pods_ready() { # pods_ready <namespace>
  local ns=$1 total ready
  total=$("$K" -n "$ns" get pods --no-headers 2>/dev/null | grep -vc 'Completed')
  ready=$("$K" -n "$ns" get pods --no-headers 2>/dev/null | grep -v 'Completed' \
    | awk '$2 ~ /^[0-9]+\/[0-9]+$/ { split($2, a, "/"); if (a[1] == a[2]) n++ } END { print n + 0 }')
  [ "$total" -gt 0 ] && [ "$ready" -eq "$total" ]
}

echo "== pods =="
for ns in "$NS_MAINNET" "$NS_VENUE" "$NS_CCTP" "$NS_DALOC"; do
  if pods_ready "$ns"; then
    ok "$ns: all running pods ready"
  else
    bad "$ns: not all running pods ready"
    "$K" -n "$ns" get pods --no-headers 2>/dev/null | grep -v 'Completed' \
      | awk '$2 ~ /^[0-9]+\/[0-9]+$/ { split($2, a, "/"); if (a[1] != a[2]) print "      " $1 " " $2 " " $3 " restarts=" $4 }'
  fi
done

echo "== halt =="
for pair in "$NS_MAINNET:engine" "$NS_VENUE:engine"; do
  ns=${pair%%:*}; deploy=${pair#*:}
  halted=$(metric "$ns" "$deploy" diorama_halted)
  if [ "${halted:-1}" = "0" ]; then
    ok "$ns/$deploy not halted"
  else
    bad "$ns/$deploy halted=${halted:-unknown} — a halted engine must not be treated as ready"
  fi
done

echo "== follow cursors =="
mainnet_lag=$(metric "$NS_MAINNET" engine diorama_lag_blocks)
mainnet_pin_age=$(metric "$NS_MAINNET" engine diorama_pin_age_blocks)
if [ -n "$mainnet_lag" ] && [ "$mainnet_lag" -le "$MAINNET_LAG_BOUND" ] 2>/dev/null; then
  ok "mainnet lag ${mainnet_lag} blocks (bound ${MAINNET_LAG_BOUND})"
else
  bad "mainnet lag ${mainnet_lag:-unknown} blocks (bound ${MAINNET_LAG_BOUND})"
fi
if [ -n "$mainnet_pin_age" ] && [ "$mainnet_pin_age" -le "$PIN_AGE_BOUND" ] 2>/dev/null; then
  ok "mainnet pin age ${mainnet_pin_age} blocks (bound ${PIN_AGE_BOUND})"
else
  bad "mainnet pin age ${mainnet_pin_age:-unknown} blocks (bound ${PIN_AGE_BOUND}) — a stale base is a new epoch risk"
fi

venue_head=$(metric "$NS_VENUE" engine venue_source_head)
venue_followed=$(metric "$NS_VENUE" engine venue_source_followed)
venue_pin=$(status "$NS_VENUE" engine fork_pin)
if [ -n "$venue_head" ] && [ -n "$venue_followed" ]; then
  venue_lag=$((venue_head - venue_followed))
  if [ "$venue_lag" -le "$VENUE_LAG_BOUND" ]; then
    ok "venue follow ${venue_followed}/${venue_head} (lag ${venue_lag}, bound ${VENUE_LAG_BOUND})"
  else
    bad "venue follow ${venue_followed}/${venue_head} (lag ${venue_lag}, bound ${VENUE_LAG_BOUND}) — venue marks are last-good, not observed"
  fi
else
  bad "venue follow cursor is unreadable (source_head/followed absent)"
fi
if [ -n "$venue_head" ] && [ -n "$venue_pin" ]; then
  venue_source_pin_age=$((venue_head - venue_pin))
  if [ "$venue_source_pin_age" -le "$VENUE_SOURCE_PIN_AGE_BOUND" ]; then
    ok "venue fork pin ${venue_pin} is ${venue_source_pin_age} blocks behind source head (fork-base age; state freshness is the follow lag above)"
  else
    warning "venue fork pin ${venue_pin} is ${venue_source_pin_age} blocks behind source head ${venue_head} — fork-base age; re-pin when the source archive no longer covers the pin (state freshness is the follow lag above)"
  fi
else
  warning "venue fork pin/source head unreadable; fork-base age not measured"
fi

# The at-pin probes are the independent parity check. A zero ok count with
# zero failures is inert, not green: the source pool is latest-only and the
# configured targets may be devnet-owned/fixtures. Probe coverage is the hard
# gate; fork-base age is not a state-freshness proxy.
venue_probe_ok=$(metric "$NS_VENUE" engine venue_probe_ok_total)
venue_probe_failed=$(metric "$NS_VENUE" engine venue_probe_failures_total)
venue_probe_skipped=$(metric "$NS_VENUE" engine venue_probe_skipped_total)
if [ "${venue_probe_ok:-0}" -gt 0 ] && [ "${venue_probe_failed:-0}" -eq 0 ]; then
  ok "venue at-pin probes: ${venue_probe_ok} ok, ${venue_probe_failed} failed, ${venue_probe_skipped:-0} skipped"
else
  bad "venue at-pin probes: ok=${venue_probe_ok:-0} failed=${venue_probe_failed:-0} skipped=${venue_probe_skipped:-0} — the fork base is unverified (latest-only source or devnet-owned targets)"
fi

echo "== venue market plane =="
sim_ready=$("$K" -n "$NS_VENUE" exec deploy/engine -- curl -s -o /dev/null -w '%{http_code}' -m 5 http://venue-sim:8090/readyz 2>/dev/null | tr -d '\r')
if [ "$sim_ready" = "200" ]; then
  ok "venue-sim /readyz 200 (market plane fresh)"
else
  bad "venue-sim /readyz ${sim_ready:-unreachable}"
fi

echo "== Daloc deployment code =="
registry=$("$K" -n "$NS_DALOC" exec deploy/gateway -- cat /etc/daloc/chain-registry.env 2>/dev/null || true)
if [ -z "$registry" ]; then
  warning "chain registry unreadable from the gateway; contract-code checks skipped"
else
  check_code() { # check_code <chain-namespace> <deployment> <label> <address>
    local ns=$1 deploy=$2 label=$3 address=$4 code
    [ -n "$address" ] && [ "$address" != "0x0000000000000000000000000000000000000000" ] || return 0
    code=$("$K" -n "$ns" exec "deploy/$deploy" -- cast code "$address" --rpc-url http://localhost:8545 2>/dev/null | tr -d '\r\n')
    if [ -n "$code" ] && [ "$code" != "0x" ]; then
      ok "$label $address has code"
    else
      bad "$label $address is CODELESS"
    fi
  }
  for key in CHAIN_1_HUB_ADDRESS CHAIN_1_UNDERWRITER_ADDRESS CHAIN_1_PROOF_EXECUTOR_ADDRESS CHAIN_1_LIQUIDATION_EXECUTOR_ADDRESS CHAIN_1_CM_ISSUER_ADDRESS CHAIN_1_DEBT_ISSUER_ADDRESS; do
    address=$(printf '%s\n' "$registry" | sed -n "s/^${key}=//p")
    check_code "$NS_MAINNET" anvil-t "$key" "$address"
  done
  for key in CHAIN_999_HUB_ADDRESS CHAIN_999_UNDERWRITER_ADDRESS CHAIN_999_PROOF_EXECUTOR_ADDRESS CHAIN_999_LIQUIDATION_EXECUTOR_ADDRESS CHAIN_999_UNIFIED_MARGIN_AGENT_FACTORY_ADDRESS; do
    address=$(printf '%s\n' "$registry" | sed -n "s/^${key}=//p")
    check_code "$NS_VENUE" anvil-v "$key" "$address"
  done
fi

echo "== CCTP bridge =="
bridge=$("$K" -n "$NS_VENUE" exec deploy/engine -- curl -sf -m 10 http://cctp-bridge.diorama-cctp.svc.cluster.local:8096/health 2>/dev/null || true)
if [ -z "$bridge" ]; then
  bad "CCTP bridge health unreadable"
else
  bridge_status=$(printf '%s' "$bridge" | json 'd.get("status")')
  parked=$(printf '%s' "$bridge" | json 'd.get("parked")')
  if [ "$bridge_status" = "ok" ] && [ "$parked" = "0" ]; then
    ok "CCTP bridge ok, no parked burns"
  else
    bad "CCTP bridge status=${bridge_status:-unknown} parked=${parked:-unknown} — parked burns are prior-epoch/rewound and need an explicit re-run or archive"
  fi
fi

echo "== storage =="
for ns in "$NS_MAINNET" "$NS_VENUE" "$NS_CCTP"; do
  notbound=$("$K" -n "$ns" get pvc --no-headers 2>/dev/null | awk '$2 != "Bound" {print $1}' | wc -l)
  if [ "$notbound" -eq 0 ]; then
    ok "$ns: all PVCs Bound"
  else
    bad "$ns: ${notbound} PVC(s) not Bound"
  fi
  classes=$("$K" -n "$ns" get pvc -o jsonpath='{range .items[*]}{.spec.storageClassName}{"\n"}{end}' 2>/dev/null | sort -u)
  for class in $classes; do
    [ -z "$class" ] && continue
    if "$K" get sc "$class" >/dev/null 2>&1; then
      ok "$ns: StorageClass $class exists"
    else
      bad "$ns: StorageClass $class is missing (a fresh cluster would leave the PVC Pending)"
    fi
  done
done

echo "== journals =="
mainnet_journal=$("$K" -n "$NS_MAINNET" exec deploy/engine -- sh -c 'ls /state/journal 2>/dev/null | wc -l' 2>/dev/null | tr -d '[:space:]')
if [ "${mainnet_journal:-0}" -gt 0 ]; then
  ok "mainnet journal has ${mainnet_journal} files"
else
  bad "mainnet journal is empty (rewind/recovery disabled)"
fi
venue_journal=$("$K" -n "$NS_VENUE" exec deploy/engine -- sh -c 'ls /state/journal 2>/dev/null | wc -l' 2>/dev/null | tr -d '[:space:]')
if [ "${venue_journal:-0}" -gt 0 ]; then
  ok "venue journal has ${venue_journal} files"
else
  warning "venue journal is empty; recovery relies on shadow.json + sim.json"
fi
venue_sim_state=$("$K" -n "$NS_VENUE" exec deploy/venue-sim -- sh -c 'test -s /state/sim.json && echo yes' 2>/dev/null | tr -d '[:space:]')
if [ "$venue_sim_state" = "yes" ]; then
  ok "venue-sim state file present"
else
  bad "venue-sim state file missing/empty"
fi

echo "== cctp store =="
store=$("$K" -n "$NS_CCTP" exec deploy/cctp-bridge -- sh -c 'cat /state/cctp.json 2>/dev/null' 2>/dev/null)
if [ -z "$store" ]; then
  bad "cctp store missing"
else
  store_count=$(printf '%s' "$store" | json 'len(d.get("messages", []))' 2>/dev/null || echo "")
  if [ -n "$store_count" ] && [ "$store_count" -gt 0 ]; then
    ok "cctp store has ${store_count} records"
  else
    warning "cctp store has 0 records; a fresh epoch or a lost store — check the archive before trusting cross-chain"
  fi
fi

echo "== pod restarts =="
for ns in "$NS_MAINNET" "$NS_VENUE" "$NS_CCTP"; do
  restarted=$("$K" -n "$ns" get pods --no-headers 2>/dev/null | awk '$4+0 > 0 {print $1"="$4}' | paste -sd, -)
  if [ -z "$restarted" ]; then
    ok "$ns: no current pod restarts"
  else
    warning "$ns: current pod restarts: $restarted"
  fi
done

echo "== database =="
if "$K" -n "$NS_DALOC" exec deploy/postgres -- pg_isready -U "$PGUSER" >/dev/null 2>&1; then
  ok "postgres answers"
  active=$("$K" -n "$NS_DALOC" exec deploy/postgres -- psql -U "$PGUSER" -d "$PGDATABASE" -Atc 'select count(*) from pg_stat_activity' 2>/dev/null | tr -d '[:space:]')
  maxconn=$("$K" -n "$NS_DALOC" exec deploy/postgres -- psql -U "$PGUSER" -d "$PGDATABASE" -Atc 'show max_connections' 2>/dev/null | tr -d '[:space:]')
  if [ -n "$active" ] && [ -n "$maxconn" ]; then
    if [ "$active" -ge $((maxconn * 8 / 10)) ]; then
      bad "postgres connections ${active}/${maxconn} (>= 80%)"
    else
      ok "postgres connections ${active}/${maxconn}"
    fi
  fi
else
  bad "postgres is not answering"
fi

echo "== host =="
avail_mb=$(awk '/MemAvailable:/ {print int($2/1024)}' /proc/meminfo)
load=$(awk '{print int($1)}' /proc/loadavg)
cpus=$(nproc)
if [ "$avail_mb" -ge 8192 ]; then
  ok "host memory available ${avail_mb} MiB"
else
  warning "host memory available ${avail_mb} MiB (< 8192)"
fi
if [ "$load" -le $((cpus * 2)) ]; then
  ok "host load ${load} on ${cpus} CPUs"
else
  warning "host load ${load} on ${cpus} CPUs (> 2x)"
fi

echo "== summary =="
echo "substrate-attest: $pass ok, $fail bad, $warn warning(s)"
[ "$fail" = "0" ]
