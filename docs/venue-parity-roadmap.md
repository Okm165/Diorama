# Venue parity roadmap: full HL exchange mirror — ideas and checklist

Companion to `venue-port-assessment.md` (what upstream has and the port order).
This is the set of ideas for taking the venue from "trades perps + configured
spot" to a **1:1 HL exchange mirror** with honest book simulation.

## Where the port stands

- Demand-driven relay + fast `l2` diffs: live-proven (354 coins, 0 missing
  contexts; 11 book updates / 10 s on BTC).
- Spot: read plane + taker lane + relay leases landed; the deployment roll
  enables the configured pairs.
- Downstream WS: leases per subscription, push-on-change (25 ms coalescing),
  socket-level tests.

## Endpoint checklist (mirror = shape, refusal, and edge behavior)

Perps: `meta`, `metaAndAssetCtxs`, `perpDexs`, `allPerpMetas`,
`perpConciseAnnotations`, `allMids`, `l2Book`, `candleSnapshot`, `activeAssetCtx`,
`activeAssetData`, `clearinghouseState`, `spotClearinghouseState`,
`userAbstraction`, `userFills`(+ByTime), `openOrders`, `historicalOrders`,
`orderStatus`, `frontendOpenOrders`, `vaultRoles`, `userRole`.
Exchange actions: `order`, `cancel`, `cancelByCloid`, `modify`, `batchModify`,
`updateLeverage`, `updateIsolatedMargin`, `scheduleCancel`, `usdClassTransfer`,
`usdSend`, `spotSend`, `sendAsset`, `approveAgent`, `approveBuilderFee`,
`userSetAbstraction`, `agentSetAbstraction`.
Discipline: every endpoint gets a measured-live fixture (we already hold
`real.json` / `real_exchange.json` / `real_liquidation.json`); a fixture diff is
the parity gate, never eyeballing.

## Ideas, ordered by value

1. **Maker (resting spot) lane on the fast books.** Our taker lane settles
   tokens; a resting spot order needs the hold lifecycle (holds exist in the
   sim). Maker fills then need the same received-token fee rule and the
   maker/taker 0.040/0.070 % split.
2. **Impact-model calibration.** Measure our fills' slippage distribution
   against real HL depth (per coin, per size bucket) and tune the consumption
   model (served = live − consumed) so a strategy's paper fill quality predicts
   the real one. A weekly calibration report keyed by coin.
3. **Funding accuracy.** Funding is priced from HL marks (trusted in witness);
   charge it on the real cadence (hourly, premium-weighted) and reconcile our
   accrued funding against HL's published `fundingRate` history per coin.
4. **Fee tiers by 24 h volume.** HL's taker/maker schedule is volume-tiered;
   the sim's accounts can carry a rolling volume counter and the same tier
   table, so fee expectations in strategies are realistic.
5. **Sub-accounts and vault endpoints.** `vaultRoles`/`userRole` and a minimal
   vault transfer surface let multi-entity strategies (curator/vault) run on
   the venue like they do on HL.
6. **Latency emulation.** An optional per-endpoint latency distribution
   (measured from the real venue) so strategies observe realistic reaction
   times instead of a local socket.
7. **`orderStatus` history window as config** (already ours); add
   `historicalOrders` pagination parity.
8. **Oracle-band metrics.** Publish the distance between our served mark and
   HL's real mark per coin (the mirror's honesty metric), alerting when a
   scripted overlay drags a coin outside the band.

## Book-simulation ideas (the "proper sims of books" ask)

- **Depth regeneration:** upstream diffs keep the top-20 honest; below that,
  synthesize resting depth with a per-coin depth profile measured from HL
  (so IOC walks beyond 20 levels meet plausible size).
- **Adversarial book drills:** scripted overlays that mimic spoofing (large
  levels that vanish) to verify our consumption model never credits phantom
  fills.
- **Cross-market coherence:** for mirrored spot+perp of the same asset
  (HYPE/@107...), check basis stays inside a plausible band; a basis blowout is
  a sim bug, not a strategy signal.
