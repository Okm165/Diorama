#!/usr/bin/env bash
# venue-selftest-local: the venue follow pipeline proven against the *real*
# HyperEVM read model (one anvil forked from the public RPC; no cluster).
#
# The other local proofs (venue-follow-local, daloc-local) use a scratch anvil
# as the source, so they cannot exercise what the real source serves: the
# system-tx envelope (`eth_getSystemTxsByBlockNumber` — a chain without it
# answers an empty list by design), the live latest-only reads, and real
# activity. This script forks V from the public RPC, watches the busiest
# recent sender (derived live — a fixed address would go quiet), and runs the
# engine's `selftest` subcommand: boot (assert, capability audit, reconcile,
# at-pin probes) then windows of compose -> COW -> land -> live parity. The
# mainnet counterpart is `mainnet-selftest-local.sh`.
#
# Requires: anvil (ANVIL=), the venue engine binary built (`make venue-build`),
# python3, curl, and outbound HTTPS to the public RPC (HL_EVM_RPC, default
# https://rpc.hyperliquid.xyz/evm). Port: 8796 (override with V_PORT).
set -euo pipefail
cd "$(dirname "$0")/.."

ANVIL="${ANVIL:-anvil}"
ENGINE="${ENGINE:-target/debug/venue-engine}"
RPC="${HL_EVM_RPC:-https://rpc.hyperliquid.xyz/evm}"
V_PORT="${V_PORT:-8796}"
DIR="${DIR:-/tmp/venue-selftest-local}"
V="http://127.0.0.1:$V_PORT"
USDC=0xb88339CB7199b77E23DB6E890353E22632Ba630f # the bridged USDC (chain 999)

command -v "$ANVIL" >/dev/null || { echo "anvil not found (set ANVIL=)"; exit 2; }
[ -x "$ENGINE" ] || { echo "$ENGINE not built (make venue-build)"; exit 2; }

cleanup() {
  [ -n "${V_PID:-}" ] && kill "$V_PID" 2>/dev/null || true
}
trap cleanup EXIT
rm -rf "$DIR"; mkdir -p "$DIR/state/journal"

# The watchlist is derived live: the busiest sender over the last 60 blocks
# (a hardcoded address goes quiet; the chain's senders rotate). The same
# python writes the fork pin (the head).
python3 - "$RPC" > "$DIR/target.txt" <<'PY'
import collections, json, sys, urllib.request

rpc_url = sys.argv[1]

def rpc(method, params):
    req = urllib.request.Request(
        rpc_url,
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read())["result"]

head = int(rpc("eth_blockNumber", []), 16)
senders = collections.Counter()
for n in range(head - 59, head + 1):
    for tx in rpc("eth_getBlockByNumber", [hex(n), True])["transactions"]:
        senders[tx["from"]] += 1
watch = senders.most_common(1)[0][0]
print(head)
print(watch)
print(f"busiest sender sent {senders[watch]} txs in the last 60 blocks", file=sys.stderr)
PY
HEAD=$(sed -n 1p "$DIR/target.txt")
WATCH=$(sed -n 2p "$DIR/target.txt")
echo "forking V at $HEAD, watching $WATCH"

"$ANVIL" --port "$V_PORT" --chain-id 999 --fork-url "$RPC" --fork-block-number "$HEAD" --silent > "$DIR/v.log" 2>&1 &
V_PID=$!
sleep 2

cat > "$DIR/instance.toml" <<EOF
[instance]
name = "venue-selftest-local"
namespace = "venue-selftest-local"
profile = "venue"

[chains.venue]
kind = "hyperliquid"
source_url = "$RPC"
api_url = "$RPC"
ws_url = "wss://api.hyperliquid.xyz/ws"

[[watchlist]]
address = "$WATCH"
note = "the busiest recent sender (derived live by this script)"

# The USDC balance closure (FiatTokenV2_2's `balanceAndBlacklistStates` at
# slot 9, verified live with the prestate tracer + the keccak derivation):
# the composer derives every Transfer participant's balance key, so the
# windows carry real balance changes and the closure itself is proven.
[[services.engine.closures.mappings]]
contract = "$USDC"
base_slot = "0x9"
topic_index = 1
[[services.engine.closures.mappings]]
contract = "$USDC"
base_slot = "0x9"
topic_index = 2

[services.engine]
state_dir = "$DIR/state"

[services.engine.anvil]
target_url = "$V"

[services.engine.journal]
dir = "$DIR/state/journal"

[[services.engine.verifier.probes]]
name = "usdc_total_supply"
to = "$USDC"
data = "0x18160ddd"

[services.engine.http]
addr = "127.0.0.1"
port = 18096
EOF

"$ENGINE" selftest --rpc "$RPC" --anvil-v "$V" --config "$DIR/instance.toml" --windows 5
echo "venue-selftest-local: PASSED (logs in $DIR)"
