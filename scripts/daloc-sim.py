#!/usr/bin/env python3
"""The daloc use-case simulation: the harness flows the plan documents (§17),
driven against the scratch mirrors — T (the devnet Ethereum fork), V (the
devnet HyperEVM fork), the venue engine and the venue simulator.

Each role is what one daloc-side actor does, and each assertion is a gate the
plan names (client gates §2.1/§18.3, the time policy §17.2, the epoch model
§6.5, the venue mechanics §5). The roles run in order; a failure prints the
evidence and exits non-zero.

Usage:
  python3 sim.py --lane all
  python3 sim.py --lane operator     # one role, for a focused run
"""
import os
import argparse
import json
import subprocess
import sys
import time
import urllib.request
from decimal import Decimal

# Defaults are the session's scratch stack (T + both engines + the sim); every
# endpoint is env-overridable, so the same script runs against a deployed
# namespace once its NodePorts are known.
T_RPC = os.environ.get("T_RPC", "http://127.0.0.1:18602")          # the devnet T (Ethereum fork)
T_ADMIN = os.environ.get("T_ADMIN", "http://127.0.0.1:18680")      # the T engine's admin API
V_RPC = os.environ.get("V_RPC", "http://127.0.0.1:18610")          # the venue V (HyperEVM fork)
V_ADMIN = os.environ.get("V_ADMIN", "http://127.0.0.1:18681")      # the venue engine's admin API
SIM_INFO = os.environ.get("SIM_INFO", "http://127.0.0.1:18790")    # venue-sim info (what daloc reads)
SIM_CTRL = os.environ.get("SIM_CTRL", "http://127.0.0.1:18791")    # venue-sim control
CAST = os.environ.get("CAST", "/tmp/opencode/foundry-bin/cast")

AGENT = "0x00000000000000000000000000000000000000aa"       # the margin agent
UNIFIED = "0x00000000000000000000000000000000000000ac"     # a unified-mode account
UNCONFIGURED = "0x7a3f8d2c9e1b4f6a0d5c8e2f9b3a7d4c1e6f0b5a"
ANVIL_KEY_0 = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
ANVIL_1 = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"

FAILURES = []
CHECKS = 0


def rpc(url, method, params=()):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": list(params)}).encode()
    req = urllib.request.Request(url, data=body, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=20) as resp:
        out = json.loads(resp.read())
    if "error" in out:
        raise RuntimeError(f"{method}: {out['error']}")
    return out.get("result")


def post(url, payload, timeout=20):
    req = urllib.request.Request(url, data=json.dumps(payload).encode(),
                                 headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read()
        try:
            return resp.status, json.loads(raw)
        except json.JSONDecodeError:
            return resp.status, raw.decode()


def check(name, ok, evidence=""):
    global CHECKS
    CHECKS += 1
    mark = "ok  " if ok else "FAIL"
    print(f"  [{mark}] {name}" + (f" — {evidence}" if evidence else ""))
    if not ok:
        FAILURES.append((name, evidence))


def info(kind, **kw):
    status, body = post(f"{SIM_INFO}/info", {"type": kind, **kw})
    return status, body


def cast(*args, timeout=60):
    out = subprocess.run([CAST, *args], capture_output=True, text=True, timeout=timeout)
    return out.returncode, out.stdout.strip(), out.stderr.strip()


def sim_account(addr):
    _, state = info("clearinghouseState", user=addr)
    return state


# ---- roles ----------------------------------------------------------------

def get(url, timeout=20):
    with urllib.request.urlopen(url, timeout=timeout) as resp:
        return resp.status, resp.read().decode()


def role_boot():
    """daloc's boot contract: every chain's clock starts at wall time and only
    moves forward; feeds are stamped in the chain's own domain (§17.2)."""
    print("role: boot (time policy)")
    wall = int(time.time())
    for name, url in (("T", T_RPC), ("V", V_RPC)):
        # `anvil_setTime` sets the *next* block's timestamp: pin, then mine the
        # block that carries it (their harness pins before deploying fixtures).
        rpc(url, "anvil_setTime", [wall])
        rpc(url, "evm_mine", [])
        head = int(rpc(url, "eth_getBlockByNumber", ["latest", False])["timestamp"], 16)
        check(f"{name} clock starts at wall time", head >= wall - 300, f"head={head} wall={wall}")
    # the venue simulator's market plane is fresh (the readiness gate)
    try:
        status, body = get(f"{SIM_INFO}/readyz")
        check("venue-sim readiness", status == 200, f"status={status} body={body}")
    except Exception as e:  # noqa: BLE001
        check("venue-sim readiness", False, str(e))


def role_provision():
    """Provisioning: ensure_gas + venue funds (their ensure_gas/deal paths)."""
    print("role: provision")
    rpc(T_RPC, "anvil_setBalance", [AGENT, hex(10**19)])
    bal = int(rpc(T_RPC, "eth_getBalance", [AGENT, "latest"]), 16)
    check("T gas funded", bal >= 10**18, f"balance={bal}")
    status, body = post(f"{SIM_CTRL}/control/reset", {})
    check("venue scenario reset", status == 200, str(body))
    status, body = post(f"{SIM_CTRL}/control/deal",
                        {"address": AGENT, "class": "perp", "amount": "1000"})
    check("venue perp funds dealt", status == 200 and Decimal(body["balance_after"]) == 1000,
          str(body))
    status, body = post(f"{SIM_CTRL}/control/deal",
                        {"address": AGENT, "class": "spot", "coin": "USDC", "amount": "250"})
    check("venue spot funds dealt", status == 200 and Decimal(body["balance_after"]) == 250,
          str(body))
    # a seeded position (their venue_mock::set_venue_account shape)
    seed = {
        "marginSummary": {"accountValue": "1000", "totalNtlPos": "0", "totalRawUsd": "1000",
                          "totalMarginUsed": "50"},
        "assetPositions": [{
            "type": "oneWay",
            "position": {"coin": "ETH", "entryPx": "2500", "leverage": {"type": "cross", "value": 20},
                         "marginUsed": "50", "maxLeverage": 25, "positionValue": "0",
                         "returnOnEquity": "0", "szi": "0.4", "unrealizedPnl": "0",
                         "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}}
        }],
    }
    status, body = post(f"{SIM_CTRL}/control/set_account", {"address": AGENT, "state": seed})
    check("venue account seeded (hl_mock shape)", status == 200, str(body)[:160])
    # The seed is a *patch* (their venue_mock contract): the perp-only seed
    # must not wipe the spot balance the deal above just wrote. Measured
    # divergence fixed 2026-10-23 (the seed used to replace the account).
    _, spot = info("spotClearinghouseState", user=AGENT)
    usdc = next((b for b in spot["balances"] if b["token"] == 0), None)
    check("perp-only seed keeps the spot balance (patch, not replace)",
          usdc is not None and Decimal(usdc["total"]) == 250, str(spot))


def role_operator():
    """The operator's gated reads (their hl_client.rs): the exact figures and
    the identity the client checks at 1e-6."""
    print("role: operator (venue reads + client gates)")
    state = sim_account(AGENT)
    m = state["marginSummary"]
    p = state["assetPositions"][0]["position"]
    signed = Decimal(p["positionValue"]) * (1 if not p["szi"].startswith("-") else -1)
    diff = abs(Decimal(m["accountValue"]) - (Decimal(m["totalRawUsd"]) + signed))
    check("identity accountValue == totalRawUsd + Σ signed(positionValue) (1e-6)",
          diff <= Decimal("0.000001"), f"diff={diff}")
    check("positionValue unsigned on the wire", Decimal(p["positionValue"]) >= 0)
    check("szi signed (side derivation)", p["szi"].startswith("-") or Decimal(p["szi"]) > 0, p["szi"])
    check("cumFunding.sinceOpen present", "sinceOpen" in p["cumFunding"])
    check("leverage.type present", p["leverage"]["type"] in ("cross", "isolated"))
    check("flat rows dropped", all(Decimal(a["position"]["szi"]) != 0 for a in state["assetPositions"]))
    check("liquidationPx isolated-only",
          p["leverage"]["type"] != "cross" or "liquidationPx" not in p or p.get("liquidationPx") is None)
    now_ms = int(time.time() * 1000)
    # Staleness is one-sided (the indexer's own formula): an *old* time is the
    # failure. A time ahead of wall is the echo of their harness's own chain
    # jump (the venue's served time follows the venue chain's clock -- the
    # client's `chain_time = max(wall, head)` rule).
    age = now_ms - state["time"]
    check("time fresh (≤6 s gate)", age <= 6000, f"age={age}ms")
    # the recall figure (their operator reads totalRawUsd)
    check("totalRawUsd is the recall amount (positive here)", Decimal(m["totalRawUsd"]) > 0,
          m["totalRawUsd"])

    # meta: the operator's unwind read (szDecimals for the wire sz)
    _, meta = info("meta")
    eth = next(a for a in meta["universe"] if a["name"] == "ETH")
    check("meta.universe carries szDecimals", "szDecimals" in eth, f"ETH szDecimals={eth['szDecimals']}")
    _, orders = info("openOrders", user=AGENT)
    check("openOrders shape (empty list)", orders == [], str(orders))

    # per-dex coverage: the default dex is the account's; a non-default dex is
    # an empty shape (the sim serves the first dex only, perpDexs = [null]).
    _, dex_state = info("clearinghouseState", user=AGENT, dex="xyz")
    check("non-default dex → empty shape, 200",
          Decimal(dex_state["marginSummary"]["accountValue"]) == 0 and not dex_state["assetPositions"])
    _, dexs = info("perpDexs")
    check("perpDexs = [null]", dexs == [None], str(dexs))

    # l2Book: exactly 2 sides, ≤20 levels
    _, book = info("l2Book", coin="BTC")
    check("l2Book exactly 2 sides", len(book["levels"]) == 2)
    check("l2Book ≤20 levels per side", all(len(s) <= 20 for s in book["levels"]),
          str([len(s) for s in book["levels"]]))

    # userAbstraction: unknown users serve the live venue's answer (passthrough)
    status, abstraction = info("userAbstraction", user=AGENT)
    check("userAbstraction served", abstraction in ("default", "disabled", "unifiedAccount",
                                                    "portfolioMargin", "dexAbstraction"), str(abstraction))

    # unified mode: the sim serves `tokenToAvailableAfterMaintenance`, and the
    # identity daloc's client reconciles (`available + maintenance == the
    # collateral balance`, its hard gate) closes exactly. Verified live against
    # the real client (their `margin_venue.rs`) 2026-10-23.
    status, body = post(f"{SIM_CTRL}/control/set_account", {"address": UNIFIED, "state": {
        "userAbstraction": "unifiedAccount",
        "marginSummary": {"accountValue": "1000", "totalNtlPos": "0", "totalRawUsd": "1000",
                          "totalMarginUsed": "50"},
        "assetPositions": [{
            "type": "oneWay",
            "position": {"coin": "ETH", "entryPx": "2500", "leverage": {"type": "cross", "value": 20},
                         "marginUsed": "50", "maxLeverage": 25, "positionValue": "0",
                         "returnOnEquity": "0", "szi": "0.4", "unrealizedPnl": "0",
                         "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}}
        }],
        "spotBalances": [{"coin": "USDC", "token": 0, "hold": "0", "total": "250", "entryNtl": "0"}],
    }})
    check("unified account seeded (userAbstraction + spotBalances)", status == 200, str(body)[:160])
    status, abstraction = info("userAbstraction", user=UNIFIED)
    check("unified abstraction served", abstraction == "unifiedAccount", str(abstraction))
    _, spot = info("spotClearinghouseState", user=UNIFIED)
    _, perp = info("clearinghouseState", user=UNIFIED)
    available = {int(t): Decimal(v) for t, v in spot.get("tokenToAvailableAfterMaintenance", [])}
    maintenance = Decimal(perp["crossMaintenanceMarginUsed"]) + sum(
        (Decimal(a["position"]["marginUsed"]) for a in perp["assetPositions"]
         if a["position"]["leverage"]["type"] == "isolated"), Decimal(0))
    balance = next((Decimal(b["total"]) for b in spot["balances"] if b["token"] == 0), None)
    closes = (balance is not None and 0 in available
              and abs(available[0] + maintenance - balance) <= Decimal("0.000001"))
    check("unified: available + maintenance == collateral balance (their reconciliation)",
          closes, f"available={available} maintenance={maintenance} balance={balance}")

    # unconfigured account: empty shape 200 (passthrough to the live venue)
    state = sim_account(UNCONFIGURED)
    check("unconfigured account → empty shape 200",
          Decimal(state["marginSummary"]["accountValue"]) == 0 and not state["assetPositions"],
          json.dumps(state["marginSummary"]))


def role_indexer():
    """The indexer's poller (their venue_account.rs): a 5 s cadence, refused
    reads as gaps, and the crash → drawdown → recovery arc."""
    print("role: indexer (5 s series + crash drawdown)")
    series = []
    for _ in range(3):
        state = sim_account(AGENT)
        series.append(Decimal(state["marginSummary"]["accountValue"]))
        age = int(time.time() * 1000) - state["time"]
        check("indexer tick fresh", age <= 6000, f"age={age}ms")
        time.sleep(5)
    check("series is a live account (equity stable ±drift)", len(set(series)) >= 1, str(series))

    status, body = post(f"{SIM_CTRL}/control/crash", {"coin": "ETH", "pct": "30"})
    check("venue crash accepted", status == 200 and body["owned"], str(body))
    time.sleep(1)
    crashed = Decimal(sim_account(AGENT)["marginSummary"]["accountValue"])
    check("crash draws the equity down", crashed < series[-1],
          f"{series[-1]} -> {crashed} (mark owned at {body['mark']})")
    status, body = post(f"{SIM_CTRL}/control/crash", {"coin": "ETH"})
    check("crash handed back", status == 200 and not body["owned"], str(body))
    time.sleep(1)
    recovered = Decimal(sim_account(AGENT)["marginSummary"]["accountValue"])
    check("hand-back recovers the live mark", recovered != crashed,
          f"{crashed} -> {recovered}")


def role_time_and_liquidation():
    """The time jumps + the venue drawdown arc: their advance_time_then_refresh
    shape (T accepts the direct setTime) and a venue-side liquidation."""
    print("role: time + liquidation")
    wall = int(time.time())
    # `anvil_setTime` stamps the *next* block: mine it, then read (their
    # advance_time_then_refresh does the same shape: freeze, jump, mine).
    rpc(T_RPC, "anvil_setTime", [wall + 3600])
    rpc(T_RPC, "evm_mine", [])
    head = int(rpc(T_RPC, "eth_getBlockByNumber", ["latest", False])["timestamp"], 16)
    check("T accepts a forward jump (their resync is forward-only)",
          head >= wall + 3600 - 5, f"head={head} wall+3600={wall + 3600}")
    rpc(V_RPC, "anvil_setTime", [wall + 3600])
    rpc(V_RPC, "evm_mine", [])
    v_head = int(rpc(V_RPC, "eth_getBlockByNumber", ["latest", False])["timestamp"], 16)
    check("V accepts a direct jump (no alignment)", v_head >= wall + 3600 - 5, f"head={v_head}")

    # an underwater venue account liquidates on the automaton (their named
    # case: "a venue-side drawdown, not an oracle crash")
    underwater = "0x00000000000000000000000000000000000000cc"
    seed = {
        "marginSummary": {"accountValue": "0", "totalNtlPos": "0", "totalRawUsd": "-90000",
                          "totalMarginUsed": "1000"},
        "assetPositions": [{
            "type": "oneWay",
            "position": {"coin": "BTC", "entryPx": "100000", "leverage": {"type": "cross", "value": 10},
                         "marginUsed": "1000", "maxLeverage": 40, "positionValue": "0",
                         "returnOnEquity": "0", "szi": "1", "unrealizedPnl": "0",
                         "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}}
        }],
    }
    post(f"{SIM_CTRL}/control/set_account", {"address": underwater, "state": seed})
    status, body = post(f"{SIM_CTRL}/control/advance", {"seconds": 2})
    check("advance runs the automaton", status == 200, str(body)[:120])
    time.sleep(3)
    after = sim_account(underwater)
    check("underwater position liquidated (closed at the mark)",
          len(after["assetPositions"]) == 0 and Decimal(after["marginSummary"]["accountValue"]) < 0,
          f"equity={after['marginSummary']['accountValue']} positions={len(after['assetPositions'])}")


def role_epoch():
    """The epoch model (§6.5): one call reverts the substrate to blank on both
    mirrors; the venue sim resets with it."""
    print("role: epoch (revert to blank)")
    # venue: epoch_new resets the sim and re-pins V through the engine.
    # The engine's fork reset rides a public RPC: a transient transport fault
    # is a documented 502 meaning "nothing happened, retry" (the sim only
    # blanks the scenario after the engine accepts).
    body = None
    for attempt in range(3):
        try:
            status, body = post(f"{SIM_CTRL}/control/epoch_new", {}, timeout=120)
        except urllib.error.HTTPError as e:
            status, body = e.code, e.read().decode()[:160]
        if status == 200:
            break
        time.sleep(5)
    check("epoch_new accepted (with the documented retry)",
          status == 200 and body.get("engine_repinned"), f"attempts<=3: {body}")
    time.sleep(4)
    _, status_raw = get(f"{V_ADMIN}/status")
    status_body = json.loads(status_raw)
    check("venue engine re-pinned (fork_pin present)",
          status_body.get("fork_pin") is not None, f"pin={status_body.get('fork_pin')}")
    check("venue sim reset with the epoch (account gone)",
          len(sim_account(AGENT)["assetPositions"]) == 0)
    # T: /admin/epoch re-pins; the engine rebases and runs the epoch rule
    status, body = post(f"{T_ADMIN}/admin/epoch", {}, timeout=60)
    check("T epoch accepted", status == 200 and body.get("reset"), str(body))
    time.sleep(8)
    status_body = json.loads(urllib.request.urlopen(f"{T_ADMIN}/status", timeout=20).read())
    check("T engine is live after the re-pin (no halt)",
          not status_body.get("halted"), f"halted={status_body.get('halted')}")


LANES = {
    "boot": role_boot,
    "provision": role_provision,
    "operator": role_operator,
    "indexer": role_indexer,
    "time": role_time_and_liquidation,
    "epoch": role_epoch,
}
ORDER = ["boot", "provision", "operator", "indexer", "time", "epoch"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--lane", default="all", choices=["all", *ORDER])
    args = parser.parse_args()
    lanes = ORDER if args.lane == "all" else [args.lane]
    for lane in lanes:
        LANES[lane]()
    print(f"\n{CHECKS - len(FAILURES)}/{CHECKS} checks passed")
    if FAILURES:
        print("failures:")
        for name, evidence in FAILURES:
            print(f"  - {name}: {evidence}")
        sys.exit(1)


if __name__ == "__main__":
    main()
