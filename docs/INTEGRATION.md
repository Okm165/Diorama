# Trading on the mirror's venue: the integrator's path

This is the document the venue's own refusal messages cite. The mirror is a
simulator that mirrors the real venue's `/info` and `/exchange` surface.

## Endpoints

- **`/info`** — every documented read: `meta`, `perpDexs`, `allMids`,
  `l2Book`, `clearinghouseState`, `openOrders`, `historicalOrders`,
  `orderStatus`, `userFills`(ByTime), `userFunding`, …
- **`/exchange`** — every documented action: `order` (Ioc/Alo/Gtc), `cancel`,
  `cancelByCloid`, `modify`, `batchModify`, `updateLeverage`,
  `updateIsolatedMargin`, `scheduleCancel`, transfers, `approveAgent`,
  `approveBuilderFee`.

Both are served on the venue service (NodePort in the deployment; the
Funnel host serves them externally too).

## Markets

The venue trades **every market the real venue lists**: the default perp
dex, every builder dex the `perpDexs` registry names, and the spot pairs.
A coin of an advertised dex resolves like any other; its book fetches on
demand. A listed coin with no depth answers the venue's own empty-levels
book shape — the unknown-coin 500 is reserved for coins nothing lists.

## Books

The served book is the venue's honest composition: live depth ⊖ the levels
your fills consumed ⊕ **every participant's resting orders** — including
yours (place a Gtc and read `l2Book` to see it). The venue caps the book at
20 levels per side, exactly as the real venue's API does; a taker larger
than visible depth fills the visible book and the remainder follows its TIF.

## Refusals you will meet (by design)

- "open-interest caps are not known yet" — the dex's cap list has not
  landed; retry after the venue's first poll cycle.
- "the market plane is warming up" — the plane has not loaded its first
  meta yet (boot); retry.
- Unified-account refusals ("Action disabled when unified account is
  active") — parity with the real venue's rules.

## Fee rule the docs must state

Taker 0.045% / maker 0.015% perp (spot 0.070% / 0.040%), charged per fill.
Spot makers are charged the fee **in the received token**. The same
constants apply to builder dexes unless the dex sets deployer fees.
