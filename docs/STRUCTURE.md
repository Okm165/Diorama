# Structure map — the two mirrors, side by side

Both engines are one design over two sources. This page is the file-by-file
map: what each crate carries, which files pair with which, and where the two
deliberately differ (with the reason). It is the artifact behind the
symmetry sweeps — regenerate the readings with
`cargo run -q -p ast-scope -- crates bins`.

## The layout at a glance

| concern | mainnet | venue | note |
|---|---|---|---|
| crate | `crates/mainnet` | `crates/venue` | the trees below |
| engine bin | `bins/mainnet-engine` | `bins/venue-engine` | |
| profile-only bin | — | `bins/venue-sim` | the simulator has no mainnet twin |
| cross-mirror bins | `bins/cctp-bridge`, `bins/ast-scope` | (shared) | the bridge and the fitness gate span both |
| instance | `config/instances/mainnet-mirror/` | `config/instances/venue-mirror/` | TOML + kustomization, section-for-section alike |
| k8s base | `k8s/base/mainnet-mirror/` | `k8s/base/venue-mirror/` | + the shared and cctp-bridge bases |
| k8s overlay | `k8s/overlays/mainnet-mirror/` | `k8s/overlays/venue-mirror/` | |
| cluster suites | `mainnet-{smoke,scenario,proof,product-sim,closure-check}` | `venue-{smoke,scenario,proof,order-proof}` | one-sided rows say why (docs/TESTING.md) |
| cross-mirror | `engine-symmetry`, `cctp-*`, `log-surface`, `attest`, `shadow-audit` | (shared) | |
| explainer | `docs/HOW-IT-WORKS.md` | `docs/HOW-IT-WORKS-VENUE.md` | the same §-numbered walk-through |
| diagram pairs | `docs/img/how-topology.png`, `how-cow.png` | `docs/img/how-topology-venue.png`, `how-cow-venue.png` | each rendered from its explainer's ASCII art |

## The trees

In the trees, **bold** marks a file name both sides carry (the pairs);
the rest of each row is the profile's own logic, named for its mechanism.

| `crates/mainnet/src/` | `crates/venue/src/` | role |
|---|---|---|
| `lib.rs` | `lib.rs` | the crate doc, the mod list, the public types' compile-time pin |
| `service.rs` | `service.rs` | the service entry: CLI (`run`/`selftest`[`/rewind`]), the boot call, the loop spawner (the venue's carries the simulator's entry too: `sim_entry`, the sim's `load_wiring`, its `serve` listeners) |
| `ctx.rs` | `ctx.rs` | `Ctx` (the profile's clients + sections + the shared `Plane`) and its `impl Engine` |
| `driver.rs` | `driver.rs` | `boot` (wait, assert the chain, anchor/reconcile, audit capabilities) and the loops |
| `apply/` (**mod**, **parity**, prepend) | `follow/` (**mod**, audit, **parity**) | the per-unit step: compose → COW → land → measure — the same parts on both sides (the step, the measurement, one distinct concern: the mainnet's devnet prepend / the venue's uncovered-log trail) |
| `error.rs` | `error.rs` | the profile's error vocabulary (the shared devnet/anvil boundaries composed in) |
| `http.rs` | `http.rs` | the shared route table + the profile's own routes (`/admin/time`; the venue's simulator API is `api/`) |
| `metrics.rs` | `metrics.rs` | the profile's metric set (rendered beside the shared gauges) |
| `profile.rs` | `profile.rs` | the divergence set, filled: `bundle`/`policies`/`build` + the `Wiring`/`Deps` pair, under the same decision table |
| `reset.rs` | `reset.rs` | the profile's `ResetDetector` (cursor hash / pin canary) |
| `selftest.rs` | `selftest.rs` | the scratch-stack selftest (`Targets` + `run`) |
| `source.rs` | `source.rs` | the profile's source facts: the `Source` impl (`RethSource`) + the `reth`/`target` builders / the `DECLARED` capabilities + the `pinned`/`follow` builders (the shared `target` is the spine's `mirror_devnet::source::target` now); the pin-exact read shape is the spine's `PinnedSource` |
| `feed/` (**mod**, chainlink, http, **metrics**, policy) | `feed/` (**mod**, budget, **metrics**, rest, ws) | the market/price plane: the Chainlink feed bridge (the parent's answers → T's mock aggregators) / the HL market feed (REST + WS + budget) into the simulator |
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
reads identically on both sides, as do the spine's own helpers the profiles
call (`build_clients`, `halt_gate`, `source::target`, `Drift::from_engine`,
`Plane::clear_reapply_from`, `journal::read_before_values`, `Write::storage_key`,
`Rpc::mine_pending`); everything else is the profile's own rule set:

| file | mainnet | venue |
|---|---|---|
| `ctx.rs` | `Ctx::{new, follow_head, follow_tag}` + `impl Engine` (`core()` + the profile's pieces) | `Ctx::{new}` + `impl Engine` (the same shape; the pin vocabulary is the spine's `pin`) |
| `driver.rs` | `boot` + `apply_loop`/`shadow_loop`/`source_sync_supervisor` + the cursor/reorg rules (`next_block`, `validate_cursor`, `on_reset`, `rebase`, `resume_point`, `on_rebase`, `gap_within_limit`, `reorg_continuity`) + `scan_once`, `probes_now` | `boot` + `follow_loop`/`mine_loop`/`pass` + `reconcile`/`pin_read`/`anchor_cursor`/`recover_from_reset`/`run_epoch_rule` + `scan_and_advance`, `mine_pending`, `probes_at_pin`, `probes_now` (the producer's own cadence: a slow window against the public source must not delay an external tx — the `MinePolicy` split) |
| `service.rs` | `main_entry` + `run` + `require_wiring(what, endpoints)` + `wiring_extra` + `build_bridge` + `spawn_loops` (the `EngineService` hooks: the label + `wiring_extra`) | `main_entry` + `run` + `require_wiring(what, endpoints)` + `spawn_loops` (+ `sim_entry`, `load_wiring`, `serve`, the simulator's spawns; the hook is the label alone) |
| `profile.rs` | `bundle`/`policies`/`build` + `Wiring`/`Deps` | `bundle`/`policies`/`build` + `Wiring`/`Deps` (the closures/watch-set builders are the spine's `closure::{closures, watch_set}`) |
| `http.rs` | `app` + `status_doc` (+ `admin_time`) | `app` + `status_doc` |
| `selftest.rs` | `run` (+ `run_one_block`) | `run` (+ `windows_loop`, `wait_for_move`) |
| `source.rs` | `RethSource::{new, trace_block_per_tx}` (a `RawSource` wrapper: the raw reads are the spine's) + `impl Source` + the `DECLARED` caps | `DECLARED` + `pinned`/`follow` (the read shape is the spine's `PinnedSource`; both crates declare their caps as one `DECLARED` const) |
| `reset.rs` | `CursorHash` + `below_fork_base` | `PinCanary` + the canary io |
| `metrics.rs` | `MainnetMetrics` (registry + render + overlap test) | `VenueMetrics` (the same three) |
| `error.rs` | the profile's variants + `impl ShellError` + `mirror_devnet::shell_fault_conversion!` | the same shape |

Struct level: both `Ctx`es carry the same ordered core (the chain section, the
engine section, the target anvil, the source, the mirror bundle, the composer
target, the plane, the profile metrics, the drift log) with the profile's own
fields at the ends (`pin`/`uncovered_seen`; `beacon`/`engine_api`/
`feed_bridge_metrics`) — one *shape*, not one struct: the core's middle fields
are the profile's own types (the `Source` impl, the `Bundle`, the metrics), so
a shared type would need generics where the ordered shape already gives the
reading, and the construction is a struct literal (there is no `Core::new` to
grow past the 5-arg ratchet); both `StatusResponse`s flatten the shared `SharedStatus`
beside the profile's fields; both `profile::Deps` are the "one struct, not five
arguments" input to `build`.

## The judged asymmetries

Each of these is a decision, not drift (the design record's divergence table
in `SPINE.md` and the profiles' own tables are the full list):

| asymmetry | why it stands |
|---|---|
| `apply/` (3 files) vs `follow/` (3 files) | the parts are symmetric now (step + `parity.rs` + one distinct concern); the third file's name follows its mechanism: the mainnet's devnet prepend, the venue's uncovered-log audit (venue-only — reth's native BALs cannot leave a log uncovered) |
| `feed/` vs `feed/` (same name, both sides) | the same role — the profile's live-data plane — in different domains: a price *bridge* (Chainlink answers → T's mock aggregators) vs the *market feed* (HL REST/WS into the simulator); the internals differ by domain (`chainlink`/`policy`/`http` vs `rest`/`ws`/`budget`), and the component names (`FeedBridge`, `feed_bridge_*` series, the config key) keep the bridge's own vocabulary |
| `source_sync/` (mainnet) | the CL + engine-API transports have no venue counterpart (the HL API is one HTTP client) |
| `api/`, `sim/`, `wire.rs` (venue) | the simulator's served surface has no mainnet counterpart |
| `service.rs` carries `sim_entry` | one home per service: the venue crate serves two binaries (`venue-engine`, `venue-sim`) |
| metric namespaces (`mirror_*` / `venue_*`) | breaking to rename; the docs pin them |
| the `EngineCore` construction (both `core()`s) | the two literals are field-for-field identical in order (`plane`, `mirror`, `target`, `source`, `source_url`, `engine`) — the shape is already shared, and the trait reading shows it; an `EngineCore::new` would take six arguments (the 5-arg ratchet) or a params struct that repeats the same literal under another name, so the literal is the minimal shared form |
| the four oldest deployed-stack suites (`mainnet-{proof,scenario,product-sim,closure-check}`) run leniently (no `set -euo pipefail`) | coherent, not drift: every check counts and the verdict is the final count, so one failed stack read cannot mask the sweep; the newer suites (the venue trio, the smokes, the cctps) fail fast instead — a failed read aborts with its status. Both reach a verdict; changing the four is unverifiable outside the cluster |

## The AST readings (and what each one found)

The tool (`bins/ast-scope`) reads the workspace with `syn` and reports;
every reading below was acted on or judged. It measures the mirrors, not
itself: its own items are excluded from the readings (the scope gate still
covers them), and it walks trait bodies too — the shell's provided defaults
are production shape. It also prints the **structure map** first: the trees table's file pairing, item by item — each pair's
shared production items, its shared **test** items (the twins TESTING lists,
generated), its one-sided names, and a closing section for the one-sided
lanes (every mirror `.rs` outside the pairing table, with its prod/test
counts — so the map covers every file):

| reading | outcome |
|---|---|
| scope (`fn` ≤ 60 lines / depth ≤ 5, struct ≤ 20 fields, enum ≤ 25 variants, `registry!` ≤ 40 series) | the largest production fn is `apply::step` (51 lines, a deliberate linear sequence); depth ≤ 4; the macro-generated sets are read by the tool since this pass (`VenueMetrics` 26, `MainnetMetrics` 22, `Metrics` 3) — the one shape `syn` cannot see from the item walk |
| dead functions / dead members | none (the AST and the text agree); the macro/value-only list is audited too — 14 genuine macro/attribute references (the trait-default rows are gone: the tool walks trait bodies since this pass) |
| duplicates | only deliberate twins (the two `run`s and the `EngineService` hooks — the symmetrized spine — the `impl Engine` accessors, the reset detectors' constructors, the per-profile error helpers, the `Source` impls' `header` — both delegate to the raw read); `healthz`/`livez` is the tool's one false positive (one is pause-aware, one is not — the bodies differ) |
| compositions (own-fn sequences, the stdlib ops filtered out) | the domain pipelines, counted by distinct caller (a run of same calls inside one function counts once): the money math (`mul -> mul` 6x, `mul -> add` 5x, `add -> add` 5x, `sub -> mul` 3x), the metrics renders (`push_counter -> push_counter` 3x — the labeled pair `push_gauge`/`push_counter` in the spine; the registry macro's unlabeled path is `push_metric`), the config validators (`check_http_url -> check_http_url` 4x), the exchange envelopes (`default_ok -> err_envelope`), the spot credit (`spot_entry -> add` — extracted as `credit_entry`), the bins' shells (`main_entry -> run_main`) |
| the tests' pairing (the map's per-file test lines) | the shared names per pair (the twins list, generated by the map); the one-sided tests are the domains, each with its why: the detectors' own rules (`the_fork_base_rule_is_strict` / `the_canary_round_trips_including_genesis`), the sources' capability declarations (the near-twin pair), the mainnet's drift and feed-policy tests beside their code (`only_anomalies_are_logged`, `the_pause_rule_never_overwrites_a_scenario`), the venue's feed/sim/api lanes (live-proven where their rules read a chain) — and the chain-bound files name their coverage: `tick.rs`/`validate.rs` say their lanes carry the tests (e.g. `a_delisted_asset_settles_and_refuses_new_orders` in `order.rs`), and the mainnet's prepend has no unit test because all four of its functions read a chain (the scenario's COW legs prove them) |
| trait implementations (both mirrors) | the shared construction shape, per trait: `Engine` (`core`, `probes_now`, `profile_metrics`, `record_scan`, `status`), `EngineService` (`boot`, `context`, `log_start`, `spawn_loops`, `urls`, and the `wiring_label` hook — `require_wiring` itself is the shell's provided default now, built from the hooks), `ResetDetector` (`detect`) — the methods whose *shapes* pair by contract while the bodies stay the profile's; the conversion boilerplate (`From`/`TryFrom`/`Into`/`Deref`) is filtered as the language's shape; the structure map pairs trait methods by `Trait.method` (so `reset.rs`'s `detect` reads as paired) |
| wrappers (one-field structs) | all 39 judged: the serde wire shapes (the mirrors' contracts, pinned by live probes), the toml sections, the domain newtypes (`Domain`, `CursorHash`, `PinCanary`, `Budget`, `Attester`) — none maps to an external crate's spelling |
| twins (same-named fns across the mirrors, shared 3-op sequences over the smaller side) | the symmetrization's full map (31 pairs, measured 2026-10-25), in three bands: the shared construction at 100% (`main_entry` — `build_clients`, `halt_gate`, `target` and the reapply rule moved to the spine in the symmetry wave); the measured skeletons (`Ctx::new` 42%, `profile::build` 40%, `selftest_cmd`/`spawn_loops` 33%, `status_doc` 17%); the divergences at 0% (`step`, `report_applied`, `parity_step`, `Ctx::{core,probes_now,record_scan,profile_metrics,status}`, `boot`, `bundle`, `policies`) — pairs by name and contract, not by code. The `EngineService` hooks (`Ctx::{urls,context,boot,log_start}`) and the two `run`s read 0% too: their bodies are one op, and a one-op body has no 3-grams (the `duplicates` reading is where their identical bodies land). Three rows are name collisions, not pairs (`feed/policy::step` vs `follow::step`; the four `run`s across modules; the two `advance_cursor`s — the applier's devnet cursor vs the venue's source cursor) |
| near-duplicates / similar records | none after the pass; the outcomes read 88% alike (`ApplyOutcome`/`FollowOutcome` — the `writes` vocabulary aligned; the venue's extra `applied` is its windows' count, and a shared type would make the mainnet's one-block apply report an `applied` it does not track); the `FrontendOpenOrder`/`StatusOrderInner` overlap is the API's own two shapes — the *mapping* is now one function (`frontend_row`), the shapes stay exact |
| reuse (shared op sequences) | the one real extraction the tool saw: the cursor advance (`3× … cursor_from → set_cursor`) became `mirror_devnet::state::advance_cursor`, used by the applier, the venue scan and the admin's reset; the rest are axum's router idiom. By inspection: the poll cadence's 200 ms floor became `LoopsSection::poll()` (three drivers) and the config's section pairs became `take_pair` (five sites). Judged: the feed's `mark`/`oracle`/`funding` stay three 4-line readers (their override sources differ; a helper would need a closure pair per site). Second pass: `store::ensure_dir` (three stores), `wire::decimal_or_refuse` (three order fields), `Sim::mutate_account` (three account verbs) extracted; `record_window_metrics` narrowed. Third pass: `MarketFeed::mark_or` (four order-lane reads), `Sim::mutate_state` (three durable writers — the `touch`/`mark_dirty` pair) |
| dispersion | `Sim` spans four files — the domain folder's pattern (one impl block per concern) |
| narrow context | `record_window_metrics` was narrowed to the metrics it writes; `parity_step` keeps `&Ctx` (the mainnet's lens takes `&Ctx` too — the pair's shape wins) |
| plumbing | judged: each parameter is a distinct role (the `step`s, the metric writers `push_*`, `scan_to`, `boot_prefix`, `require_engine_wiring`, the wire builders `burn_calldata`/`handle_deploy_receipt`) — a parameter object would obscure them |
| the `Engine` accessors + the reset detectors' `new` | judged: the accessor hooks are the trait's required *data* (fields, not behavior — a default cannot read a field), and the detectors' single-field constructors are Rust's newtype idiom, not shared behavior; a shared base type would add indirection without removing a divergence |
| the test-only `is_normal` helper | judged: one line per crate's test module; a spine helper would ship test scaffolding in the public API |
| the three `NoReset` copies (each profile's test module + the spine's) | resolved: one home in the spine (`mirror_devnet::spine::NoReset`) — a detector, not a fixture: the identity element of the `ResetDetector` algebra, so the public API is where it belongs; the fixture helpers (`is_normal`, `series_names`) stay per-crate |
| the gap guard's home (`driver.rs` mainnet / `follow/mod.rs` venue) | judged: the mainnet's is a *cursor* rule beside `next_block`; the venue's is the window's own guard. Both predicates are pure and tested on their own side (the mainnet's adds the rebase bound, the venue has none by design); the pause itself is a plane state — one counter bump, one drift row and one error per pause *event*, never per pass (measured: a 760-block gap re-logged every pass), with a resume line on the edge out |
| the venue driver's unit tests | judged: its rules (the epoch decision, the pin anchor) read a chain — the live proofs cover them (`venue-selftest`, `venue-scenario`), and a chainless unit test would assert nothing (the placeholder was removed) |
| the paired test names | pinned: shared files share their test names (`the_http_status_mapping…`, `the_registry_series_do_not_overlap`, `the_shell_fault_converts_into_the_taxonomy`); the step folders' tests name their own contracts (`the_parity_…`) |
| ~130 `pub` items that are file-local by name | judged: the count is name-based — a struct's *fields* are read cross-file by inference without the type name, so a mass tightening would be unsafe; the AST gate and `-D warnings` cover the real cases |
| the per-crate `series_names` test helper (and `is_normal`) | judged: a shared helper would ship test scaffolding in the public API (and three of the six crates cannot depend on the spine); one line per test module is the honest cost |
| divisions (non-literal divisors) | all guarded: `ratio_or` (an explicit zero branch), `maintenance_rate` (`max_leverage == 0`), `liquidation_px` (`szi`/`denominator`), `adl_ranking` (a non-positive account value skips), `floor_to_core_wei`/`round_to_sig_figs` (a checked `pow10`/`step ≥ 1`) |
| casts (narrow `as`) | judged safe: date fields, decimal scales (≤ 28), decimals (≤ 18), booleans to `u8` |
| repeated literals | named once: `mirror_anvil::{LATEST, TRACE_BLOCK_METHOD, JSON_CONTENT_TYPE, ETH_GET_BALANCE}`, `mirror_devnet::handlers::{HEALTHZ, METRICS}`, the drivers' `ANVIL`, the config's section requirements, the journal's io context, the sim's money contexts, the venue's order-type names and margin mode, `SIM_POISONED` — the reading is now empty: the instrument's own labels (`struct`, `enum`, the tree prefixes) are excluded, and the remaining pair (`"none"`: the diff kind's name and the order status) is two vocabularies, under the three-body line |

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
| the KZG versioned hash | `alloy-eips::eip4844::kzg_to_versioned_hash` | **imported** this wave: the local wrapper keeps its `&Bytes48` signature (stronger than the external's `&[u8]`), the `sha2` dependency retired |
| `0x`-prefixed hex | const-hex's `encode_prefixed` | **imported** this wave (three CCTP sites); `decode_hex`'s redundant prefix trim dropped (const-hex strips it itself) |
| `pow10` | `rust_decimal::Decimal::from_scientific` | **imported** this wave: the same `Option` at the scale cap, pinned by a test |
| money | `rust_decimal` | **imported** (f64 is not a money type) |
| HTTP/WS/JSON/CLI/runtime | `axum`, `tokio-tungstenite`, `serde`, `clap`, `tokio`, `reqwest` | **imported** |
| the write/read model (`Write`, `Key`, `Capabilities`) | — | ours: no crate models anvil's cheatcode surface; the enums are narrower than any general diff type |
| `ChainCursor`, `StateFile`, `Shadow`, `Journal` | — | ours: the devnet's persisted state is the mirror's own contract |
| the metrics registry + exposition | `metrics`/`prometheus` | ours, deliberately: a dependency-light registry whose render is gated (the overlap tests, the docs' metric table) |
| `FollowHead` | `alloy`'s `BlockNumberOrTag` | ours: three variants, `deny_unknown_fields`, one `tag()` |
| the venue's `wire.rs` | a Hyperliquid SDK | ours: the wire is the mirror's contract, pinned by live probes; the SDK would add validation the devnet deliberately does not do (e.g. `Signature` is a shape, never verified) |
| `Backoff`, the feed `Budget` | `tokio-retry`, `governor` | ours: measured semantics (the trace-rung fallback's bounds; the poll budget) that the gates and docs pin |
| the 40 one-field structs (the `wrappers` reading) | — | ours, systematically: the wire shapes, the toml sections and the domain newtypes above — the reading lists them so the judgment is made once, not rediscovered |

## Keeping it symmetric

- new files land in the *domain folders*, one file per concern (a domain
  becomes a folder when it has parts: `apply/`, `feed/`);
- a shared fact goes to the spine (`mirror-devnet`/`mirror-anvil`/
  `mirror-config`) the moment a second profile needs it (`advance_cursor`,
  `parse_cli`, `exit_verdict`, `poll`, `witness_for`, `holds_state`,
  `LATEST`, `HEALTHZ`);
- naming: the profiles' verbs stay their own (`apply`/`follow`), the shared
  vocabulary is one spelling (`LATEST`, `HEALTHZ`, `ANVIL`);
- the spine crates (`devnet`, `anvil`, `config`, `cctp`) stay flat: their
  files are one layered system (the registry, the journal, the state, the
  handlers, the engine) that every step cross-references — a folder split
  would fight the dependency law the closure matrix enforces; the folders
  are the profile crates' domain pattern;
- the gates (`make fitness`) hold the line: the lint ratchet, the design
  assertions, the config check, the AST scope.
