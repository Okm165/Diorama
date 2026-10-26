#!/usr/bin/env bash
# Fitness function: log-surface — the symmetric log contract, attested against
# the running services.
#
# The audit that motivated it (2026-10-26) measured 1697 DEBUG + 155 INFO
# lines on the mainnet engine against 7 DEBUG on the venue engine over the
# same 20 minutes: the venue *was* following live (its state metrics were
# perfect, every suite passed) but no line said so — a silent surface. State
# suites cannot catch that class of gap; this one asserts the surface itself:
#   - both engines beat: `follow status` (INFO) within 3 minutes, the *same
#     field set* on both sides (one `FollowStatus` on the shared spine);
#   - the venue's window line carries from/to/source/source_head/lag, and the
#     `to` advances between consecutive windows (the following is moving);
#   - the mainnet's `probes complete` (INFO) is fresh;
#   - the sim's `market plane live` (INFO) is fresh, names every configured
#     coin, and reports identity_violations = 0;
#   - the bridge's `bridge status` (INFO) is fresh with its four counts.
#
# The surface warms on its own cadence (60 s heartbeats; the sim's first line
# lands ~60 s after start, and the engines' boot heartbeat carries zeros), so
# the suite waits for every key line before judging — a fresh restart is not
# a failure.
#
# Run from the repo root:
#   make log-surface
set -euo pipefail

NS_T="${NS_T:-mainnet-mirror}"
NS_V="${NS_V:-venue-mirror}"
SIM_CTL_URL="${SIM_CTL_URL:-http://localhost:31091}"
FRESH_S="${FRESH_S:-180}"

pass=0; fail=0
ok()  { pass=$((pass+1)); echo "  ✔ $*"; }
bad() { fail=$((fail+1)); echo "  ✘ $*"; }

logs() { timeout 25 kubectl -n "$1" logs "deploy/$2" --since="${3:-4m}" 2>/dev/null || true; }
latest() { grep -F "\"message\":\"$2\"" <<< "$1" | tail -1 || true; }
field() { python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["fields"].get(sys.argv[1],""))' "$2" <<< "$1"; }
fields_of() { python3 -c 'import json,sys; d=json.loads(sys.stdin.read()); print(",".join(sorted(k for k in d["fields"] if k!="message")))' <<< "$1"; }
age() { python3 -c '
import json, sys, datetime
d = json.loads(sys.stdin.read())
t = datetime.datetime.fromisoformat(d["timestamp"].replace("Z", "+00:00"))
print(int((datetime.datetime.now(datetime.timezone.utc) - t).total_seconds()))
' <<< "$1"; }

echo "== log surface: the symmetric log contract =="

# The warm-up: fetch until every key line exists and both engines' heartbeats
# carry a real source head (`0` is the boot heartbeat — nothing followed yet).
for attempt in 1 2 3 4 5; do
  t_logs=$(logs "$NS_T" engine)
  v_logs=$(logs "$NS_V" engine)
  sim_logs=$(logs "$NS_V" venue-sim)
  br_logs=$(logs "$NS_V" cctp-bridge)
  t_hb=$(latest "$t_logs" "follow status")
  v_hb=$(latest "$v_logs" "follow status")
  sim_hb=$(latest "$sim_logs" "market plane live")
  br_hb=$(latest "$br_logs" "bridge status")
  t_probes=$(latest "$t_logs" "probes complete")
  if [ -n "$t_hb" ] && [ -n "$v_hb" ] && [ -n "$sim_hb" ] && [ -n "$br_hb" ] && [ -n "$t_probes" ] \
    && [ "$(field "$v_hb" source_head)" != "0" ] && [ "$(field "$t_hb" source_head)" != "0" ]; then
    break
  fi
  if [ "$attempt" -lt 5 ]; then
    echo "  · warming (attempt $attempt): waiting for the 60 s heartbeats"
    sleep 20
  fi
done

# 1-2. both engines beat (the heartbeat liveness + the fields it names)
for pair in "T:$t_hb" "V:$v_hb"; do
  side="${pair%%:*}"; line="${pair#*:}"
  if [ -z "$line" ]; then
    bad "$side: no follow status in the last 4 minutes"
    continue
  fi
  a=$(age "$line")
  if [ "$a" -le "$FRESH_S" ] && [ "$(field "$line" source_head)" != "0" ]; then
    ok "$side follow status fresh (${a}s): via=$(field "$line" via) source_head=$(field "$line" source_head) followed=$(field "$line" followed) lag=$(field "$line" lag_blocks) guard=$(field "$line" guard)"
  else
    bad "$side follow status stale or still booting (${a}s, source_head=$(field "$line" source_head))"
  fi
done

# 3. the same field set on both sides (the symmetry invariant)
want="applied,followed,guard,lag_blocks,mirror,source_head,via"
if [ -n "$t_hb" ] && [ -n "$v_hb" ]; then
  tf=$(fields_of "$t_hb"); vf=$(fields_of "$v_hb")
  if [ "$tf" = "$want" ] && [ "$vf" = "$want" ]; then
    ok "the heartbeats carry the same field set ($want)"
  else
    bad "heartbeat field sets: T=[$tf] V=[$vf] want=[$want]"
  fi
fi

# 4. the venue's windows advance (the following is moving, not a frozen cursor)
v_wins=$(grep -F '"message":"source window composed onto V"' <<< "$v_logs" | tail -2 || true)
n=$(grep -c . <<< "$v_wins" || true)
if [ "${n:-0}" -ge 2 ]; then
  to_a=$(field "$(head -1 <<< "$v_wins")" to)
  to_b=$(field "$(tail -1 <<< "$v_wins")" to)
  if [ "$to_b" -gt "$to_a" ]; then
    ok "V windows advance (to $to_a -> $to_b, source_head=$(field "$(tail -1 <<< "$v_wins")" source_head))"
  else
    bad "V windows did not advance ($to_a -> $to_b)"
  fi
else
  bad "V: fewer than two windows in the last 4 minutes"
fi

# 5. the window line's own fields (from/to/source/source_head/lag — the same
# shapes the mainnet's per-block line carries)
v_last=$(tail -1 <<< "$v_wins" || true)
if [ -n "$v_last" ]; then
  missing=""
  for f in from to source source_head lag; do
    grep -qF "\"$f\":" <<< "$v_last" || missing="$missing $f"
  done
  if [ -z "$missing" ]; then
    ok "V: the window line carries from/to/source/source_head/lag"
  else
    bad "V: the window line is missing:$missing"
  fi
fi

# 6. the mainnet's probe summary
if [ -z "$t_probes" ]; then
  bad "T: no probes complete in the last 4 minutes"
else
  a=$(age "$t_probes")
  if [ "$a" -le "$FRESH_S" ]; then
    ok "T probes complete fresh (${a}s): block=$(field "$t_probes" block) ok=$(field "$t_probes" ok) failed=$(field "$t_probes" failed) skipped=$(field "$t_probes" skipped)"
  else
    bad "T probes complete stale (${a}s > ${FRESH_S}s)"
  fi
fi

# 7-8. the sim's market plane (fresh, every configured coin, no violations)
if [ -z "$sim_hb" ]; then
  bad "sim: no market plane live in the last 4 minutes"
else
  a=$(age "$sim_hb")
  coins=$(field "$sim_hb" coins)
  # The configured markets are the feed's own staleness gauges (the real
  # venue's meta carries every listed coin; the sim serves its config).
  metrics=$(timeout 15 curl -sf -m 10 "$SIM_CTL_URL/metrics" 2>/dev/null || true)
  names=$(python3 -c '
import re, sys
print(" ".join(sorted(set(re.findall(r"venue_sim_stale_ms\{coin=\"([^\"]+)\"\}", sys.stdin.read())))))
' <<< "$metrics" 2>/dev/null || true)
  if [ -z "$names" ]; then
    bad "sim: the stale-gauge fetch failed (no coin list to check)"
  else
    missing=""
    for c in $names; do
      grep -qF "$c=" <<< "$coins" || missing="$missing $c"
    done
    if [ "$a" -le "$FRESH_S" ] && [ -z "$missing" ]; then
      ok "sim market plane live fresh (${a}s), every configured coin named ($(tr ' ' ',' <<< "$names"))"
    else
      bad "sim market plane stale or missing coins:${missing:- none} (${a}s)"
    fi
  fi
  viol=$(field "$sim_hb" identity_violations)
  if [ "$viol" = "0" ]; then
    ok "sim identity_violations = 0"
  else
    bad "sim identity_violations = $viol"
  fi
fi

# 9. the bridge's heartbeat + its four counts
if [ -z "$br_hb" ]; then
  bad "bridge: no bridge status in the last 4 minutes"
else
  a=$(age "$br_hb")
  missing=""
  for f in pending signed relayed parked; do
    grep -qF "\"$f\":" <<< "$br_hb" || missing="$missing $f"
  done
  if [ "$a" -le "$FRESH_S" ] && [ -z "$missing" ]; then
    ok "bridge status fresh (${a}s): pending=$(field "$br_hb" pending) signed=$(field "$br_hb" signed) relayed=$(field "$br_hb" relayed) parked=$(field "$br_hb" parked)"
  else
    bad "bridge status stale or missing fields:$missing (${a}s)"
  fi
fi

# 10. the level contract: every heartbeat is INFO
noninfo=0
for l in "$t_hb" "$v_hb" "$sim_hb" "$br_hb"; do
  if [ -n "$l" ] && ! grep -qF '"level":"INFO"' <<< "$l"; then
    noninfo=$((noninfo + 1))
  fi
done
if [ "$noninfo" = "0" ]; then
  ok "every heartbeat is INFO"
else
  bad "heartbeats not at INFO: $noninfo"
fi

echo "== log surface: $pass passed, $fail failed"
[ "$fail" = "0" ]
