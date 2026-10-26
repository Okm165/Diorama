#!/usr/bin/env bash
# The CCTP bridge smoke (the plan's `cctp-*` scripts; the counterpart of
# `make venue-smoke`): the bridge answers on both ports, the queue is
# coherent with the store, the metric set is present, and the declared chains
# are the two the instance names.
#
# Run from the repo root:
#   make cctp-smoke                          # against the deployed diorama-venue
#   BRIDGE_CTRL_URL=http://127.0.0.1:8096 BRIDGE_IRIS_URL=http://127.0.0.1:8095 \
#     ./scripts/cctp-smoke.sh                # direct mode: a scratch stack
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib-cctp.sh"

echo "== CCTP bridge smoke =="

biget /healthz >/dev/null 2>&1 \
  && ok "the Iris port answers /healthz" \
  || bad "the Iris port is silent (/healthz)"

health=$(bget /health)
status=$(echo "$health" | json 'd.get("status")')
domains=$(echo "$health" | json '",".join(str(c["domain"]) for c in d.get("chains", []))')
parked=$(echo "$health" | json 'd.get("parked")')
[ -n "$status" ] && [ "$status" != "None" ] \
  && ok "control /health answers (status=$status, parked=$parked)" \
  || bad "control /health did not answer a status"
[ "$domains" = "0,19" ] \
  && ok "both chains declared, domains 0 and 19" \
  || bad "declared chains are [$domains], expected [0,19]"

metrics=$(bget /metrics)
for series in cctp_messages_seen_total cctp_messages_pending cctp_messages_signed cctp_messages_parked cctp_relay_balance_wei; do
  echo "$metrics" | grep -q "^$series" \
    && ok "metric present: $series" \
    || bad "metric missing: $series"
done

state=$(bget /control/state)
records=$(echo "$state" | json 'len(d.get("messages", []))')
[ -n "$records" ] && [ "$records" != "None" ] \
  && ok "the store answers ($records records)" \
  || bad "the store did not answer"
# The cross-surface invariant: /health's counts are the store's states.
python3 - "$health" "$state" <<'PY' \
  && ok "health counts == store states (pending/signed/relayed/parked)" \
  || bad "health counts disagree with the store"
import json, sys
health, state = json.loads(sys.argv[1]), json.loads(sys.argv[2])
want = {s: sum(1 for m in state.get("messages", []) if m.get("state") == s)
        for s in ("pending", "signed", "relayed", "parked")}
got = {s: health.get(s) for s in want}
raise SystemExit(0 if want == got else f"{want} != {got}")
PY
states=$(echo "$state" | json '", ".join(f"{k}={v}" for k, v in sorted({m["state"]: sum(1 for x in d["messages"] if x["state"] == m["state"]) for m in d["messages"]}.items()))')
info "queue: ${states:-empty}"

echo "== cctp smoke: $pass passed, $fail failed"
[ "$fail" = "0" ]
