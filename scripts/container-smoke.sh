#!/usr/bin/env bash
# container-smoke: the deployable mainnet-engine image, run for real — the
# container drives a host scratch stack (source anvil + T fork) over host
# networking, serves its HTTP surface, and applies blocks. The in-Docker
# counterpart of scripts/mainnet-engine-local.sh; the image build itself is
# the `docker-members` gate's other half (a missing COPY is a build failure
# here, minutes into podman — measured: config/defaults.toml was never
# copied, so every image failed to build).
set -euo pipefail
cd "$(dirname "$0")/.."

ANVIL="${ANVIL:-/tmp/opencode/anvil-186-bin/anvil}"
DIR=/tmp/opencode/container-smoke
SRC_PORT=19800; T_PORT=19802; HTTP_PORT=19880
A0=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266

cleanup() {
  [ -n "${CTR:-}" ] && podman rm -f "$CTR" >/dev/null 2>&1 || true
  for pid in ${PIDS:-}; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT
rm -rf "$DIR"; mkdir -p "$DIR/state/journal"

rpc() { curl -s -X POST -H 'content-type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}" "$1"; }

"$ANVIL" --port "$SRC_PORT" --chain-id 1 --block-time 1 --silent --hardfork amsterdam > "$DIR/src.log" 2>&1 &
PIDS="$!"
sleep 1.5
PIN=$(rpc "http://127.0.0.1:$SRC_PORT" eth_blockNumber '[]' | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))')
"$ANVIL" --port "$T_PORT" --chain-id 1 --fork-url "http://127.0.0.1:$SRC_PORT" --fork-block-number "$PIN" --silent > "$DIR/t.log" 2>&1 &
PIDS="$PIDS $!"
sleep 1.5

cat > "$DIR/t.toml" <<EOF
[instance]
name = "container-smoke"
namespace = "container-smoke"
profile = "mainnet"

[chains.l1]
kind = "ethereum"
source_url = "http://127.0.0.1:$SRC_PORT"
follow = "latest"
fork_offset = 128

[[watchlist]]
address = "$A0"
note = "the scratch sender"

[services.engine]
state_dir = "/data/state"
fixture_addresses = []

[services.engine.anvil]
target_url = "http://127.0.0.1:$T_PORT"

[services.engine.journal]
dir = "/data/state/journal"

[services.engine.verifier]
probes = []

[services.engine.http]
addr = "127.0.0.1"
port = $HTTP_PORT
EOF

podman run --rm --name mirror-container-smoke --network host -v "$DIR:/data:z" \
  localhost/mainnet-engine:v1 run --config /data/t.toml > "$DIR/container.log" 2>&1 &
CTR=mirror-container-smoke
for _ in $(seq 1 90); do curl -sf "http://127.0.0.1:$HTTP_PORT/livez" >/dev/null 2>&1 && break; sleep 1; done
if ! curl -sf "http://127.0.0.1:$HTTP_PORT/livez" >/dev/null; then
  echo "FAILED: the containerized engine did not come up"; tail -20 "$DIR/container.log"; exit 1
fi
echo "containerized engine up"

# /status: the shared fields; the applier must advance the cursor.
ok=0
for _ in $(seq 1 60); do
  c=$(curl -s "http://127.0.0.1:$HTTP_PORT/status" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("cursor_t") or 0)' 2>/dev/null || echo 0)
  [ "$c" -gt 0 ] && { ok=1; break; }
  sleep 1
done
if [ "$ok" = "1" ]; then
  echo "  ✔ the containerized applier is applying (cursor_t=$c)"
else
  echo "  ✘ the containerized applier did not advance"; tail -20 "$DIR/container.log"; exit 1
fi
curl -s "http://127.0.0.1:$HTTP_PORT/metrics" | grep -q "^mirror_blocks_applied_total " \
  && echo "  ✔ /metrics serves the shared + profile series" || { echo "  ✘ /metrics"; exit 1; }
echo "container-smoke: OK (log in $DIR/container.log)"
