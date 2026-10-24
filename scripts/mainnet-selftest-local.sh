#!/usr/bin/env bash
# mainnet-selftest-local: the mainnet applier's BAL/trace rung proven natively
# (two anvils; no reth, no public RPC, no cluster).
#
# A local source anvil stands in for the parent chain: it serves the same
# envelopes and the prestateTracer (anvil's *block*-level trace answers an
# empty list, so the rung's per-tx fallback runs — the measured path). T forks
# it at a pin, and the engine's selftest applies the blocks above the pin,
# checking the sampled storage parity per block.
#
# Requires: anvil (ANVIL=/path/to/anvil), the engine binary built
# (`make mainnet-build`), python3, curl. Ports: 8588/8589 (override below).
set -euo pipefail
cd "$(dirname "$0")/.."

ANVIL="${ANVIL:-anvil}"
ENGINE="${ENGINE:-target/debug/mainnet-engine}"
SRC_PORT="${SRC_PORT:-8589}"
T_PORT="${T_PORT:-8588}"
DIR="${DIR:-/tmp/mainnet-selftest-local}"
SRC="http://127.0.0.1:$SRC_PORT"
T="http://127.0.0.1:$T_PORT"
A0=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
A1=0x70997970C51812dc3A010C7d01b50e0d17dc79C8
A2=0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC
CONTRACT=0x000000000000000000000000000000000000c0de

command -v "$ANVIL" >/dev/null || { echo "anvil not found (set ANVIL=)"; exit 2; }
[ -x "$ENGINE" ] || { echo "$ENGINE not built (make mainnet-build)"; exit 2; }

cleanup() {
  [ -n "${SRC_PID:-}" ] && kill "$SRC_PID" 2>/dev/null || true
  [ -n "${T_PID:-}" ] && kill "$T_PID" 2>/dev/null || true
}
trap cleanup EXIT
rm -rf "$DIR"; mkdir -p "$DIR/state/journal"

rpc() { curl -s -X POST -H 'content-type: application/json' --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}" "$1"; }
send() { rpc "$SRC" eth_sendTransaction "[{\"from\":\"$1\",\"to\":\"$2\",\"value\":\"$3\"}]" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"])'; }
block_of() { rpc "$SRC" eth_getTransactionReceipt "[\"$1\"]" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"]["blockNumber"],16))'; }

"$ANVIL" --port "$SRC_PORT" --chain-id 1 --block-time 1 --silent > "$DIR/source.log" 2>&1 &
SRC_PID=$!
sleep 1.5

# A storage-writing contract on the source (PUSH1 1 PUSH1 0 SSTORE STOP):
# the sampled parity needs a storage write to check (balances/nonces alone
# leave the sample empty).
rpc "$SRC" anvil_setCode "[\"$CONTRACT\",\"0x60016000556000\"]" >/dev/null
sleep 0.3

# Four txs, spaced so each lands in its own block (block-time 1).
TX1=$(send "$A0" "$A1" 0xde0b6b3a7640000)
sleep 1.2
TX2=$(send "$A1" "$A2" 0x1bc16d674ec80000)
sleep 1.2
TX3=$(send "$A2" "$A0" 0x16345785d8a0000)
sleep 1.2
TX4=$(send "$A0" "$CONTRACT" 0x0)
sleep 1.5

# The pin is the block before the first tx block; the run covers every tx.
B1=$(block_of "$TX1")
B4=$(block_of "$TX4")
PIN=$((B1 - 1))
NBLOCKS=$((B4 - PIN))
echo "tx blocks: $B1..$B4; pin: $PIN; blocks: $NBLOCKS"

"$ANVIL" --port "$T_PORT" --chain-id 1 --fork-url "$SRC" --fork-block-number "$PIN" --silent > "$DIR/t.log" 2>&1 &
T_PID=$!
sleep 2

cat > "$DIR/instance.toml" <<EOF
[instance]
name = "mainnet-selftest-local"
namespace = "mainnet-selftest-local"
profile = "mainnet"

[chains.l1]
kind = "ethereum"
source_url = "$SRC"
follow = "latest"
fork_offset = 128

[[watchlist]]
address = "$A0"
note = "anvil account 0"
[[watchlist]]
address = "$A1"
note = "anvil account 1"
[[watchlist]]
address = "$A2"
note = "anvil account 2"
[[watchlist]]
address = "$CONTRACT"
note = "the storage-writing contract"

[services.engine]
state_dir = "$DIR/state"
fixture_addresses = []

[services.engine.anvil]
target_url = "$T"

[services.engine.journal]
dir = "$DIR/state/journal"

[services.engine.verifier]
probes = []

[services.engine.http]
addr = "127.0.0.1"
port = 18084
EOF

"$ENGINE" selftest --rpc "$SRC" --anvil-t "$T" --config "$DIR/instance.toml" --blocks "$NBLOCKS" --timeout-secs 300
