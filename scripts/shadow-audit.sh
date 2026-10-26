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
#   F. the BAL accounting  - applied <= filtered <= bal_writes per block, and
#                            the fixture's skips are live (skipped_shadowed)
#   G. the field pair (V)  - a watched account's *fields* are planned every
#                            window: an unshadowed divergence is written back
#                            to the source's truth (the control), the same
#                            divergence survives once the fields are shadowed
#                            (the COW) — deterministic, unlike the deal's
#                            slot, which the parent rarely touches
#   H. the hand-back's scoping (T) - two marked keys: a partial clear hands
#                            back exactly one (the other stays held), and the
#                            `all` clear resets the account's whole claim
#   I. the scanned devnet write (V) - a *mined* devnet token transfer (not
#                            the register route) is marked by the scan
#                            (fresh storage slots) and held across windows
#   J. the rewind x devnet writes - the two journal scopes: T (frozen) keeps
#                            the deal and its claim across a rewind; V
#                            (journaled) restores the deal's slot and hands
#                            the claim back (unshadowed)
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
WINDOW_WAIT="${WINDOW_WAIT:-15}"
# The watched accounts that are *not* fixtures (the CCTP contracts + the
# CoreWriter): their fields are planned every window, so the pair below is
# deterministic. The pair rotates through them — a hand-back cannot clear a
# mark while V's source is pinned-latest (the guard, by design), so each
# fresh run takes the next unshadowed account; once all five are shadowed the
# pair runs its COW branch only (the marks are inert; a re-pin clears them).
WATCHED_POOL=(
  "0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d" # CCTP V2 TokenMessenger
  "0x81D40F21F12A8F0E3252Bccb954D722d4c464B64" # CCTP V2 MessageTransmitter
  "0xfd78EE919681417d192449715b2594ab58f5D002" # CCTP V2 TokenMinter
  "0xec546b6B005471ECf012e5aF77FBeC07e0FD8f78" # CCTP V2 Message
  "0x3333333333333333333333333333333333333333" # CoreWriter
)
FIELD_STAGE="${FIELD_STAGE:-0xDE0B6B3A7640000}"
FIELD_STAGE_DEC=$(python3 -c "print(int('${FIELD_STAGE}', 16))")

USDC_T="0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
WETH_T="0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
PROBE_ACCOUNT_B="${PROBE_ACCOUNT_B:-0x0000000000000000000000000000000000000a12}"
# The scanned-write leg's sender: anvil's dev key 0 (funded on the fork) and
# the value its mined tx carries (0.01 ETH).
SCAN_SENDER="${SCAN_SENDER:-0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266}"
SCAN_VALUE_WEI="${SCAN_VALUE_WEI:-10000000000000000}"
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

  # --- F. the BAL accounting: the counts cohere, the skips are live ---
  if [ "$ns" = "$NS_T" ]; then
    local lines res coh tot cnt
    lines=$(kubectl -n "$NS_T" logs deploy/engine --since=5m 2>/dev/null | grep "parent diffs applied" | tail -8 || true)
    res=$(echo "$lines" | python3 -c "
import json, sys
ok = True; total = 0; n = 0
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    f = json.loads(line)['fields']
    n += 1
    if not (f['applied'] <= f['filtered'] <= f['bal_writes']):
        ok = False
    total += f.get('skipped_shadowed', 0)
print(f'{1 if ok else 0} {total} {n}')")
    read -r coh tot cnt <<< "$res"
    echo "  BAL: ${cnt} blocks: applied <= filtered <= bal_writes; skipped_shadowed sum=${tot}"
    check "${label}: every block's accounting coheres (applied <= filtered <= bal_writes)" "$( [ "${coh:-0}" = "1" ] && [ "${cnt:-0}" -ge 3 ] && echo 1 || echo 0 )"
    check "${label}: the fixture's skips are live (skipped_shadowed sum ${tot} > 0)" "$( [ "${tot:-0}" -gt 0 ] && echo 1 || echo 0 )"
  else
    local lines res coh cnt
    lines=$(kubectl -n "$NS_V" logs deploy/engine --since=5m 2>/dev/null | grep "composed onto V" | tail -8 || true)
    res=$(echo "$lines" | python3 -c "
import json, sys
ok = True; n = 0
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    f = json.loads(line)['fields']
    n += 1
    if not (f['applied'] <= f['writes']):
        ok = False
print(f'{1 if ok else 0} {n}')")
    read -r coh cnt <<< "$res"
    echo "  BAL: ${cnt} windows: applied <= writes"
    check "${label}: every window's accounting coheres (applied <= writes)" "$( [ "${coh:-0}" = "1" ] && [ "${cnt:-0}" -ge 3 ] && echo 1 || echo 0 )"
  fi
}

# --- G. the watched-field pair (V): control, then COW ---
# The watched accounts' fields (balance/nonce/code) are planned every window,
# so both branches are deterministic. The deal's slot above is not: the
# parent rarely touches it, so its hold check is a regression guard, not a
# live skip. This pair is the live one: the unshadowed divergence is written
# back to the source's truth within a window; the same divergence survives
# once the fields are shadowed. The pair rotates through the non-fixture
# watched accounts (a mark cannot be cleared while V's source is
# pinned-latest) and re-stages the field to the source's own value at the
# end, so the marks stay inert.
field_shadow_pair() {
  local ns="$NS_V" anvil="http://anvil-v:8545" src="$SRC_V"
  echo
  local shadowed_list; shadowed_list=$(eget "$ns" /status | json "','.join(a.lower() for a in d.get('shadowed', []))")
  local watched="" a
  for a in "${WATCHED_POOL[@]}"; do
    if ! echo ",$shadowed_list," | grep -q ",${a,,},"; then watched="$a"; break; fi
  done
  local control=1
  if [ -z "$watched" ]; then
    watched="${WATCHED_POOL[0]}"
    control=0
    echo "== the watched-field pair (V): ${watched} (all pool accounts already shadowed; the COW branch only) =="
  else
    echo "== the watched-field pair (V): ${watched} =="
  fi
  local src_bal; src_bal=$(rpc_anvil "$ns" "$src" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBalance\",\"params\":[\"$watched\",\"latest\"]}" | json "int(d['result'],16)")
  rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setBalance\",\"params\":[\"$watched\",\"$FIELD_STAGE\"]}" > /dev/null
  local staged; staged=$(rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBalance\",\"params\":[\"$watched\",\"latest\"]}" | json "int(d['result'],16)")
  check "the field divergence staged (${src_bal} -> ${staged})" "$( [ "$staged" != "$src_bal" ] && echo 1 || echo 0 )"
  if [ "$control" = "1" ]; then
    echo "  waiting ${WINDOW_WAIT}s for a window to compose the watched fields..."
    sleep "$WINDOW_WAIT"
    local restored; restored=$(rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBalance\",\"params\":[\"$watched\",\"latest\"]}" | json "int(d['result'],16)")
    check "the control: the unshadowed divergence is written back to the source's truth (${staged} -> ${restored} = ${src_bal})" "$( [ "$restored" = "$src_bal" ] && echo 1 || echo 0 )"
  fi
  local accounts_before; accounts_before=$(eget "$ns" /status | json "d.get('shadowed_accounts')")
  rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setBalance\",\"params\":[\"$watched\",\"$FIELD_STAGE\"]}" > /dev/null
  epost "$ns" /admin/shadow "{\"address\":\"$watched\",\"account_fields\":true}" > /dev/null
  local accounts_after; accounts_after=$(eget "$ns" /status | json "d.get('shadowed_accounts')")
  if [ "$control" = "1" ]; then
    check "the field shadow registered (accounts ${accounts_before} -> ${accounts_after})" "$( [ "$accounts_after" -gt "$accounts_before" ] && echo 1 || echo 0 )"
  fi
  echo "  waiting ${WINDOW_WAIT}s for the windows with the shadowed fields..."
  sleep "$WINDOW_WAIT"
  local held; held=$(rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBalance\",\"params\":[\"$watched\",\"latest\"]}" | json "int(d['result'],16)")
  check "the COW: the same divergence survives the windows while shadowed (${held} = ${FIELD_STAGE_DEC})" "$( [ "$held" = "$FIELD_STAGE_DEC" ] && echo 1 || echo 0 )"
  local back; back=$(epost "$ns" /admin/shadow "{\"address\":\"$watched\",\"account_fields\":true,\"clear\":true}" || true)
  local after_back; after_back=$(rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBalance\",\"params\":[\"$watched\",\"latest\"]}" | json "int(d['result'],16)")
  if echo "$back" | grep -q "advanced past the pin"; then
    echo "  field hand-back refused: $(echo "$back" | head -c 120)"
    ok "the pinned-latest guard refuses the field hand-back (never poisons)"
    check "the refusal left the field untouched (${after_back} = ${FIELD_STAGE_DEC})" "$( [ "$after_back" = "$FIELD_STAGE_DEC" ] && echo 1 || echo 0 )"
  else
    check "the field hand-back restored the source's truth (${after_back} = ${src_bal})" "$( [ "$after_back" = "$src_bal" ] && echo 1 || echo 0 )"
  fi
  # The mark stays (the guard's documented state); re-stage the value to the
  # source's own so the deviation itself is gone — the mark stays inert.
  rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setBalance\",\"params\":[\"$watched\",\"0x$(printf '%x' "$src_bal")\"]}" > /dev/null
  echo "  the field re-staged to the source's own value (${src_bal}); the mark stays inert"
}

# --- H. the hand-back's scoping (T): a partial clear is precise, `all` resets ---
# The two deals land on a *second* fixture token (WETH) so the keys this
# section clears are its own. The partial clear must hand back exactly one
# key; the `all` clear is the operator's "reset this account to source
# truth" (every marked key of the account, nothing else).
hand_back_scoping() {
  local ns="$NS_T" anvil="http://anvil-t:8545" src="$SRC_T" token="$WETH_T"
  echo
  echo "== the hand-back's scoping (T): partial clear, then all (${token}) =="
  local d1 d2 s1 s2
  d1=$(epost "$ns" /admin/deal "{\"token\":\"$token\",\"to\":\"$PROBE_ACCOUNT\",\"amount\":\"$DEAL_AMOUNT\"}")
  s1=$(echo "$d1" | json "d.get('slot') or ''")
  d2=$(epost "$ns" /admin/deal "{\"token\":\"$token\",\"to\":\"$PROBE_ACCOUNT_B\",\"amount\":\"$DEAL_AMOUNT\"}")
  s2=$(echo "$d2" | json "d.get('slot') or ''")
  check "two deals marked two distinct slots" "$( [ -n "$s1" ] && [ -n "$s2" ] && [ "$s1" != "$s2" ] && echo 1 || echo 0 )"
  local b1 b2 src_b
  b1=$(balance_of "$ns" "$anvil" "$token" "$PROBE_ACCOUNT")
  b2=$(balance_of "$ns" "$anvil" "$token" "$PROBE_ACCOUNT_B")
  src_b=$(balance_of_source "$ns" "$src" "$token" "$PROBE_ACCOUNT")
  check "the anvil carries both deals (${b1}, ${b2}; source ${src_b})" "$( [ "$b1" = "$DEAL_AMOUNT" ] && [ "$b2" = "$DEAL_AMOUNT" ] && echo 1 || echo 0 )"
  local back resynced b1b b2b
  back=$(epost "$ns" /admin/shadow "{\"address\":\"$token\",\"slots\":[\"$s1\"],\"clear\":true}")
  resynced=$(echo "$back" | json "d.get('resynced_keys')")
  b1b=$(balance_of "$ns" "$anvil" "$token" "$PROBE_ACCOUNT")
  b2b=$(balance_of "$ns" "$anvil" "$token" "$PROBE_ACCOUNT_B")
  check "the partial clear handed back exactly one key (resynced=${resynced})" "$( [ "${resynced:-0}" = "1" ] && echo 1 || echo 0 )"
  check "the cleared key restored to the source's truth (${b1b} = ${src_b})" "$( [ "$b1b" = "$src_b" ] && echo 1 || echo 0 )"
  check "the other key stayed held (${b2b} = ${DEAL_AMOUNT})" "$( [ "$b2b" = "$DEAL_AMOUNT" ] && echo 1 || echo 0 )"
  local all all_resynced b2c src_b2
  all=$(epost "$ns" /admin/shadow "{\"address\":\"$token\",\"clear\":true,\"all\":true}")
  all_resynced=$(echo "$all" | json "d.get('resynced_keys')")
  b2c=$(balance_of "$ns" "$anvil" "$token" "$PROBE_ACCOUNT_B")
  src_b2=$(balance_of_source "$ns" "$src" "$token" "$PROBE_ACCOUNT_B")
  check "the all-clear reset the rest (resynced=${all_resynced}, ${b2c} = ${src_b2})" "$( [ "${all_resynced:-0}" -ge 1 ] && [ "$b2c" = "$src_b2" ] && echo 1 || echo 0 )"
}

# --- I. the scanned devnet write (V): a mined tx is marked by the scan ---
# The register route is the cure for *cheatcode* writes; a *mined* devnet tx
# is the scan's job (every sender's fields, the recipients of value
# transfers, the traces' storage writes). The watched non-fixture accounts
# are contracts that reject plain value (no `receive`), so the staging is a
# mined *token transfer* to a fresh recipient (the head's hex — new every
# run), and the scan must mark its fresh storage slots with no register
# involved. The trader is topped up through the deal route, so the leg
# re-runs forever.
scanned_write() {
  local ns="$NS_V" anvil="http://anvil-v:8545"
  local holder="0x00000000000000000000000000000000000000e5"   # the roundtrip's trader
  local amount="1000000"                                     # 1 USDC (6 decimals)
  local head to
  head=$(eget "$ns" /status | json "d.get('head')")
  to=$(printf '0x%040x' "$head")
  echo
  echo "== the scanned devnet write (V): a mined transfer ${holder} -> ${to} =="
  local held_usdc; held_usdc=$(balance_of "$ns" "$anvil" "$USDC_V" "$holder")
  if [ "${held_usdc:-0}" -lt "$amount" ]; then
    epost "$ns" /admin/deal "{\"token\":\"$USDC_V\",\"to\":\"$holder\",\"amount\":\"$((amount * 2))\"}" > /dev/null
    echo "  the trader topped up through the deal route (${held_usdc} -> $(balance_of "$ns" "$anvil" "$USDC_V" "$holder"))"
  fi
  rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setBalance\",\"params\":[\"$holder\",\"0xDE0B6B3A7640000\"]}" > /dev/null
  local slots_before; slots_before=$(eget "$ns" /status | json "d.get('shadowed_slots')")
  local bal_before; bal_before=$(balance_of "$ns" "$anvil" "$USDC_V" "$to")
  local data="0xa9059cbb000000000000000000000000${to:2}$(printf '%064x' "$amount")"
  rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_impersonateAccount\",\"params\":[\"$holder\"]}" > /dev/null
  local tx; tx=$(rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$holder\",\"to\":\"$USDC_V\",\"data\":\"$data\"}]}" | json "d.get('result') or ''")
  rpc_anvil "$ns" "$anvil" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_stopImpersonatingAccount\",\"params\":[\"$holder\"]}" > /dev/null
  check "the devnet tx submitted (${tx:0:12}…)" "$( [ -n "$tx" ] && echo 1 || echo 0 )"
  echo "  waiting ${WINDOW_WAIT}s for the producer to mine and the scan to mark..."
  sleep "$WINDOW_WAIT"
  local slots_after; slots_after=$(eget "$ns" /status | json "d.get('shadowed_slots')")
  check "the scan marked the transfer's fresh storage slots (${slots_before} -> ${slots_after})" "$( [ "$slots_after" -gt "$slots_before" ] && echo 1 || echo 0 )"
  local marked; marked=$(kubectl -n "$ns" logs deploy/engine --since=2m 2>/dev/null | grep "scanned devnet block for shadow writes" | tail -1 | python3 -c "import json,sys; s=sys.stdin.read().strip(); print(json.loads(s)['fields']['shadowed_writes'] if s else 0)" 2>/dev/null || echo 0)
  check "the scan's own line names the marks (shadowed_writes=${marked} >= 1)" "$( [ "${marked:-0}" -ge 1 ] && echo 1 || echo 0 )"
  local bal_after; bal_after=$(balance_of "$ns" "$anvil" "$USDC_V" "$to")
  check "the mined transfer landed (${bal_before} -> ${bal_after} = +${amount})" "$( [ "$((bal_after - bal_before))" = "$amount" ] && echo 1 || echo 0 )"
  echo "  waiting 10s for the follow's windows with the marked slots..."
  sleep 10
  local bal_held; bal_held=$(balance_of "$ns" "$anvil" "$USDC_V" "$to")
  check "the follow held the mined transfer (${bal_held} = ${bal_after})" "$( [ "$bal_held" = "$bal_after" ] && echo 1 || echo 0 )"
  echo "  (the marks stay: the fixture owns the token account; the slots are the scan's record)"
}

# --- J. the rewind × devnet writes: the two journal scopes ---
# The mainnet's journal holds the *applier's* parent before-values (its own
# writes are frozen — `JournalScope::ParentBeforeValues`): a deal survives a
# rewind and keeps its claim. The venue's journal holds its *own* writes
# (`OwnWrites`): a rewind restores the deal's slot and hands the claim back
# — kept, the source's future updates to it would be skipped forever. Both
# legs are the documented asymmetry, asserted live.
rewind_hand_back() {
  local ns="$NS_T" anvil="http://anvil-t:8545" src="$SRC_T" token="$USDC_T"
  echo
  echo "== the rewind × devnet writes: T frozen, V journaled =="
  local c0 slots_before
  c0=$(eget "$ns" /status | json "d.get('cursor')")
  slots_before=$(eget "$ns" /status | json "d.get('shadowed_slots')")
  local d
  d=$(epost "$ns" /admin/deal "{\"token\":\"$token\",\"to\":\"$PROBE_ACCOUNT\",\"amount\":\"$DEAL_AMOUNT\"}")
  local c1 blocks rw unshadowed restored
  c1=$(eget "$ns" /status | json "d.get('cursor')")
  blocks=$(( c1 - c0 + 1 ))
  rw=$(epost "$ns" /admin/rewind "{\"blocks\":$blocks}")
  unshadowed=$(echo "$rw" | json "d.get('unshadowed')")
  restored=$(echo "$rw" | json "d.get('restored_values')")
  local bal
  bal=$(balance_of "$ns" "$anvil" "$token" "$PROBE_ACCOUNT")
  check "T (frozen): the rewind restored parent values (${restored}) and handed nothing back (unshadowed=${unshadowed})" "$( [ "${restored:-0}" -ge 1 ] && [ "${unshadowed:-0}" = "0" ] && echo 1 || echo 0 )"
  check "T (frozen): the deal survived the rewind (${bal} = ${DEAL_AMOUNT})" "$( [ "$bal" = "$DEAL_AMOUNT" ] && echo 1 || echo 0 )"
  local slots_after; slots_after=$(eget "$ns" /status | json "d.get('shadowed_slots')")
  check "T (frozen): the deal's claim stayed (${slots_before} -> ${slots_after})" "$( [ "$slots_after" -gt "$slots_before" ] && echo 1 || echo 0 )"
  # The precise clear (the deal's own slot): the `all` form would reset every
  # marked key of the token to the source — including balances other suites
  # rely on (measured: it zeroed the bridge's wallet, which the roundtrip
  # needs). `all` is the deliberate "reset this account to source truth".
  local slot_back resynced bal2 src_bal
  slot_back=$(echo "$d" | json "d.get('slot') or ''")
  local back
  back=$(epost "$ns" /admin/shadow "{\"address\":\"$token\",\"slots\":[\"$slot_back\"],\"clear\":true}")
  resynced=$(echo "$back" | json "d.get('resynced_keys')")
  bal2=$(balance_of "$ns" "$anvil" "$token" "$PROBE_ACCOUNT")
  src_bal=$(balance_of_source "$ns" "$src" "$token" "$PROBE_ACCOUNT")
  check "T (frozen): the route handed the claim back (resynced=${resynced}, ${bal2} = ${src_bal})" "$( [ "${resynced:-0}" -ge 1 ] && [ "$bal2" = "$src_bal" ] && echo 1 || echo 0 )"
  echo "  waiting for the re-apply to converge the range..."
  local waited=0 recovered=0
  while [ "$waited" -lt 90 ]; do
    local last_failed cur
    last_failed=$(metric "$ns" mirror_t_convergence_last_failed)
    cur=$(eget "$ns" /status | json "d.get('cursor')")
    if [ "${last_failed:-1}" = "0" ] && [ "${cur:-0}" -ge "$c1" ]; then recovered=1; break; fi
    sleep 5; waited=$((waited+5))
  done
  check "T: the re-apply converged the range (cursor >= ${c1}, no failed parity)" "$( [ "$recovered" = "1" ] && echo 1 || echo 0 )"

  local ns_v="$NS_V" anvil_v="http://anvil-v:8545" token_v="$USDC_V"
  local vc0 v_slots_before v_bal_before
  vc0=$(eget "$ns_v" /status | json "d.get('cursor')")
  v_slots_before=$(eget "$ns_v" /status | json "d.get('shadowed_slots')")
  v_bal_before=$(balance_of "$ns_v" "$anvil_v" "$token_v" "$PROBE_ACCOUNT_B")
  local dv dealt_v
  dv=$(epost "$ns_v" /admin/deal "{\"token\":\"$token_v\",\"to\":\"$PROBE_ACCOUNT_B\",\"amount\":\"$DEAL_AMOUNT\"}")
  dealt_v=$(echo "$dv" | json "d.get('ok', False)")
  check "V (journaled): the deal landed (${v_bal_before} -> ${DEAL_AMOUNT})" "$( [ "$dealt_v" = "True" ] && echo 1 || echo 0 )"
  local vc1 vblocks rwv v_unshadowed v_restored
  vc1=$(eget "$ns_v" /status | json "d.get('cursor')")
  vblocks=$(( vc1 - vc0 + 1 ))
  rwv=$(epost "$ns_v" /admin/rewind "{\"blocks\":$vblocks}")
  v_unshadowed=$(echo "$rwv" | json "d.get('unshadowed')")
  v_restored=$(echo "$rwv" | json "d.get('restored_values')")
  check "V (journaled): the rewind restored the deal's slot and handed its claim back (unshadowed=${v_unshadowed}, restored=${v_restored})" "$( [ "${v_unshadowed:-0}" -ge 1 ] && [ "${v_restored:-0}" -ge 1 ] && echo 1 || echo 0 )"
  local v_bal_after; v_bal_after=$(balance_of "$ns_v" "$anvil_v" "$token_v" "$PROBE_ACCOUNT_B")
  check "V (journaled): the deal was undone (${v_bal_after} = ${v_bal_before})" "$( [ "$v_bal_after" = "$v_bal_before" ] && echo 1 || echo 0 )"
  local v_slots_after; v_slots_after=$(eget "$ns_v" /status | json "d.get('shadowed_slots')")
  check "V (journaled): the slot's mark dropped (${v_slots_after} = ${v_slots_before})" "$( [ "$v_slots_after" = "$v_slots_before" ] && echo 1 || echo 0 )"
}

audit_mirror "$NS_V" "venue (V)" "http://anvil-v:8545" "$SRC_V" "$USDC_V"
audit_mirror "$NS_T" "l1 (T)" "http://anvil-t:8545" "$SRC_T" "$USDC_T"
field_shadow_pair
hand_back_scoping
scanned_write
rewind_hand_back

echo
echo "  shadowing audit: ${pass} ok, ${fail} failed"
[ "$fail" -eq 0 ]
