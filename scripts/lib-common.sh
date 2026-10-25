#!/usr/bin/env bash
# lib-common: the proof scripts' shared furniture — the pass/fail ledger, the
# summary line (with the failure exit the runners read), and the plain
# JSON-RPC call. The cluster libs (lib-mainnet/lib-venue/lib-cctp) source this
# and override `rpc` with their in-pod variant; the standalone local proofs
# source it directly.
pass=0; fail=0
ok()   { echo "  ✔ $*"; pass=$((pass + 1)); }
bad()  { echo "  ✘ $*"; fail=$((fail + 1)); }
info() { echo "  · $*"; }

# The summary every proof prints, with its failure exit as the last command.
summary() { echo "$1: $pass ok, $fail bad (logs in $2)"; [ "$fail" = "0" ]; }

rpc() { # rpc <url> <method> [params-json]
  curl -s -X POST -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":${3:-[]}}" "$1"
}
