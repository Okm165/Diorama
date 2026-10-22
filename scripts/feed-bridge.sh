#!/usr/bin/env bash
# Feed-bridge helpers (plan §7.1): list / pause / resume / override the live
# feeds that the engine publishes to the devnet's mock aggregators.
#
# Run from the repo root:  make feed-bridge ARGS="list"
# The bridge lives in the *mainnet* engine (NS=mainnet-mirror by default); the
# commands exec from the engine pod so they talk to the cluster-internal admin
# API. Usage:
#   feed-bridge list
#   feed-bridge pause  ETH/USD
#   feed-bridge resume ETH/USD
#   feed-bridge override ETH/USD 150000000000      # raw aggregator answer
set -euo pipefail

NS="${NS:-mainnet-mirror}"
cmd="${1:-list}"

run() { kubectl -n "$NS" exec -i deploy/engine -- "$@"; }

api() { # api <path> [body]
  local path="$1" body="${2:-}"
  if [ -n "$body" ]; then
    run curl -sf -m 20 -X POST "localhost:8080$path" \
      -H 'Content-Type: application/json' -d "$body"
  else
    run curl -sf -m 20 "localhost:8080$path"
  fi
}

case "$cmd" in
  list)
    api /admin/feed | run python3 -m json.tool
    ;;
  pause)
    name="${2:?usage: feed-bridge pause <feed>}"
    api /admin/feed/pause "{\"name\":\"$name\"}"
    ;;
  resume)
    name="${2:?usage: feed-bridge resume <feed>}"
    api /admin/feed/resume "{\"name\":\"$name\"}"
    ;;
  override)
    name="${2:?usage: feed-bridge override <feed> <raw-answer>}"
    answer="${3:?usage: feed-bridge override <feed> <raw-answer>}"
    api /admin/feed/override "{\"name\":\"$name\",\"answer\":$answer}"
    ;;
  *)
    echo "usage: feed-bridge {list|pause <feed>|resume <feed>|override <feed> <raw-answer>}" >&2
    exit 2
    ;;
esac
echo
