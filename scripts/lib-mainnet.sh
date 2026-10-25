#!/usr/bin/env bash
# Shared helpers for the mirror's test scripts. Source it, never run it:
#   source "$(dirname "${BASH_SOURCE[0]}")/lib-mainnet.sh"
#
# The conventions here are measured, not stylistic:
#   - `rpc` runs through the engine pod: it has curl+jq, the anvil pods do not.
#   - `castoff` (offline calldata/index) must NOT get --rpc-url: cast rejects the
#     flag for those subcommands, the error goes to /dev/null and the tx ships
#     empty calldata (measured: a 21000-gas "supply" that reverted - a plain
#     transfer).
#   - devnet sends carry an explicit gas: anvil's auto-fill estimates at
#     submission, and a tx that lands while the applier is writing parent diffs
#     would execute against a different state (measured: a borrow estimated ~5%
#     short reverted). The engine's prepend loop makes that rare; a devnet client
#     should still send with a buffer.
#   - receipts are polled, not awaited client-side: the engine mines one T block
#     per applied parent block (~12 s), longer while catching up.

NS=${NS:-mainnet-mirror}
K="kubectl -n $NS"
ENGINE="$K exec deploy/engine --"
ANVIL_T="$K exec deploy/anvil-t --"
source "$(dirname "${BASH_SOURCE[0]}")/lib-common.sh"
num() { printf '%d' "${1:-0}" 2>/dev/null || echo 0; }

# Overrides lib-common's plain rpc: the cluster's runs inside the pod.
rpc() { # rpc <url> <method> [params-json]
  local url=$1 method=$2 params=${3:-[]}
  $ENGINE curl -s --fail-with-body -m 25 "$url" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" 2>/dev/null
}
engine_api() { $ENGINE curl -s --fail-with-body -m 20 "localhost:8080$1" 2>/dev/null; }
castoff() { $ANVIL_T cast "$@" 2>/dev/null | tr -d '\r'; }
castT() { $ANVIL_T cast "$@" --rpc-url http://localhost:8545 2>/dev/null | tr -d '\r'; }
castR() { $ANVIL_T cast "$@" --rpc-url http://reth:8545 2>/dev/null | tr -d '\r'; }

send_tx() { # send_tx <from> <to> <calldata> -> hash | error text
  rpc http://anvil-t:8545 eth_sendTransaction \
    "[{\"from\":\"$1\",\"to\":\"$2\",\"data\":\"$3\",\"gasPrice\":\"0x174876e800\",\"gas\":\"0x1e8480\"}]" | jq -r '.result // .error.message'
}
wait_tx() { # wait_tx <hash> [tries] -> status (0x1/0x0) or "pending"
  local tries=${2:-45}
  for _ in $(seq 1 "$tries"); do
    local st
    st=$(rpc http://anvil-t:8545 eth_getTransactionReceipt "[\"$1\"]" | jq -r '.result.status // "pending"' 2>/dev/null)
    [ "$st" != "pending" ] && { echo "$st"; return 0; }
    sleep 4
  done
  echo "pending"
}
sim() { # sim <from> <to> <calldata> -> "ok" or the revert reason (simulate-then-send)
  rpc http://anvil-t:8545 eth_call "[{\"from\":\"$1\",\"to\":\"$2\",\"data\":\"$3\"},\"latest\"]" \
    | jq -r '.error.message // "ok"'
}
send_ok() { # send_ok <from> <to> <calldata> -> hash, or empty on reject/revert
  local h st r
  r=$(sim "$1" "$2" "$3")
  [ "$r" != "ok" ] && info "simulate first: $r" >&2
  h=$(send_tx "$1" "$2" "$3")
  case "$h" in 0x*) : ;; *) info "send rejected: $h" >&2; return 1 ;; esac
  st=$(wait_tx "$h" 45)
  [ "$st" = "0x1" ] || { info "send reverted ($st): $h" >&2; return 1; }
  echo "$h"
}
