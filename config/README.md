# config/ — the layered configuration

One grammar, four layers (`defaults <- project <- instance <- env`):

| Layer | Home | Nature |
|---|---|---|
| defaults | `defaults.toml` | global for every instance and service; the single home of shared numbers (cadences, ring sizes, chunk limits). Embedded into the binaries at build time. |
| project | `instances/<name>/<project>.toml` | the product specialization: endpoint groups, watchlist, fixture ownership, probes, closures, markets. Named by `[instance].project`; **gitignored**, with a tracked `<project>.example.toml` template. Absent = a generic mirror. |
| instance | `instances/<name>/instance.toml` | one deployment's facts: identity, profile, chain shape, state paths, ports. Mounted for every pod of the instance (one file per instance, many readers). |
| env | `DIORAMA__*` | operator debug overrides, highest precedence, logged loudly at boot. |

`${VAR}` references in string values resolve from the pod environment at load
time (deploy-time wiring). A service refuses to boot on an unresolved
reference, naming the variable; `diorama-config check` (which needs no
deployment environment) reports them as deploy-time wiring instead.

## Endpoints: one list, one home

Every URL list lives in a named `[endpoints.<name>]` group and is referenced
from a field as `@<name>`:

```toml
[endpoints.hyperevm]
http = ["${HL_RPC_1}", "${HL_RPC_2}"]
ws   = ["wss://rpc.example/ws"]
roles = ["archive"]          # optional; empty = no declared capability
chain_id = 999               # optional; the boot assert still pins the engine

[chains.venue]
source_urls = "@hyperevm"
api_urls    = "@hyperliquid_api"
ws_urls     = "@hyperliquid_ws"
```

`http` entries are JSON-RPC/REST read pools; `ws` entries are the websocket
pool. The loader expands the reference before validation, so the same group
feeds the engine, the simulator, the CCTP bridge and the `export-env` client
block without a second spelling. A group reference that is unknown or empty
is a boot error naming the group, never a silent single URL.

The shared `diorama-anvil::EndpointPool` gives every lane the same policy:
each round starts at a random healthy endpoint, a transport/throttle/plan/
gateway fault moves to the next, a failed endpoint is cooled down
exponentially (1 s → 60 s) and retried in a later round. When every endpoint
is cooling the pool **refuses admission**, names the earliest retry, and
allows one jittered emergency probe per window — an outage is neither a
permanent refusal nor a retry amplifier. The policy is one table:

```toml
[endpoint_defaults]
rounds = 4              # full rotations per logical call
max_attempts = 12       # hard per-call budget across all rounds
cooldown_base_ms = 1000
cooldown_max_ms = 60000
jitter_ms = 250
emergency_probe = true
emergency_probe_ms = 1000
```

JSON-RPC, the venue REST client, the venue websocket client and the beacon
client all consume the same executor (`EndpointPool::execute`); a transport
implements one request against one endpoint and never re-implements rotation,
sleep or health. Writes use a single-endpoint client and a pooled client
refuses them. The websocket lane is live only after its first data frame and
fails an endpoint that goes data-idle, so a half-open socket rotates instead
of serving stale marks.

**One file per instance, many readers — each pod provides only its own
sections' wiring.** A venue instance is read by its engine and simulator with
different env sets, so each service boots with `Instance::inspect` +
`require_resolved` over *its* section values: the other services' `${VAR}`s
stay unresolved in that pod by design, while a reference still present in its
own wiring is a named boot refusal (`load` is the strict form for
single-reader instances, e.g. the mainnet mirror's). The CCTP bridge is its
own `cctp` profile and its own instance — it never rides a mirror's file.

**Composition is across instances.** One process = one role. To run two
venues, one mainnet and two bridges, create five instance directories (each
`instance.toml` + optional `<project>.toml`) and five deployments; they share
the spine crates and the endpoint-pool policy, not a config file. Nothing in
the schema assumes a single mainnet, a single venue, or one bridge.

A second mirror is a copy of a profile's instance directory with a new
`[instance].name`/`namespace`, its own state paths and ports, and its own
endpoint groups; its overlay is a copy of the matching `k8s/overlays/<name>`
with the namespace and instance path changed. A second bridge is the same
recipe under `profile = "cctp"` with its own `[services.cctp_bridge]` chain
list and lanes. The profiles (`mainnet`, `venue`, `cctp`, and future kinds)
never share a process, so a deployment can compose any mix without a schema
change. `make config-bootstrap` copies each tracked `daloc.example.toml` to
the gitignored live `daloc.toml` when the live file is absent.

The CCTP bridge's topology is data, not a hardcoded pair:

```toml
[[services.cctp_bridge.chains]]
id = "ethereum"
family = "ethereum"
chain_id = 1
domain = 0
rpc_urls = "@t_rpc"
usdc = "0xA0b8…"
token_messenger = "0x28b5…"
message_transmitter = "0x81D4…"

[[services.cctp_bridge.chains]]
id = "hyperevm"
family = "hyperliquid"
chain_id = 999
domain = 19
rpc_urls = "@anvil_v"
# …the same three addresses on this domain

[[services.cctp_bridge.lanes]]
source = "ethereum"
destination = "hyperevm"

[[services.cctp_bridge.lanes]]
source = "hyperevm"
destination = "ethereum"
```

One watcher runs per lane, the relayer serves every lane, and the boot wiring
asserts each lane's source against its destination. A 2-venue/1-mainnet or
4-mainnet/3-venue deployment is a set of mirror instances plus one or more
CCTP instances whose lanes name the chains they bridge; a second provider is
a second `cctp` instance with its own relay key, state directory and lanes.
The legacy `t_url`/`v_url` pair remains accepted as a two-chain shorthand, but
a composed topology should state `[[chains]]` and `[[lanes]]` explicitly.

Merge semantics: deep merge; arrays replace, never concatenate; unknown keys
are errors at every level.

## The section map (who reads what)

| Section | Read by | Carries |
|---|---|---|
| `[instance]` | every service | identity: name, namespace, profile, optional `project` |
| `[endpoints.<name>]` | every service that reads a URL | the one home of HTTP and WS URL lists |
| `[chains.l1]` | the mainnet engine, the anvil-t init container | the mainnet source (`@reth`); `fork_offset` (its single home) |
| `[chains.venue]` | the venue engine, the simulator, the CCTP bridge | the venue source (`@hyperevm`), API/WS pools (`@hyperliquid_api`, `@hyperliquid_ws`) |
| `[services.engine.*]` | the engine (both profiles — the same key, the profile's shape) | reth / anvil (`target_url`; each instance points it at `ANVIL_T_URL` or `ANVIL_V_URL`) / source_sync / verifier / journal / beacon / http / feed_bridge (optional: absent = the devnet's own price writers own the mock aggregators; keys: `poll_interval_ms`, `confirm_window_ms` (required: how long a publish may stay unconfirmed before it is re-sent), `writer` (give it a dedicated account), `feeds`) |
| `[services.venue_sim.*]` | the simulator | info/control ports, markets, rate budget, persistence |
| `[services.cctp_bridge.*]` | the CCTP bridge (its own `cctp` profile) | the `[[chains]]` list (id/family/chain id/domain/RPC/contracts), the directed `[[lanes]]`, relay key, attester seed, the Iris `fast_burn_allowance`, iris/control ports, the relayer mode (`auto`/`manual`) |
| `[endpoint_defaults]` | every lane | rounds, per-call attempt budget, cooldown base/cap, jitter, emergency probe |
| `[watchlist]` | the mainnet engine | the closure scope; one watchlist, two readers (`check` + `closure-check`) |

The engines' sections are named per mirror — `engine` is the mainnet
profile's, `venue` is the venue profile's (the per-service grouping;
the venue's engine section doubles as the mirror's own section for
chain-level knobs). Each service reads only its own section; one file per
instance, many readers.

## The CLI

```sh
diorama-config check config/instances/diorama-mainnet/instance.toml          # validate + wiring report
diorama-config get   config/instances/diorama-mainnet/instance.toml chains.l1.fork_offset
diorama-config export-env config/instances/diorama-mainnet/instance.toml --section anvil   # FORK_OFFSET=128
```

`make config-check` checks **every** instance unit under `config/instances/` and is part
of `make fitness`. The anvil-t init container reads `FORK_OFFSET` straight from
the mounted TOML (a `sed` read — the alpine init container carries no Rust
binary; the instance file is the offset's single home), and
`diorama-config export-env --section anvil` renders the same fact for anything
with the CLI available. The venue's anvil forks at the source's latest block,
so it reads no offset.

**How a service finds its instance file.** Every service is invoked
`run --config <path>` (the manifests' `args:`), defaulting to
`/etc/mirror/instance.toml` — the path every pod mounts via
`configMapGenerator`. The mainnet engine's CLI adds the `selftest` and
`rewind` subcommands; the plain services accept exactly `run`. The plan's
`DIORAMA_INSTANCE`/`DIORAMA_CONFIG` envs are therefore unnecessary: one file per
instance, many readers, and the mount already selects it; the argument exists
for local runs (the scratch stacks pass their own file).

## Where a fact lives

- protocol facts and measured conventions → code constants (`crates/anvil`,
  the project crates)
- deployment wiring (URLs, paths) → env, referenced from the instance as `${VAR}`
- operational tunables → the two TOML layers above
- secrets → k8s Secrets (paths only, never values)

## `DIORAMA__` override syntax

```sh
DIORAMA__SERVICES__ENGINE__LOOPS__POLL_INTERVAL_MS=2000
```

Path segments are the TOML path, uppercased, joined by `__` (a single `_`
stays inside a key). The path must already exist in the merged configuration;
a typo fails the boot naming it. Overrides never appear in manifests — they
are incident-time operator tools.
