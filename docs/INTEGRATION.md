# Integration surface — what the mirrors expose to the daloc devnet

Our tool is the two mirrors (the mainnet mirror and the HL markets mirror);
it stays as it is. This page is the single index of every surface they expose
— endpoints, shapes, named refusals, the env contract — plus the lanes that
are deliberately deferred, so the daloc devnet's redesign can plan against
facts. Authoritative details: [ARCHITECTURE](ARCHITECTURE.md) (mechanisms),
[OPERATIONS](OPERATIONS.md) (drills, alarms), [TESTING](TESTING.md) (measured
acceptance).

## 1. The chain side — T (the mainnet mirror's devnet) and V (the HyperEVM fork)

| Surface | Where | Notes |
|---|---|---|
| JSON-RPC + WS | T :8545, V :8545 (NodePorts 30545 / 31545) | anvil, with the full cheatcode set: `anvil_setBalance`, `anvil_setStorageAt`, `anvil_setCode`, `evm_snapshot`/`evm_revert`, `anvil_setTime`/`evm_increaseTime`, `anvil_impersonateAccount`, `evm_mine`, unsigned `eth_sendTransaction` (auto-impersonate on) |
| Engine admin (both engines) | :8080 | `GET /status`, `/metrics`, `/healthz`, `/livez`; `POST /admin/{deal, shadow, rewind, pause, resume, epoch, probes}` |
| T-only admin | the mainnet engine | `POST /admin/time` (the persistent clock offset) and `/admin/feed{,/pause,/resume,/override}` (the feed bridge) — the symmetry gate's deliberate absences |
| Feed bridge | T | live Chainlink answers → the mock aggregators; a crash is owned until handed back (`/admin/feed/override`, then clear) |
| Watchlist + probes | the instance TOML | 88 watched accounts on the mainnet instance; the closure audit is `make mainnet-closure-check` |
| Epochs | `POST /admin/epoch` (T) / the sim's `POST /control/epoch_new` (V) | a fresh pin clears the devnet's marks *and* values; a same-pin reset restores them |

**The engine exposes one chain: T, the devnet.** There is no internal replay
chain to expose: the applier reconstructs state from the parent's BAL and the
engine checks it in place (sampled parity, the convergence probes,
`verifier.full_sweep`). The removal's rationale is in ARCHITECTURE, "One
Anvil, one source".

## 2. The venue side — venue-sim (info :8090, control :8091)

**Served `/info` types (16):** `clearinghouseState`, `spotClearinghouseState`,
`userAbstraction`, `openOrders`, `frontendOpenOrders`, `orderStatus`,
`perpsAtOpenInterestCap`, `meta`, `metaAndAssetCtxs`, `perpDexs`, `l2Book`,
`marginTable`, `activeAssetData`, `exchangeStatus`, `spotMeta`, `allMids`. An unknown coin's `l2Book` answers the venue's own absence shape
(HTTP 200 `null`; a *configured* market still warming is a 503). Anything else
answers `"unrouted"` (hl_mock's contract — a *named*
non-answer, never a 500). Un-simulated types are **not proxied** to the live
venue: the overlay's discipline is that a read either comes from the sim's own
state or is named absent — a proxy would leak live data past the COW boundary
and break the identity guarantees below.

Guarantees the gated read relies on: an unknown address serves the **empty
shape** (zeros, 200); `userAbstraction` is `default` for unknown; `time` is
always fresh (≤6 s); `accountValue == totalRawUsd + Σ signed positionValue`
holds to the digit (measured, signed strings throughout); per-dex reads are
flat; `positionValue` is unsigned with the sign in `szi`.

**WS `/ws`:** subscribe/unsubscribe acks; frames for `allMids`,
`activeAssetCtx` and `l2Book` (per coin; an unknown coin answers an error).
`l2Book`'s `nSigFigs`/`mantissa` are applied (the docs' aggregation) and
validated (a value outside `{2,3,4,5}` / `{1,2,5}` is a named 400).

**Control API (§7.3):** `deal` (`{address, class: spot|perp, coin?, amount}`),
`order` (`{address, coin, is_buy, size, limit_px?, tif?, reduce_only?}` — the
same lane `/exchange` drives; the response carries `totalSz`/`avgPx`/`oid`
and the consumed levels), `advance`, `crash` (`{coin, pct}`; no `pct` hands
the mark back), `set_book`, `set_funding`, `set_account` (the hl_mock patch
shape), `reset`, `epoch_new`, `stall` (`{seconds}`; the market plane holds —
`/readyz` and the order lane see a real outage; `0` clears),
`GET/POST /control/state/{address}`, plus `/health` and `/metrics`.

**`POST /exchange`** models the `order` action end to end: a compact order
(`{"type":"order","orders":[{"a","b","p","s","r","t":{"limit":{"tif"}}}]}`)
fills against the **real book** — the live depth the feed caches — with
per-level consumption (plan §4.5's impact lane: the order moves the *book*,
never the *mark*), answered in HL's own ok envelope
(`{"status":"ok","response":{"type":"order","data":{"statuses":
[{"filled":{"totalSz","avgPx","oid"}}]}}}`; a per-order refusal rides as
`{"error":"…"}` inside the statuses). `Gtc`/`Alo` orders rest in the sim's
book — `openOrders`/`frontendOpenOrders` serve them, the live book fills them,
`cancel`/`cancelByCloid`/`scheduleCancel` close them, an armed trigger fires
when the mark crosses — and every other action tag answers HL's err
envelope (`{"status":"err","response":"…"}`, HTTP 200 — the shape their
client already parses), never a 404. The signature is accepted, never
verified (the devnet has no keys — the anvil-impersonation analogue); the
acting account is read from the body's `user` — optional in the body so the
SDK's `noop` (which carries none) parses; an *order* without it refuses by
name. `noop` itself is modeled: `{"type":"noop"}` answers the venue's no-data
success (`{"status":"ok","response":{"type":"default"}}`).

## 3. The CCTP bridge (Iris :8095, control :8096)

Iris-compatible: `GET /v2/messages/{source_domain}?transactionHash=…` (the
authoritative tx-keyed lookup; `attestation: null` while the PENDING window
runs), `POST /v2/reattest/{nonce}`, `GET /v2/publicKeys`,
`GET /v2/burn/USDC/fees/{source}/{destination}`,
`GET /v2/fastBurn/USDC/allowance?sourceDomain=…`, `GET /healthz`. Control:
`GET /health` (the queue counts), `/metrics`, `POST /control/transfer`
(`hook: {target, calldata}` for the credit lane), `POST /control/relay`,
`GET /control/state`. `make cctp-smoke` and `make cctp-roundtrip` are the
scripted checks; OPERATIONS carries the re-pin recovery dance and the
vanished-burn/keeper caveats.

## 4. The environment contract

- The config loader is the **only** env reader: `${VAR}` references in the
  instance TOML expand at load time, and a missing one is a boot error naming
  the variable. Services never read env values themselves.
- `mirror-config export-env --section daloc <instance.toml>` renders the
  daloc-facing set. Measured: the mainnet instance renders `CHAIN_RPC_URL`,
  `CHAIN_WS_URL`, `MIRROR_ADMIN_URL`; the venue instance renders
  `CHAIN_1_CHAIN_RPC_URL`/`_WS_URL`, `HL_INFO_API_URL`, `VENUE_CONTROL_URL`,
  `VENUE_ACCOUNT_FEEDS`, `VENUE_RATE_FEEDS`, `VENUE_ADMIN_URL`,
  `IRIS_API_URL`.
- `--section anvil` renders the anvil pods' wiring (the init containers read
  it from the mounted instance file); `mirror-config check` validates an
  instance; `mirror-config get` prints one resolved value.

## 5. The refusal discipline (so a probe learns *why*, not just that)

| Boundary | Refusal |
|---|---|
| sim `/info` | an unknown type answers `"unrouted"` (200) |
| sim `/exchange` | an order fills, or refuses per-order by name (IOC only); `noop` answers the no-data success; any other action answers HL's err envelope naming the boundary (200) |
| sim readiness | 503, naming the cause (`market plane warming up` / `sim state poisoned`) |
| engine `/admin/deal` | 400 naming the numbers (`holds 0 USDC on ethereum, needs 1000000 (deal via the chain's admin surface)`) |
| engine `/admin/shadow` | 400 naming the missing keys (`clear` needs `slots`, `account_fields`, or `all`) |
| bridge control | 400 naming the state (`no watched message with nonce …`) |
| CCTP relay | a record never drops: it parks with `park_reason`, counted by `/health` |
| `mirror-config` | the missing `${VAR}` by name; a malformed key by rule |

## 6. Deferred lanes (plan refs — nothing is silently absent)

| Lane | Plan | What it would add |
|---|---|---|
| Remaining venue mechanics | §5, §7.1, §18.4 | the remaining order-error strings (the measured vocabulary — tick size, minimum value, insufficient margin, reduce-only, no-match, no-liquidity, the mis-sided trigger — is served verbatim), multi-dex, spot ledger, fees (a research lane per §16.3 — daloc's paths don't read venue fees). Landed: the `order` action end to end (IOC fills against the real book with per-level consumption, the resting lane — `Gtc`/`Alo`, cancels, triggers, the dead-man's switch — the §4.5 impact invariants I1–I6, three doors), nonces/API wallets (the 100-highest set with the floor, agent wallets), the margin modes (cross and isolated legs, strict isolated), the OI cap and margin-tier refusals |
| Portfolio margin | §5 | the mode's cross-asset portfolio (the eligible set HYPE/BTC/USDC/USDT valued and margined together; the portfolio-wide `tokenToAvailableAfterMaintenance`): `setAbstraction`'s value 3 and the control patch refuse it by name and count the refusal — `unifiedAccount` is the modeled single balance (its live identity holds to the digit) |
| User-stream reads and pushes | §5 | the venue's per-user subscriptions (`orderUpdates`, `userEvents`, `userFills`) and the history reads (`userFills`, `historicalOrders`, `userFunding`): the product reads fills through `orderStatus` and `openOrders` today (the flows the daloc simulation proves), and adding the push channel needs the docs' measured frame shapes — the sim's mutations would publish to a subscriber set instead of being polled |
| Phase 4 lanes | §7.1 | the HIP-3 star venue on testnet; the real-data replay lane |
| Phase 5 facade | §7.1 | one deal/crash/reset surface across both mirrors (each mirror's own admin works today) |
| `venue-closure` | §7.8 | the V-side closure audit (no BALs on HyperEVM; `make engine-symmetry` covers the shared verbs) |
