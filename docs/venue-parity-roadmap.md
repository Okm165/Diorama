# Venue parity roadmap: full HL exchange mirror — ideas and checklist

Companion to `venue-port-assessment.md` (what upstream has and the port order).
This is the set of ideas for taking the venue from "trades perps + configured
spot" to a **1:1 HL exchange mirror** with honest book simulation.

## Where the port stands

- Demand-driven relay + fast `l2` diffs: live-proven (354 coins, 0 missing
  contexts; 11 book updates / 10 s on BTC).
- Spot: read plane + taker lane + relay leases landed; the deployment roll
  enables the configured pairs.
- Market breadth (2026-11-21): the relayed `perpDexs` registry is served
  verbatim (10 dexes: xyz, flx, vntl, hyna, km, abcd, cash, para, mkts, io —
  235 default perps, 519 spot tokens / 8 pairs). The poller now cold-loads a
  meta for **every** advertised dex, not only the dexes a configured market
  names: a coin of any advertised dex resolves as listed and its books fetch
  on demand (before: `hyna:BTC` answered the unknown-coin 500 shape while the
  real venue serves it an empty-levels book). Configured dexes still refresh
  every poll; cold-only for the rest so nine extra heavy polls cannot starve
  the book lane.
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
  fills. *(Done 2026-11-21: the vanished-overlay drill — `a_vanished_spoof_never_fills_a_resting_order`.)*
- **Cross-market coherence:** for mirrored spot+perp of the same asset
  (HYPE/@107...), check basis stays inside a plausible band; a basis blowout is
  a sim bug, not a strategy signal. *(Measured 2026-11-21: +0.97 bps HYPE — planes agree; worth a standing check.)*
- **Participant books (1:1 symmetry, done 2026-11-21):** the served book is
  the venue's honest composition — live ⊖ consumptions ⊕ the sim's own
  resting orders (`with_resting`, in `/info l2Book` and both ws book
  channels, before aggregation). A resting maker is displayed liquidity
  exactly as on HL; a passive fill shrinks the maker's own size and consumes
  **no** live level (the old consumption was a display proxy from when the
  maker was invisible — it ate twice the liquidity one trade took).
- **Participant-trade tape:** HL's `recentTrades` shows the venue's own
  trades; the sim's fills (taker and passive maker) should join the served
  tape the way `with_resting` joins the book, so a consumer reading trades
  sees the liquidity their book showed disappear.
- **Own-order optics option:** HL's UI highlights *your* resting levels in
  the book; a per-user `l2Book` variant (or a flag on the ws sub) that tags
  the caller's levels would let integrators verify placement without
  polling `openOrders`.
- **Impact symmetry check:** a standing property test — after any fill, the
  total displayed size removed equals the traded size, never 2× (the
  double-count class this pass fixed). Cheap to assert in the tick tests.
- **Depth-regeneration from the tape:** when regenerating below the top-20,
  seed sizes from the measured depth profile *and* the recent trade rate, so
  quiet markets keep thin tails and active ones keep realistic walls.
