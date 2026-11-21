# Venue port assessment: upstream `HerodotusDev/Diorama` vs our `port/herodotus-venue-parity`

Measured 2026-11-21 against `herodotus/develop` (`f362017`, 512 commits) — an
independent lineage of the same codebase (both trees carry `ws_codec`,
`liquidation_parity`, `walk_served_book`, the control plane and the parity
fixtures). The gap is concentrated, not wholesale: 62 venue files, +6 907/−3 887,
and only two files exist solely upstream.

## What upstream has that we lack (port order by value/risk)

1. **Demand-driven upstream relay** (`feed/relay.rs`, `feed/l2.rs`, new; the
   `feed/mod.rs` rewrite): every coin on every dex becomes a market on demand —
   the socket carries a book per coin and the venue reads `l2Book` on demand
   instead of a configured market list. This is the "all supported markets,
   alive by incoming txs" requirement done upstream's way. Port as the relay
   strategy; keep our `spot_markets` gate only where readiness must stay
   pinned (the deployment's contract surface).
2. **WS push channels** (`feed/ws.rs`, `api/ws.rs`): bbo, orderUpdates,
   userEvents, aggregated l2Book forwarded with the raw spread; push-on-change
   with 25 ms coalescing, per-subscription dedup; `market_push_interval_ms`
   deprecated. We still push on a 500 ms clock.
3. **L1 read precompile mirror** (`bridge/precompiles.rs` + `PrecompileMock`):
   installs `spotBalance` (0x0801), `withdrawable` (0x0803),
   `accountMarginSummary` (0x080f), `coreUserExists` (0x0810) answered from the
   sim. Without it, a forked HyperEVM margin agent reads zero on recall — a
   direct fund-safety gap for the unified product.
4. **Engine/bridge parity fixes**: V's window blocks carry the source's base
   fee (gas estimates match HyperCore); `destinationDex` honored on
   EVM→Core deposits (0 = default perp dex, `u32::MAX` = spot, depositFor);
   activation fee on an account's first outbound transfer; unified-account
   switch and API `spotSend` parity; L2 relay so books move at the venue's
   cadence with fast `l2Book`.
5. **Resting spot holds** (`sim/account.rs` spot hold, `sim/transfer.rs`):
   upstream's maker-side spot lane. Complements our taker lane; merge both
   rather than choosing.
6. **Liquidation parity suite** (fixtures `real_liquidation.json`,
   `liquidation_parity.rs`): tiered maintenance with deduction, cross
   liquidation price, liquidation fills carrying the venue's liquidation
   object, real-account fixtures.

## What we have that upstream lacks (keep)

- `spot_markets` config gate + readiness contract (our config plane).
- `print_sync.rs` (the on-chain print follow; upstream deleted it — the
  deployment still uses it for the xyz:* oracle follow).
- Our venue k8s facts (Funnel, NodePort 31090, freshness runbook).

## Method

Unrelated git histories — a `merge` is impossible; this is a file-level port.
Sequence: (1) bridge/precompiles + PrecompileMock (contained, fund-safety),
(2) feed relay + l2 on-demand, (3) WS push, (4) engine parity fixes
(base fee, destinationDex, activation fee), (5) maker spot holds reconciled
with our taker settlement, (6) liquidation parity suite adopted, resolving
both sides' test expectations. Each step lands green (cargo test -p
diorama-venue, clippy) and is verified against the live mirror before the next.
