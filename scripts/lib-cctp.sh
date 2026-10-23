#!/usr/bin/env bash
# Shared helpers for the cctp-* scripts (the bridge family: smoke, roundtrip,
# margin-cycle) — the third family, beside lib-mainnet.sh and lib-venue.sh.
# Source it, never run it:
#   source "$(dirname "$0")/lib-cctp.sh"
#
# One transport per mode, chosen once (the same convention as lib-venue.sh):
#   direct  (BRIDGE_CTRL_URL set): the scratch stack's URLs — BRIDGE_CTRL_URL,
#           BRIDGE_IRIS_URL, T_URL, V_URL, ENGINE_T_URL, ENGINE_V_URL,
#           SIM_INFO, SIM_CTRL — no kubectl;
#   cluster (default): the venue-mirror namespace, every call exec'd from the
#           cctp-bridge pod. From there the same-namespace services resolve by
#           name (anvil-v, engine, venue-sim) and T resolves cross-namespace
#           (anvil-t.mainnet-mirror.svc — the same value the venue's ConfigMap
#           carries as T_RPC_URL). The engines are driven through `cget`/
#           `cpost` with explicit URLs (the family must reach *two* engines);
#           lib-venue's `eget`/`epost` doors are inert here.
#
# lib-venue.sh (sourced below) owns the venue-shaped gates the family shares:
# the counters, `json`, the simulator doors (`sipost`/`spost`/`smetrics`), the
# served-identity check (`identity_ok`) and `flatten_coin`.
NS="${NS:-venue-mirror}"
CAST="${CAST:-cast}"

# The venue lib picks its mode from ENGINE_URL; the bridge family keys on
# BRIDGE_CTRL_URL, so point ENGINE_URL at V when it is otherwise unset (the
# family's venue-side doors are the simulator's, and its engine doors are
# `cget`/`cpost` — but the lib's mode must be chosen deterministically).
if [ -n "${BRIDGE_CTRL_URL:-}" ]; then
  ENGINE_URL="${ENGINE_URL:-${ENGINE_V_URL:-}}"
fi
source "$(dirname "$0")/lib-venue.sh"

if [ -n "${BRIDGE_CTRL_URL:-}" ]; then
  bget()  { curl -s --fail-with-body -m 30 "$BRIDGE_CTRL_URL$1"; }
  bpost() { curl -s --fail-with-body -m 120 -X POST "$BRIDGE_CTRL_URL$1" -H 'Content-Type: application/json' -d "$2"; }
  biget() { curl -s --fail-with-body -m 30 "${BRIDGE_IRIS_URL:?set BRIDGE_IRIS_URL in direct mode}$1"; }
  cget()  { curl -s --fail-with-body -m 60 "$2$1"; }
  cpost() { curl -s --fail-with-body -m 60 -X POST "$2$1" -H 'Content-Type: application/json' -d "$3"; }
  balance() { "$CAST" call "$1" 'balanceOf(address)(uint256)' "$2" --rpc-url "$3" 2>/dev/null | awk '{print $1}'; }
  rpcv()  { curl -s --fail-with-body -m 60 -X POST "$1" -H 'Content-Type: application/json' -d "$2"; }
  # Lazy on purpose: the smoke never touches a chain; the scripts that do
  # assert their endpoints at their own start (`require_chain_urls`).
  SRC_URL="${T_URL:-}"; DST_URL="${V_URL:-}"
  SRC_ENGINE="${ENGINE_T_URL:-}"; DST_ENGINE="${ENGINE_V_URL:-}"
else
  exec_pod() { kubectl -n "$NS" exec -i deploy/cctp-bridge -- "$@"; }
  bget()  { exec_pod curl -sf -m 30 "localhost:8096$1"; }
  bpost() { exec_pod curl -sf -m 120 -X POST "localhost:8096$1" -H 'Content-Type: application/json' -d "$2"; }
  biget() { exec_pod curl -sf -m 30 "localhost:8095$1"; }
  cget()  { exec_pod curl -sf -m 60 "$2$1"; }
  cpost() { exec_pod curl -sf -m 60 -X POST "$2$1" -H 'Content-Type: application/json' -d "$3"; }
  balance() { exec_pod "$CAST" call "$1" 'balanceOf(address)(uint256)' "$2" --rpc-url "$3" 2>/dev/null | awk '{print $1}'; }
  rpcv()  { exec_pod curl -sf -m 60 -X POST "$1" -H 'Content-Type: application/json' -d "$2"; }
  SRC_URL="http://anvil-t.mainnet-mirror.svc:8545"
  DST_URL="http://anvil-v:8545"
  SRC_ENGINE="http://mainnet-engine.mainnet-mirror.svc:8080"
  DST_ENGINE="http://engine:8080"
fi

# The bridge's own verbs, one home each. `AMOUNT` is the family's shared
# amount convention (raw base units); `BUDGET` the relay-wait budget.
burn() { # burn <source_domain> <destination_domain> <mint_recipient>
  bpost /control/transfer "{\"source\":$1,\"destination\":$2,\"amount\":\"$AMOUNT\",\"mint_recipient\":\"$3\"}"
}

# Wait for a bridge record to reach `relayed`; prints `relayed`, `parked: …`,
# or `timeout (state=…)` — the caller decides the verdict.
await_relay() { # await_relay <burn_tx>
  local state="" i=0 rec
  while [ "$i" -lt "${BUDGET:-180}" ]; do
    rec=$(bget /control/state | python3 -c "
import json, sys
d = json.load(sys.stdin)
for m in d.get('messages', []):
    if m.get('burn_tx', '').lower() == '$1'.lower():
        print(m.get('state', ''), '|', m.get('park_reason') or '')
        break
")
    state=${rec%% *}
    case "$state" in
      relayed) printf 'relayed'; return 0 ;;
      parked)  printf 'parked: %s' "${rec#*| }"; return 1 ;;
    esac
    i=$((i + 1))
    sleep 1
  done
  printf 'timeout (state=%s)' "${state:-none}"
  return 1
}

# The V-side chain helpers (V is the destination of the family's default leg;
# the margin cycle drives both directions through them).
mine() { rpcv "$DST_URL" '{"jsonrpc":"2.0","id":1,"method":"evm_mine","params":[]}' > /dev/null; }
receipt_status() { # receipt_status <tx> -> 0x1 | 0x0 | None
  rpcv "$DST_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$1\"]}" \
    | json 'd["result"]["status"] if d.get("result") else None'
}
send_tx() { # send_tx <from> <to> <data> (auto-impersonate + mine, V's convention)
  local tx
  tx=$(rpcv "$DST_URL" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$1\",\"to\":\"$2\",\"data\":\"$3\"}]}" | json 'd["result"]')
  mine
  printf '%s' "$tx"
}

# The chain-using scripts' one-line guard (direct mode supplies the URLs; the
# cluster branch above sets them from the pod's own DNS).
require_chain_urls() {
  : "${SRC_URL:?set T_URL in direct mode}"
  : "${DST_URL:?set V_URL in direct mode}"
  : "${SRC_ENGINE:?set ENGINE_T_URL in direct mode}"
  : "${DST_ENGINE:?set ENGINE_V_URL in direct mode}"
}
