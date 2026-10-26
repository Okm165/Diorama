#!/usr/bin/env bash
# The behavioral attestation: one pass over both mirrors' running services —
# every pod once, the key live facts per service, and the recent WARN/ERROR
# catalogue — followed (via `make attest`) by the asserting suites: the log
# surface and the engine symmetry.
#
# Informational by design beyond the pods: the metrics print even while a
# study or a drill is firing deliberate WARNs (the studies' price moves, a
# rewind's own fact); the *assertions* live in the suites this target chains.
#
# Run from the repo root:
#   make attest               # the pass + log-surface + engine-symmetry
#   ./scripts/attest.sh 30m   # the pass alone, a wider WARN window
set -euo pipefail

WIN="${1:-10m}"
pass=0; fail=0
ok()  { pass=$((pass+1)); echo "  ✔ $*"; }
bad() { fail=$((fail+1)); echo "  ✘ $*"; }

pick() { # pick <url> <names...> — the named series from one /metrics
  local url="$1"; shift
  local body; body=$(curl -sf -m 10 "$url/metrics" 2>/dev/null || true)
  for n in "$@"; do
    printf '    %-42s %s\n' "$n" "$(awk -v n="$n" '$1 ~ "^"n"($|\\{)" {print $2; exit}' <<< "$body")"
  done
}

echo "== the behavioral attestation ($(date -u +%H:%M:%S) UTC) =="

# Every pod once: running, ready, and no in-pod restarts (a restart is an
# anomaly even when the service recovered).
for ns in mainnet-mirror venue-mirror; do
  while read -r name ready status restarts _; do
    [ -z "${name:-}" ] && continue
    if [ "$ready" = "1/1" ] && [ "$status" = "Running" ] && [ "$restarts" = "0" ]; then
      ok "$ns/$name 1/1 Running, no restarts"
    else
      bad "$ns/$name $ready $status, restarts $restarts"
    fi
  done < <(timeout 20 kubectl -n "$ns" get pods --no-headers 2>/dev/null || true)
done

echo "-- T engine (30880): the applier and the follow"
pick http://localhost:30880 mirror_cursor_block mirror_reth_head mirror_lag_blocks mirror_blocks_applied_total \
  mirror_t_convergence_ok_total mirror_t_convergence_failures_total mirror_t_convergence_last_failed \
  mirror_t_probe_skipped_total mirror_t_resets_total mirror_reorgs_detected_total \
  mirror_shadowed_slots mirror_shadowed_accounts mirror_halted mirror_source_sync_gap_blocks

echo "-- V engine (31880): the follow's windows and the polls"
pick http://localhost:31880 venue_cursor venue_head venue_source_head venue_source_followed \
  venue_follow_windows_total venue_follow_applied_total venue_follow_gap_catchups_total \
  venue_follow_sampled_total venue_follow_sampled_ok_total venue_follow_skipped_shadowed_total \
  venue_poll_touched venue_poll_planned_keys venue_poll_changed_keys venue_poll_uncovered_logs_total \
  venue_source_reorg_total venue_epochs_total venue_halted

echo "-- sim (31091): the market plane and the Core<->EVM bridge"
pick http://localhost:31091 venue_sim_accounts venue_sim_owned_levels venue_feed_overrides \
  venue_bridge_actions_total venue_bridge_credits_total venue_bridge_completions_total \
  venue_bridge_refusals_total venue_bridge_malformed_total venue_bridge_trace_faults_total \
  venue_bridge_lag_blocks venue_sim_advances_total venue_sim_adl_closes_total \
  venue_sim_backstop_liquidations_total

echo "-- bridge (31096): the CCTP queue and the relays"
pick http://localhost:31096 cctp_messages_seen_total cctp_messages_pending cctp_messages_signed \
  cctp_messages_parked cctp_relays_confirmed_total cctp_relays_parked_total cctp_relay_attempts_total \
  cctp_scan_errors_total cctp_last_scan_block cctp_gas_low_total cctp_gas_critical_total

echo "-- WARN/ERROR (last $WIN, deduped; deliberate study/drill facts appear here too)"
for ns in mainnet-mirror venue-mirror; do
  for d in $(timeout 20 kubectl -n "$ns" get deploy -o name 2>/dev/null | sed 's|deployment.apps/||'); do
    timeout 25 kubectl -n "$ns" logs "deploy/$d" --since="$WIN" 2>/dev/null | python3 -c "
import json, sys, collections
c = collections.Counter()
for line in sys.stdin:
    try:
        j = json.loads(line)
        lvl = j.get('level')
        if lvl in ('WARN', 'ERROR'):
            c[(lvl, j.get('fields', {}).get('message', '?')[:80])] += 1
    except Exception:
        pass
for (lvl, msg), k in c.most_common(3):
    print(f'  $ns/$d {k}x {lvl}: {msg}')
"
  done
done

echo "== attestation: $pass pods running clean, $fail flagged (the metrics above are the live facts; log-surface and engine-symmetry assert the contract) =="
[ "$fail" = "0" ]
