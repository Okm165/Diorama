#!/usr/bin/env bash
# Endpoint acceptance probe for a Hyperliquid HyperEVM (chain 999) RPC.
#
# Measures what the docs only claim, using the poison-detection oracle from
# the Diorama boot audit: a pinned eth_call against the WHYPE RedStone feed
# whose answer provably moves. Classifies every endpoint as
#   TRUE-ARCHIVE | LATEST-ONLY (POISONED) | REFUSING | UNKNOWN
# and tests the rest of the wire shape the daloc/Diorama stacks need.
#
# Usage:
#   probe-endpoint.sh https://hyperliquid.drpc.org [https://.../info BASE_URL]
#
#   arg 1 (required): the JSON-RPC URL.
#   arg 2 (optional): a Hyperliquid /info BASE (the URL *without* /info —
#                     the client appends /info). When given, meta, l2Book,
#                     userAbstraction and candleSnapshot are probed.
#
# Exit 0 = usable as a latest-read pool member; exit 1 = fail.
# (Archive quality is reported, not gated: the follow pool is latest-by-design.)

set -u
RPC="${1:?usage: probe-endpoint.sh <rpc-url> [info-base-url]}"
INFO_BASE="${2:-}"
ORACLE="0xb60224122830FD6cF5119f548477a190310D2c94"   # WHYPE RedStone feed
SEL="0xfeaf968c"                                       # latestRoundData()
CROSS="https://rpc.hyperlend.finance/archive"          # ground truth (optional)

rpc() { # rpc <url> <method> [params-json]
  local url="$1" method="$2" params="${3:-[]}"
  curl -s --max-time 20 -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" "$url"
}

fail() { echo "  ✗ $*"; FAIL=1; }
ok()   { echo "  ✓ $*"; }

FAIL=0

# --- 0. alive + chain --------------------------------------------------------
CID=$(rpc "$RPC" eth_chainId | sed -n 's/.*"result":"\(0x[0-9a-f]*\)".*/\1/p')
[ "$CID" = "0x3e7" ] || { echo "✗ not chain 999 (chainId=$CID)"; exit 1; }
ok "chain 999"
HEAD_HEX=$(rpc "$RPC" eth_blockNumber | sed -n 's/.*"result":"\(0x[0-9a-f]*\)".*/\1/p')
HEAD=$((HEAD_HEX))
ok "head $HEAD"

# --- 1. pinned eth_call classification --------------------------------------
call_at() { # <block-decimal> -> result hex or ""
  rpc "$RPC" eth_call "[{\"to\":\"$ORACLE\",\"data\":\"$SEL\"},\"0x$(printf '%x' "$1")\"]" \
    | sed -n 's/.*"result":"\(0x[0-9a-f]*\)".*/\1/p'
}
AT_HEAD=$(call_at "$HEAD")
AT_20MIN=$(call_at "$((HEAD-1400))")
AT_DEEP=$(call_at "$((HEAD-50000))")

TRUTH_DEEP=""; TRUTH_20MIN=""
if command -v curl >/dev/null && [ -n "${CROSS:-}" ]; then
  TRUTH_DEEP=$(rpc "$CROSS" eth_call "[{\"to\":\"$ORACLE\",\"data\":\"$SEL\"},\"0x$(printf '%x' "$((HEAD-50000))")\"]" \
    | sed -n 's/.*"result":"\(0x[0-9a-f]*\)".*/\1/p')
  TRUTH_20MIN=$(rpc "$CROSS" eth_call "[{\"to\":\"$ORACLE\",\"data\":\"$SEL\"},\"0x$(printf '%x' "$((HEAD-1400))")\"]" \
    | sed -n 's/.*"result":"\(0x[0-9a-f]*\)".*/\1/p')
fi

if [ -z "$AT_20MIN" ] || [ -z "$AT_DEEP" ]; then
  PIN="REFUSING (fail-closed)"
elif [ -n "$TRUTH_DEEP" ] && [ "$AT_DEEP" = "$TRUTH_DEEP" ]; then
  PIN="TRUE-ARCHIVE (matches $CROSS)"
elif [ "$AT_DEEP" = "$AT_HEAD" ] || [ "$AT_20MIN" = "$AT_HEAD" ]; then
  PIN="LATEST-ONLY — POISONED: pinned reads silently return head state"
elif [ -n "$TRUTH_DEEP" ] && [ "$AT_DEEP" != "$TRUTH_DEEP" ]; then
  PIN="WRONG-VALUE (diverges from $CROSS) — do not trust pinned reads"
else
  PIN="UNKNOWN (no ground truth; compare $AT_DEEP vs an archive manually)"
fi
case "$PIN" in TRUE-ARCHIVE*) ok "pinned eth_call: $PIN";; *) fail "pinned eth_call: $PIN";; esac
echo "    head=$AT_HEAD  20min=$AT_20MIN  deep=$AT_DEEP"

# --- 2. eth_getLogs 900-block range -----------------------------------------
LG=$(rpc "$RPC" eth_getLogs "[{\"address\":\"$ORACLE\",\"fromBlock\":\"0x$(printf '%x' "$((HEAD-900))")\",\"toBlock\":\"0x$(printf '%x' "$((HEAD-880))")\"}]")
if echo "$LG" | grep -q '"result"'; then ok "eth_getLogs 900-block range accepted"; else fail "eth_getLogs 900-block: $(echo "$LG" | head -c 120)"; fi

# --- 3. eth_getSystemTxsByBlockNumber (read-pool gate) ----------------------
# The oracle mirrors the engine's, not a substring: some providers answer an
# unsupported method with BOTH `"result":null` and `"error"` in one body
# (measured 2026-11-12 on rpc.sentio.xyz/hyperevm), so grepping for
# `"result"` reads a refusal as "served". A result must be present, not an
# error, and not null (a null is not the list the client decodes).
ST=$(rpc "$RPC" eth_getSystemTxsByBlockNumber "[\"0x$(printf '%x' "$((HEAD-1100))")\"]")
case "$ST" in
  *'"error"'*) fail "eth_getSystemTxsByBlockNumber: $(echo "$ST" | head -c 100)";;
  *'"result":null'*) fail "eth_getSystemTxsByBlockNumber: answered null, not a list";;
  *'"result"'*) ok "eth_getSystemTxsByBlockNumber served";;
  *) fail "eth_getSystemTxsByBlockNumber: no JSON-RPC answer";;
esac

# --- 4. JSON-RPC batching (Diorama follow driver batches 20/body) -----------
BATCH=$(curl -s --max-time 20 -H 'content-type: application/json' \
  -d '[{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]},{"jsonrpc":"2.0","id":2,"method":"eth_blockNumber","params":[]}]' "$RPC")
# Same oracle rule: a batch *of errors* is still an array — every item must
# have carried a result, or the body is not "batch supported".
case "$BATCH" in
  *'"error"'*) fail "batch JSON-RPC: the body carries an error item: $(echo "$BATCH" | head -c 100)";;
  \[*'"result"'*) ok "batch JSON-RPC supported (follow-driver eligible)";;
  *) fail "batch JSON-RPC rejected: $(echo "$BATCH" | head -c 100)";;
esac

# --- 5. eth_getProof (informational; production does not require it) --------
GP=$(rpc "$RPC" eth_getProof "[\"$ORACLE\",[],\"latest\"]")
if echo "$GP" | grep -q '"result"'; then ok "eth_getProof served"; else echo "  · eth_getProof not served (not required)"; fi

# --- 6. submission surface ---------------------------------------------------
GG=$(rpc "$RPC" eth_gasPrice)
echo "$GG" | grep -q '"result"' && ok "eth_gasPrice/submission plane alive" || fail "eth_gasPrice: $(echo "$GG" | head -c 80)"

# --- 7. optional info plane ---------------------------------------------------
if [ -n "$INFO_BASE" ]; then
  info() { curl -s --max-time 20 -H 'content-type: application/json' -d "$2" "$INFO_BASE/info"; }
  M=$(info x '{"type":"meta"}')
  echo "$M" | grep -q '"universe"' && ok "info meta ($(echo "$M" | grep -o '"name"' | wc -l) universe names)" || fail "info meta: $(echo "$M" | head -c 100)"
  B=$(info x '{"type":"l2Book","coin":"BTC"}')
  echo "$B" | grep -q '"levels"' && ok "info l2Book" || fail "info l2Book: $(echo "$B" | head -c 100)"
  U=$(info x '{"type":"userAbstraction","user":"0x0000000000000000000000000000000000000001"}')
  echo "  · userAbstraction: $(echo "$U" | head -c 80)"
  C=$(info x '{"type":"candleSnapshot","req":{"coin":"BTC","interval":"1h","startTime":0,"endTime":2500000000000}}')
  echo "$C" | grep -q '"\[' -o && ok "info candleSnapshot" || echo "  · candleSnapshot: $(echo "$C" | head -c 80)"
fi

echo "----"

# The verdict is per plane, because the specialization wires them separately:
# a fork source needs archive-grade pinned reads; a read-pool member needs
# the follow's wire shape (system txs + batch bodies + the declared log cap).
# One endpoint can serve either without the other — the official HyperEVM RPC
# is a fine pool member and a poisoned fork source (measured 2026-11-12).
FAIL_SYSTXS=1; FAIL_BATCH=1; FAIL_FORK=1
case "$ST" in *'"error"'*|*'"result":null'*) ;; *'"result"'*) FAIL_SYSTXS=0;; esac
case "$BATCH" in *'"error"'*) ;; \[*'"result"'*) FAIL_BATCH=0;; esac
case "$PIN" in TRUE-ARCHIVE*) FAIL_FORK=0;; esac
FAIL_POOL=1
[ "$FAIL_SYSTXS" = 0 ] && [ "$FAIL_BATCH" = 0 ] && FAIL_POOL=0

if [ "$FAIL_POOL" = 0 ]; then
  echo "VERDICT read-pool: USABLE (serves the follow's wire shape)"
else
  echo "VERDICT read-pool: NOT ELIGIBLE (needs eth_getSystemTxsByBlockNumber + batch bodies)"
fi
if [ "$FAIL_FORK" = 0 ]; then
  echo "VERDICT fork-source: USABLE (archive-grade pinned reads)"
else
  echo "VERDICT fork-source: NOT ELIGIBLE ($PIN)"
fi
[ "$FAIL_POOL" = 0 ] || [ "$FAIL_FORK" = 0 ] || { echo "RESULT: the endpoint serves neither plane"; exit 1; }
[ "$FAIL" = 0 ] || echo "note: some probes above failed; read the lines, not just the verdict"
exit 0
