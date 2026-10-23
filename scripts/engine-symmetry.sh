#!/usr/bin/env bash
# Fitness function: engine-symmetry — the two engines' HTTP surfaces must
# answer the shared verbs identically (status + shape), expose the shared
# admin gauges, and keep exactly the documented absences.
#
# The design rule it enforces (plan §11.3): the admin *semantics* live in
# `mirror-devnet` and both engines classify them the same way; a new refusal
# variant or a renamed field must land on both sides in one commit.
#
# Run from the repo root:
#   ./scripts/engine-symmetry.sh                 # cluster: both namespaces
#   ENGINE_T_URL=http://127.0.0.1:18680 \
#     ENGINE_V_URL=http://127.0.0.1:18681 ./scripts/engine-symmetry.sh
set -euo pipefail

NS_T="${NS_T:-mainnet-mirror}"
NS_V="${NS_V:-venue-mirror}"
USDC_T="${USDC_T:-0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48}"
USDC_V="${USDC_V:-0xb88339CB7199B77E23DB6E890353E22632Ba630f}"
ACCT="${ACCT:-0x00000000000000000000000000000000000000f1}"

pass=0; fail=0
ok()  { echo "  ✔ $*"; pass=$((pass+1)); }
bad() { echo "  ✘ $*"; fail=$((fail+1)); }

# fetch <side> <curl args...> — LAST_PATH selects the route; the body lands in
# /tmp/sym-<side>.out, the status is echoed.
fetch() {
  local side=$1; shift
  if [ -n "${ENGINE_T_URL:-}" ]; then
    local url; [ "$side" = t ] && url="$ENGINE_T_URL" || url="$ENGINE_V_URL"
    curl -s -o "/tmp/sym-$side.out" -w '%{http_code}' -m 30 "$@" "$url$LAST_PATH"
  else
    local ns; [ "$side" = t ] && ns="$NS_T" || ns="$NS_V"
    kubectl -n "$ns" exec -i deploy/engine -- curl -s -w '\n%{http_code}' -m 30 "$@" "localhost:8080$LAST_PATH" > "/tmp/sym-$side.out"
    tail -1 "/tmp/sym-$side.out"
  fi
}
body() { # the response body (the status line is stripped in cluster mode)
  if [ -n "${ENGINE_T_URL:-}" ]; then cat "/tmp/sym-$1.out"; else sed '$d' "/tmp/sym-$1.out"; fi
}
keys() { python3 -c 'import json,sys
try: print(",".join(sorted(json.load(sys.stdin).keys())))
except Exception: print("(not-json)")'; }

# same <path> [curl args...] — both engines: same status, same JSON key set.
same() {
  LAST_PATH="$1"; shift
  t=$(fetch t "$@"); tb=$(body t)
  v=$(fetch v "$@"); vb=$(body v)
  if [ "$t" != "$v" ]; then bad "$LAST_PATH: status T=$t V=$v"; return; fi
  tk=$(printf '%s' "$tb" | keys); vk=$(printf '%s' "$vb" | keys)
  if [ "$tk" != "$vk" ]; then bad "$LAST_PATH: T=$t keys[$tk] V=$v keys[$vk]"; return; fi
  ok "$LAST_PATH: both $t, keys[$tk]"
}

# same_error <path> [curl args...] — a refusal: same status and the same
# message *wording* (digits normalized: a message may legitimately embed each
# chain's own cursor or ring size, both config/state, not code).
same_error() {
  LAST_PATH="$1"; shift
  t=$(fetch t "$@"); tb=$(body t)
  v=$(fetch v "$@"); vb=$(body v)
  tn=$(printf '%s' "$tb" | tr -d '0-9'); vn=$(printf '%s' "$vb" | tr -d '0-9')
  if [ "$t" = "$v" ] && [ "$tn" = "$vn" ]; then
    ok "$LAST_PATH: both $t \"$tb\""
  else
    bad "$LAST_PATH: T=$t \"$tb\" V=$v \"$vb\""
  fi
}

echo "== engine symmetry (T: ${ENGINE_T_URL:-ns $NS_T} / V: ${ENGINE_V_URL:-ns $NS_V}) =="

echo "-- the shared read surface"
same /healthz
same /metrics
# /status reports each mirror's own facts; the *shared* fields must be present
# on both (the scripts read them by name).
LAST_PATH=/status
t=$(fetch t); tb=$(body t)
v=$(fetch v); vb=$(body v)
missing_t=$(printf '%s' "$tb" | python3 -c 'import json,sys
d=json.load(sys.stdin)
want=["cursor","halted","shadowed","shadowed_accounts","shadowed_slots"]
print(",".join(k for k in want if k not in d))')
missing_v=$(printf '%s' "$vb" | python3 -c 'import json,sys
d=json.load(sys.stdin)
want=["cursor","halted","shadowed","shadowed_accounts","shadowed_slots"]
print(",".join(k for k in want if k not in d))')
if [ "$t" = "200" ] && [ "$v" = "200" ] && [ -z "$missing_t" ] && [ -z "$missing_v" ]; then
  ok "/status: both 200 with the shared fields (cursor, halted, shadowed*)"
else
  bad "/status: T=$t missing[$missing_t] V=$v missing[$missing_v]"
fi

echo "-- the shared admin verbs"
same /admin/pause -X POST -H 'Content-Type: application/json' -d '{}'
same /admin/resume -X POST -H 'Content-Type: application/json' -d '{}'
# A rewind beyond the journal ring is one named refusal, byte for byte on
# both (400; the ring is 1024 on both mirrors, and 2000 stays inside each
# chain's applied range — the range refusal would embed the chain's own
# cursor, which is a legitimate difference).
same_error /admin/rewind -X POST -H 'Content-Type: application/json' -d '{"blocks":2000}'
# A malformed shadow request is the extractor's 422 on both (axum's own body).
same /admin/shadow -X POST -H 'Content-Type: application/json' -d '{"clear":"not-a-bool"}'
# A deal lands on both with each side's own token; both are Circle's
# FiatTokenV2 layout, so the discovered slot must be the *same* mapping slot.
LAST_PATH=/admin/deal
t=$(fetch t -X POST -H 'Content-Type: application/json' -d "{\"token\":\"$USDC_T\",\"to\":\"$ACCT\",\"amount\":\"1\"}")
v=$(fetch v -X POST -H 'Content-Type: application/json' -d "{\"token\":\"$USDC_V\",\"to\":\"$ACCT\",\"amount\":\"1\"}")
slot_t=$(body t | python3 -c 'import json,sys; print(json.load(sys.stdin).get("slot",""))' 2>/dev/null || echo "")
slot_v=$(body v | python3 -c 'import json,sys; print(json.load(sys.stdin).get("slot",""))' 2>/dev/null || echo "")
if [ "$(body t | keys)" = "balance,ok,slot" ] && [ "$(body v | keys)" = "balance,ok,slot" ] && [ -n "$slot_t" ] && [ "$slot_t" = "$slot_v" ]; then
  ok "/admin/deal: both ok with the same discovered slot ($slot_t)"
else
  bad "/admin/deal: T=[$(body t)] V=[$(body v)]"
fi
# The failure path is symmetric too: a token that does not exist on the chain
# is a named probe-fault outcome, never a 5xx.
LAST_PATH=/admin/deal
t=$(fetch t -X POST -H 'Content-Type: application/json' -d "{\"token\":\"$USDC_V\",\"to\":\"$ACCT\",\"amount\":\"1\"}")
v=$(fetch v -X POST -H 'Content-Type: application/json' -d "{\"token\":\"$USDC_T\",\"to\":\"$ACCT\",\"amount\":\"1\"}")
if [ "$t" = "200" ] && [ "$v" = "200" ] \
  && [ "$(body t | keys)" = "$(body v | keys)" ] \
  && [ "$(body t | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ok"))')" = "False" ] \
  && [ "$(body v | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ok"))')" = "False" ]; then
  ok "/admin/deal: a foreign token is the same named refusal on both (keys[$(body t | keys)])"
else
  bad "/admin/deal (foreign token): T=[$(body t)] V=[$(body v)]"
fi

echo "-- the shared probe verb (run now, report the run)"
LAST_PATH=/admin/probes
t=$(fetch t -X POST -H 'Content-Type: application/json' -d '{}'); tb=$(body t)
v=$(fetch v -X POST -H 'Content-Type: application/json' -d '{}'); vb=$(body v)
if [ "$t" = "200" ] && [ "$v" = "200" ] \
  && [ "$(printf '%s' "$tb" | keys)" = "failed,ok,skipped" ] \
  && [ "$(printf '%s' "$vb" | keys)" = "failed,ok,skipped" ]; then
  ok "/admin/probes: both 200 {ok, failed, skipped}"
else
  bad "/admin/probes: T=$t V=$v (T=[$tb] V=[$vb])"
fi

echo "-- the shared gauges in /metrics"
LAST_PATH=/metrics
fetch t > /dev/null; fetch v > /dev/null
for gauge in mirror_shadowed_slots mirror_halted mirror_frozen; do
  has_t=$(grep -c "^$gauge" "/tmp/sym-t.out" || true)
  has_v=$(grep -c "^$gauge" "/tmp/sym-v.out" || true)
  if [ "$has_t" -ge 1 ] && [ "$has_v" -ge 1 ]; then ok "$gauge on both"; else bad "$gauge: T=$has_t V=$has_v"; fi
done

echo "-- the deliberate absences (documented asymmetries)"
LAST_PATH=/admin/time
t=$(fetch t -X POST -H 'Content-Type: application/json' -d '{"offset_seconds":0}'); v=$(fetch v -X POST -H 'Content-Type: application/json' -d '{"offset_seconds":0}')
if [ "$t" = "200" ] && [ "$v" = "404" ]; then ok "/admin/time: T-only (T=200, V=404)"; else bad "/admin/time: T=$t V=$v (want T=200 V=404)"; fi
LAST_PATH=/admin/feed
t=$(fetch t); v=$(fetch v)
if [ "$t" != "404" ] && [ "$v" = "404" ]; then ok "/admin/feed: T-only (T=$t, V=404)"; else bad "/admin/feed: T=$t V=$v (want T!=404 V=404)"; fi

echo "== engine symmetry: $pass passed, $fail failed"
[ "$fail" = "0" ]
