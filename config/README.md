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

Merge semantics: deep merge; arrays replace, never concatenate; unknown keys
are errors at every level.

## The CLI

```sh
mirror-config check config/instances/mainnet-mirror.toml          # validate + wiring report
mirror-config get   config/instances/mainnet-mirror.toml chains.l1.fork_offset
mirror-config export-env config/instances/mainnet-mirror.toml --section anvil   # FORK_OFFSET=128
```

`make config-check` runs the check in CI. The anvil init containers read
`FORK_OFFSET` via `export-env` — the instance file is the offset's single
home.

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
