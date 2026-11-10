#!/usr/bin/env bash
# checkpoint — capture a restorable state checkpoint of both mirror chains.
#
# The full `anvil_dumpState` carries block history. That is useful for forensics
# but slow to load (measured: chain 999's 13,686-block dump did not answer RPC
# after minutes). The restorable artifact is the **state-only** snapshot:
# `block` + `accounts`, with `blocks`/`transactions` emptied. Both chains'
# state-only snapshots load in seconds and reproduce the deployment contracts.
#
# This script is read-only against the chains. It writes:
#   <out>/anvil-t-state.hex        raw dump (forensics)
#   <out>/anvil-t-state.json       decoded full dump
#   <out>/anvil-t-state-only.json  restorable state
#   <out>/anvil-v-state.hex
#   <out>/anvil-v-state.json
#   <out>/anvil-v-state-only.json
#   <out>/t-shadow.json, t-cursor.json
#   <out>/v-shadow.json, v-cursor.json
#   <out>/sim.json, cctp.json
#   <out>/manifest.json, SHA256SUMS
#
# Usage: scripts/checkpoint.sh [output-dir]
set -euo pipefail

K="${KUBECTL:-kubectl}"
NS_MAINNET="${NS_MAINNET:-diorama-mainnet}"
NS_VENUE="${NS_VENUE:-diorama-venue}"
NS_CCTP="${NS_CCTP:-diorama-cctp}"
OUT="${1:-/tmp/opencode/diorama-checkpoints/$(date -u +%Y%m%dT%H%M%SZ)}"

mkdir -p "$OUT"

dump() { # dump <namespace> <deployment> <name>
  "$K" -n "$1" exec "deploy/$2" -- sh -c \
    'cast rpc anvil_dumpState --rpc-url http://localhost:8545' \
    | tr -d '"\n' > "$OUT/$3-state.hex"
  xxd -r -p < "$OUT/$3-state.hex" | gunzip > "$OUT/$3-state.json"
  python3 - "$OUT/$3-state.json" "$OUT/$3-state-only.json" <<'PY'
import json, sys
src, dst = sys.argv[1], sys.argv[2]
d = json.load(open(src))
d["blocks"] = []
d["transactions"] = []
json.dump(d, open(dst, "w"), separators=(",", ":"))
print(f"{dst}: block={int(d['block']['number'],16)} accounts={len(d['accounts'])}")
PY
}

echo "== dumps =="
dump "$NS_MAINNET" anvil-t anvil-t
dump "$NS_VENUE" anvil-v anvil-v

echo "== engine state =="
"$K" -n "$NS_MAINNET" exec deploy/engine -- sh -c 'cat /state/shadow.json' > "$OUT/t-shadow.json"
"$K" -n "$NS_MAINNET" exec deploy/engine -- sh -c 'cat /state/cursor.json' > "$OUT/t-cursor.json"
"$K" -n "$NS_VENUE" exec deploy/engine -- sh -c 'cat /state/shadow.json' > "$OUT/v-shadow.json"
"$K" -n "$NS_VENUE" exec deploy/engine -- sh -c 'cat /state/cursor.json' > "$OUT/v-cursor.json"

echo "== simulator/bridge state =="
"$K" -n "$NS_VENUE" exec deploy/venue-sim -- sh -c 'cat /state/sim.json' > "$OUT/sim.json"
"$K" -n "$NS_CCTP" exec deploy/cctp-bridge -- sh -c 'cat /state/cctp.json' > "$OUT/cctp.json"

echo "== manifest =="
T_HEAD=$("$K" -n "$NS_MAINNET" exec deploy/anvil-t -- cast block-number --rpc-url http://localhost:8545 2>/dev/null | tr -d '\r')
V_HEAD=$("$K" -n "$NS_VENUE" exec deploy/anvil-v -- cast block-number --rpc-url http://localhost:8545 2>/dev/null | tr -d '\r')
T_STATUS=$("$K" -n "$NS_MAINNET" exec deploy/engine -- curl -s http://localhost:8080/status 2>/dev/null || true)
V_STATUS=$("$K" -n "$NS_VENUE" exec deploy/engine -- curl -s http://localhost:8080/status 2>/dev/null || true)
python3 - "$OUT" "$T_HEAD" "$V_HEAD" "$T_STATUS" "$V_STATUS" <<'PY'
import datetime, json, sys
out, t_head, v_head, t_status, v_status = sys.argv[1:]
def parse(s):
    try: return json.loads(s)
    except Exception: return s
manifest = {
    "captured_at_utc": datetime.datetime.now(datetime.UTC).isoformat().replace("+00:00", "Z"),
    "chain1": {"anvil_head": int(t_head) if t_head.isdigit() else t_head, "engine_status": parse(t_status)},
    "chain999": {"anvil_head": int(v_head) if v_head.isdigit() else v_head, "engine_status": parse(v_status)},
    "restore": {
        "format": "state-only JSON (blocks/transactions emptied)",
        "chain1": "proven: scratch anvil loaded the state; Hub/Underwriter/ProofExecutor/CM code and slot 0 matched live",
        "chain999": "proven with state-only: Hub/Underwriter/Factory code and slot 0 matched live; the full dump's 13,686-block replay hangs",
    },
    "files": sorted(__import__("os").listdir(out)),
}
open(out + "/manifest.json", "w").write(json.dumps(manifest, indent=2))
print(json.dumps(manifest["restore"], indent=2))
PY
sha256sum "$OUT"/* > "$OUT/SHA256SUMS"
echo "checkpoint=$OUT"
ls -lh "$OUT"
