# Architecture decision records

One page each: the context, the decision, the consequences. Every record names
the measured fact that drove it - the repo's rule is that code + rustdoc are
truth, and a decision that cost a real run gets a record here.

| ADR | Decision |
|---|---|
| [0001](0001-two-engine-binaries.md) | Two engine binaries over one profile-routing binary |
| [0002](0002-chain-instance-canary.md) | The chain-instance canary over cursor-hash reset detection |
| [0003](0003-epoch-rule.md) | The epoch rule: clear on a new pin, restore on the same pin |
| [0004](0004-sim-served-identity.md) | The simulator's served identity is computed, never seeded |
| [0005](0005-bridge-block-atomicity.md) | The bridge applies a block's effects and its cursor atomically |
| [0006](0006-ws-first-market-data.md) | WS-first market data with a budget-gated REST backfill |
| [0007](0007-cctp-devnet-lanes.md) | The CCTP lanes: real contracts, a substituted attester, rewind parking |
| [0008](0008-base-anchored-validation.md) | The base is the anchor: scans, cursors and applies clamp at the fork pin |
| [0009](0009-venue-unshadow-truth.md) | The venue's unshadow truth: the pin's window, the journal's before-values |
