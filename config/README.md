# config/ — the layered configuration (plan §19)

One grammar, three layers:

| Layer | Home | Nature |
|---|---|---|
| defaults | `defaults.toml` | global for every instance and service; the single home of shared numbers (cadences, ring sizes, chunk limits). Embedded into the binaries at build time. |
| instance | `instances/<name>.toml` | one deployment: identity, chains, watchlist, service sections, ports. Mounted for every pod of the instance (one file per instance, many readers). |
| env | `MIRROR__*` | operator debug overrides, highest precedence, logged loudly at boot. |

`${VAR}` references in string values resolve from the pod environment at load
time (deploy-time wiring). A service refuses to boot on an unresolved
reference, naming the variable; `mirror-config check` (which needs no
deployment environment) reports them as deploy-time wiring instead.

**One file per instance, many readers — each pod provides only its own
sections' wiring.** The venue instance is read by three pods with three
different env sets (the engine's, the simulator's, the bridge's), so each
service boots with `Instance::inspect` + `require_resolved` over *its* section
values: the other services' `${VAR}`s stay unresolved in that pod by design,
while a reference still present in its own wiring is a named boot refusal
(`load` is the strict form for single-reader instances, e.g. the mainnet
mirror's).

Merge semantics: deep merge; arrays replace, never concatenate; unknown keys
are errors at every level.

## The section map (who reads what)

| Section | Read by | Carries |
|---|---|---|
| `[instance]` | every service | identity: name, namespace, profile |
| `[chains.l1]` | the mainnet engine, the anvil-t init container | the mainnet source; `fork_offset` (its single home) |
| `[chains.venue]` | the venue engine, the simulator, the CCTP bridge | the venue source: HL API/WS, the fork RPC |
| `[services.engine.*]` | the engine (both profiles — the same key, the profile's shape) | reth / anvil (`target_url`; each instance points it at `ANVIL_T_URL` or `ANVIL_V_URL`) / source_sync / verifier / journal / beacon / http / feed_bridge (optional: absent = the devnet's own price writers own the mock aggregators; keys: `poll_interval_ms`, `confirm_window_ms` (default 60 s — a publish that does not land is re-sent), `writer` (give it a dedicated account), `feeds`) |
| `[services.venue_sim.*]` | the simulator | info/control ports, markets, rate budget, persistence |
| `[services.cctp_bridge.*]` | the CCTP bridge | T/V RPCs, relay key, attester seed, iris/control ports, the relayer mode (`auto`/`manual`) |
| `[watchlist]` | the mainnet engine | the closure scope; one watchlist, two readers (`check` + `closure-check`) |

The engines' sections are named per mirror — `engine` is the mainnet
profile's, `venue` is the venue profile's (the plan's per-service grouping,
§19.3; the venue's engine section doubles as the mirror's own section for
chain-level knobs). Each service reads only its own section; one file per
instance, many readers.

## The CLI

```sh
mirror-config check config/instances/mainnet-mirror/instance.toml          # validate + wiring report
mirror-config get   config/instances/mainnet-mirror/instance.toml chains.l1.fork_offset
mirror-config export-env config/instances/mainnet-mirror/instance.toml --section anvil   # FORK_OFFSET=128
```

`make config-check` checks **every** instance unit under `config/instances/` and is part
of `make fitness`. The anvil-t init container reads `FORK_OFFSET` straight from
the mounted TOML (a `sed` read — the alpine init container carries no Rust
binary; the instance file is the offset's single home), and
`mirror-config export-env --section anvil` renders the same fact for anything
with the CLI available. The venue's anvil forks at the source's latest block,
so it reads no offset.

**How a service finds its instance file.** Every service is invoked
`run --config <path>` (the manifests' `args:`), defaulting to
`/etc/mirror/instance.toml` — the path every pod mounts via
`configMapGenerator`. The mainnet engine's CLI adds the `selftest` and
`rewind` subcommands; the plain services accept exactly `run`. The plan's
`MIRROR_INSTANCE`/`MIRROR_CONFIG` envs are therefore unnecessary: one file per
instance, many readers, and the mount already selects it; the argument exists
for local runs (the scratch stacks pass their own file).

## Where a fact lives (§19.2)

- protocol facts and measured conventions → code constants (`crates/anvil`,
  the project crates)
- deployment wiring (URLs, paths) → env, referenced from the instance as `${VAR}`
- operational tunables → the two TOML layers above
- secrets → k8s Secrets (paths only, never values)

## `MIRROR__` override syntax

```sh
MIRROR__SERVICES__ENGINE__LOOPS__POLL_INTERVAL_MS=2000
```

Path segments are the TOML path, uppercased, joined by `__` (a single `_`
stays inside a key). The path must already exist in the merged configuration;
a typo fails the boot naming it. Overrides never appear in manifests — they
are incident-time operator tools.
