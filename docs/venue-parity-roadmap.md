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

Deposit credit latency (the report's "recall reverts `ZeroAmount` right after
Active" family), measured 2026-11-21: a CoreDepositWallet deposit's credit is
visible at `/info` **0.5 s after the deposit tx confirms** — the bridge scans
every block and mirrors the L1 read precompiles in the same pass. A recall
sent seconds after a deposit should no longer race the credit; if a consumer
still sees a stale zero, the suspect is its own read cache, not the bridge.
The gateway's `top_up_quote` route now also quotes the creation fee on
top-ups (the report's "empty revert" integrator complaint).

### The venue position is the loan's collateral — measured live (2026-11-21)

The report's never-tested core idea ran end to end on loan `0xefe3ef00`
(10 WHYPE collateral, 60 USDC debt, agent account `0x609e8618…`, a
**unified** account holding 60 USDC spot on the venue):

| step | agent venue equity | loan `venue_term_usd` | loan HF |
|---|---|---|---|
| flat (baseline) | 0 perp + 60 spot | 66.08 | 8.44 |
| 0.1 ETH long opened (spot collateral backs the order — no perp balance needed) | −0.07 (fees) | — | — |
| `/control/crash` ETH −5 % | −8.48 (uPnL −8.40) | 54.24 | **8.341** |
| rally handed back | −0.003 | — | — |
| flattened | −0.05 (fees) | 66.02 | **8.455** |

The chain that moved: venue fill → sim account equity → indexer
`venue_account_ticks` (5 s) → operator's HF fold → `position_health`. The
unified-account mechanics are live parity too: `usdClassTransfer` is refused
on an active unified account (`Action disabled when unified account is
active`, as the venue serves), and the perp order is margined from the
account's spot collateral.

**New repeating pattern (section-4 material):** a scenario price move
(`/control/crash`/`rally` **with** a pct) freezes that coin's served mark
until its **hand-back** (the same verb with **no** pct). While held, the
indexer's projection refuses the coin's marks (`409 … no observation within
the bound — the market plane is held`) and the operator's HF scan goes blind
for every holder of the coin (`positions were excluded from this HF scan`,
1/9 during the measurement). Always hand a move back; a held plane is a
silent, swarm-wide degradation, not a local scenario state.

**Held-plane, measured to the second (2026-11-21 23:40 UTC):** the blindness
begins between **10 s and 20 s** of hold (the price feed's own freshness
bound), excludes exactly the holders of the held coin
(`hf_scan_observed_positions` 9 → 8, `hf_scan_blind_positions` 1, cause
`venue_term`), and clears on the first tick after hand-back (8 → 9, blind 0).
A position liquidated or flattened before the bound passes never shows the
flag, which is why short holds look clean. The rule is unchanged: always hand
back.

### Multi-dex account plane, measured live (2026-11-21)

The venue keys one `clearinghouseState` **per dex**, and a unified account's
money is spot **plus** perp. Both halves verified against the ledger rather
than assumed:

- **The indexer already folds every dex.** One tick read
  `spot_collateral_total 47.760 − hold 5.982 = account_equity 41.778` and
  `perp_equity 1.784` (itself the sum of the default dex's `−0.114`, `xyz`'s
  `6.072` and `para`'s `−4.174`); the operator's `venue_equity_usd` was
  `43.5988` — the same figure. A HIP-3 move reaches the tick in **≤ 2 s**
  (`perp_equity` 5.960 → 3.900 on a held −5 % of `para:SMH`, recovered on
  hand-back).
- **The user-sim was the single-dex reader** (not the venue): `venue-portfolio`
  and `venue-account` sent one `clearinghouseState` and reported 1 of 3 open
  positions, omitting every `xyz:*`/`para:*` leg. Fixed (user-sim `e4a743e8`):
  the untouched read now walks `perpDexs` and adds the spot leg
  (`spot_usdc`, `spot_balances`); a `--coin` remains single-dex.
- **Isolated-margin liquidation is real and faithful.** A held −10 % on an
  isolated `0.1 para:SMH` (margin 4.11) consumed the margin: the venue closed
  it at `371.52` against a `371.7099` mark (a book walk), realized `−4.17`,
  served the real `liquidation {liquidatedUser, markPx, method: market}`
  object, and the account kept the realized loss. The hand-back restored the
  mark to `413.011`, **identical to the real venue's** mark at the same
  moment.
- **Spot maker lane verified** (see idea 2): 4.00 bps maker / 7.00 bps taker,
  fee in the received token, hold lifecycle and book visibility intact.
- **Rollout churn, not a live fault.** Every `venue account poll refused` in
  the indexer's last 3 h mapped to a `venue-sim` ReplicaSet creation to the
  second (10/10 swaps: 20:34:59, 21:25:49, 21:32:36, 22:02:38, 22:24:05,
  22:27:57, 22:29:36, 22:38:22, 22:55:07, 23:06:10). Steady state: zero
  refusals. Each single-replica swap costs ~10–20 s of no endpoints (the 5 s
  timeout refuses every poll in flight); a shared-storage rolling update is
  the only way to remove it, and it is not worth the state-coupling.




## Ideas, ordered by value

0. **Upstream ports reviewed (fork tip `d58afdc`, content-diffed 2026-11-21):**
   no behavioral gaps left in the venue core. Verified ours is a superset
   where it matters — multi-dex `sendAsset` (upstream defers multi-dex and
   *refuses* it; ours routes it, measured live), per-dex `meta` (upstream
   caches one margin meta; ours relays every advertised dex, `xyz` 147/147
   vs real), builder-dex asset ids on every asset action (order, leverage,
   isolated margin, cancel, cancelByCloid, with the venue's
   `unknown asset index N` refusal), precompile projection (floor/refuse
   units, mock presence read at the pass's head, `balance < stipend` funding).
   Ported this pass: **a refusal speaks on its transition** (the bridge's
   run loop and the callTracer fault warn once per state, repeats at debug,
   recovery speaks once — the measured 1/s-for-hours WARN class).
1. **Trading-enablement matrix as a standing check.** For every dex the
   venue advertises, pick its deepest coin and assert the whole loop: book
   serves (or is empty *exactly as the real venue's*), an IOC round trip
   fills from its own book, cancel/modify/leverage by asset id, and the
   account read carries the position. Proven live on three dex classes
   2026-11-21 (ETH 4.80 bps, `para:SMH` 5.44, `xyz:TSLA` 1.04); the matrix
   makes "all markets tradeable" a gate, not an anecdote. Empty and unknown
   coins already match real shape-for-shape (`flx:TSLA` empty both sides;
   `km:SOL` → 500 `null` both sides).
2. **Maker (resting spot) lane — verified live 2026-11-21, nothing to build.**
   The lane existed; the earlier "missing" note was wrong. Measured on
   `@107` (HYPE/USDC): a `Gtc` sell from a funded spot account rests
   (`oid` assigned, `hold` = the resting size in `spotClearinghouseState`,
   the served book gains the level), a taker IOC buys into it, and both
   fills carry the real fee rule — **maker 4.00 bps, taker 7.00 bps**, the
   fee denominated in the token *received* (sell → USDC, buy → HYPE:
   `feeToken` + the credited size both agree), exactly the venue's spot
   schedule. The resting order leaves the book on fill and the hold releases.
   No code change.
3. **Impact-model calibration.** Measure our fills' slippage distribution
   against real HL depth (per coin, per size bucket) and tune the consumption
   model (served = live − consumed) so a strategy's paper fill quality predicts
   the real one. A weekly calibration report keyed by coin.
4. **Funding accuracy.** Funding is priced from HL marks (trusted in witness);
   charge it on the real cadence (hourly, premium-weighted) and reconcile our
   accrued funding against HL's published `fundingRate` history per coin.
5. **Fee tiers by 24 h volume (investigated 2026-11-21, deliberately not
   built):** the venue's API never exposes a user's fee tier — `webData2`
   carries no fee fields and `userRole` answers `missing` (probed live);
   only the charged `fee` per fill is observable. A tier schedule keyed on
   rolling volume would therefore be behavior no venue consumer can verify
   against — unobservable complexity. The mirror keeps the base constants
   (measured 4.5/1.5 bps taker/maker, matching observed charges) and the
   honesty metric stays: the sim's charged fees vs the fill rows, which a
   later per-user tier would have to show up in anyway.
6. **Sub-accounts and vault endpoints.** (`vaultRoles`/`userRole` and a minimal
   vault transfer surface let multi-entity strategies (curator/vault) run on
   the venue like they do on HL.) **Account reads verified at parity
   2026-11-21:** `userRole` (a known sim account answers `user` — a fresh
   venue account; unknown users pass through to the real venue's answer,
   which for an address with no HL history is `missing`), `subAccounts`
   (`null` both sides), `extraAgents`, `isVip`, delegations. The vault
   *transfer* surface (deposit/withdraw into a vault) is deferred until a
   scenario needs it: the sim models no vault entity, and inventing one
   before a consumer exists is unobservable complexity.
7. **Latency emulation.** An optional per-endpoint latency distribution
   (measured from the real venue) so strategies observe realistic reaction
   times instead of a local socket.
8. **`orderStatus` history window as config** (already ours);
   `historicalOrders` pagination **(verified 2026-11-21, nothing to build)**:
   the venue documents no cursor scheme — "at most 2000 most recent
   historical orders" — and the sim's `USER_FILLS_PAGE = 2_000` is exactly
   that bound, applied to `userFills` and `historicalOrders` alike. The
   undocumented `startTime`/`startAfter` params are accepted by both with
   HTTP 200 (probed live upstream and on the mirror).
9. **Oracle-band metrics.** Publish the distance between our served mark and
   HL's real mark per coin (the mirror's honesty metric), alerting when a
   scripted overlay drags a coin outside the band.
10. **Terminal-refusal coverage for the orphan reconciler (measured defect).**
   One `MarginBridging` position whose `CreationPhase2` proof the keeper
   retired deterministically (`empty_revert_unresolved`) produced **472
   `re-enqueuing` WARNs + 287 `retired this proof … not re-driving` WARNs in
   6 h** — one pair every 15 s — until the position was `Cancelled`.
   Root cause: `keeper_submission_blocks` reads the *keeper's* terminal verdict
   (a separate database) and returns early, but writes nothing on the
   operator side, so the status-keyed orphan query re-selects the position on
   every tick and the guard re-reads the same verdict. The guard is correct;
   its coverage is invisible to its own selector. Minimal fix: the first
   observation records the verdict as a failed, non-retryable coverage row
   (so the existing `proof_queue` coverage branch matches and the selector
   stops), and the WARN speaks on the transition only — the same discipline
   the bridge's refusal speech already uses. Do not roll this alone: it
   belongs with the next operator roll.

### Root cause of every wedged unified creation: the beacon's implementation (2026-11-22)

The `MarginBridging` stall, the "no order path" symptom and the operator's
phantom exposures are one fault, measured end to end on a fresh cross-chain
loan (`0x51bb077e…`, 0.1 WETH-1 → 87.158 USDC):

- Phase-2's batch calls `UnifiedMarginAgent.registerAgentWallet` (`0x99630f7d`)
  at instruction 7 and reverts with **empty data** (`InstructionFailed(7, 0x)`).
- The payload is well formed (33-byte name, two 65-byte signatures), the
  factory deploys at the predicted address, the beacon and its implementation
  hold code, and the CoreWriter mock accepts the `addApiWallet` action.
- **The live implementation (`0x4AA02a23…`) does not contain the selector
  `0x99630f7d` at all; the staged one (`0x8C4f85c6…`, the timelock's upgrade
  target) does.** A dispatcher miss is exactly an empty revert. The mirror's
  last restore left the beacon on the older implementation.
- Repair: `upgradeTo(0x8C4f85c6…)` on the beacon `0x9b1F08…` as its owner (the
  timelock), executed early on the mirror because every unified creation was
  blocked. The exact batch calldata then returns success, the keeper's retry
  lands, and the loan reaches `Active`. All four agents on that beacon
  (`0x8ff0…`, `0x2a48…`, `0x609e…`, `0xee98…`) still read `owner`/
  `portfolioId` after the upgrade; the agent on the other beacon
  (`0xacfb…`) was untouched.
- Consequence for the tooling: **`registerAgentWallet` cannot be exercised
  against a downgraded beacon** — any "register rejected" measurement taken
  before this repair was a dispatcher miss, not a policy refusal.

**Venue term while positions are open (measured):** `V = spot_total − spot_hold
+ perp_equity`. The new loan read `87.141` flat, then `82.449` the moment three
positions opened (hold `4.643`) — HF `1.6697 → 1.5796` — and the term did not
recover on a hand-back because the margin stayed reserved. Locked margin is
not lender collateral; that is the correct, and economically load-bearing,
shape.

**Tool gap found:** `draw` cannot resolve a cross-chain unified loan's
collateral reference ("the unified loan projection is account-scoped: no
collateral asset"), so a draw against a chain-1-collateral loan is not
drivable from user-sim yet.

### Two systemic gaps the repair exposed (2026-11-22)

1. **The mock MPC regime records no account.** `underwriter_db_hyper.mpc_portfolios = 0`
   while ten unified loans trade: the `keygen_at_quote` branch derives the key and
   carries the registration on the quote, but skips `mpc_accounts::record`. So
   `GET /v1/me/prime/unified` lists nothing for any wallet, and every
   account-scoped read must guess its `{ref}` (the draw had to be given the
   collateral explicitly). One record per account at quote time — the same row
   the interactive path writes — makes the list, the views and the tooling agree.
2. **No boot-time selector preflight.** The beacon sat on an implementation
   without `registerAgentWallet` and only the prover noticed — ten retries, a
   cancel, a stranded principal. Comparing the selectors the prover composes
   against the live implementation's dispatcher is a cheap startup refusal that
   would have named this in seconds.

**Both fixed in source 2026-11-22, and the draw wait too:**
`f077a054` records the account at mock quote time (same parts, same CM address,
same registration the quote carries); `39846403`/`cc0e6d97` give the opencode
lane the account scope, the trim-row debt fold, and the 1,000-raw truncation
tolerance the settle wait lacked; `e55ee49f` fixes the correlation expansion
below. The underwriter and operator rolls carry them.

## The full cross-chain unified lifecycle, live (2026-11-22)

One loan (`0x51bb077e…`, 0.1 WETH-1 collateral, USDC-999 principal) driven from
user-sim end to end while the venue book was traded:

| Step | Measured |
| --- | --- |
| Collateral + bridge | 87.158 USDC landed, relay clean |
| Flat health | debt 87.141, HF 1.6697 |
| Draws (3, during runtime) | 87.141 → 96.639, each verified by its own settle wait; HF 1.4945 |
| 3-dex book | ETH 0.01 + `xyz:NVDA` 0.1 + `para:SMH` 0.1; margin used 6.26, spot hold 6.287 |
| Venue fold while held | V = 96.639 − 6.2871 + (−0.1162) = 90.2357 = the operator's `venue_equity_usd`; term 85.136 (equity − 5.0997 maintenance); HF 1.3866 |
| Resize during runtime | `recallFromCore` 1 USDC out of free spot (90.35), debt 96.6396 → 95.6396, Active in 3 s |
| Flatten | all three dexes flat, spot 94.6378 |
| Close | recall-all (in-kind gas) → shortfall 1.002454 → owner top-up on 999 → `Closing` → `PendingCloseUnwind` → **`Closed`** in 68 s; collateral 0.1 WETH-1 released to the wallet |
| Close, second loan, after the venue fix | shortfall **0.001228 USDC** — exactly the deducted bridge gas — funded from the wallet, `Closed` in 62 s: the in-kind rule is confirmed to the micro-dollar |

### Bugs this lifecycle exposed (all fixed in source, two pending a roll)

1. **A WETH-escrowed loan holding an ETH perp refused every requote and draw.**
   The canonical hedged book. `portfolio-margin value refused —
   CorrelationDimension { declared: 2, expected: 3 }`: the operator collapses
   same-series factors (WETH escrow ≡ ETH perp) but labelled the matrix with the
   *series* count while the PM validates one factor per declared leg. The
   expansion now repeats the series' row/column back into the declared space
   (`e55ee49f`, unit-tested, 565 operator tests pass). **Ships with the next
   operator roll.**
2. **`recallAllFromCore()` could never move a full venue balance.** The
   Core→EVM gas was charged *on top* of the transferred amount; the mirror's
   normal account holds no spot HYPE, so the fee is always priced in the moved
   token and a transfer of `total − hold` was refused forever
   (`94.63902833 needed … 94.63780003 free`). Every close of a venue-funded
   unified loan was blocked. The venue now takes the gas **in kind** (the real
   venue's rule) for the exact-full case only; an over-request is still a
   shortfall refusal (`bf1b17f`, rolled as `relay-bf1b17f`, 390 tests, sweep
   13/13). The bridge's own fill times are the **chain** clock (+1 h vs the
   host here) — correct for a mirror, but a reader must not mistake it for wall
   time.
3. **The close's presence test was stricter than the bridge.** It demanded the
   exact expected amount and called a landed recall a failure once the in-kind
   gas was deducted, and it waited 30 s for a creation deposit when a previous
   attempt had already swept the venue (the cash was on the agent). Both fixed
   (`09dff59e`): 1 bp gas allowance (≈70× the measured gas, at least $0.005),
   and "empty venue + cash on the agent" is *nothing to recall*, not a missing
   bridge.

### Multi-loan, multi-product session (2026-11-22 01:2x–01:47)

Two wallets, two products, both cross-chain (WETH-1 collateral, USDC-999
principal), books traded concurrently and closed:

| Loan | Product / wallet | Runtime ops while the other book traded | Close |
| --- | --- | --- | --- |
| `0xaf0e4316…` | unified, wallet 1 | short 0.02 ETH; **draw +1** landed with the position open (capacity was the binding 1.03 — the stress line, see the hedge section) | recall-all → `Closed` |
| `0x473545fe…` | simple, wallet 2 | long 0.08 `xyz:NVDA`; **resize −1** deferred (`insufficient EVM balance for the in-batch repay`), settled 45 s after the agent was funded: 17.3236 → 16.3236 | `Closed` in 35 s |

Final `venue-portfolio` over both accounts: A 23 fills, fees 0.2561, realized
−0.1325, flat; B 2 fills, fees 0.0019, realized −0.0024, flat, 3.00 USDC left in
its spot (the owner's, recallable).

**Three tool/ops gaps this session exposed:**

1. **No `fund-venue` command.** The deposits differ per product — the unified
   agent takes `depositToCore(uint256)`, the simple agent
   `depositToCore(uint256,uint32)` (the dex; `0xFF..FF` = spot) — and user-sim
   has neither. A simple book additionally needs the cash moved to the *trading
   dex*: a simple account is **not** unified (only `UnifiedMarginAgent` sends
   `setAbstraction(2)`), so spot does not back its perp margin — the order lane
   refused `xyz:NVDA` with 17 USDC in spot until `sendAsset spot→xyz` moved 12.
   The canary's "fund-venue" was always a hand-built tx.
2. **The simple resize prep does not self-serve.** The unified prep recalls the
   shortfall from the venue; the simple prep plans none, so with 0.32 USDC on the
   agent and a 1 USDC repay the proof deferred until the owner transferred cash.
   It should plan the same recall (the simple agent has
   `recallFromCore(RecallSource[], uint64)`).
3. **A pre-funded CM blocks its own creation forever.** The wallet's WHYPE CM
   (`0xD9F42f…`) holds 100 WHYPE from an earlier session, so the same-chain
   WHYPE loan refuses at Phase1 ("a first creation must start empty; pre-funding
   the CM bypasses the creation fee") — the guard is right, the stranded pot has
   no owner path back (no loan exists to recall against). Worth a decision: an
   owner sweep for an empty-book CM, or a documented "this collateral is spent".

### The 2026-11-22 audit: five findings, one fix rolled

**Market-data fidelity re-measured and confirmed excellent.** Six samples
across `BTC/ETH/SOL/xyz:ANT/para:SMH/xyz:XYZ100`: mid Δ 0 bp p50/p95 on every
coin, spread identical to the real venue to the fourth decimal, 10 bp book
depth within 0.1–2 % of real, and the mirror's book age ~300 ms — *fresher*
than the real API's ~500–700 ms. The mirror claim is a measurement now, not a
design note. Funding Δ ~1e-8, mark Δ 0. `para:SMH`'s 7.99 bp spread and $100
10 bp depth are the real market's thinness, not a sim artifact.

**1. A `Closing` leg proposed Enforcement (fixed, rolled as `rel-19415ef7`).**
Every ordinary close left the position in `Closing` while debt was still whole
and the collateral/venue term was already moving; the HF scan's group fold read
that transient state, crossed the breach line and queued an Enforcement
proposal the orphan monitor then cancelled. **Measured: 36 cancelled proposals
across 5 ordinary closes** (10/6/8/6/2). The fix skips the proposal for a
`Closing` leg — the close path is authoritative — while the blind scan still
observes it. `Active`/`Liquidating` remain the proposal sources.

**2. Multi-loan portfolios are same-chain only, and w1's stranded CM blocks
the one same-chain unified loan.** `create-unified` on an existing cross-chain
portfolio refuses: *"an append requires a same-chain Direct portfolio"*. The
append path needs the WHYPE-999 CM, which still holds 100 WHYPE pre-funded in
an earlier session — no owner path back (`push` is proof-executor only; no
sweep exists for a position-less CM). **The stranded pot now blocks the
multi-loan demonstration, not just the same-chain one** — that is the strongest
argument yet for the owner-sweep decision.

**3. Simple margin cannot draw** ("draw requires a unified-margin agent —
simple margin has no venue deposit") — by design, and now demonstrated. A
simple book's runtime ops are resize (repay) and close only. Its venue cash is
the owner's, moved by hand.

**4. The simple resize prep still does not self-serve** (re-confirmed live).
Loan `0xca2590c3…` (simple same-chain): the prep approved the agent's USDC to
the proof executor and accepted the resize, then the proof deferred with
*"insufficient EVM balance for the in-batch repay"* — the agent held 0.15 USDC
against a 1 USDC repay while 12 sat on the venue's `xyz` dex. It settled 50 s
after a manual top-up. The unified prep recalls the shortfall; the simple prep
plans none, though the simple agent has `recallFromCore(RecallSource[],
uint64)`.

**5. A short position id is left-padded into a misleading 404.**
`parse_hex_id` left-pads any id shorter than 32 bytes, so a truncated id reads
as a valid `MarginId` and the gateway answers *"position not found:
0x0000…ca2590…"* — the caller cannot tell a typo from a missing loan. Either
refuse short ids for `position_id` paths or name the padding in the error.

**Multi-loan, multi-product session (02 loans + 1 multi-loan portfolio, all
closed):** unified cross-chain `0x562fb541…` (short 0.007 ETH, draw +1, resize
−1, all with the position open, closed after a reduce-only flatten), simple
cross-chain `0x38fec111…`, simple same-chain `0xca2590c3…` (NVDA long) — the
two simple loans share one agent, so one venue account carried two concurrent
positions on two dexes (a real multi-loan venue book). Multi-PnL:
account A 24 fills / fees 0.2614 / realized −0.1325 / unrealized +0.0036;
account B+C 4 fills / fees 0.0078 / realized −0.0024 / unrealized +0.0065.

### The 2026-11-22 second audit: information flow, liveness, and the control plane

**The venue control plane exists and is not exposed.** Port 8091 carries
`/control/{deal,order,advance,crash,rally,stall,set_book,set_funding,
set_account,clear_account,clear_fixture,state/{address},reset,epoch_new}`.
Only 8090 is NodePort-exposed (31090); 8091 is cluster-only, so a host-side
scenario runner cannot drive controlled scenarios without a port-forward. This
is the missing piece for the "scenario matrix": `advance` crosses funding
boundaries and runs the liquidation waterfall, `crash`/`rally` move mark+oracle,
`set_book` scripts depth. **Do not advance the shared clock** while the nine
live fixtures hold positions (it charges their funding and can liquidate the
HF-1.03 book); a scenario run needs a dedicated epoch/account, not this mirror.
Recommendation: expose 8091 as a NodePort (additive) or document the forward.

**Funding is modelled faithfully — confirmed, not assumed.** `sim/funding.rs`
accrues one *whole hourly* payment per crossed boundary at the live,
oracle-priced rate, from the account's stamp to now; 14 funding tests pass. A
position held under an hour pays nothing — the real venue's rule, so a
short-horizon strategy's PnL is faithful (it simply never crosses a boundary).

**Venue-account ticks are refused on a spot/perp read-skew.** Measured on the
unified agent `0xA2dF…` at 02:05:54: the indexer paired a *post-recall* spot
total (0.5) with a *pre-flatten* perp hold (0.59045) and hit its own
`hold ≤ total` invariant, recording a gap. The value is never wrong (fail-
closed), but the producer composes a spot snapshot and a perp snapshot taken at
different instants. Root cause: non-atomic snapshot composition, not the venue.
A re-read within a small skew window (or clamping a transient over-hold) would
keep the series continuous.

**The position-book fold drops closes whose opening predates its window** (6 in
2 h, including loan `0x38fec111…`). It deliberately refuses to invent an
opening block and advances the cursor, so `position_state` goes stale for those
positions. The operator's `positions` table stays authoritative; only the
indexer's read model is affected.

**Two sale-cost gaps in the quote path.** `swap rate fetch failed … class
no_pool` (8 in 2 h) — a collateral token (`0x6a8E…`) has no pool to sell the 1 %
fee/liquidation into; and the underwriter logs `escrow sale: re-priced at the
measured cost (worse than the indicative prepare figure)` (4). The indicative
prepare is optimistic by the measured sale cost, and an unpriced token has no
sale at all.

**Engine freshness probe is transient, engine is healthy.** 7 × `venue engine
UNOBSERVED` in 6 h came from `engine:8080/status` being momentarily unreachable
while the engine itself followed at lag 0–4 blocks (4 804 windows applied). When
it happens, marks are held last-good and pollers write gaps — the fail-safe, but
worth a retry before declaring unobserved.

**Top-up verified end-to-end.** `top-up --amount 0.05` quoted `total_pull_wei =
0.05/0.99 = 50505050505050506`, approved exactly that, pulled exactly that from
the wallet, credited the CM the net 0.05 (0.12 → 0.17), and sold the
0.00505 fee in-kind. The 19 Nov "approve exactly, empty revert" bug is gone: the
tool now reads the gateway's total-pull quote. The remaining question is policy,
not mechanics — a top-up is charged the 1 % *creation* fee.

**Post-fix close is clean:** a simple same-chain create→close produced zero
Enforcement proposals and zero cancellations under `rel-19415ef7` (pre-fix the
same path produced 6).

### The two-arm equity fix — implemented, rolled, and live (`rel-f540696d`)

The decision recorded above is now code. The producer's served
`account_equity_usd` is the *post-hold* spot component, so feeding it to a fold
that also subtracts `venue_maintenance_usd` charged the held margin **twice**:
the hold is still the account's own money (it backs the position and returns on
close), and on the mirror the hold equals the maintenance line, so the charge
was exactly 2 × margin. That is the hedge measurement's unexplained
`4.2 + 1.69 ≈ 7.0` of 9.86.

The fix: the projection now serves the raw `spot_collateral_hold_usd` (it was
persisted but never served), and the PM input prices the **pre-hold** equity —
`whole_account_equity + spot_hold` — leaving the waterfall's one maintenance
deduction as the only charge. Forced close realizes the hold back into cash, so
`pre-hold − close cost` is the right forced figure; going concern deducts
maintenance once, so `pre-hold − maintenance` is the survivable figure. The HF
breach arm keeps the conservative post-hold figure (`whole_account_equity_usd`
is untouched) — exactly the two arms the decision called for.

Tests: client 184, operator 565, indexer 292 — all green. Rolled as
`rel-f540696d` (operator + indexer; pins `6788a074`).

**Live A/B on one book** (unified `0x79d91f36…`, 0.04 WETH escrow, 34.66 USDC
bridged, debt 34.66): flat, `capacity_equity 61.488`; short 0.01 ETH opened
(notional 16.69, spot hold 0.8344): `capacity_equity 61.282` — a **0.34 %**
move, where the pre-fix double deduction would have charged 2 × 0.8344. The
HF-side `venue_equity_usd` correctly stays the conservative post-hold breach
figure (34.266 → 33.416, the hold removed once for the breach comparison). The
book was then flattened and closed cleanly.

### Reality check against the 19 Nov report's blockers

The report's four standing complaints, checked against today's mirror rather than
assumed:

1. **"No order path"** — the path exists and was used for every trade in this
   round: the **venue-sim** answers `/info` and `/exchange` on NodePort **31090**
   (the gateway is the *credit* plane and has no trading routes by design).
   Spot pairs are addressed the venue's way: `PURR/USDC`, and `@107` = HYPE/USDC,
   asset id `10000 + pair.index`; `venue-order --coin @107` resolves it. What the
   report could not do on generation `20261119m` is drive *venue* positions from
   the loan's account — that works now (`venue-order/venue-account/
   venue-portfolio`), and the three-dex book in this round was real venue state,
   not paper.
2. **"The Unified agent refuses `registerAgentWallet`"** — by design: only the
   proof executor may register, and under the mock regime the registration rides
   the quote, so the account's trading key is the quote signer's key. A separate
   strategy key needs the interactive MPC path, not an owner call.
3. **"One stuck loan degrades the whole gateway"** — measured today with 9 live
   positions and two operator anomalies: gateway `healthy`, `advisories []`,
   operator anomalies served as named advisories (the route-scoped health work).
   A stuck position is still a *blocking* fact for the operator's own status when
   it sits past the action grace, but it no longer 503s the borrow plane.
4. **"recallAllFromCore reverts ZeroAmount right after Active"** — the async
   CoreWriter deposit; the close prep now waits for the venue classes to appear
   (30 s settle window) before it reads them, and the two closes in this round
   had no such revert.

### The hedge measurement (operator rolled, 2026-11-22 01:2x)

With the correlation expansion live, the canonical book was measured on a fresh
cross-chain loan (`0x39cecb6d…`, 0.05 WETH-1 ≈ $87.65, debt 43.31, HF 1.6756):

| Book | venue equity | venue term | HF | stress capacity |
| --- | --- | --- | --- | --- |
| flat | 43.13 | 43.13 | 1.6756 | **9.86** |
| matched hedge: short 0.05 ETH | 38.85 | 37.16 | 1.5379 | **2.82** |
| double long: long 0.05 ETH | 38.76 | 37.08 | 1.5359 | **0.00** |

So the model does recognize the hedge (2.82 vs 0.00 for the same notional), but a
delta-neutral book still costs **7.0 of a 9.86 line**. The decomposition from the
venue's own numbers (spot total 43.308905, hold 4.216 = `totalMarginUsed`, perp
accountValue −0.1528):

- `hold` 4.216 — the reserved margin, subtracted from spot **and** not present in
  the perp account value;
- venue maintenance 1.686 — charged again by the PM;
- basis + direction stress ≈ 1.1.

**The first two are a double deduction of the same margin.** `whole_account_equity_usd`
documents its two components as disjoint "because `perp_equity_usd` already
contains that hold" — but the mirror's perp `accountValue` does not
(`totalRawUsd` is the position's own notional and `accountValue` is fees only), so
the sum `(spot − hold) + perp` loses the reserved margin outright. Measured: the
account's true equity is 43.14, the operator's base is 38.94.

The correct shape is the one the PM already has two arms for:
- **forced close** (`margin_value`): `spot − hold + perp` — unwinding consumes
  the margin;
- **going concern** (`capacity_equity`): `spot + perp − maintenance` — the hold
  is still the account's cash, and the venue execution line is the maintenance
  the model already charges.

That needs the input to carry the live equity beside the forced-close one
(today `venue_equity_usd` is both). Until then the mirror systematically
under-lends a hedged book — and a hedged book is the product.

### The open coverage gap: the mirror prices 2 of the venue's 11 dexes

Measured in `risk_db`: `venue_calibration` holds **46 instruments** — main
(BTC, ETH, HYPE, SOL) and `xyz` (42) — while `/info perpDexs` lists **11
dexes** (main, xyz, flx, vntl, hyna, km, mkts, para, abcd, cash, io) and the
venue accepts orders on all of them. The consequence is not cosmetic: a book
holding one position on an uncalibrated dex refuses the **whole account's**
portfolio-margin value ("the venue asset's configured vol_floor_annual/
vol_ceiling_annual band is unavailable"), which is exactly what a `para:SMH`
leg did to the live loan (682 operator WARNs in 10 minutes, and the gateway's
`-4024` refusal on every requote). Two coherent ends, one must be picked:

- **tradeable = priceable**: the venue refuses orders on instruments the mirror
  does not price, so a strategy cannot trap its own credit line; or
- **price everything**: add the missing assets and their measured bands and let
  the calibration pass cover the other nine dexes (≈200 instruments — the book
  poller and the hourly fits are the cost).

Doing neither is the current state: every venue market is tradeable, and most
of them make the loan unpriceable after the trade. Until it is chosen, the
honest scenario rule is: trade main and `xyz` only.

### Economic property measured while the book was open

Draw capacity is not linear in the venue notional: a 13 USD *long* `xyz:NVDA`
perp on a 175 USD escrow took the PM's `stress_capacity` line from 15.14 to
**0**, while the flat realizable value moved only 2.40 (the 1σ venue stress).
The binding arm is the venue-liquidation **tail**, not the grid: an unhedged
long perp adds directional risk to a long escrow, so the lender stops lending
through the book. That is defensible — but it means the *hedge* case had to be
measured with the same-series perp, which bug 1 blocked. After the operator roll
the experiment is: WETH escrow long + ETH perp short → the line should stay
near the flat 15.1 (basis-only risk). Any other result is a tail-model problem.

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
