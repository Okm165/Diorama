#!/usr/bin/env python3
"""The local Hyperliquid upstream for the order-lane proof's stack.

The venue simulator's market plane reads the venue's public API (REST /info
+ the WS subscription protocol). Off-host egress is not a proof's business,
so the stack boots this mock on 127.0.0.1 and points the real
`diorama-venue-sim` bin at it: the simulator exercises its *own* poller, WS
client, freshness gates and order lane against a live market plane — only
the market data itself is scripted.

Served surface (the shapes the feed's poller and WS client read):
  REST  POST /info: perpDexs, metaAndAssetCtxs, spotMeta, l2Book, allMids,
        perpsAtOpenInterestCap, plus a passthrough-safe clearinghouseState
        (an untouched account's reads hit the upstream verbatim).
  WS        /ws : subscribe/subscriptionResponse, then periodic allMids +
        activeAssetCtx + l2Book pushes (liveness is data, not the handshake).

Usage: venue-upstream-mock.py <rest-addr> <ws-addr>
   e.g. venue-upstream-mock.py 127.0.0.1:18795 127.0.0.1:18796
"""
import asyncio
import json
import sys
import time

COIN = "ETH"
MARK = "3000.5"
MID = "3000.75"
# The live book: one bid level, two ask levels (the touch the proof fills at).
BIDS = [{"px": "2998.0", "sz": "5.0", "n": 1}]
ASKS = [{"px": "2999.5", "sz": "3.0", "n": 1}, {"px": "2999.8", "sz": "2.0", "n": 1}]


def now_ms() -> int:
    return int(time.time() * 1000)


def ctx() -> dict:
    return {
        "funding": "0.0",
        "openInterest": "120.0",
        "prevDayPx": "2990.0",
        "dayNtlVlm": "1000000.0",
        "premium": None,
        "oraclePx": MARK,
        "markPx": MARK,
        "midPx": MID,
        "impactPxs": None,
        "dayBaseVlm": "500.0",
    }


def book() -> dict:
    return {"coin": COIN, "time": now_ms(), "levels": [BIDS, ASKS], "spread": None}


def empty_account() -> dict:
    """An untouched account's clearinghouse state (the passthrough shape)."""
    zero = {"accountValue": "0.0", "totalNtlPos": "0.0", "totalRawUsd": "0.0"}
    return {
        "assetPositions": [],
        "marginSummary": zero,
        "crossMarginSummary": zero,
        "withdrawable": "0.0",
    }


def info_answer(req: dict):
    kind = req.get("type")
    if kind == "perpDexs":
        return [None]
    if kind == "metaAndAssetCtxs":
        meta = {
            "universe": [{"name": COIN, "szDecimals": 4, "maxLeverage": 20}],
            "marginTables": [],
            "collateralToken": 0,
        }
        return [meta, [ctx()]]
    if kind == "spotMeta":
        return {"tokens": [], "universe": []}
    if kind == "l2Book":
        return book()
    if kind == "allMids":
        return {COIN: MID}
    if kind == "perpsAtOpenInterestCap":
        return []
    if kind == "clearinghouseState":
        return empty_account()
    # Unknown/unrouted: the venue answers *something* typed; an empty object
    # keeps a passthrough read harmless.
    return {}


async def handle_rest(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    try:
        request_line = (await reader.readline()).decode()
        length = 0
        while True:
            line = (await reader.readline()).decode()
            if line in ("\r\n", "\n", ""):
                break
            if line.lower().startswith("content-length:"):
                length = int(line.split(":", 1)[1].strip())
        body = await reader.readexactly(length) if length else b"{}"
        try:
            answer = info_answer(json.loads(body or b"{}"))
        except Exception:
            answer = {}
        payload = json.dumps(answer).encode()
        writer.write(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: "
            + str(len(payload)).encode()
            + b"\r\nConnection: close\r\n\r\n"
            + payload
        )
        await writer.drain()
    except (asyncio.IncompleteReadError, ConnectionError):
        pass
    finally:
        writer.close()


async def pusher(ws):
    """The data lane: liveness is data, not the handshake (the sim's WS
    client drops a socket that never pushes)."""
    while True:
        await ws.send(json.dumps({"channel": "allMids", "data": {"mids": {COIN: MID}}}))
        await ws.send(
            json.dumps({"channel": "activeAssetCtx", "data": {"coin": COIN, "ctx": ctx()}})
        )
        await ws.send(json.dumps({"channel": "l2Book", "data": book()}))
        await asyncio.sleep(1)


async def handle_ws(ws):
    started = False
    async for raw in ws:
        try:
            msg = json.loads(raw)
        except Exception:
            continue
        if msg.get("method") != "subscribe":
            continue
        sub = msg.get("subscription", {})
        await ws.send(
            json.dumps(
                {
                    "channel": "subscriptionResponse",
                    "data": {"method": "subscribe", "subscription": sub},
                }
            )
        )
        if not started:
            started = True
            asyncio.ensure_future(pusher(ws))


async def main():
    rest_addr, ws_addr = sys.argv[1], sys.argv[2]
    rest_host, rest_port = rest_addr.rsplit(":", 1)
    ws_host, ws_port = ws_addr.rsplit(":", 1)
    rest_server = await asyncio.start_server(
        handle_rest, rest_host, int(rest_port)
    )
    import websockets

    ws_server = await websockets.serve(handle_ws, ws_host, int(ws_port))
    print(f"venue-upstream-mock: rest {rest_addr}, ws {ws_addr}", flush=True)
    async with rest_server, ws_server:
        await asyncio.Future()  # run forever


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
