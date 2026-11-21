#!/usr/bin/env bash
# The order-lane proof's local stack: one locally booted venue simulator and
# its market plane, all on 127.0.0.1.
#
#   scripts/venue-sim-stack.sh up <dir>   # boot; writes <dir>/stack.env
#   scripts/venue-sim-stack.sh down <dir> # stop; removes the dir
#
# The stack is the real `diorama-venue-sim` bin (its poller, WS client,
# freshness gates, order lane and both API surfaces) pointed at a local
# Hyperliquid-shaped upstream (scripts/venue-upstream-mock.py) — no egress
# off the host. `up` waits for /readyz before it returns.
set -euo pipefail

CMD="${1:?usage: venue-sim-stack.sh up|down <dir>}"
DIR="${2:?usage: venue-sim-stack.sh up|down <dir>}"

# Fixed local ports (the fork's direct-mode convention; the upstream mock
# rides two more beside them).
INFO_PORT="${INFO_PORT:-18790}"
CTRL_PORT="${CTRL_PORT:-18791}"
UPSTREAM_REST="${UPSTREAM_REST:-127.0.0.1:18795}"
UPSTREAM_WS="${UPSTREAM_WS:-127.0.0.1:18796}"

repo_root() { cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd; }

bin() {
  if [ -n "${DIORAMA_VENUE_SIM_BIN:-}" ]; then
    echo "$DIORAMA_VENUE_SIM_BIN"
    return
  fi
  (cd "$(repo_root)" && cargo build -q -p diorama-venue-sim)
  echo "$(repo_root)/target/debug/diorama-venue-sim"
}

write_instance() { # write_instance <path>
  cat > "$1" <<EOF
# The order-lane proof's local instance: the real venue-sim wiring, every
# endpoint on 127.0.0.1 (the local upstream mock).
[instance]
name = "venue-order-proof"
namespace = "venue-order-proof"
profile = "venue"

[endpoints.anvil]
http = ["http://127.0.0.1:8545"]
chain_id = 999

[chains.venue]
kind = "hyperliquid"
source_urls = ["http://$UPSTREAM_REST"]
api_urls = ["http://$UPSTREAM_REST"]
ws_urls = ["ws://$UPSTREAM_WS"]

[services.engine]
state_dir = "$DIR/state"

[services.engine.loops]
chunk_size = 100
status_log_interval_ms = 60000

[services.venue_sim]
state_dir = "$DIR/state"
markets = ["ETH"]
poll_interval_ms = 500

[services.venue_sim.info]
addr = "127.0.0.1"
port = $INFO_PORT

[services.venue_sim.control]
addr = "127.0.0.1"
port = $CTRL_PORT
EOF
}

up() {
  mkdir -p "$DIR/state"
  local sim_bin
  sim_bin="$(bin)"
  write_instance "$DIR/instance.toml"

  python3 "$(repo_root)/scripts/venue-upstream-mock.py" "$UPSTREAM_REST" "$UPSTREAM_WS" \
    > "$DIR/upstream.log" 2>&1 &
  echo $! > "$DIR/upstream.pid"

  "$sim_bin" run --config "$DIR/instance.toml" > "$DIR/sim.log" 2>&1 &
  echo $! > "$DIR/sim.pid"

  cat > "$DIR/stack.env" <<EOF
export SIM_INFO="http://127.0.0.1:$INFO_PORT"
export SIM_CTRL="http://127.0.0.1:$CTRL_PORT"
EOF

  # The readiness wait: the market plane must be warm (meta, ctxs, book, the
  # spot registry) and the WS lane connected before a proof may start.
  local code=""
  for _ in $(seq 1 120); do
    code="$(curl -s -m 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$INFO_PORT/readyz" || true)"
    [ "$code" = "200" ] && break
    if ! kill -0 "$(cat "$DIR/sim.pid")" 2>/dev/null; then
      echo "venue-sim-stack: the simulator exited during boot:" >&2
      tail -20 "$DIR/sim.log" >&2
      exit 1
    fi
    sleep 1
  done
  if [ "$code" != "200" ]; then
    echo "venue-sim-stack: the simulator never went ready (last /readyz: $code)" >&2
    tail -20 "$DIR/sim.log" >&2
    exit 1
  fi
  echo "venue-sim-stack: ready (info :$INFO_PORT, control :$CTRL_PORT)"
}

down() {
  for pidfile in sim.pid upstream.pid; do
    if [ -f "$DIR/$pidfile" ]; then
      kill "$(cat "$DIR/$pidfile")" 2>/dev/null || true
      rm -f "$DIR/$pidfile"
    fi
  done
  rm -rf "$DIR"
}

case "${CMD}" in
  up) up ;;
  down) down ;;
  *) echo "usage: venue-sim-stack.sh up|down <dir>" >&2; exit 2 ;;
esac
