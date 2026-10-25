# Structure map — the two mirrors, side by side

Both engines are one design over two sources. This page is the file-by-file
map: what each crate carries, which files pair with which, and where the two
deliberately differ (with the reason). It is the artifact behind the
symmetry sweeps — regenerate the readings with
`cargo run -q -p ast-scope -- crates bins`.

## The trees

| `crates/mainnet/src/` | `crates/venue/src/` | role |
|---|---|---|
| `lib.rs` | `lib.rs` | the crate doc, the mod list, the public types' compile-time pin |
| `service.rs` | `service.rs` | the service entry: CLI (`run`/`selftest`[`/rewind`]), the boot call, the loop spawner (the venue's carries the simulator's entry too) |
| `ctx.rs` | `ctx.rs` | `Ctx` (the profile's clients + sections + the shared `Plane`) and its `impl Engine` |
| `driver.rs` | `driver.rs` | `boot` (wait, assert the chain, anchor/reconcile, audit capabilities) and the loops |
| `apply/` (mod, prepend, parity) | `follow.rs` | the per-unit step: compose → COW → land → measure (the mainnet's step has parts: the devnet prepend, the drift lens + probes) |
| `error.rs` | `error.rs` | the profile's error vocabulary (the shared devnet/anvil boundaries composed in) |
| `http.rs` | `http.rs` | the shared route table + the profile's own routes (`/admin/time`; the venue's simulator API is `api/`) |
| `metrics.rs` | `metrics.rs` | the profile's metric set (rendered beside the shared gauges) |
| `profile.rs` | `profile.rs` | the divergence set, filled: `bundle`/`policies`/`build` + the `Wiring`/`Deps` pair, under the same decision table |
| `reset.rs` | `reset.rs` | the profile's `ResetDetector` (cursor hash / pin canary) |
| `selftest.rs` | `selftest.rs` | the scratch-stack selftest (`Targets` + `run`) |
| `source.rs` | `source.rs` | the `Source` impl (`RethSource` / `VenueSource`) |
| `feed_bridge/` (mod, chainlink, http, metrics, policy) | `feed/` (mod, budget, metrics, rest, ws) | the market/price plane: the Chainlink feed bridge (the parent's answers → T's mock aggregators) / the HL market feed (REST + WS + budget) into the simulator |
| `source_sync/` (mod, beacon, engine_api) | `bridge/` (mod, corewriter, metrics, system) | mainnet-only transports (CL + engine API, EIP-8379 top-up) / venue-only Core↔EVM bridge |
| — | `api/` (mod, control, exchange, info, ws) | the simulator's served API (info 8090 / control 8091) |
| — | `sim/` (mod, account, funding, metrics, order, seed, tick, validate, waterfall) | the simulator's model |
| — | `wire.rs` | the HL wire schema (requests, served shapes, signatures-as-shapes) |

Shared by both (never each other): `mirror-devnet` (the shadow registry, the
journal, the cursor state, the admin surface, the scanner, the probes, the
epoch rule, the composer, the drift log, the fact bus, the shared HTTP
table), `mirror-anvil` (the typed RPC client, the measured padding rules, the
trace flattening), `mirror-config` (the layered config + the boot
conventions: `run_main`, `parse_cli`, `exit_verdict`, `service_config_or_exit`,
`init_logging`, the worker stack budget, `DEFAULT_INSTANCE_PATH`).

## The paired names (fn level)

The files pair, and so do the entry points inside them. The shared vocabulary
(`boot`, `run`, `app`, `status_doc`, `bundle`, `policies`, `build`, `probes_now`)
reads identically on both sides; everything else is the profile's own rule set:

| file | mainnet | venue |
|---|---|---|
| `ctx.rs` | `Ctx::{new, follow_head, follow_tag}` + `impl Engine` | `Ctx::{new}` + `resolved_pin` + `impl Engine` |
| `driver.rs` | `boot` + `apply_loop`/`shadow_loop`/`source_sync_supervisor` + the cursor/reorg rules (`next_block`, `validate_cursor`, `on_reset`, `rebase`, `resume_point`, `on_rebase`, `gap_within_limit`, `reorg_continuity`) + `scan_once`, `probes_now` | `boot` + `run`/`pass` + `reconcile`/`pin_read`/`anchor_cursor`/`recover_from_reset`/`run_epoch_rule` + `scan_and_advance`, `mine_pending`, `probes_at_pin`, `probes_now` |
| `service.rs` | `main_entry` + `run` + `require_wiring` + `build_bridge` + `spawn_loops` | `main_entry` + `run` + `require_wiring` + `spawn_loops` (+ `sim_entry` and the simulator's spawns) |
| `profile.rs` | `bundle`/`policies`/`build` + `Wiring`/`Deps` | `bundle`/`policies`/`build` + `Wiring`/`Deps` (+ `closures`/`watch_set`) |
| `http.rs` | `app` + `status_doc` (+ `admin_time`) | `app` + `status_doc` |
| `selftest.rs` | `run` (+ `run_one_block`) | `run` (+ `windows_loop`, `wait_for_move`) |
| `source.rs` | `RethSource::{new, trace_block_per_tx}` + `impl Source` | `VenueSource::{new}` + `public_capabilities` + `impl Source` |
| `reset.rs` | `CursorHash` + `below_fork_base` | `PinCanary` + the canary io |
| `metrics.rs` | `MainnetMetrics` (registry + render + overlap test) | `VenueMetrics` (the same three) |
| `error.rs` | the profile's variants + `impl ShellError` | the same shape |

Struct level: both `Ctx`es carry the same ordered core (the chain section, the
engine section, the target anvil, the source, the mirror bundle, the composer
target, the plane, the profile metrics, the drift log) with the profile's own
fields at the ends (`pin`/`uncovered_seen`; `beacon`/`engine_api`/
`feed_bridge_metrics`); both `StatusResponse`s flatten the shared `SharedStatus`
beside the profile's fields; both `profile::Deps` are the "one struct, not five
arguments" input to `build`.

## The judged asymmetries

Each of these is a decision, not drift (the design record's divergence table
in `SPINE.md` and the profiles' own tables are the full list):

| asymmetry | why it stands |
|---|---|
| `apply/` (3 files) vs `follow.rs` (1) | the mainnet's step has parts (the devnet prepend, the drift lens); the venue's composes one window |
| `feed_bridge/` vs `feed/` | different domains: a price *bridge* (Chainlink mocks) vs the *market feed* (HL REST/WS) |
| `source_sync/` (mainnet) | the CL + engine-API transports have no venue counterpart (the HL API is one HTTP client) |
| `api/`, `sim/`, `wire.rs` (venue) | the simulator's served surface has no mainnet counterpart |
| `service.rs` carries `sim_entry` | one home per service: the venue crate serves two binaries (`venue-engine`, `venue-sim`) |
| metric namespaces (`mirror_*` / `venue_*`) | breaking to rename; the docs pin them |

## The AST readings (and what each one found)

The tool (`bins/ast-scope`) reads the workspace with `syn` and reports;
every reading below was acted on or judged:

| reading | outcome |
|---|---|
| scope (`fn` ≤ 60 lines / depth ≤ 5, struct ≤ 20 fields, enum ≤ 25 variants) | the largest production fn is `apply_block` (50 lines, a deliberate linear sequence); depth ≤ 4 |
| dead functions / dead members | none (the AST and the text agree) |
| duplicates | only deliberate twins (the two `impl Engine` accessors, `healthz`/`livez`, the two reset detectors' constructors, the per-profile error helpers) |
| near-duplicates / similar records | none after the pass; the `FrontendOpenOrder`/`StatusOrderInner` overlap is the API's own two shapes — the *mapping* is now one function (`frontend_row`), the shapes stay exact |
| reuse (shared op sequences) | the one real extraction: the cursor advance (`3× … cursor_from → set_cursor`) became `mirror_devnet::state::advance_cursor`, used by the applier, the venue scan and the admin's reset; the rest are axum's router idiom |
| dispersion | `Sim` spans four files — the domain folder's pattern (one impl block per concern) |
| narrow context | none: every `Ctx`-taking function touches ≥ 2 fields or passes the context on |
| plumbing | judged: pure policy/formatting helpers (`step`, `push_metric`, `scan_to`) keep positional parameters — a parameter object would obscure them |
| divisions (non-literal divisors) | all guarded: `ratio_or` (an explicit zero branch), `maintenance_rate` (`max_leverage == 0`), `liquidation_px` (`szi`/`denominator`), `adl_ranking` (a non-positive account value skips), `floor_to_core_wei`/`round_to_sig_figs` (a checked `pow10`/`step ≥ 1`) |
| casts (narrow `as`) | judged safe: date fields, decimal scales (≤ 28), decimals (≤ 18), booleans to `u8` |
| repeated literals | named once: `mirror_anvil::{LATEST, TRACE_BLOCK_METHOD, JSON_CONTENT_TYPE, ETH_GET_BALANCE}`, `mirror_devnet::handlers::{HEALTHZ, METRICS}`, the drivers' `ANVIL`, the config's section requirements, the journal's io context, the sim's money contexts, the venue's order-type names and margin mode, `SIM_POISONED` |

## The external-lib audit

"Most types can be imported instead of defined" — the audit's verdict, type
family by type family:

| our type | external candidate | verdict |
|---|---|---|
| the BAL types | `alloy-eips::eip7928` | **imported** (the spec crate's own validator authenticates them) |
| chain ids, addresses, `U256`, hex | `alloy-primitives`, `alloy-chains` | **imported** (`ChainId` is the newtype; no bare `u64`) |
| the JSON-RPC envelope | `alloy-json-rpc` | **imported** (the protocol, not our vocabulary) |
| the beacon/engine wire | `alloy-rpc-types-beacon`/`-engine` | **imported** |
| ABI encode/decode | `alloy-sol-types` | **imported** |
| ISO-8601 timestamps (the Iris API) | `chrono` | **imported** this wave: a hand-rolled civil-from-days became `DateTime::from_timestamp_millis` (the tests pin the identical output) |
| money | `rust_decimal` | **imported** (f64 is not a money type) |
| HTTP/WS/JSON/CLI/runtime | `axum`, `tokio-tungstenite`, `serde`, `clap`, `tokio`, `reqwest` | **imported** |
| the write/read model (`Write`, `Key`, `Capabilities`) | — | ours: no crate models anvil's cheatcode surface; the enums are narrower than any general diff type |
| `ChainCursor`, `StateFile`, `Shadow`, `Journal` | — | ours: the devnet's persisted state is the mirror's own contract |
| the metrics registry + exposition | `metrics`/`prometheus` | ours, deliberately: a dependency-light registry whose render is gated (the overlap tests, the docs' metric table) |
| `FollowHead` | `alloy`'s `BlockNumberOrTag` | ours: three variants, `deny_unknown_fields`, one `tag()` |
| the venue's `wire.rs` | a Hyperliquid SDK | ours: the wire is the mirror's contract, pinned by live probes; the SDK would add validation the devnet deliberately does not do (e.g. `Signature` is a shape, never verified) |
| `Backoff`, the feed `Budget` | `tokio-retry`, `governor` | ours: measured semantics (the trace-rung fallback's bounds; the poll budget) that the gates and docs pin |

## Keeping it symmetric

- new files land in the *domain folders*, one file per concern (a domain
  becomes a folder when it has parts: `apply/`, `feed_bridge/`);
- a shared fact goes to the spine (`mirror-devnet`/`mirror-anvil`/
  `mirror-config`) the moment a second profile needs it (`advance_cursor`,
  `parse_cli`, `exit_verdict`, `witness_for`, `holds_state`);
- naming: the profiles' verbs stay their own (`apply`/`follow`), the shared
  vocabulary is one spelling (`LATEST`, `HEALTHZ`, `ANVIL`);
- the fitness gates (`make fitness`) hold the line: the closure matrix, the
  orphan check, the AST scope, the docs/metrics cross-check.
