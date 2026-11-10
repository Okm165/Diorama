#!/usr/bin/env bash
# mirror-freshness.sh — one read-only verdict over the four mirror clocks.
#
# Execution plane : anvil-t/anvil-v height vs the engine cursor.
# Epoch plane     : source head − fork pin (the *source* blocks since the pin;
#                   this is the clock that ages out the source state window).
# Fixture plane   : the feed bridge, the precompile install, the venue sim.
# Settlement plane: the CCTP bridge's live/archived records.
#
# Read-only: kubectl exec + HTTP GETs only. No writes, no restarts.
# Exit: 0 fresh, 1 degraded, 2 stale, 3 unknown.
set -euo pipefail
MAINNET_NS="${MAINNET_NS:-diorama-mainnet}"
VENUE_NS="${VENUE_NS:-diorama-venue}"
CONSUMER_NS="${CONSUMER_NS:-consumer}"
CCTP_NS="${CCTP_NS:-diorama-cctp}"

# The measured source state windows (blocks). The mainnet window is the reth
# changeset window; the venue window is the fork source's archive window. The
# pin must be inside its window for the applier/follow to fetch parent diffs.
MAINNET_WINDOW="${MAINNET_WINDOW:-10000}"
VENUE_WINDOW="${VENUE_WINDOW:-120000}"
FEED_BOUND_S="${FEED_BOUND_S:-900}"        # the indexer's 15-minute freshness bound
PRECOMPILE_BOUND_S="${PRECOMPILE_BOUND_S:-300}"

say() { printf '%-30s %s\n' "$1" "$2"; }
kv()  { printf '  %-28s %s\n' "$1" "$2"; }
num() { printf '%s' "${1:-}" | tr -dc '0-9'; }

# A kubectl exec that never fails the script: a missing tool is "unreadable".
try() { "$@" 2>/dev/null || true; }

engine_status() { # ns
  try kubectl -n "$1" exec deploy/engine -- curl -sf --max-time 5 http://127.0.0.1:8080/status
}
engine_metrics() { # ns
  try kubectl -n "$1" exec deploy/engine -- curl -sf --max-time 5 http://127.0.0.1:8080/metrics
}
metric() { # body name
  printf '%s\n' "$1" | awk -v n="$2" '$1==n {print $2; exit}'
}

VERDICT=0
degrade() { if [ "$VERDICT" -lt 1 ]; then VERDICT=1; fi; }
stale()   { VERDICT=2; }
unknown() { if [ "$VERDICT" -lt 3 ]; then VERDICT=3; fi; }

echo "== execution plane =="
M_STATUS=$(engine_status "$MAINNET_NS")
V_STATUS=$(engine_status "$VENUE_NS")
M_METRICS=$(engine_metrics "$MAINNET_NS")
V_METRICS=$(engine_metrics "$VENUE_NS")

M_HEAD=$(printf '%s' "$M_STATUS" | jq -r '.head // empty' 2>/dev/null)
M_CURSOR=$(printf '%s' "$M_STATUS" | jq -r '.cursor // empty' 2>/dev/null)
M_LAG=$(printf '%s' "$M_STATUS" | jq -r '.lag // empty' 2>/dev/null)
M_GUARD=$(printf '%s' "$M_STATUS" | jq -r '.guard // empty' 2>/dev/null)
M_GAP_LIMIT=$(printf '%s' "$M_STATUS" | jq -r '.gap_limit // empty' 2>/dev/null)
# The deployed engine may predate the `guard` field; derive the same verdict
# from the lag and the configured limit rather than reporting an unknown.
if [ -z "${M_GUARD:-}" ] && [ -n "${M_LAG:-}" ] && [ -n "${M_GAP_LIMIT:-}" ]; then
  if [ "$M_LAG" -gt "$M_GAP_LIMIT" ]; then M_GUARD=paused; else M_GUARD=ok; fi
fi
# Last resort: the engine's own follow-status line (the deployed image may
# predate the `/status` guard fields; the log is the same verdict).
if [ -z "${M_GUARD:-}" ]; then
  # The fallback reads a log line that may not exist while the engine is
  # crash-looping. A failing `grep` inside the pipeline must not trip
  # `set -euo pipefail` and abort the whole verdict: the plane is unknown, not
  # the script.
  M_GUARD=$( (try kubectl -n "$MAINNET_NS" logs deploy/engine --tail=200 | grep -a 'follow status' | tail -1 | jq -r '.fields.guard // empty' 2>/dev/null) || true )
fi
M_PIN=$(try kubectl -n "$MAINNET_NS" exec deploy/anvil-t -- sh -c 'cat /state/pin 2>/dev/null' | tr -d '\r' | tail -1)
M_FOLLOW=$(metric "$M_METRICS" diorama_reth_follow_head)
M_SOURCE_GAP=$(metric "$M_METRICS" diorama_source_sync_gap_blocks)

V_HEAD=$(printf '%s' "$V_STATUS" | jq -r '.head // empty' 2>/dev/null)
V_CURSOR=$(printf '%s' "$V_STATUS" | jq -r '.cursor // empty' 2>/dev/null)
V_PIN=$(printf '%s' "$V_STATUS" | jq -r '.fork_pin // empty' 2>/dev/null)
V_SOURCE=$(metric "$V_METRICS" venue_source_head)
V_FOLLOWED=$(metric "$V_METRICS" venue_source_followed)
V_CATCHUPS=$(metric "$V_METRICS" venue_follow_gap_catchups_total)

kv "mainnet head/cursor" "${M_HEAD:-?}/${M_CURSOR:-?}  lag=${M_LAG:-?}  guard=${M_GUARD:-?}"
kv "mainnet source head" "${M_FOLLOW:-?}  reth_gap=${M_SOURCE_GAP:-?}"
kv "mainnet pin" "${M_PIN:-?}"
if [ -n "${M_PIN:-}" ] && [ -n "${M_FOLLOW:-}" ]; then
  M_PIN_AGE=$(( M_FOLLOW - M_PIN ))
  kv "mainnet source pin age" "$M_PIN_AGE blocks (window $MAINNET_WINDOW)"
  if [ "$M_PIN_AGE" -ge "$MAINNET_WINDOW" ]; then
    kv "mainnet pin verdict" "AGED OUT — the source cannot serve the pin; re-pin required"
    stale
  elif [ "$M_PIN_AGE" -ge $(( MAINNET_WINDOW * 7 / 10 )) ]; then
    kv "mainnet pin verdict" "AGING — schedule a re-pin before the window closes"
    degrade
  fi
fi
if [ "${M_GUARD:-}" = "paused" ]; then
  kv "mainnet follow verdict" "PAUSED — see the engine log's gap reason"
  stale
fi

kv "venue head/cursor" "${V_HEAD:-?}/${V_CURSOR:-?}  source=${V_SOURCE:-?}  followed=${V_FOLLOWED:-?}"
kv "venue pin" "${V_PIN:-?}  catch-ups=${V_CATCHUPS:-0}"
if [ -n "${V_PIN:-}" ] && [ -n "${V_SOURCE:-}" ]; then
  V_PIN_AGE=$(( V_SOURCE - V_PIN ))
  V_LAG=$(( V_SOURCE - V_FOLLOWED ))
  kv "venue source pin age" "$V_PIN_AGE blocks (window $VENUE_WINDOW)"
  kv "venue follow lag" "$V_LAG blocks"
  if [ "$V_PIN_AGE" -ge "$VENUE_WINDOW" ]; then
    kv "venue pin verdict" "AGED OUT — re-pin required"
    stale
  elif [ "$V_PIN_AGE" -ge $(( VENUE_WINDOW * 7 / 10 )) ]; then
    kv "venue pin verdict" "AGING — schedule a re-pin"
    degrade
  fi
  if [ "$V_LAG" -gt 0 ]; then
    kv "venue follow verdict" "CATCHING UP — bounded windows; not a hard pause"
    degrade
  fi
fi

echo "== fixture plane =="
FEED=$(try kubectl -n "$MAINNET_NS" exec deploy/engine -- curl -sf --max-time 5 http://127.0.0.1:8080/admin/feed)
if [ -n "$FEED" ] && printf '%s' "$FEED" | jq -e . >/dev/null 2>&1; then
  kv "feed bridge" "configured"
  printf '%s' "$FEED" | jq -r '.[]? | "  \(.name): owner=\(.owner // "?") error=\(.last_error // "none")"' 2>/dev/null || true
else
  kv "feed bridge" "UNCONFIGURED (the chain-1 mocks have no live writer)"
  degrade
fi
IDX=$(try kubectl -n "$CONSUMER_NS" exec deploy/indexer -- curl -s --max-time 5 http://127.0.0.1:8080/health)
if [ -n "$IDX" ] && printf '%s' "$IDX" | jq -e . >/dev/null 2>&1; then
  printf '%s' "$IDX" | jq -r '.metrics | "  stale assets: \(.price_feed_stale_assets // "?"); oldest age: \(.price_feed_advisory_oldest_age_s // "?")s", "  venue account failures/10m: \(.venue_account_failures_10m // "?")"' 2>/dev/null || true
  OLDEST=$(printf '%s' "$IDX" | jq -r '.metrics.price_feed_advisory_oldest_age_s // empty' 2>/dev/null)
  if [ -n "${OLDEST:-}" ] && [ "$(num "$OLDEST")" -gt "$FEED_BOUND_S" ]; then
    kv "feed freshness" "STALE (${OLDEST}s > ${FEED_BOUND_S}s)"
    stale
  fi
fi

JOB=$(try kubectl -n "$CONSUMER_NS" get jobs -o json)
if [ -n "$JOB" ]; then
  printf '%s' "$JOB" | jq -r '[.items[] | select(.metadata.name|startswith("precompile-mock-install"))] | sort_by(.status.completionTime // .metadata.creationTimestamp) | last | if . == null then "  precompile: no job" else "  precompile: \(.metadata.name) succeeded=\(.status.succeeded // 0) failed=\(.status.failed // 0)" end' 2>/dev/null || true
  PRECOMPILE_FAILED=$(printf '%s' "$JOB" | jq -r '[.items[] | select(.metadata.name|startswith("precompile-mock-install"))] | sort_by(.metadata.creationTimestamp) | last | .status.failed // 0' 2>/dev/null)
  if [ "${PRECOMPILE_FAILED:-0}" -gt 0 ]; then
    kv "precompile verdict" "last job has failures — inspect the CronJob"
    degrade
  fi
fi

SIM=$(try kubectl -n "$VENUE_NS" exec deploy/venue-sim -- curl -sf --max-time 5 http://127.0.0.1:8090/health)
kv "venue sim" "${SIM:-unreadable}"
if [ -z "${SIM:-}" ]; then degrade; fi

echo "== settlement plane =="
BRIDGE=$(try kubectl -n "$CCTP_NS" exec deploy/cctp-bridge -- curl -sf --max-time 5 http://127.0.0.1:8096/health)
if [ -n "$BRIDGE" ] && printf '%s' "$BRIDGE" | jq -e . >/dev/null 2>&1; then
  printf '%s' "$BRIDGE" | jq -r '"  bridge: pending=\(.pending // "?") signed=\(.signed // "?") relayed=\(.relayed // "?") parked=\(.parked // "?") archived_parked=\(.archived_parked // "?")"' 2>/dev/null || true
  ARCHIVED=$(printf '%s' "$BRIDGE" | jq -r '.archived_parked // 0' 2>/dev/null)
  if [ "${ARCHIVED:-0}" -gt 0 ]; then
    kv "bridge verdict" "archived parked records exist (old epoch; acknowledge, do not re-relay)"
    degrade
  fi
else
  kv "bridge" "unreadable"
  unknown
fi

echo "== verdict =="
case "$VERDICT" in
  0) echo "FRESH — all four planes inside their bounds" ;;
  1) echo "DEGRADED — a plane is aging or a fixture is unconfigured; see above" ;;
  2) echo "STALE — a plane is outside its bound (pin, follow, or fixture); see above" ;;
  *) echo "UNKNOWN — a plane could not be read; do not treat as fresh" ;;
esac
exit "$VERDICT"
