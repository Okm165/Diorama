# Operations

## Signals and thresholds

| Metric (`/metrics`) | Threshold | Action |
|---|---|---|
| `mirror_reth_head` | not moving | top-up stalled: check `logs-engine` for "top-up failed" (reth unreachable, beacon API down, or a gap the CL cannot serve) |
| `mirror_source_sync_gap_blocks` | > 0 for long | reth is catching up to the CL head via the Engine API (~10-11 blocks/s measured); a stuck gap means the CL's backfill has not reached the gap window yet |
| `mirror_source_sync_delivered_total` | rate 0 while `gap_blocks` > 0 | the top-up loop is dead or erroring - it is supervised, so check `logs-engine` |
| `mirror_lag_blocks` | > 60 | `follow_head - cursor_t`. A steady 2-3 is the scheduling margin (`reth.follow_margin_blocks`); a *growing* value means the applier is behind - check reth/CL health and the engine log |
| `mirror_lag_blocks` | > `max_gap_blocks` (300) | engine pauses itself (the plan's "> 300 → pause and re-sync"); re-pin drill or restore reth. A cursor that was just rebased to a fresh pin is exempt and may re-apply its whole pin window (bounded by reth's 10,064-block state window) |
| `mirror_t_convergence_failures_total` | increase | T's state ≠ mainnet at the parent block just applied, on a probe whose target the devnet does *not* own (shadowed targets are skipped by design). A missing BAL entry, a watchlist gap, or a clock offset left set (`/admin/time`). A deliberate manipulation whose target is shadowed does *not* fire it; one that is read *through* an unowned contract (e.g. crashing a Chainlink aggregator behind its proxy) does, until restored - the counter is cumulative, `mirror_t_convergence_last_failed` shows the current state |
| `mirror_full_sweep_mismatches_total` | increase | a full-sweep apply mismatch (`verifier.sweep = "full"`): T's value ≠ reth's at the applied block for a key that was applied. Inspect `drift.jsonl` and re-run with the sweep for a conformance window |
| `mirror_reorgs_detected_total` | any increase | upstream reorg: the applier continues on the new branch (absolute writes self-heal); re-pin only if it keeps recurring |
| `mirror_t_fork_age_blocks` | > ~8000 | T's fork base is nearing reth's 10,064-block state window: below the fork block every read is served by reth, so once it falls out of the window T can no longer read slots it has not written. Re-pin (`make mainnet-repin`) - the fresh pin starts a new epoch: the devnet's writes are gone with the old base (re-create positions from your journal) |
| `mirror_pin_age_blocks` | informational | blocks since this epoch's first parent block (the journal/cursor marker) |
| Anvil RSS (kubectl top) | see table below | restart the pod; the engine rebases and re-applies |

Anvil memory is a *state* cost, not a leak:

| Chain | Growth | Steady state | Notes |
|---|---|---|---|
| `anvil-t` | ~0.7 MB per applied block in steady state; a full drill run (scenario + proof) adds ~300 MB | ~6.5 GB after a session of drills, recycles at the 16Gi limit | watch-list writes only (~90-500 per block) plus the drills' own writes and the `--prune-history 32` state snapshots. A restart re-forks T: the devnet's writes are gone and the engine clears their shadow marks |

**T's pin is re-resolved on every container start**: that bounds the
post-restart re-apply to `FORK_OFFSET` (~128 blocks) instead of the pin age.

**A T restart starts a new epoch.** The engine detects the reset (the cursor's
block hash no longer matches) and, when the pin changed, clears the shadow
registry - marks *and* values (WARN + `mirror_t_resets_total`). The devnet
starts clean on the fresh base; **re-create your positions from your journal**
(the plan's 3.6 venue-restart discipline). Restoring instead is what mixes
eras: measured, a +30d time-travel write came back as a future
`lastUpdateTimestamp` and every Aave supply panicked with an underflow. A
reset at the *same* pin restores the recorded writes, and orphan values (keys
handed back earlier) are always pruned, never re-owned.

**The anvil state dump is disabled.** `--state`/`--state-interval`
writes per-block historical state: a T dump grew to 13 GB and never completed,
leaving orphaned `~/.foundry/anvil/tmp/anvil-state-*` dirs that filled the 40Gi
PVC (measured: 3 dirs, 41 GB). State is rebuilt instead - a fresh pin re-applies
the pin window, and T re-applies its parent diffs from its own pin. If tmp dirs
ever reappear: `kubectl -n mainnet-mirror exec deploy/anvil-t -- rm -rf /state/.foundry/anvil/tmp/*`.

## The symmetric log surface

Both mirrors log the same *shapes* at the same levels; the steady-state
evidence beats every minute on both sides (all measured live):

| Line | Level | Cadence | Where |
|---|---|---|---|
| `parent diffs applied to T` | INFO | per applied block | mainnet engine |
| `source window composed onto V` (carries `source` + `source_head` + `lag` + `watched`) | INFO | per composed window (~4 s) | venue engine |
| `probes complete {block, ok, failed, skipped}` | INFO | per applied block (T) / per re-pin (V) | both engines (one `probes::report`) |
| `follow status {mirror, via, source_head, followed, lag_blocks, applied, guard}` | INFO | 60 s | both engines (one `FollowStatus`) |
| `market plane live {coins, via, v_bridge_lag, accounts, owned_levels, identity_violations}` | INFO | 60 s | venue sim |
| `bridge status {pending, signed, relayed, parked}` | INFO | 60 s | cctp-bridge |
| `sim order filled` (carries `levels` + `book`) | INFO | per fill | venue sim |

The DEBUG tier is per-pass diagnostics: `chain is at the follow head` (the
steady line - it only fires for an *idle* source, which the venue's live
HyperEVM never is), `probe skipped: target is devnet-owned` (one per skipped
probe), the mined-tx lines (INFO on both - the same event).

Why the heartbeats exist: the audit that motivated them measured 1697 DEBUG +
155 INFO lines on the mainnet engine against **7 DEBUG** on the venue engine
over the same 20 minutes - the venue *was* following live (source head within
3-5 blocks of the public HyperEVM head, windows every ~4 s, 0 catch-ups) but
no line said so, because the only steady line is DEBUG and only speaks for an
idle chain. When the live following is ever in doubt, read `follow status`
(identical fields on both mirrors) and `market plane live` (each coin's mark
and its age in ms; `owned_levels` are the order lane's consumed book levels -
the COW analogue, pruned as the live book moves past them).

The surface is itself a tested invariant: `make log-surface` attests it
against the running services - both heartbeats fresh and field-identical, the
venue's windows advancing, the mainnet's probe summary fresh, the sim naming
every configured coin with zero identity violations, the bridge's counts, and
every heartbeat at INFO. State suites cannot catch a silent surface, so the
surface has its own suite.

`make attest` is the routine that runs it: one pass over both mirrors (every
pod once, the key live facts per service, the recent WARN/ERROR catalogue)
followed by the asserting suites (`log-surface`, `engine-symmetry`) - the
standing "monitor all services" command.

The order lane's fills are self-explaining too: `sim order filled` carries
`levels` (the consumed book levels, `price x size`) and `book`
(`live@<age>ms` for the live venue's depth, `scripted+live@...` for a
scenario-owned base with its live remainder) - a fill can never be mistaken
for an invented price.

## Metrics reference (complete)

Every metric the engine emits, so nothing is silent. The action table above
covers the ones that page someone; these are the rest.

| Metric | Meaning |
|---|---|
| `mirror_reth_head` / `mirror_reth_follow_head` | reth's head / the follow head the engine schedules against (raw, before the replay margin) |
| `mirror_source_sync_gap_blocks` / `mirror_source_sync_delivered_total` | the CL-head gap reth's top-up still owes / blocks it has delivered |
| `mirror_head` | the devnet chain's height (T drifts ahead: one block per parent block plus one per devnet batch) |
| `mirror_cursor_block` | the last *parent* block applied (the honest progress signal) |
| `mirror_t_fork_age_blocks` | how far T's fork base is behind its head (reth serves reads below it) |
| `mirror_t_resets_total` | times T was found re-forked (its OOM restart / a re-pin): a new epoch starts (the devnet's writes are gone with the old base) - re-create positions from your journal |
| `mirror_lag_blocks` | `follow_head - cursor_t` (2-3 in steady state = the scheduling margin) |
| `mirror_halted` | 1 while `/admin/pause` holds both loops |
| `mirror_blocks_applied_total` | parent blocks applied to T |
| `mirror_last_apply_seconds` | duration of the last T apply |
| `mirror_pin_age_blocks` | blocks since the epoch's first block (re-pin before reth's 10,064-block window) |
| `mirror_bal_writes_last_block` | watch-list writes applied to T in the last applied parent block (a gauge: this is a per-block value, not a cumulative total) |
| `mirror_bal_filtered_out_last_block` | BAL entries for accounts outside the watchlist in the last block (skipped by design) |
| `mirror_bal_skipped_shadowed_last_block` | BAL entries skipped in the last block because the devnet owns the key (copy-on-write) |
| `mirror_shadowed_slots` | devnet-owned storage keys (see `/status.shadowed` for the accounts) |
| `mirror_shadow_scanned_blocks_total` / `mirror_shadow_skipped_empty_blocks_total` | T blocks the shadow scan walked / the empty share of the walk (one call, no traces — the harness mines one block per poll, so this is the scan-cost reading) |

The shadow scan fails **closed** on a block whose trace it cannot get
*transiently*, and retries from the last clean block: an owned-key registry
that skipped a block would let parent diffs clobber devnet writes silently.
A cursor that falls further behind than anvil's `--prune-history` window is a
different case — the pre-state is gone for good, so the scan **names and
skips** those blocks: the window's edge is found with ~a dozen zero-slot
storage probes and the walk resumes there (one summary per walk, the watermark
advances; measured: an 8k-block lag would have been hours of
one-call-per-block walking, while the stuck scan re-logged one block 150 times
in five minutes and 500'd every hand-back, with the re-pin as the only way
out). A transient fault still
answers **503** (retryable, the block named in the body — `Error::ShadowScan`
is in the shared unavailable taxonomy), and the epoch reset remains the
operator's remedy: `POST /admin/epoch` re-forks T at the source head, the
registry clears, and the scan re-anchors at the new pin (measured live: after
the re-pin the same hand-back answered 400, the same as V's). Keep the scan's
cursor within the prune window by letting it run — a long engine stop with the
harness still mining T is the way to fall behind.
| `mirror_t_convergence_ok_total` / `mirror_t_convergence_failures_total` | the same probes against T at the applied parent block (the plan's "probe parity on T after context alignment"); devnet-owned targets are skipped, and the check is suspended while a `/admin/time` offset is set |
| `mirror_t_probe_skipped_total` | informational | probes skipped because their target is devnet-owned - a devnet write to a shared contract (e.g. the Aave oracle) hides *all* of that contract's probes until it is handed back. If it grows while you are not writing, inspect `/status` `shadowed` and unshadow the venue |
| `mirror_t_convergence_last_failed` | how many T probes diverged in the last applied block (0 in steady state) |
| `mirror_full_sweep_mismatches_total` | full-sweep parity mismatches (`verifier.sweep = "full"`); should stay 0 |
| `mirror_reorgs_detected_total` | parent-hash discontinuities seen by the applier |

## Day-to-day

```bash
make status            # pods, PVCs, engine status JSON
make images            # re-import the pinned images into k3s (after a bump)
make mainnet-backfill          # CL backfill progress + lighthouse datadir sizes
make logs              # tail everything
make logs-engine       # one component (also: logs-reth, logs-lighthouse, logs-anvil-t)
make mainnet-smoke             # endpoint checks + parity spot check on T's head vs reth
make mainnet-scenario          # self-containment + T-vs-reth parity + devnet workflow
make pin-show          # current fork pins
```

Debug logs: `RUST_LOG` filters at boot, and its targets match by **literal
prefix** — a `mirror_*=debug` wildcard is a silent no-op (measured: zero debug
lines with the filter looking right). Name the crates, scoped to what you are
reading:

```bash
kubectl -n mainnet-mirror set env deploy/engine \
  RUST_LOG='info,mirror_mainnet=debug,mirror_devnet=debug,mirror_anvil=debug,mirror_config=debug'
```

`info` is the shipped default (the manifests carry no `RUST_LOG`), and the
imperative env is lost on a kustomize apply — re-set it after `make apply`.

## The engine image and its manifest travel together

The engine reads the **instance TOML** (mounted from the `mainnet-instance`
ConfigMap); the previous image read the retired `engine-config` YAML. Rolling
out the workspace-built image therefore pairs the image import with the
manifest change:

```bash
make build && make import        # the workspace-built image (workspace root)
make diff                        # expect: engine-config gone, mainnet-instance mounted
make apply && make deploy-engine
```

A rolling `make deploy-engine` **does not sync manifests** — it restarts the
deployment only. Any manifest change must ride a `make apply` (the docs' order
above); otherwise the retired fields survive every rollout (measured: the
retired `engine-config` volume, unmanaged by apply's three-way merge, lived
through eight rollouts and would have failed the next pod creation once the
ConfigMap was pruned — `make diff` would have shown it). The same rule covers
the ConfigMap prunes: `kubectl apply -k` never deletes removed resources —
check `make diff` for `configmap/<name>` lines and delete them by hand.

Rollback = re-import the previous image **and** `git revert` the manifest
change (the old image needs the old ConfigMap). `make mainnet-smoke` + `make mainnet-scenario`
are the gate; the anvil pods pick the offset up from the mounted instance file
(`fork_offset` in `[chains.l1]`), so no env var carries it any more.

### Standing maintenance item: the Lighthouse pin

The running pin is **Lighthouse v8.3.0**. The current patch is **v8.3.2**
(2026-10-23): it carries v8.3.1's **eMudhra CA certificate mitigation**
(rated High for stakers, Medium for a non-staking mirror) and **cancels the
scheduled Gloas fork on Hoodi** — the 2026-10-26 activation was called off,
mainnet users need not upgrade, and the mirror follows a mainnet CL, so the
Hoodi schedule was never ours. The bump is a two-step rollout (pull + import
the new tag+digest, then `make apply` with the manifest edit) and therefore
rides the **next rollout window**, together with the venue overlay: replace
the line in `k8s/base/mainnet-mirror/lighthouse.yaml` with

```
image: docker.io/sigp/lighthouse:v8.3.2@sha256:1fda7c2d9c067dcfab20a0bfb83cb3c351a05673859455df4516e5708c5fa031
```

(the multi-arch index digest — the same form the v8.3.0 pin uses), then
`make images` (the pull), `make check-pins`, `make apply`. Until then the
running mirror is unaffected (the mitigation is a CA trust change, not a
consensus change). reth v2.8.0 stays as pinned; **foundry v1.8.6** is still the
latest release (a security-only patch on v1.8.5) — **v1.8.7 is imminent** (the
2026-10-22 nightly already bumps the version) and its anvil fixes touch ground
we stand on: log-filter expiry refresh (#17617 — the long-running log-delivery
class), fork RPC connection isolation (#17598), fork `prefetch` (#17673),
delegated-EOA nonce reset (#17686, EIP-7702), the EIP-8037 state-gas reservoir
(#17719), genesis-number RPC lookups (#17731), and fork blob-fee params
(#17754/#17756). Watch for the tag and ride the same rollout; re-run the §17.4
WS acceptance after the bump (a long-running instance is the case to re-check).

### Watch item: the Amsterdam (Glamsterdam) activation

Mainnet is not scheduled (Hoodi's 2026-10-26 activation was **cancelled**
2026-10-23 — Lighthouse v8.3.2 carries the cancellation; Sepolia activated
2026-10-06). When mainnet lands, three things move together:

- **EIP-7708** — ETH transfers emit a `Transfer` log from
  `SYSTEM_ADDRESS (0xffff…fffe)` at tx level, before the EVM logs, withdrawals
  excluded. The source's `eth_getLogs` then carries logs a replayed block
  cannot produce on a pre-Amsterdam anvil — harmless to the parity checks
  (storage keys and probes, never logs or blooms); raise the anvil hardfork
  with the activation.
- **EIP-7778 + 8037** — `gasUsed = max(exec_gas, state_gas)` and refunds no
  longer subtracted: receipt `gas_used` parity and the gas-limit rules change.
- **EIP-7928** — BALs leave reth's computed `eth_getBlockAccessList` and become
  consensus-carried (`ExecutionPayloadV4`; header `block_access_list_hash`;
  `engine_newPayloadV5` / `getPayloadV6` / `getPayloadBodiesBy*V2`). The BAL
  source in `crates/mainnet` switches from the RPC method to the engine
  payload at that point. The EIP's Last Call deadline is 2026-11-01.

Anvil's Amsterdam support is mapped from foundry v1.8.0 (EIP-8037 state gas in
v1.8.3, EIP-7928 BAL production in v1.8.4): pin a foundry release carrying
them before the activation, then re-run the capability proof and review
`/state/drift.jsonl` on a post-Amsterdam fork base.

Read the flight recorder:

```bash
kubectl -n mainnet-mirror exec deploy/engine -- tail -5 /state/drift.jsonl | jq .
# drift.jsonl is a *divergence* log (one line per divergence or apply anomaly),
# rotated at 16 MiB. Clean blocks are only in /metrics.
kubectl -n mainnet-mirror exec deploy/engine -- ls -la /state/journal | tail
kubectl -n mainnet-mirror exec deploy/engine -- cat /state/cursor.json
```

Every divergence line carries `divergence_class` (`none` | `value_level` |
`fatal`) and the full `checks` array, so a single drift line explains itself.

## Source liveness (self-contained)

The mirror needs no external nodes at runtime. reth's head (`mirror_reth_head`)
advances when the local Lighthouse feeds it payloads, and for any gap between
the snapshot's head and the CL's checkpoint the engine's top-up loop supplies
the missing ancestors over the Engine API (EIP-8379, see ARCHITECTURE →
"Source liveness"). reth has no P2P peers by design (`--disable-discovery
--trusted-only`), so nothing else can move it.

Check:

```bash
# catch-up progress (should fall to 0 and stay there)
kubectl -n mainnet-mirror exec deploy/engine -- curl -s localhost:8080/metrics | grep -E "source_sync_gap_blocks|source_sync_delivered_total"
# the top-up is delivering (one line per ~5 s while catching up)
kubectl -n mainnet-mirror logs deploy/engine --tail=20 | grep "top-up"
# reth really has no peers
kubectl -n mainnet-mirror exec deploy/engine -- curl -s http://reth:8545 -X POST -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"net_peerCount"}' | jq -r .result
```

- `top-up failed: top-up gap: reth head X, next CL payload Y (missing ancestors
  are outside the CL's retained history)` ⇒ the CL's block backfill has not
  reached the gap window yet (it walks backwards from the checkpoint, ~10-15
  slots/s). It clears by itself; on a fresh bootstrap expect ~10 minutes.
- `top-up failed: ... engine_newPayloadV4: -32602 Invalid params` ⇒ the request
  encoding regressed (see ARCHITECTURE's spec notes).
- `top-up failed: eth_blockNumber -> http://reth:8545` ⇒ reth is restarting;
  the loop retries every 5 s.
- Once `source_sync_gap_blocks` reaches 0, reth tracks the CL head live, and
  Lighthouse's own `forkchoiceUpdated` calls (which fail with "Received
  forkchoice updated message when syncing" during a catch-up) start updating
  reth's `safe`/`finalized` pointers again - the mirror's follow head then jumps
  from the snapshot's value to live. With `reth.follow: latest` the mirror then
  tracks the head one block per ~12 s.

## Drills

### Restart the chain (fresh fork + restore)

The anvil re-resolves its fork pin on every container start, so a restart
re-forks at `reth head - FORK_OFFSET` and the engine applies from there. There
is no state dump any more (see "The anvil state dump is disabled").

```bash
kubectl -n mainnet-mirror rollout restart deploy/anvil-t
kubectl -n mainnet-mirror rollout status  deploy/anvil-t
```

- **T**: the cursor's block hash no longer matches, so the engine reconciles
  and re-applies from the pin. The engine re-applies the devnet's own writes
  from the shadow registry's value map (`mirror_t_resets_total` increments, one
  WARN per reset, keys without a recorded value are handed back to mainnet
  sync). The devnet's *transaction history* does not survive a re-fork; its
  *state* does. The journal is cleared with the old chain, so rewind drills
  start from the new one.

### The crosschain unified-margin cycle — `make cctp-margin-cycle`

One actor's funds travel the whole substrate: the T mint (`/admin/deal`), the
CCTP burn + relay + the V mint, the venue-side spot ledger and the CoreWriter
spot→perp transfer, a perp fill against the real book, the CoreWriter order
door, the unified-collateral path (a spot-only unified account backing a perp
order) and the return leg closing exactly. In cluster mode the script derives
the bridge's wallet from the venue secret (`cctp-relay-key`), so the actor is
always the wallet the burn sends from — never the local harness's.

### The benchmark session — `make benchmark`

The four portfolio studies in sequence against the deployed mirrors — the one
command a benchmark session starts with. Each study hands its scenario back
(the overrides cleared, the books returned, the trader flat), so they compose:
the basis book's carry and shock, the crosschain margin book's costs and
served unrealized PnL, the capital frontier's liquidation distances, and the
hedged book's risk-based efficiency. `CAST=` must point at a cast binary.

### The cross-mirror capital-efficiency read (measured)

Per $1,000 of collateral, the venues' buying power, measured on the mirrors:

| venue | max leverage | liquidation distance | the note |
|---|---|---|---|
| T — Aave V3 (WETH/USDC) | 5.13x base (LTV 80.5%, LT 83%); 14.29x in the ETH-correlated e-mode (93%/95%) | at 1.88x: ~50% (HF 2.005) | the loop measured live: 1.0x -> 1.88x -> 2.59x, converging to 5.13x |
| V — ETH perp | 25x (the meta) | 48.98% at 2x; 2.0% at 25x (`make capital-frontier`) | the crash owns the mark/oracle; the liquidation closes at the live book |
| the hedged book | gross 2x | flat through a 60% market-wide crash (`make hedged-book`) | the maintenance table gives no hedge credit; the equity does |

The Aave base-unit conversion bites: `getUserAccountData` returns 8-decimal
USD while the tokens are 6/18-decimal — a borrow amount must be converted
(measured: passing the 8-dec figure as 6-dec is a 100x borrow, correctly
reverted).

### The hedged book — `make hedged-book`

The hedge's capital efficiency, measured: the same collateral and the same
*gross* exposure as the unhedged equivalent, but market-neutral (long ETH,
short BTC, dollar-matched on V's real books). The venue's maintenance table
gives the hedge no credit — the cross figure sums both legs (ETH at 25x -> 2%,
BTC at 40x -> 1.25%) — yet the equity, which is what the waterfall reads,
stays flat within 0.5% through a 10/30/50/60% market-wide crash. The unhedged
equivalent with the same gross/collateral ratio liquidates at 48.98%
(`make capital-frontier`). The hedge's beta coverage is not basis coverage:
the legs' divergence is the residual risk.

### The capital-efficiency frontier — `make capital-frontier`

Per leverage, how far can the market move before the venue liquidates? The
position is filled on V's real book with the collateral set exactly (the
faucet *adds*, so the script deals the delta), the mark is crashed in 0.25%
steps — each step hands the override back first, because `crash` is relative
to the *current* mark and repeated steps would compound — and the sim's own
waterfall decides. The measured distances match the classic
`(1/L - 0.02)/0.98` bound (the maintenance is half the initial margin at max
leverage 25 *of the crashed value*); the max-leverage row refuses at the
fill's margin check — the edge is real. This is the frontier daloc's
capital-efficiency benchmarks read.

### The crosschain margin book — `make crosschain-margin`

The collateral's whole journey measured: the T mint, the CCTP burn/relay/mint
(exact), the venue spot ledger, the CoreWriter spot→perp transfer, a long
filled on V's real book, N scripted funding hours read as cost (the account
delta and the served `cumFunding.sinceOpen`), a mark move read as served
`unrealizedPnl` with the margin state (accountValue vs marginUsed), the
economics (effective leverage, funding in bps of notional, unrealized on the
collateral) and the loop closed — the CoreWriter perp→spot *reverse* plus the
CCTP return landing exactly. The bridge leg's fee is zero on the devnet and
the deltas are exact, so the cost line is the funding itself. The scripted
`advance` jumps the sim's clock (monotonic by design): the offset accumulates
across runs and `/control/reset` (or an epoch) restores it. The faucet sets
a balance (a slot write), never adds; the reverse transfer moves the whole
perp balance at 8-dec (the measured costs leave less than the original).

### The portfolio simulation — `make portfolio-sim`

A delta-neutral basis book across both mirrors (long WETH on T, short the ETH
perp on V), measured end to end: the live context (V's mark/oracle/funding +
T's Chainlink feed), the fills against V's real book with their impact, the
coin and dollar deltas, one scripted hour of funding read as collected carry,
a crash's hedged PnL (the short gains, the T leg under the same shock stated),
and the capital-efficiency table (effective leverage, annualized carry on
capital, shock PnL on capital) — the prime-brokerage read the daloc benchmarks
extend.

### The basis capture — `make basis-capture`

The live cross-venue basis, sampled and then traded: T's Chainlink ETH/USD
against V's perp mark, `SAMPLES` times `INTERVAL` seconds apart — measured
live: min +19.8 / mean +23.6 / max +25.8 bps, std 2.0 bps over 12 samples
(the perp's premium is the trade). The book is the same delta-neutral pair
(long WETH on T, short the ETH perp on V), the hold is one hour at the
*venue's own* funding rate, and every read stays the venue's own: the carry
is `cumFunding.sinceOpen`, the capture is the served `unrealizedPnl` delta
plus the T leg's move (stated), and the identity — the served equity delta =
the served unrealized delta + the carry — holds exactly (residual 0.0000).
Two numbers are deliberately distinct: the *quoted* basis move (the samples)
and the *served* capture, because `advance` pins the sim's accounting mark at
the advance while the live plane keeps ticking — the sim never invents a
future price. The fill's own basis (e.g. +33.9 bps against the quoted
+28.8) is the book's cost line.

### The portfolio reaction — `make portfolio-reaction`

A held portfolio against another account's market impact, measured on both
channels: the whale's book round trip moves the *execution* cost (the
trader's equity moves only by the mark's own delta — measured: `$+0.00` on a
300 ETH round trip), while the scenario's `crash` and `rally` move the
*mark*. On the same short the reactions mirror at `-size x delta-mark`
(measured: `$+1,500.00` / `$-1,499.95` on a 12.87 ETH short, 5%), the served
buffer matches the sim's own liquidation per direction (the short liquidates
at a rally of 7.97% against a 7.97% served buffer; the long at a crash of
8.20% against 7.95% — the 0.25% scan grid), and the hedged book (perp short +
spot long) is flat both ways (perp `+1,397.16` + spot `-1,397.16` = `0.00`).
Beyond the buffer the trigger fires; the mark-only shock's close executes at
the *live* book (the documented split — the book channel is
`make impact-scenarios`' own test).

### Re-pin — `make mainnet-repin`

Three occasions:

1. **After a fresh bootstrap**, once the top-up catch-up has finished
   (`mirror_source_sync_gap_blocks` = 0). The first pin is resolved while reth
   is still behind, so the mirror starts near the snapshot's head; re-pinning
   moves T to a live, exact base instead of applying the catch-up window.
2. When the pin ages out of reth's state window (~10k blocks ≈ 1.4 days).
3. After an upstream reorg.

The drill:

1. scales `anvil-t` down,
2. wipes `/state/pin` and `/state/state.json` (fork cache is keyed by block and
   is kept),
3. scales back up: the init container resolves a fresh pin (`reth head - FORK_OFFSET`,
   default 128),
4. the engine detects the chain reset on its next poll, starts a new epoch
   (the devnet's writes are gone with the old base) and rebases its cursor
   (start = pin + 1). Re-create positions from your journal.

### Rewind applied parent blocks

```bash
kubectl -n mainnet-mirror exec deploy/engine -- \
  curl -s -X POST localhost:8080/admin/rewind -H 'Content-Type: application/json' -d '{"blocks":5}'
```

Restores the before-values recorded in the journal (newest block first) and
moves the T cursor back, so the applier re-applies from there. Expect at
most one transient convergence-probe mismatch at the rewound block (the
restored state is one block stale for the instant before the re-apply — a
time-dependent view like Aave's normalized income catches exactly that);
`mirror_t_convergence_last_failed` returns to 0 on the next block.

### Fund an account with any ERC20 (the unlimited faucet)

```bash
kubectl -n mainnet-mirror exec deploy/engine -- curl -s -X POST localhost:8080/admin/deal \
  -H 'Content-Type: application/json' \
  -d '{"token":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","to":"0xYourAddr","amount":"1000000000"}'
# -> {"ok":true,"slot":"0x…"}   (1000 USDC; silent storage write)
```

The engine finds the balance slot itself (standard `mapping(address => uint256)`
layouts and packed ones like USDC's `balanceAndBlacklistStates`; probes slots
0..16 with an `eth_call` `stateDiff` override, so nothing is mined while
discovering) and marks the slot devnet-owned, so mainnet diffs never take it
back. ETH is `anvil_setBalance`; if a token uses a non-standard layout the
response lists the probes it tried - write the slot yourself then.

Anvil's own `anvil_dealERC20` works on both mirror chains too - the argument
order is foundry's `(account, token, amount)`, it is an absolute storage write
(raises, lowers, and drains to 0), and the reversed order answers "no slot
found". The mirror's scripts use `/admin/deal` because it also marks the slot
devnet-owned (the cheatcode write is invisible to the tx scanner, so on a
watched token the mark is what keeps a parent diff from taking it back).

### Fast-forward the devnet (interest, expiry, funding)

```bash
# 30 days ahead of mainnet, persistently (mainnet data keeps flowing):
kubectl -n mainnet-mirror exec deploy/engine -- curl -s -X POST localhost:8080/admin/time \
  -H 'Content-Type: application/json' -d '{"offset_seconds": 2592000}'
# ... your scenario: read the accrued debt, exercise the expiry, ...
# back to mainnet's clock (timestamps are monotonic: the clock converges, it
# cannot jump backwards - the offset shrinks as mainnet advances):
kubectl -n mainnet-mirror exec deploy/engine -- curl -s -X POST localhost:8080/admin/time \
  -H 'Content-Type: application/json' -d '{"offset_seconds": 0}'
```

Measured on the live Aave USDC reserve: +30 days moved the income index by a
month of interest and grew a borrow's debt in the same read. While the offset is
non-zero the T probe parity is skipped (views are meant to differ). The offset
is **persisted with the cursors** (`/state/cursor.json`): it survives engine
restarts and redeploys, because T's blocks carry it - a restart that silently
dropped it would leave time-dependent contracts (Aave's
`lastUpdateTimestamp`) ahead of the clock, and their next write panics with an
arithmetic underflow (measured). Set it back to 0 to return to mainnet's clock.

### Fast iteration: mine your own blocks

The engine mines one T block per parent block (~12 s) - the right cadence for
"live mainnet next to my devnet", but slow for a tight test loop. Hold the
loops and mine by hand instead:

```bash
# 1. hold both loops (reads, writes and cheatcodes keep working):
kubectl -n mainnet-mirror exec deploy/engine -- curl -s -X POST localhost:8080/admin/pause
# 2. send transactions - automine is off by design, so they queue in T's pool
# 3. mine when *you* want, with the timestamp you want:
kubectl -n mainnet-mirror exec deploy/anvil-t -- cast rpc anvil_mine 1 --rpc-url http://localhost:8545
#    (or anvil_setNextBlockTimestamp <ts> first, for a chosen clock)
# 4. resume: the engine applies the parent blocks it missed, then re-aligns
kubectl -n mainnet-mirror exec deploy/engine -- curl -s -X POST localhost:8080/admin/resume
```

While paused, mainnet keeps advancing on reth; on resume the engine catches up
at roughly one parent block per second (the poll interval, not the 12 s head
cadence), and the gap guard allows up to `max_gap_blocks` (300) - so a pause is
cheap for minutes, not hours.

The k8s probes deliberately use `/livez` (always 200), not `/healthz`: a pause
must not take the engine - its NodePort included - out of the Service, and a
liveness probe on `/healthz` (503 while halted) would restart the pod in the
middle of this drill.

### Register devnet writes made outside the block pipeline

Cheatcodes applied directly to T (`anvil_setBalance`, `anvil_setStorageAt`, …)
do not create blocks, so the shadow scanner cannot see them:

```bash
curl -X POST .../admin/shadow -H 'Content-Type: application/json' \
  -d '{"address":"0xYourContract","slots":["0x1","0x2"],"account_fields":true}'
```

### Hand a key back to mainnet sync

`anvil_setStorageAt` / `anvil_setBalance` writes are owned by the devnet (COW):
parent diffs never overwrite them. To give a key back:

```bash
kubectl -n mainnet-mirror exec deploy/engine -- \
  curl -s -X POST localhost:8080/admin/shadow -H 'Content-Type: application/json' \
  -d '{"address":"0xYourContract","slots":["0x1"],"account_fields":false,"clear":true}'
```

The engine drops the shadow and immediately writes mainnet's current value
(read at T's cursor, so T stays consistent with the parent blocks it has
applied).

### Resume after a pause

```bash
kubectl -n mainnet-mirror exec deploy/engine -- curl -s -X POST localhost:8080/admin/resume
```

`/admin/resume` clears a `/admin/pause` halt; divergences are recorded, never
fatal, so no other state needs clearing.

## Devnet transactions

A devnet tx confirms in ~12 s (one T block per parent block, `follow: latest`).
Two measured gotchas:

- **Give every devnet tx a gas buffer.** The applier mines your txs in their own
  block *before* the parent's diffs land (the prepend rule), so anvil's
  auto-gas for `eth_sendTransaction` is accurate - but a tx that *does* share a
  block with the diffs can run out of gas (measured: an Aave borrow estimated at
  257,560 needed 271,389). `cast send --gas-limit`, or a wallet's own buffer,
  removes the class.
- **Manipulated state persists by design.** Writes through the devnet (txs or
  cheatcodes) shadow their keys (copy-on-write), so mainnet's diffs stop
  updating them. Hand them back with `/admin/shadow {clear: true}` (optionally
  `all: true` to reset a whole account - "reset this venue to mainnet truth"):
  the engine un-shadows the keys *and* re-syncs their current reth values. The
  re-sync is immediate and deterministic: the engine first scans T up to its
  head, so a write mined moments ago is included in the reset instead of being
  re-marked right after it (the poisoned-source race).
  Forgetting this is how a drill leaves a poisoned oracle behind: the *next*
  run's parity probes then fail on values nobody remembers writing.

### Stale watched accounts

A watchlist entry only takes effect for *future* mainnet writes. If an account
changed between its last sync and the moment it was added, it stays stale until
mainnet writes it again - for a Chainlink aggregator that can be an hour. The
per-reserve price probes (`aave_weth_price`, `aave_wbtc_price`, ...) are the
alarm for this class (measured twice: the WETH source's aggregator was missing -
8 rounds / ~$44 stale; and the WBTC source is *composite*, so its two
proxy/aggregator pairs were missing - a ~$1.40 stale price with no probe to see
it until the probe was added). Heal without waiting:

```bash
# 1. which keys differ?  (read slots on reth vs T; cast storage <addr> <slot>)
# 2. hand them back:  POST /admin/shadow {"address": ..., "slots": [...], "clear": true}
```

For an account that was *never* watched (so nothing is shadowed to hand back),
trace its read path on reth and write the slots it actually reads - the plain
`prestateTracer` (no `diffMode`: a view call changes nothing, so a diff-mode
trace is empty):

```bash
AT=$(printf '0x%x' "$(engine_api /status | jq -r .cursor_t)")
curl -s -X POST -H 'content-type: application/json' http://reth:8545 \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"debug_traceCall\",\"params\":[{\"to\":\"$AGG\",\"data\":\"0x50d25bcd\"},\"$AT\",{\"tracer\":\"prestateTracer\"}]}" \
  | jq -r ".result | to_entries[] | select(.key|ascii_downcase==\"${AGG,,}\") | .value.storage | to_entries[] | \"\(.key) \(.value)\""
# then anvil_setStorageAt each (slot, value) on T - padded to 32 bytes.
```

## Storage

| PVC | Size | Contents |
|---|---|---|
| `reth-data` | 400 Gi | reth DB + static files (snapshot + growth, ~300 GB expected) |
| `lighthouse-data` | 1500 Gi | beacon DB + 18-day column custody. Measured during the one-time backfill: ~377 KB/slot inside the DA window, blocks only beyond it; ~200 GB expected when it completes |
| `anvil-t-state` | 40 Gi | pin (the fork cache is RAM; no state dump - see above) |
| `engine-state` | 10 Gi | cursor.json, shadow.json, drift.jsonl (divergence-only; a legacy `drift.jsonl.1` may hold older per-block records from before that rewrite), journal/ |

All PVCs use the `mirror-retain` StorageClass (`reclaimPolicy: Retain`):
**deleting a PVC never deletes the data**. The PV stays; re-create a PVC with
the same name to re-attach it, or delete the PV explicitly to free the space.

- Inspect on the host: `/var/lib/rancher/k3s/storage/pvc-*`.
- Grow a PVC: edit `k8s/base/mainnet-mirror/pvcs.yaml` and `make apply`. With `local-path` the
  numbers are **nominal**: `allowVolumeExpansion` lets the *request* be raised,
  but the driver never resizes anything, so `status.capacity` can lag it
  (measured: `lighthouse-data` request 1500Gi, capacity 500Gi, no condition).
  The real limit is the node filesystem. A raised request cannot be lowered
  again (the API rejects it), which is why the manifest keeps the raised value.
- Full reset of a component: `kubectl delete pvc <name>` (data survives),
  then delete the PV if you really want the space back, then `make apply`.
- **Orphans**: deleting a PV leaves its directory in
  `/var/lib/rancher/k3s/storage/pvc-*_<ns>_<pvc>/`. Check with
  `kubectl get pv | grep -v Bound` and remove the directory once nothing
  references it (a Released PV keeps the data until you delete both).

## Failure playbook

| Symptom | Likely cause | Action |
|---|---|---|
| `mirror_reth_head` static for hours | top-up stalled (CL backfill not yet at the gap, reth restarting, beacon API down) | check `logs-engine` for "top-up failed"; see "Source liveness" |
| Lighthouse in CrashLoopBackOff | OOM during checkpoint sync (needs >6 GiB) | keep the 16Gi limit; `logs-lighthouse` shows `State cache missed` before OOM |
| Anvil OOMKilled | state snapshots + fork cache (`--prune-history`) | restart; lower `--prune-history`; see the memory table above |
| reth `eth_syncing` true for a long time | unwind/replay after a restart, or a stale engine buffer | wait; check `logs-reth` for stage progress |
| Lighthouse "no checkpoint" / sync stuck | checkpoint provider down | switch `CHECKPOINT_SYNC_URL` in `mirror-config` to a working provider (beaconcha.in's sync endpoint is the known-good alternative) |
| Lighthouse `system resources may be overloaded` (WARN) | a state advance at an epoch boundary took longer than usual | during the one-time backfill it recurs every ~32 slots (the backfill's I/O competes with the advance) and clears when the backfill completes - measured: 6 events in one 30-min window, none after. Not an alarm while reth tracks the CL head |
| Engine: "cursor not found on chain" | anvil restarted with a stale dump | expected; the engine re-bases T and re-applies its recorded writes |
| Engine: "gap too large" | engine was down > `max_gap_blocks` | re-pin (fast) or raise `max_gap_blocks` and restart the engine |
| The applier is silent after a re-pin | the re-pin raced an in-flight apply (a slow source read) | the engine aborts that apply (`ResetDuringApply`) and reconciles on the next pass — the cursor sits below the base, so the epoch rule fires; a restart re-resolves the pins if it persists |
| T mines empty blocks (`tx_count: reth=N t=0`) | automine is on (anvil restart reset it) | check `anvil_getAutomine` on T; the deployment runs `--no-mining` (the engine also re-asserts it on rebases) |
| Senders' nonces hundreds behind | automine incident (chain height ran away) | re-pin for a clean chain |

| `mirror_t_convergence_failures_total` climbing | T drifted from mainnet on a devnet-unowned probe (missing BAL entry, watchlist gap) | check `/status.shadowed`; extend the watchlist if the account should be tracked; clear any `/admin/time` offset |
| T state looks stale | the applier is behind, or the account is not watched | check `mirror_lag_blocks` and `mirror_cursor_block`; extend `watchlist` |

## Venue operations

The Venue Mirror is its own namespace (`venue-mirror`) with its own components:
V (`anvil-v`, 8545), the venue engine (`venue-mirror`, 8080), the simulator
(`venue-sim`, info 8090 / control 8091), and the CCTP bridge (8095/8096) when
deployed. The NodePorts keep the 31xxx range (31545 / 31880 / 31090 / 31091),
so both mirrors fit on one node. There is no `reth`/`lighthouse` here and no
mainnet-side fork: V is a single fork-mode chain, and the engine's job is the
fork lifecycle plus the devnet's own blocks. The engine's state dir carries
the same shape as the mainnet engine's (`cursor.json`, `shadow.json`,
`journal/`) plus `drift.jsonl` — the follow's anomalies (gap pauses, source
reorgs, uncovered emitters), the venue counterpart of the applier's lines.

**First rollout**: `make ns INSTANCE=venue-mirror` creates the namespace, and
`make venue-secrets CCTP_RELAY_KEY=0x…` creates the bridge's relay-wallet
Secret (`cctp-relay-key`, referenced by the manifest but not in the k8s tree —
a key is not a manifest). Use a dedicated key, never the deployer's; on the
devnet the bridge self-funds the wallet — at boot and whenever the balance
falls below the low threshold (a re-pin wipes V's state and the stipend with
it; measured: the gas alert stayed critical until the next boot) — so it needs
no balance yet.
Without the Secret the bridge pod waits in `CreateContainerConfigError`; the
engine and the simulator boot without it.

**The shadow audit** (`make shadow-audit`, measured live on both mirrors):
puts each anvil into a deliberate state and checks the copy-on-write shadow
end to end — a devnet deal diverges the anvil from the source, the shadow
registry marks the keys, a follow window (V) / an applier pass (T) runs with
the divergent value and the devnet's truth survives (the COW hold), the
parity skips the devnet-owned account (no new probe failures), and the
hand-back's behavior matches each source's nature: T's reth serves the pin's
truth (a 10,064-block window), so `POST /admin/shadow` restores it; V's
pinned-latest source cannot read the pin's truth once the head has moved, so
the same call *refuses* with the numbers (the guard never poisons — the
anvil's value is untouched). The hand-back's keys are per-address: a token's
storage slot is named *on the token* (`{"address": <token>, "slots": [...]}`),
an account's balance/nonce/code on the account (`account_fields`). The
fixture-owned tokens (the devnet's mocks over the source's addresses) report
their mock supply and are never overwritten by the follow. Two deeper legs
close the reliability question: the **BAL accounting** is asserted per block
(`applied <= filtered <= bal_writes`, with the fixture's skips live in
`skipped_shadowed` — the source's BAL touches the mock every busy block, so
the skip evidence is continuous), and the **watched-field pair** is
deterministic: a watched account's balance/nonce/code are planned every
window, so a staged divergence on an unshadowed watched account is written
back to the source's truth within one window (the control), while the same
divergence survives once the account's fields are shadowed (the COW). The
deal-cycle's own hold check is a regression guard, not a live skip: the
deal's slot lives in the token's storage and the parent rarely touches it.
The pair rotates through the non-fixture watched accounts (a mark cannot be
cleared while V's source is pinned-latest) and re-stages the value to the
source's own at the end, so the marks stay inert.

**The follow's cadence against the public source** (measured): the reads ride
JSON-RPC batches, and every one of the endpoint's measured limits is honored —
20 items per batch, 1 MiB per answer (the logs range halves on the refusal and
keeps the last good size), receipts avoided entirely (the endpoint throttles
their batches; the envelope reads `eth_getLogs` ranges plus the system txs,
and the watchlist sweep covers plain value transfers). The planned keys the
COW filter would drop anyway (fixture-owned accounts, the devnet's own
shadowed marks) are never read — the read-side skip, measured to cut a
window's plan from ~208 keys to ~55. One request in flight (concurrent bodies
tripped the burst limiter). Measured steady state after the re-pin: a 5-7
block window every ~4.6 s, 1.8-2.5 s of compose, a stable ~4-block lag behind
the source, zero failed passes. A pass that fails (a throttle, a transport
fault) backs off 1 s → 60 s, reset by a pass that serves. A gap past
`max_gap_blocks` (100) composes bounded windows until the cursor reaches the
source head (the chunked catch-up; `venue_follow_gap_catchups_total` and one
drift `gap_catchup` row per catch-up state, never per pass) — the follow never
latches paused (measured: the old latching pause froze V's chain-side state
~9.9k blocks behind while the source moved ~60 blocks/min, and only a manual
re-pin recovered it). The devnet's own writes are unaffected throughout (they
are COW-owned, and the sim's market plane is its own); a re-pin remains the
operator's choice for a fresh base, and after any T or V re-pin: restart the
bridge so it reinstalls the attester and the wrapper (measured live: a
mainnet re-fork left the T-side attester from the old base and the return
direction parked with `Invalid attestation length` until the restart).

**V's block production is the engine's** — the counterpart of T's applier
(one block per applied parent block): the driver mines a non-empty pool on its
own tick (`venue_mined_blocks_total`) — a dedicated producer loop, so the
follow's window work (minutes against the public source) cannot delay an
external tx (measured: a 143-block compose ran 3m49s and the bridge's 30s
receipt window missed while its tx sat pending). The bridge's boot txs and
relay mints land without waiting for a harness mine (measured: without it the attester
install failed `… was not mined within 120s` on a chain nobody mined); an idle
chain stays static, and the harness's own `anvil_mine` calls are just extra
(empty) blocks.

### The venue engine's `/status`

| Field | Meaning |
|---|---|
| `head` | V's head |
| `cursor` | the last scanned V block |
| `fork_pin` | V's fork base (the source block V forked at) |
| `epoch_pin` | the pin the shadow registry was built against; a mismatch with `fork_pin` means a reset is being reconciled |
| `halted` | 1 while `/admin/pause` holds the driver |
| `shadowed_slots` / `shadowed_accounts` / `shadowed` | the devnet-owned COW state; the list is what parity checks must skip |
| `probes` | the configured at-pin probe count |
| `source` | the fork source the pin derives from (the unshadow truth's home) |

### The sim reset runbook

`POST {sim_control}/control/reset` blanks **every simulated account** and bumps
the scenario epoch (the sim's own `venue_sim_epoch`). The chains keep their
history, so a reset silently desyncs the pairing: a CCTP deposit already
relayed into the sim is missing here while the bridge's cursor sits past it,
and accounts the engine's shadow plane mirrors are gone. Nothing downstream can
detect the divergence by itself — the reset logs the remedy, and the remedy is
the one the T re-pin carries:

1. restart the bridge (its scan re-mirrors the chains' state into the sim),
2. re-pin V through the engine (`/admin/epoch`), so the shadow plane and the
   devnet's own writes re-base together,
3. re-run the scenario's setup (deals, orders) — the minted funds are gone with
   the accounts, by design.

Use it freely on a scratch sim (the drills do); on a shared one, treat it as a
chain re-pin and schedule it.

### Venue metrics

The venue engine's `/metrics` renders the `venue_*` set plus the shared admin
gauges; the simulator's control `/metrics` (8091) renders `venue_sim_*` and
`venue_bridge_*`; the mainnet engine's `/metrics` gains `feed_bridge_*`.

| Metric | Meaning |
|---|---|
| `venue_head` / `venue_cursor` | V's head / the last scanned V block |
| `venue_fork_pin` / `venue_pin_age_blocks` | V's fork base / blocks since this epoch's first scanned block |
| `venue_epochs_total` | times V was found re-forked under the driver (the epoch rule ran) |
| `venue_restore_writes_total` | writes re-applied by same-pin restores |
| `venue_scanned_blocks_total` / `venue_skipped_empty_blocks_total` / `venue_mined_blocks_total` | V blocks the shadow scan walked / the empty share of the walk (one call, no traces) / V blocks the engine mined (the pool was non-empty — the engine is V's block producer; T's is the applier) |
| `venue_source_head` / `venue_source_followed` | the HL source's head / the highest source block composed onto V (the follow cursor) |
| `venue_follow_windows_total` / `venue_follow_applied_total` / `venue_follow_skipped_shadowed_total` | follow windows composed / keys applied / keys skipped because the devnet owns them (COW) |
| `venue_follow_gap_catchups_total` / `venue_source_reorg_total` | catch-up states entered (the source ran ahead of one bounded window; bounded windows compose until current) / source continuity mismatches (a parent hash changed under the follow) |
| `venue_follow_sampled_total` / `venue_follow_sampled_ok_total` | the live parity's sample and its passes: the follow re-reads every applied key on V and compares against the composed value |
| `venue_poll_touched` / `venue_poll_planned_keys` / `venue_poll_changed_keys` | the last window: addresses touched / keys the closures planned / keys found changed |
| `venue_poll_uncovered_logs_total` | logs from contracts no closure covers (declare a closure to follow them) |
| `venue_probe_ok_total` / `venue_probe_failures_total` | at-pin probes matching / diverging from the fork source |
| `venue_probe_skipped_total` / `venue_probe_last_failed` | probes skipped (devnet-owned target, or a pin the source cannot serve) / diverged in the last run |
| `venue_bridge_lag_blocks` | V head minus the bridge's processed cursor (growing = stuck) |
| `mirror_shadowed_slots` / `mirror_shadowed_accounts` / `mirror_halted` | the shared admin gauges, rendered beside the venue's own (the registry pushes them on change) |
| `venue_sim_accounts` / `venue_sim_epoch` | simulated (touched) accounts / scenario epoch |
| `venue_sim_liquidations_total` / `venue_sim_funding_payments_total` | liquidations executed / funding payments applied |
| `venue_sim_partial_liquidations_total` / `venue_sim_backstop_liquidations_total` / `venue_sim_adl_closes_total` | the waterfall's steps: partial (20 %) book closes above $100k / backstop takeovers (the vault) below ⅔ maintenance / ADL closes at the previous mark |
| `venue_sim_fees_charged_total` / `venue_sim_triggers_armed_total` | fills that charged a fee (taker and maker sides) / trigger orders armed |
| `venue_sim_ws_connected` / `venue_sim_ws_reconnects_total` | the market WS lane's health |
| `venue_sim_rest_errors_total` / `venue_sim_budget_skips_total` / `venue_sim_budget_weight_used` | REST faults / calls skipped at the rate budget / weight spent in the rolling minute |
| `venue_sim_stale_ms{coin}` | milliseconds since the coin's last live context update |
| `venue_sim_owned_levels` / `venue_sim_owned_skips_total` / `venue_sim_owned_pruned_total` | book levels the devnet has consumed (the order lane's ownership; live updates keep the consumed delta) / live updates that met an owned level / owned levels pruned because the live book moved past them |
| `venue_sim_orders_filled_total` | orders filled against the real book (the impact lane) |
| `venue_bridge_credits_total` / `venue_bridge_completions_total` | EVM→Core credits / Core→EVM completions |
| `venue_bridge_refusals_total` / `venue_bridge_malformed_total` / `venue_bridge_trace_faults_total` | refused effects / undecodable actions / blocks whose callTracer failed |
| `venue_bridge_actions_total{id}` | decoded CoreWriter actions by id |
| `feed_bridge_updates_total{feed}` / `feed_bridge_pauses_total{feed}` | answers published to the mock aggregators / feeds paused on a scenario's write |
| `feed_bridge_pass_errors_total` | feed passes that failed; `GET /admin/feed`'s `last_error` names the cause (source read, target read, publish, decimals mismatch) |

Alarms: `venue_epochs_total` rising means V re-forked (the devnet's writes are
gone with the old base unless the pin is unchanged); `venue_probe_failures_total`
rising means a poisoned value survived a hand-back; `venue_sim_ws_connected` 0
for >1 min means the market plane is on the REST lane (watch
`venue_sim_budget_skips_total`); `venue_sim_stale_ms{coin}` above the readiness
window means `/readyz` will fail; `venue_bridge_trace_faults_total` rising means
contract-internal CoreWriter calls are being missed that block;
`venue_bridge_lag_blocks` growing means the bridge is stuck behind V.

**Plan §7.8's sketch names vs the implemented series** (the plan's operations
map was written before the `venue_sim_*`/`venue_bridge_*` prefixes settled; the
implemented names are the contract with the alarms above):

- the probe-failure counter is `venue_probe_failures_total` (the plan's sketch
  spelled it with "failed");
- the feed lag is `venue_sim_stale_ms{coin}` — milliseconds, per coin (the
  plan's sketch asked for seconds under a single feed label);
- the WS flag, the rate-budget gauge and the identity counter carry the
  `venue_sim_` prefix (`venue_sim_ws_connected`,
  `venue_sim_budget_weight_used`, `venue_sim_identity_violations_total`);
- the registry's size is the **shared** `mirror_shadowed_slots` /
  `mirror_shadowed_accounts` pair (one fact, one home — the same series on both
  engines, pushed by the registry on every change), not a venue-prefixed twin;
- the bridge's signal is the lag, `venue_bridge_lag_blocks`: the bridge scans V
  block by block, so there is no pending queue to gauge.


### The daloc-facing endpoints

The harness's external-mode env block lives in the manifests: the
`test-endpoints` ConfigMap of each overlay (`k8s/overlays/mainnet-mirror/nodeports.yaml` for the
L1 side — `CHAIN_RPC_URL`/`CHAIN_WS_URL`/`MIRROR_ADMIN_URL` — and
`k8s/overlays/venue-mirror/nodeports.yaml` for the venue side, whose keys carry
the second chain's `CHAIN_1_` prefix so sourcing both blocks yields one
collision-free env). The NodePorts are the services' own (`kubectl get svc`),
so the ConfigMaps and the Services cannot drift.

`mirror-config export-env --section daloc` (alias `--for`) renders the same
block from the mounted instance file with **in-cluster** addresses — the
Service names (`engine`, `venue-sim`, `cctp-bridge`) and the instance's own
ports, under exactly the ConfigMaps' key spellings (a unit test pins the
pair). Use it from inside a pod (or for a pod-shaped harness); use the
ConfigMaps for host access over the NodePorts. Both surfaces read the same
facts: the export takes the chain URLs from the instance's own wiring
(`[chains.venue].anvil_url`, `[services.engine.anvil].t_url`) and refuses a
lingering `${VAR}` — its purpose is values, never references.

### The CCTP bridge

The bridge (`cctp-bridge`, Iris 8095 / control 8096; 31095/31096 in-cluster)
serves the real CCTP lane beside daloc's untouched `0x17` mock. Its boot
installs the devnet attester on both chains and deploys the hook wrapper
(owner = the relay key); both are idempotent and verified read-back.

| Check | Where |
|---|---|
| parked messages (never dropped) | `GET /health` (`parked` count), `GET /control/state` |
| the relay's gas | `/metrics` `cctp_*`; the boot tops the key up to the low threshold |
| the attester / wrapper wiring | the boot log lines (`devnet attester installed`, `hook wrapper present/deployed`) |
| a transfer end-to-end | `POST /control/transfer {source, destination, amount}` |

The bridge's series (all `cctp_*`, on the control port):

| Metric | Meaning |
|---|---|
| `cctp_messages_seen_total` | `MessageSent` logs absorbed (new records) |
| `cctp_messages_signed_total` | records transitioned PENDING → SIGNED |
| `cctp_messages_pending` / `cctp_messages_signed` / `cctp_messages_parked` | the queue's current state counts (what `/health` reports) |
| `cctp_relays_confirmed_total` | `receiveMessage` landed (a consumed nonce reconciles as confirmed) |
| `cctp_relays_parked_total` | definitive failures parked for an operator |
| `cctp_relay_attempts_total` | signed records re-checked by the relayer (one per tick per record) |
| `cctp_scan_errors_total` | scan passes that failed (logged with the chain) |
| `cctp_last_scan_block` | the last head the watcher scanned (a coarse liveness gauge) |
| `cctp_gas_low_total` / `cctp_gas_critical_total` | gas-alert crossings (each observation below the threshold counts) |
| `cctp_relay_balance_wei{chain}` | the relay key's native balance per chain (full-width wei — a devnet balance can exceed `u64`) |

**A parked message.** A message parks on a definitive failure (an expired
window, a reverted `receiveMessage`, a vanished burn after a re-pin, a rewound
mint on the destination). It is never re-relayed silently. To recover one
whose burn still exists on the source:

```bash
curl -s -X POST localhost:31095/v2/reattest/<burn-tx-hash>   # fresh window, re-armed
```

The relayer picks it up on its next pass. A burn that a re-pin removed cannot
be recovered — re-run the transfer.

**A re-pin or a snapshot revert.** The watcher clamps its scan at the fork pin
and parks records whose burn vanished; the relayer applies the same window to
each destination and parks relayed records whose mint was un-minted — a rewind
(head below the watermark) or a forward re-pin (fork base above it: the epoch
model's live path; a restart's boot pass reconciles the rest). Watch `cctp_*`
metrics and the `parked` count; the reasons name the case.

A re-pin also removes the bridge's own devnet deployments — the **attester
install** and the **hook wrapper** live on the old base. The bridge detects
this at the points that matter: the control path refuses a hook transfer to a
wrapper with no code, and the relay path parks a hook message whose wrapper
vanished (both name the re-pin); a **restart** redeploys the wrapper,
reinstalls the attester on both chains, and runs the reconciler's full pass
(parking any record whose mint the re-pin wiped). So after any re-pin of T or
V: **restart the bridge** (`kubectl rollout restart deploy/cctp-bridge`), then
`GET /health` — `parked` should explain itself and stay flat. A parked record
whose burn still exists re-arms with `POST /v2/reattest/<burn-tx-hash>`.

**The hook (credit-carrying) lane.** `POST /control/transfer` with
`"hook": {"target": "0x…", "calldata": "0x…"}` burns with
`depositForBurnWithHook` and pins `destinationCaller` to the wrapper; one relay
tx then mints and runs the call (non-atomic: a failing hook cannot lose the
mint). `mint_recipient` is usually the wrapper itself when the hook forwards
the funds.

**Pointing daloc's `cctp-relay` at this service — one known fix needed.** Their
relay is written against their own mock lane, which serves the *event* message
verbatim; a protocol-faithful Iris (Circle's, and ours) serves the
**attester-patched** message — nonce and `finalityThresholdExecuted` filled, so
it is the artifact `receiveMessage` verifies. Measured on a live round trip
through this bridge (the store keeps both): event message nonce `0x00…00`,
served nonce `0x0d297d…48f1` (the burn-tx hash), event
`finalityThresholdExecuted = 0`, served `= 2000`; `keccak256(event) =
0xa5498934…` ≠ `keccak256(served) = 0x3332df64…`. Their
`services/cctp-relay/src/relay.rs` therefore (a) never matches — the `find` at
`keccak256(&m.message) == hash` compares the served keccak against the event
keccak and the attempt stays `AwaitingAttestation` forever — and (b) even on a
match would submit the stored **event** message (`message.clone()`), which
reverts `Nonce already used` (the zero-nonce pre-block; proven live). The fix
on their side is two lines: take the message the tx-scoped query returns (the
query is `?transactionHash=<burn>`; if a tx carries several burns, match on the
fields the attester never patches — `burnToken`, `amount`, `mintRecipient`,
`messageSender`, the header's `sender`/`recipient`/`destinationDomain`), and
submit `matched.message`, not the event payload. Their mock lane is unaffected;
this is the delta for the real lane only.

### Drills

**Revert to blank - `epoch_new` (one call, both halves).**

```bash
curl -s -X POST localhost:31091/control/epoch_new
```

The simulator resets (accounts + overrides, epoch +1) and the engine re-pins V
to the source's latest; the driver's canary notices on its next pass and runs
the epoch rule (new base = clear marks and values). Watch: `/status` `fork_pin`
advances, `venue_epochs_total` +1, `venue_sim_epoch` +1, the touched account is
gone. T's half is the same call on the mainnet engine:
`POST .../admin/epoch` (re-pins T; the engine rebases and runs its own epoch
rule).

**Crash and hand-back (venue side).**

```bash
curl -s -X POST localhost:31091/control/crash -d '{"coin":"ETH","pct":"30"}'
# ... read the account: the equity draws down at the owned mark ...
curl -s -X POST localhost:31091/control/crash -d '{"coin":"ETH"}'   # hand back
```

The crash owns mark *and* oracle until handed back; below maintenance the
waterfall runs (the book first, then the backstop, then ADL - `sim/waterfall.rs`).
The L1 analog is the feed bridge: a scenario's write to
a mock aggregator pauses that feed automatically; `POST .../admin/feed/resume`
hands it back, and `GET .../admin/feed` shows confirmed/pending state. On the
cluster the mocks come from daloc's fixture deploy; when the instance
enables `[services.engine.feed_bridge]`, the bridge publishes the *real*
Chainlink answers (`mainnet-feed-bridge` drives the routes);
`make mainnet-feed-local` boots a scratch stack instead — a stub parent, T
forked from it, the engine with `[services.engine.feed_bridge]` — installs
the same Chainlink-shaped stub at each configured target, and proves the
whole lane: parent answers published, a scenario's write paused (its value
preserved), the hand-back restored.

**The bridge round-trip.**

```bash
# EVM→Core: transfer USDC to its system address (token 0) on V. V runs with
# --no-mining and the venue engine is V's block producer: it mines the queued
# tx within a driver pass (venue_mined_blocks_total) - the explicit mine below
# just lands it immediately. cast needs the explicit impersonation even with
# auto-impersonate on (measured).
cast rpc anvil_impersonateAccount 0xYourAddr --rpc-url http://localhost:31545
cast send 0xb88339CB7199b77E23DB6E890353E22632Ba630f \
  "transfer(address,uint256)" 0x2000000000000000000000000000000000000000 1000000 \
  --unlocked --from 0xYourAddr --rpc-url http://localhost:31545 --async
cast rpc anvil_mine 1 --rpc-url http://localhost:31545
# read the sender's Core spot balance (USDC rises by 1.0)
curl -s -X POST localhost:31090/info \
  -d '{"type":"spotClearinghouseState","user":"0xYourAddr"}'
```

`venue_bridge_credits_total` +1 and the sender's USDC spot balance rises by 1.0
(the ERC20 is 6-decimals, Core 8). The reverse direction is a `sendAsset` naming
the system address: the sim debits the account and submits a transfer **from**
the system address; `venue_bridge_completions_total` +1 and the recipient's
ERC20 balance rises. A `sendAsset` to any other address is a spot send inside
Core (no EVM tx).

**Storage.** The venue namespace's PVCs use the same `mirror-retain`
StorageClass as the mainnet mirror; the state layout is the shared one
(`cursor.json`, `shadow.json`, `journal/` on the engine's PVC; `sim.json`,
`feed.json` on the simulator's).

### Venue failure playbook

| Symptom | Likely cause | Action |
|---|---|---|
| `venue_epochs_total` increased and `shadowed_slots` dropped to 0 | V was re-forked at a *new* pin (an OOM restart, `/admin/epoch`, an out-of-band `anvil_reset`): a new epoch - the devnet's writes are gone with the old base | re-create the scenario from its journal; the sim's `epoch_new` does both halves in one call |
| `venue_epochs_total` increased and `venue_restore_writes_total` grew | V was reset at the *same* pin: the recorded writes were re-applied, orphans pruned | none; verify `/status.shadowed` matches expectation |
| `venue_follow_sampled_ok_total` below `venue_follow_sampled_total` | an applied key did not read back the composed value (a write that did not land, or a composition bug): inspect `drift.jsonl` and the window's log line | re-run the window (`/admin/epoch` re-pins) and check the key by hand; a repeat is a bug, never drift |
| `venue_probe_skipped_total` climbing, `venue_probe_failures_total` flat | the pin is not the source's head (the source serves its latest state for `eth_call` - measured), so probes are skipped, never alarmed | re-pin (`/admin/epoch` or `epoch_new`) to probe against a fresh base |
| `venue_probe_failures_total` climbing | V diverged from the fork source on a devnet-unowned target (a missing hand-back, a poisoned value) | inspect `/status.shadowed`; hand the keys back (`/admin/shadow {clear: true, all: true}`); re-pin if it persists |
| `/admin/shadow {clear: true}` refused with "the source advanced past the pin" | the unshadow source can no longer serve the pin (`eth_call` serves its latest state - measured) | re-pin (`POST /admin/epoch`); the refusal is the protection against handing back a poisoned value |
| a feed's `feed_bridge_pauses_total` rose | a scenario wrote the mock aggregator (or crashed it): the bridge paused, by design | `POST /admin/feed/resume` to hand it back; `GET /admin/feed` shows confirmed/pending |
| `feed_bridge_pass_errors_total` climbing | a feed's pass keeps failing: a source read (reth down, bad address), a target read, a publish, or a decimals mismatch | `GET /admin/feed`'s `last_error` names the cause; check reth and the feed's addresses in the instance file |
| `venue_bridge_credits_total` flat while a transfer landed on V | the bridge is not anchored, or the block predates the pin (source blocks are never scanned) | check `sim.json`'s `bridge_cursor` (0 = not anchored; the first pass anchors at `head.max(pin)`); check `venue_bridge_trace_faults_total` |
| `venue_bridge_trace_faults_total` rising | V's callTracer failed: contract-internal CoreWriter calls are missed that block | check V's health; the block is counted, not half-applied - re-mine the activity |
| `venue_sim_stale_ms{coin}` above the freshness window | the market plane is behind (WS down, REST budget exhausted) | check `venue_sim_ws_connected` / `venue_sim_budget_skips_total`; `/readyz` reflects it, and the order lane refuses stale fills by name. Drill: `POST /control/stall {"seconds":45}`, wait past the window, watch `/readyz` 503 + the refusal, clear with `{"seconds":0}` (venue-order-proof's H leg does exactly this) |
| `venue_sim_budget_weight_used` pinned at 1200 | the rate budget is exhausted: REST calls are being skipped | raise the poll interval or trim `markets`; WS pushes keep carrying marks |
