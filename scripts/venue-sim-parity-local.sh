#!/usr/bin/env bash
# venue-sim-parity-local: the simulator's served answers vs the real venue API,
# request by request. One anvil forked from the real HyperEVM (the sim's
# bridge watches it, as in production), the sim binary, and a comparison
# against https://api.hyperliquid.xyz/info:
#
#   - every served info type's recursive key/type signature must equal the
#     real answer's (the wire is the venue's, not a lookalike);
#   - the static views carry the same content: meta's universe names and
#     spotMeta's tokens are identical sets;
#   - the book is live: the sim's ETH mid is within 50 bps of the real one;
#   - `marginTable` id 1 equals the venue's implicit default (measured live
#     2026-10-25: {"description":"","marginTiers":[{"lowerBound":"0.0",
#     "maxLeverage":1}]} — the metadata never lists id 1, the API serves it);
#   - a *touched* account (after a control deal) is answered from the sim with
#     the same shape the venue serves, and `userAbstraction` agrees.
#
# Requires: anvil (ANVIL=), the venue binaries built (`make venue-build`),
# python3, curl, outbound HTTPS to the real HyperEVM and to the real API.
# Ports: 8794 (V) + 18086/18087 (info/control) + 18088 (the engine section's
# admin port, unused here but required by the profile) — override with the
# *_PORT variables. KEEP_UP=1 leaves the stack up (PIDs in $DIR/pids).
set -euo pipefail
cd "$(dirname "$0")/.."

ANVIL="${ANVIL:-anvil}"
HL_RPC="${HL_RPC:-https://rpc.hyperliquid.xyz/evm}"
REAL_API="${REAL_API:-https://api.hyperliquid.xyz}"
V_PORT="${V_PORT:-8794}"
SIM_INFO_PORT="${SIM_INFO_PORT:-18086}"
SIM_CTRL_PORT="${SIM_CTRL_PORT:-18087}"
ENGINE_PORT="${ENGINE_PORT:-18088}"
DIR="${DIR:-/tmp/venue-sim-parity-local}"
TOUCHED=0x00000000000000000000000000000000000000ea

V_URL="http://127.0.0.1:$V_PORT"
command -v "$ANVIL" >/dev/null || { echo "anvil not found (set ANVIL=)"; exit 2; }
[ -x target/debug/venue-sim ] || { echo "target/debug/venue-sim not built (make venue-build)"; exit 2; }

cleanup() {
  for pid in ${PIDS:-}; do kill "$pid" 2>/dev/null || true; done
}
if [ "${KEEP_UP:-0}" = 1 ]; then
  trap 'printf "%s\n" ${PIDS:-} > "$DIR/pids"' EXIT
else
  trap cleanup EXIT
fi
rm -rf "$DIR"; mkdir -p "$DIR/state/journal"

head_of() {
  local out
  for _ in $(seq 1 6); do
    out=$(curl -s -m 30 -X POST -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' "$1" \
      | python3 -c 'import json,sys
try:
    print(int(json.load(sys.stdin)["result"], 16))
except Exception:
    sys.exit(1)' 2>/dev/null || true)
    [ -n "$out" ] && { printf '%s' "$out"; return 0; }
    sleep 3
  done
  return 1
}

echo "== venue-sim-parity-local: forking V from $HL_RPC"
HEAD=$(head_of "$HL_RPC") || { echo "the HyperEVM RPC did not answer ($HL_RPC)"; exit 1; }
PIN=$((HEAD - 8))
"$ANVIL" --port "$V_PORT" --chain-id 999 --fork-url "$HL_RPC" --fork-block-number "$PIN" \
  --auto-impersonate --silent > "$DIR/v.log" 2>&1 &
PIDS="$!"
for _ in $(seq 1 60); do
  curl -sf -m 2 -X POST -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' "$V_URL" >/dev/null 2>&1 && break
  sleep 1
done
curl -sf -m 2 -X POST -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' "$V_URL" >/dev/null \
  || { echo "V did not come up"; tail -5 "$DIR/v.log"; exit 1; }
echo "   V up (the real HyperEVM at $PIN)"

cat > "$DIR/instance.toml" <<EOF
[instance]
name = "venue-sim-parity-local"
namespace = "venue-sim-parity-local"
profile = "venue"

[chains.venue]
kind = "hyperliquid"
source_url = "$V_URL"
api_url = "$REAL_API"
ws_url = "wss://api.hyperliquid.xyz/ws"

[[watchlist]]
address = "$TOUCHED"
note = "the comparison's touched account"

[services.engine]
state_dir = "$DIR/state/engine"

[services.engine.anvil]
target_url = "$V_URL"

[services.engine.journal]
dir = "$DIR/state/journal"

[services.engine.verifier]
probes = []

[services.engine.http]
addr = "127.0.0.1"
port = $ENGINE_PORT

[services.venue_sim]
state_dir = "$DIR/state/sim"
markets = ["BTC", "ETH", "HYPE", "SOL"]
anvil_url = "$V_URL"

[services.venue_sim.info]
addr = "127.0.0.1"
port = $SIM_INFO_PORT

[services.venue_sim.control]
addr = "127.0.0.1"
port = $SIM_CTRL_PORT
EOF
target/debug/venue-sim run --config "$DIR/instance.toml" > "$DIR/sim.log" 2>&1 &
PIDS="$PIDS $!"
ok=""
for _ in $(seq 1 300); do
  curl -sf "http://127.0.0.1:$SIM_INFO_PORT/readyz" >/dev/null 2>&1 && { ok=yes; break; }
  sleep 1
done
[ -n "$ok" ] || { echo "the sim not ready"; tail -20 "$DIR/sim.log"; exit 1; }
echo "   sim ready (live market data)"

# Touch one account (a spot deal): the sim then answers it from its own state
# — the COW rule the real venue's "unconfigured accounts" contract composes
# with.
curl -sf -X POST -H 'content-type: application/json' \
  --data "{\"address\":\"$TOUCHED\",\"class\":\"spot\",\"amount\":\"250\"}" \
  "http://127.0.0.1:$SIM_CTRL_PORT/control/deal" > /dev/null
echo "   the touched account dealt (spot 250)"

python3 - "http://127.0.0.1:$SIM_INFO_PORT/info" "$REAL_API/info" "$TOUCHED" <<'PY'
import json, sys, urllib.request

sim_url, real_url, touched = sys.argv[1], sys.argv[2], sys.argv[3]

def post(url, body):
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.loads(r.read())

def same_shape(a, b):
    """The wire-shape equality: dicts by key set (recursively), lists by
    element shape when both sides carry elements (JSON cannot tell an empty
    list's element type), scalars by type."""
    if isinstance(a, dict) and isinstance(b, dict):
        if set(a) != set(b):
            return False
        return all(same_shape(a[k], b[k]) for k in a)
    if isinstance(a, list) and isinstance(b, list):
        if not a or not b:
            return True
        return same_shape(a[0], b[0])
    return type(a) is type(b)

def first_diff(path, a, b):
    if isinstance(a, dict) and isinstance(b, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a:
                return f"{path}.{k}: only the real answer has it"
            if k not in b:
                return f"{path}.{k}: only the sim answer has it"
            d = first_diff(f"{path}.{k}", a[k], b[k])
            if d:
                return d
        return None
    if isinstance(a, list) and isinstance(b, list):
        if not a or not b:
            return None
        return first_diff(f"{path}[0]", a[0], b[0])
    if type(a) is not type(b):
        return f"{path}: {type(a).__name__} vs {type(b).__name__}"
    return None

def missing_from_sim(real, sim, path="$"):
    """Venue keys the sim's answer lacks — the hard failures."""
    out = []
    if isinstance(real, dict) and isinstance(sim, dict):
        for k in real:
            if k not in sim:
                out.append(f"{path}.{k}")
            else:
                out += missing_from_sim(real[k], sim[k], f"{path}.{k}")
    elif isinstance(real, list) and isinstance(sim, list) and real and sim:
        out += missing_from_sim(real[0], sim[0], f"{path}[0]")
    return out

def extra_in_sim(real, sim, path="$"):
    """Sim keys the venue's answer lacks. On a *funded* sim account these are
    the venue's own state-dependent optional fields (e.g. `leverage.rawUsd`,
    measured present with margin state, absent on an empty account) — the
    crate's served-field-names test pins the vocabulary; here they are
    reported, never failed."""
    out = []
    if isinstance(real, dict) and isinstance(sim, dict):
        for k in sim:
            if k not in real:
                out.append(f"{path}.{k}")
            else:
                out += extra_in_sim(real[k], sim[k], f"{path}.{k}")
    elif isinstance(real, list) and isinstance(sim, list) and real and sim:
        out += extra_in_sim(real[0], sim[0], f"{path}[0]")
    return out

def intersect(a, b):
    """The two answers' shared surface (dicts by common keys, lists by their
    first elements): the account views' shape comparison."""
    if isinstance(a, dict) and isinstance(b, dict):
        return {k: intersect(a[k], b[k]) for k in a if k in b}
    if isinstance(a, list) and isinstance(b, list) and a and b:
        return [intersect(a[0], b[0])]
    return a

MARKET_REQS = [
    {"type": "meta"},
    {"type": "metaAndAssetCtxs"},
    {"type": "spotMeta"},
    {"type": "allMids"},
    {"type": "l2Book", "coin": "ETH"},
    {"type": "exchangeStatus"},
    {"type": "perpDexs"},
    {"type": "marginTable", "id": 1},
    {"type": "perpsAtOpenInterestCap"},
]
# The account-scoped views: the venue answers an empty account, the sim a
# funded one (the deal), so the rule is the venue's key set and the shared
# keys' shapes — the sim's extra keys are the venue's state-dependent
# optional fields, reported below.
ACCOUNT_REQS = [
    {"type": "clearinghouseState", "user": touched},
    {"type": "spotClearinghouseState", "user": touched},
    {"type": "userAbstraction", "user": touched},
    {"type": "openOrders", "user": touched},
    {"type": "frontendOpenOrders", "user": touched},
    {"type": "orderStatus", "user": touched, "oid": 1},
    {"type": "activeAssetData", "user": touched, "coin": "ETH"},
]

passed = failed = 0
def verdict(ok, line):
    global passed, failed
    if ok:
        passed += 1
        print(f"  \u2714 {line}")
    else:
        failed += 1
        print(f"  \u2718 {line}")

for req in MARKET_REQS:
    name = req["type"]
    sim = post(sim_url, req)
    real = post(real_url, req)
    if same_shape(sim, real):
        verdict(True, f"{name}: the shape matches the venue's")
    else:
        verdict(False, f"{name}: {first_diff('$', sim, real)}")

for req in ACCOUNT_REQS:
    name = req["type"]
    sim = post(sim_url, req)
    real = post(real_url, req)
    missing = missing_from_sim(real, sim)
    shared_ok = same_shape(intersect(real, sim), intersect(sim, real))
    if missing:
        verdict(False, f"{name}: the venue's keys are not all served ({', '.join(missing[:3])})")
    elif not shared_ok:
        verdict(False, f"{name}: {first_diff('$', intersect(sim, real), intersect(real, sim))}")
    else:
        extra = extra_in_sim(real, sim)
        note = f" (+{len(extra)} state-dependent field{'s' if len(extra) != 1 else ''}: {', '.join(e.split('.')[-1] for e in extra[:3])})" if extra else ""
        verdict(True, f"{name}: the venue's keys all served, shared shapes match{note}")

# The static views carry the same content, not just the same shape.
sim_meta = post(sim_url, {"type": "meta"})
real_meta = post(real_url, {"type": "meta"})
sn = {a["name"] for a in sim_meta["universe"]}
rn = {a["name"] for a in real_meta["universe"]}
verdict(sn == rn, f"meta: the same {len(rn)} universe names")

sim_spot = post(sim_url, {"type": "spotMeta"})
real_spot = post(real_url, {"type": "spotMeta"})
st = {t["name"] for t in sim_spot["tokens"]}
rt = {t["name"] for t in real_spot["tokens"]}
verdict(st == rt, f"spotMeta: the same {len(rt)} tokens")

# The book is live: the sim's ETH mid tracks the venue's.
def mid(book):
    levels = book["levels"]
    bid = float(levels[0][0]["px"])
    ask = float(levels[1][0]["px"])
    return (bid + ask) / 2

sim_book = post(sim_url, {"type": "l2Book", "coin": "ETH"})
real_book = post(real_url, {"type": "l2Book", "coin": "ETH"})
sm, rm = mid(sim_book), mid(real_book)
bps = abs(sm - rm) / rm * 10_000
verdict(bps < 50, f"l2Book ETH: the sim mid {sm:.2f} is {bps:.1f} bps from the venue's {rm:.2f}")

# The implicit default table: the exact served body, both sides.
sim_mt = post(sim_url, {"type": "marginTable", "id": 1})
real_mt = post(real_url, {"type": "marginTable", "id": 1})
verdict(sim_mt == real_mt, f"marginTable 1: the exact default body ({json.dumps(real_mt)})")

# The touched account: the sim's own state (the deal's credit), the venue's
# shape — and the abstraction agrees.
sim_spot_state = post(sim_url, {"type": "spotClearinghouseState", "user": touched})
credit = sim_spot_state["balances"][0]["total"] if sim_spot_state.get("balances") else "0"
verdict(credit == "250", f"the touched account is the sim's own (spot credit {credit})")
sim_abs = post(sim_url, {"type": "userAbstraction", "user": touched})
real_abs = post(real_url, {"type": "userAbstraction", "user": touched})
verdict(sim_abs == real_abs, f"userAbstraction agrees ({sim_abs})")

print(f"== venue-sim-parity-local: {passed} passed, {failed} failed")
sys.exit(0 if failed == 0 else 1)
PY
rc=$?
echo "venue-sim-parity-local: exit=$rc (logs in $DIR)"
exit "$rc"
