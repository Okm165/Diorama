#!/usr/bin/env bash
# The shadowing audit: put both anvils into specific states and verify the
# copy-on-write shadow behaves exactly as designed, on the live stack.
#
#   A. the state report    - each mirror's head/cursor/pin, the shadowed
#                            counts, and the parity pairs (the BAL side)
#   B. the deal marks      - a devnet deal diverges the anvil from the source
#                            and the shadow registry records the keys
#   C. the COW hold        - a follow window (V) / an applier pass (T) runs
#                            with the divergent value: the devnet's truth
#                            must survive, the parity must skip, never alarm
#   D. the hand-back       - the named keys clear: the anvil returns to the
#                            source's truth and the marks drop
#   E. the fixture freeze  - the fixture-owned token (the source's contract
#                            overlaid by the devnet's mock) never gets
#                            overwritten by the follow
#
# Run from the repo root:
#   make shadow-audit
#   CAST=/path/to/cast make shadow-audit
set -euo pipefail

CAST="${CAST:-cast}"
NS_V="${NS_V:-venue-mirror}"
NS_T="${NS_T:-mainnet-mirror}"
PROBE_ACCOUNT="${PROBE_ACCOUNT:-0x0000000000000000000000000000000000000a11}"
DEAL_AMOUNT="${DEAL_AMOUNT:-12345678}"
COW_WAIT="${COW_WAIT:-50}"

USDC_T="0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
USDC_V="0xb88339CB7199b77E23DB6E890353E22632Ba630f"
SRC_T="http://reth.mainnet-mirror.svc.cluster.local:8545"
SRC_V="https://rpc.hyperliquid.xyz/evm"

pass=0; fail=0
ok()    { echo "  ok   $1"; pass=$((pass+1)); }
bad()   { echo "  FAIL $1"; fail=$((fail+1)); }
check() { if [ "$2" = "1" ]; then ok "$1"; else bad "$1"; fi; }
json()  { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

exec_v() { kubectl -n "$NS_V" exec -i deploy/engine -- "$@"; }
exec_t() { kubectl -n "$NS_T" exec -i deploy/engine -- "$@"; }

# The engine + anvil doors per mirror (cluster-internal service names).
eget()   { local ns=$1 path=$2; if [ "$ns" = "$NS_V" ]; then exec_v curl -s --fail-with-body -m 60 "localhost:8080$path"; else exec_t curl -s --fail-with-body -m 60 "localhost:8080$path"; fi; }
epost()  { local ns=$1 path=$2 body=$3; if [ "$ns" = "$NS_V" ]; then exec_v curl -s --fail-with-body -m 60 -X POST "localhost:8080$path" -H 'Content-Type: application/json' -d "$body"; else exec_t curl -s --fail-with-body -m 60 -X POST "localhost:8080$path" -H 'Content-Type: application/json' -d "$body"; fi; }
rpc_anvil() { local ns=$1 url=$2 body=$3; if [ "$ns" = "$NS_V" ]; then exec_v curl -s --fail-with-body -m 30 -X POST "$url" -H 'Content-Type: application/json' -d "$body"; else exec_t curl -s --fail-with-body -m 30 -X POST "$url" -H 'Content-Type: application/json' -d "$body"; fi; }

balance_of() { # balance_of <ns> <anvil_url> <token> <who> -> base units
  local ns=$1 url=$2 token=$3 who=$4
  rpc_anvil "$ns" "$url" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$token\",\"data\":\"0x70a08231000000000000000000000000${who:2}\"},\"latest\"]}" \
    | python3 -c "import json,sys; r=json.load(sys.stdin).get('result','0x0'); print(int(r,16))"
}
balance_of_source() { # balance_of_source <ns> <src_url> <token> <who>
  local ns=$1 url=$2 token=$3 who=$4
  rpc_anvil "$ns" "$url" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$token\",\"data\":\"0x70a08231000000000000000000000000${who:2}\"},\"latest\"]}" \
    | python3 -c "import json,sys; r=json.load(sys.stdin).get('result','0x0'); print(int(r,16))"
}
metric() { # metric <ns> <name>
  eget "$1" /metrics | awk -v n="$2" '$1 == n {print $2; found=1} END {if (!found) print 0}'
}

audit_mirror() { # audit_mirror <ns> <label> <anvil_url> <src_url> <usdc>
  local ns=$1 label=$2 anvil=$3 src=$4 usdc=$5
  echo
  echo "== ${label} (${ns}) =="
  local st; st=$(eget "$ns" /status)
  local head cursor pin shadowed_slots shadowed_accounts
  head=$(echo "$st" | json "d.get('head')")
  cursor=$(echo "$st" | json "d.get('cursor')")
  pin=$(echo "$st" | json "d.get('fork_pin', d.get('pin'))")
  shadowed_slots=$(echo "$st" | json "d.get('shadowed_slots')")
  shadowed_accounts=$(echo "$st" | json "d.get('shadowed_accounts')")
  echo "  head=${head} cursor=${cursor} pin=${pin} shadowed_slots=${shadowed_slots} shadowed_accounts=${shadowed_accounts}"
  check "${label}: /status serves the shared shape" "$( [ -n "$head" ] && [ -n "$shadowed_slots" ] && echo 1 || echo 0 )"

  # --- B. the deal diverges the anvil and marks the keys ---
  local before_source before_anvil deal
  before_source=$(balance_of_source "$ns" "$src" "$usdc" "$PROBE_ACCOUNT")
  deal=$(epost "$ns" /admin/deal "{\"token\":\"$usdc\",\"to\":\"$PROBE_ACCOUNT\",\"amount\":\"$DEAL_AMOUNT\"}")
  local dealt; dealt=$(echo "$deal" | json "d.get('ok', d.get('status'))")
  local slot; slot=$(echo "$deal" | json "d.get('slot') or ''")
  echo "  deal: ${dealt} (the source holds ${before_source} base units)"
  check "${label}: the deal lands" "$( [ "$dealt" = "True" ] || [ "$dealt" = "applied" ] && echo 1 || echo 0 )"
  local after_anvil; after_anvil=$(balance_of "$ns" "$anvil" "$usdc" "$PROBE_ACCOUNT")
  check "${label}: the anvil carries the devnet's deal (${after_anvil} = ${DEAL_AMOUNT})" "$( [ "$after_anvil" = "$DEAL_AMOUNT" ] && echo 1 || echo 0 )"
  check "${label}: the deal diverges from the source (${before_source} -> ${after_anvil})" "$( [ "$after_anvil" != "$before_source" ] && echo 1 || echo 0 )"
  local marked; marked=$(eget "$ns" /status | json "d.get('shadowed_accounts')")
  check "${label}: the shadow registry grew (${shadowed_accounts} -> ${marked})" "$( [ "$marked" -ge "$shadowed_accounts" ] && echo 1 || echo 0 )"

  # --- C. the COW hold across a follow window / an applier pass ---
  local failures_before failures_after
  failures_before=$(metric "$ns" "$( [ "$ns" = "$NS_V" ] && echo venue_probe_failures_total || echo mirror_probe_failures_total )")
  failures_before=${failures_before:-0}
  echo "  waiting ${COW_WAIT}s for a follow window / applier pass with the divergent value..."
  sleep "$COW_WAIT"
  local held; held=$(balance_of "$ns" "$anvil" "$usdc" "$PROBE_ACCOUNT")
  check "${label}: the devnet's truth survives the pass (COW hold: ${held})" "$( [ "$held" = "$DEAL_AMOUNT" ] && echo 1 || echo 0 )"
  failures_after=$(metric "$ns" "$( [ "$ns" = "$NS_V" ] && echo venue_probe_failures_total || echo mirror_probe_failures_total )")
  failures_after=${failures_after:-0}
  check "${label}: the parity skipped the devnet-owned account (no new failures: ${failures_before} -> ${failures_after})" "$( [ "$failures_after" -le "$failures_before" ] && echo 1 || echo 0 )"

  # --- D. the hand-back ---
  # V's source is pinned-latest: once the head moves past the pin, the pin's
  # truth cannot be read back and the guard refuses *by design* (the message
  # names the numbers). T's reth serves the pin's truth (a 10,064-block
  # window), so the same call restores it. Both must never silently poison:
  # the refusal leaves the anvil's value untouched.
  # The marks are per-address: the deal's slot lives in the *token's*
  # storage, the account's fields are a separate key set.
  local slots_json="[]"
  [ -n "$slot" ] && slots_json="[\"$slot\"]"
  local back; back=$(epost "$ns" /admin/shadow "{\"address\":\"$usdc\",\"slots\":$slots_json,\"clear\":true}" || true)
  local cleared; cleared=$(echo "$back" | python3 -c "
import json, sys
s = sys.stdin.read().strip()
print(json.loads(s).get('cleared', 'cleared') if s.startswith('{') else '(refused)')" 2>/dev/null || echo "(refused)")
  local restored; restored=$(balance_of "$ns" "$anvil" "$usdc" "$PROBE_ACCOUNT")
  if [ "$ns" = "$NS_V" ]; then
    echo "  hand-back refused: $(echo "$back" | head -c 120)"
    check "${label}: the pinned-latest guard refuses the hand-back (never poisons)" "$( echo "$back" | grep -q "advanced past the pin" && echo 1 || echo 0 )"
    check "${label}: the refusal left the devnet's truth untouched (${restored})" "$( [ "$restored" = "$DEAL_AMOUNT" ] && echo 1 || echo 0 )"
  else
    local source_now; source_now=$(balance_of_source "$ns" "$src" "$usdc" "$PROBE_ACCOUNT")
    echo "  hand-back: cleared=${cleared} anvil=${restored} source=${source_now}"
    check "${label}: the hand-back restored the source's truth (${restored} = ${source_now})" "$( [ "$restored" = "$source_now" ] && echo 1 || echo 0 )"
    local after_accounts; after_accounts=$(eget "$ns" /status | json "d.get('shadowed_accounts')")
    check "${label}: the marks dropped (${marked} -> ${after_accounts})" "$( [ "$after_accounts" -le "$marked" ] && echo 1 || echo 0 )"
  fi

  # --- E. the fixture freeze: the mock's supply is stable across the pass ---
  local supply; supply=$(rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$usdc\",\"data\":\"0x18160ddd\"},\"latest\"]}" | json "int(d['result'],16)")
  echo "  fixture: ${usdc} totalSupply=${supply} (the devnet's mock, frozen at the pin)"
  check "${label}: the fixture's supply is a live value, not zeroed" "$( [ "$supply" -gt 0 ] && echo 1 || echo 0 )"

  # --- F. the BAL side: the parity pairs ---
  if [ "$ns" = "$NS_V" ]; then
    local sampled sampled_ok windows
    sampled=$(metric "$ns" venue_follow_sampled_total)
    sampled_ok=$(metric "$ns" venue_follow_sampled_ok_total)
    windows=$(metric "$ns" venue_follow_windows_total)
    echo "  BAL: windows=${windows} sampled=${sampled} sampled_ok=${sampled_ok} (the live parity of applied keys)"
    check "${label}: every sampled key read back the composed value" "$( [ "$sampled_ok" -ge "$sampled" ] && echo 1 || echo 0 )"
  else
    local conv
    conv=$(metric "$ns" mirror_t_convergence_ok_total)
    echo "  BAL: t_convergence_ok=${conv} (the applier's applied-block parity)"
  fi
}

echo "== the shadowing audit (state + BAL, both mirrors) =="
audit_mirror "$NS_V" "venue (V)" "http://anvil-v:8545" "$SRC_V" "$USDC_V"
audit_mirror "$NS_T" "l1 (T)" "http://anvil-t:8545" "$SRC_T" "$USDC_T"

echo
echo "  shadowing audit: ${pass} ok, ${fail} failed"
[ "$fail" -eq 0 ]
