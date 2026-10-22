# 0006 — WS-first market data with a budget-gated REST backfill

**Status:** accepted (R3).

## Context

The public venue API allows 1200 weight/min/IP, with measured per-endpoint
weights (`metaAndAssetCtxs`/`meta`/other info = 20; `l2Book`/`allMids`/
`clearinghouseState` = 2), and daloc's per-snapshot read costs N+3 requests
(model, perp state, perpDexs, one per dex, spot). Naive polling 429s; a 429
during a test run is an outage.

## Decision

WS is the primary freshness source: subscriptions to `allMids` and
`activeAssetCtx` (the server must send a message within 60 s, so the client
pings every 30 s; a dropped connection reconnects with exponential backoff
1 s -> 30 s). A REST poller backfills `metaAndAssetCtxs`, `spotMeta`, one
`l2Book` per configured market, and `allMids`, gated by a rolling-60 s budget
meter: a call whose weight does not fit is skipped (never queued) and counted.
Per-coin staleness gauges feed the readiness gate.

## Consequences

- The default 5 s poll of four markets costs ~600 weight/min at the measured
  weights (metaAndAssetCtxs + spotMeta at 20 each, four books + allMids at 2
  each), leaving headroom for the passthrough reads (untouched accounts), which
  share the meter.
- WS exhaustion degrades to cached REST values behind the staleness gate - a
  named, observable, bounded degradation, never a silent stall.
- The weights and the capacity are code constants (a weight change is a
  protocol change), not configuration.
- `venue_sim_stale_ms{coin}` is the freshness signal; `/readyz` gates on it.
