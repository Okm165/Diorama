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
- Bridge: a CoreDepositWallet `deposit` / `depositFor` names the Core wallet
  its funds land in (`destinationDex`: `u32::MAX` spot, `0` the default perp
  dex, an index the `perpDexs` position). The route is read from the
  transaction's call tree — depositor-made, exact amount, not reverted —
  and the deposit credits that wallet; a plain system-address transfer still
  credits the sender's spot wallet. The block's call traces are read once
  and shared with the contract-internal CoreWriter calls.

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

### Live action sweep (2026-11-21)

Every action above was exercised against the running mirror with a funded
account, and each effect was read back from `/info` (not from the action's own
answer). The sweep proved, end to end:

- **Resting Gtc**: `openOrders` row with the oid, and the same oid visible as a
  level in `/info l2Book` (the participant book). `cancelByCloid` removes it.
- **`modify` / `batchModify`**: the documented cancel-and-place — a new oid at
  the new price, the venue's `{"type":"default"}` answer (the docs' response
  carries no oid). This is the `always_place` semantics, not an in-place edit.
- **Post-only (Alo) crossing**: refused with the venue's string
  (`Post only order would have immediately matched, bbo was …`), no fill.
- **`updateLeverage`**: `activeAssetData.leverage.value` follows.
- **Trigger stop-loss**: armed (visible with `isTrigger=true`, `triggerPx`, a
  non-`N/A` trigger condition), then **fired** on a controlled 5 % crash
  (`/control/crash`): the position closed, the trigger left `openOrders`, and
  `userFills` recorded a `Close Long` fill. This is the first live proof of the
  trigger lane (before, only unit-tested).
- **`scheduleCancel` (dead-man's switch)**: resting orders stayed until the
  deadline and were gone after it.
- **`usdClassTransfer`**: spot −25, perp +25, and an `accountClassTransfer` row
  in `userNonFundingLedgerUpdates`.

One real deviation was found and fixed: a **reduce-only order that cannot
reduce** (flat account, or the position's own side) used to be accepted and
rested, although the venue documents that case as an order error
(`Order / ReduceOnly → "Reduce only order would increase position."`). It is
now refused at placement with that string, and a reduce-only maker whose
position disappears while it rests is canceled (its fill size capped to the
live position) instead of being refused on every tick. Regression tests:
`a_reduce_only_order_with_nothing_to_reduce_is_refused`,
`a_resting_reduce_only_order_is_canceled_when_its_position_is_gone`.

The fix was rolled into the live mirror as `relay-aaa662d` and the same
13-probe sweep then passed 13/13 on the live venue, including the
`Reduce only order would increase position.` refusal on a flat account.
One harness detail: `/control/clear_account` does not reset positions, so the
sweep now flattens any residue first — a leftover 0.04 ETH long otherwise
makes the reduce-only and stop-loss probes look wrong.

### CoreDepositWallet route, measured live (2026-11-21)

`deposit(amount, destinationDex)` and `depositFor(recipient, amount,
destinationDex)` were exercised on the mirror with fresh USDC from the test
wallet `0xb774…37f`, and every effect was read back from `/info`:

| call | route | observed |
|---|---|---|
| `deposit(5, 0)` | default perp dex | perp `accountValue` +5, spot untouched |
| `deposit(2, u32::MAX)` | spot | `spotClearinghouseState` USDC `total` 2.0, perp untouched |
| `deposit(1, 8)` | `para` (HIP-3 perp dex) | `clearinghouseState?dex=para` +1, default perp untouched |
| `depositFor(0x57AC…109, 1, 0)` | named recipient | recipient's perp +1, the funder's balances untouched |

Each deposit credited exactly once: the wallet contract's own `sendAsset`
(the same deposit, seen from Core) is refused by the pre-existence rule
because the wallet has no Core account before the block — the route model
credits the destination directly, and the two never double-credit.

### The account plane answers from the mirror (2026-11-21)

Measured on the live mirror: a per-user `/info` read for an account the sim
had never touched cost **p50 316 ms, p90 435 ms** (`clearinghouseState`,
`spotClearinghouseState`, `openOrders`), while the same read for a touched
account cost **0.2–0.4 ms**. The untouched case was a live-venue passthrough:
one real `POST /info` to `api.hyperliquid.xyz` per poll, plus a spot in the
live API's rate budget.

That is not only slow, it was the swarm's flakiness: the indexer polls every
account on a 5 s cadence, so the live venue's latency and throttling became
the mirror's. The evidence chain, measured over 90 minutes before the fix:

- indexer: 74 `venue account poll refused — recorded as a gap` events
  (`margin-venue API request timed out`, `read_ms` 5005) — beyond the few at
  the rollout windows;
- `venue_account_ticks`: gap rows for accounts with no sim state;
- operator: `the stored venue term outlived its reuse bound — refusing this
  position's HF check`, `positions were excluded from this HF scan for a
  refused venue term`, `venue HF term refused by the indexer's projection`.

An untouched account holds no Core state in the sim — its state *is* empty —
so the mirror now answers the venue's own measured empty shape locally (the
same shape the WS lane already served): zero `clearinghouseState` with the
served clock, `{"balances":[]}`, `[]` for the order/fill/funding/ledger lists,
`"default"` abstraction, `0` `maxBuilderFee`, `{"status":"unknownOid"}`.
Types the mirror does not model (`activeAssetData` — it needs a margin
account — and the fee/referral/portfolio family) keep the verbatim
passthrough.

After the roll: untouched-account reads are **0.2 ms**, 901 account ticks in 5
minutes with **0 gaps** (all 23 accounts), no refusal lines outside the
rollout window, gateway `healthy` with no advisories. The 13-probe action
sweep still passes 13/13.

**One wire fact worth knowing (found while measuring):** the account's
ledger rows (`userFills`, `userFunding`, the transfer ledger) carry the
mirror's **scenario** clock — `/control/advance` moves it forward, and it can
sit minutes ahead of wall time — while `clearinghouseState.time` stays the
wall clock (the freshness gate's fact). Fills were ~33 minutes "ahead" on the
current mirror. Anything that windows fills by wall time is therefore wrong:
`venue-roundtrip`/`venue-maker-cycle` anchored their fill snapshot on wall
time and summed the wallet's pre-run history (131 bps reported for a round
trip whose own fills cost 4.8 bps); they now anchor on the account's newest
fill. Live re-measure: roundtrip 4.8 bps, maker cycle maker 1.5 / taker 4.5
bps, each from its own oids only.




## Representation check against the real venue

The mirror is a pass-through of the real venue's deltas, so "representative"
is measurable rather than assumed. A read-only sampler compared the mirror's
`/info` with the real HL `/info` 30 times, 3 s apart (`venue_fidelity.py`,
2026-11-21):

| coin | mid Δ bps p50 / p95 / max | spread sim/real bps | 10 bp depth | book age sim/real ms |
|---|---|---|---|---|
| ETH | 0 / 0 / 0 | 0.594 / 0.594 | 1.000× | 590 / 656 |
| BTC | 0 / 0.58 / 1.33 | 0.166 / 0.166 | 1.000× | 635 / 567 |
| SOL | 0 / 0.28 / 0.43 | 0.143 / 0.143 | 1.000× | 524 / 664 |
| xyz:ANT | 0 / 0.83 / 0.89 | 1.388 / 1.333 | 1.000× | 698 / 608 |
| para:SMH | 0 / 0 / 0 | 10.16 / 10.16 | 1.000× | 5 901 / 615 |
| xyz:XYZ100 | 0 / 0.20 / 0.39 | 0.392 / 0.392 | 1.000× | 634 / 633 |

Funding matched to ≤1e-7 on every sample. Top of book, spread and 10 bp depth
are the real book to the tick. `para:SMH`'s larger age is a thin-book fact (its
last change was 5.9 s old and its 10 bp depth is $100), not a stalled feed: its
mid matched the real book exactly on every sample. The sampler is read-only and
safe to re-run against the live mirror.

Impact calibration (`venue-calibrate`, mirror vs real VWAP slippage per size
bucket, 5 samples, 2026-11-21 after the `l2Book.time` fix):

| coin | size | mirror bps | real bps | Δ bps |
|---|---|---|---|---|
| ETH | 0.05 | 0.2977 | 0.2977 | 0.0 |
| ETH | 0.5 | 0.2977 | 0.2977 | 0.0 |
| ETH | 5 | 0.2977 | 0.2977 | 0.0 |
| para:SMH | 0.5 | 15.04 | 13.27 | 1.77 |
| para:SMH | 5 | 31.41 | 31.11 | 0.30 |
| para:SMH | 50 | 48.85 | 48.77 | 0.08 |

ETH (deep book) is byte-identical at every size. `para:SMH`'s small-bucket Δ is
read-time book churn on a thin, actively-quoting market (its levels move
between the mirror's read and the real read); at size the structures converge.
`mark_drift_bps` is 0.0 everywhere.

The live runtime that exercised the same planes (2026-11-21): 5 ETH round
trips at 4.80 bps each, 2 maker cycles (maker 1.5 / taker 4.5 / flatten 4.5),
a `para:SMH` round trip at 5.44 bps, two `venue-watch` sessions reading live
mark/PnL deltas every 5 s across held positions, and the portfolio statement
over both accounts (71 + 36 fills; realized, fees, funding, unrealized per
account and total). Every fill was judged from the venue's own ledger.


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
4. **Fee tiers by 24 h volume (investigated 2026-11-21, deliberately not
   built):** the venue's API never exposes a user's fee tier — `webData2`
   carries no fee fields and `userRole` answers `missing` (probed live);
   only the charged `fee` per fill is observable. A tier schedule keyed on
   rolling volume would therefore be behavior no venue consumer can verify
   against — unobservable complexity. The mirror keeps the base constants
   (measured 4.5/1.5 bps taker/maker, matching observed charges) and the
   honesty metric stays: the sim's charged fees vs the fill rows, which a
   later per-user tier would have to show up in anyway.
5. **Sub-accounts and vault endpoints.** (`vaultRoles`/`userRole` and a minimal
   vault transfer surface let multi-entity strategies (curator/vault) run on
   the venue like they do on HL.) **Account reads verified at parity
   2026-11-21:** `userRole` (a known sim account answers `user` — a fresh
   venue account; unknown users pass through to the real venue's answer,
   which for an address with no HL history is `missing`), `subAccounts`
   (`null` both sides), `extraAgents`, `isVip`, delegations. The vault
   *transfer* surface (deposit/withdraw into a vault) is deferred until a
   scenario needs it: the sim models no vault entity, and inventing one
   before a consumer exists is unobservable complexity.
6. **Latency emulation.** An optional per-endpoint latency distribution
   (measured from the real venue) so strategies observe realistic reaction
   times instead of a local socket.
7. **`orderStatus` history window as config** (already ours);
   `historicalOrders` pagination **(verified 2026-11-21, nothing to build)**:
   the venue documents no cursor scheme — "at most 2000 most recent
   historical orders" — and the sim's `USER_FILLS_PAGE = 2_000` is exactly
   that bound, applied to `userFills` and `historicalOrders` alike. The
   undocumented `startTime`/`startAfter` params are accepted by both with
   HTTP 200 (probed live upstream and on the mirror).
8. **Oracle-band metrics.** Publish the distance between our served mark and
   HL's real mark per coin (the mirror's honesty metric), alerting when a
   scripted overlay drags a coin outside the band.

## Book-simulation ideas (the "proper sims of books" ask)

- **Depth regeneration (investigated 2026-11-21, deliberately not built):**
  the real venue's `/info l2Book` itself returns **20 levels per side**
  (measured live on BTC and ETH) — deeper liquidity is hidden from every
  venue client, and the mirror's 20-level books are exactly what the venue
  exposes. Synthesizing depth below the top-20 would invent liquidity no
  venue consumer can see, breaking the mirror's honesty rule. The named
  simplification stays: a taker larger than visible depth fills the visible
  book and its remainder behaves per its TIF (an IOC cancels), while real HL
  can fill against hidden depth — so the sim is **conservative**, never
  over-filling. `venue-calibrate` is the tool that quantifies when visible
  depth binds (a size bucket whose measured slippage exceeds the book's)
  and the measured numbers, not synthesized walls, drive any later decision.
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
