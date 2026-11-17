# Diorama

Two blockchain **mirrors** on one Kubernetes cluster, the shared crates they
run on, and the CCTP bridge between them:

- **diorama-mainnet** — an Ethereum mirror: reth + a local Lighthouse, a
  fork-mode anvil (`anvil-t`), and the engine that follows the chain;
- **diorama-venue** — a HyperEVM mirror: a fork-mode anvil (`anvil-v`), the
  venue engine (follow + bridge + shadow registry), and the venue simulator
  (the market/account plane the test harness drives);
- **diorama-cctp** — the standalone bridge that relays burns between the two
  mirrors.

Every deployment value lives in one gitignored file per instance — the
specialization — hardcoded, mounted into the pods as a file; everything
tracked is a template that boots unchanged on another machine. The three
instances are structurally identical — the same Makefile surface, the same
overlay shape — and a second deployment on another host is a clone plus one
edited specialization (see "Porting to another host").

## Boot a stack (from a fresh clone)

```sh
make up INSTANCE=diorama-venue    # the venue mirror (engine, anvil-v, venue-sim)
make up INSTANCE=diorama-mainnet  # the Ethereum mirror (reth, lighthouse, anvil-t, engine)
make up INSTANCE=diorama-cctp     # the bridge (its relay key goes in the specialization first)
```

`make up` requires exactly one deployment input — the specialization file
(it names the remedy if it is missing: copy the tracked template, hardcode
the values) — then images (`build`: the repo's binaries are built fresh, the
pinned third-party set imported) → secrets (the reth↔lighthouse JWT) → chain
state (`snapshot`: the venue re-forks from its source; the mainnet's reth
downloads its snapshot — the one step measured in hours, resumable) →
`apply`. Prerequisites:

- a single-node **k3s** cluster reachable by `kubectl`
  (`KUBECTL`, `CTR`, `SUDO`, `RUNTIME`, `K3S_STORAGE` in the `Makefile`
  override the plumbing if your box differs — they default to
  `kubectl`, `sudo k3s ctr`, `podman`, `/var/lib/rancher/k3s/storage`);
- `podman` (or `docker`) and a Rust toolchain for `cargo`-based gates.

Then watch it come up:

```sh
make status INSTANCE=diorama-venue   # pods, PVCs, engine status
make logs INSTANCE=diorama-venue     # follow every pod's log
make smoke INSTANCE=diorama-venue    # the venue's end-to-end smoke test
```

## Reach the stack from the host

The services are ClusterIP by design: the anvil cheatcode set and the
`/admin`+`/control` APIs must never bind a host interface (a NodePort exposes
them to the LAN). The host uses loopback port-forwards, and the harness's env
is derived, never hand-written:

```sh
make port-forward INSTANCE=diorama-venue &   # the forwards chosen in harness.env
make endpoints INSTANCE=diorama-venue        # CHAIN_999_* / VENUE_* / HL_INFO_API_URL (in-cluster DNS)
make endpoints INSTANCE=diorama-mainnet      # CHAIN_1_* / DIORAMA_ADMIN_URL
```

`make endpoints` renders `diorama-config export-env --section client`: service
DNS and chain ids derived from the merged instance config, under the exact key
names the consumer harness sources. The loopback ports live in `harness.env`
(see "Deployment data"); a port collision on another machine is a harness
edit, not a manifest one.

## Deployment data (the files that are never committed)

| file | what it carries |
| --- | --- |
| `config/instances/<instance>/specialization.toml` | the TOML mounted at `/etc/mirror`: every per-deployment value, hardcoded — endpoint groups (keyed ones included), fork source and pin, state roots, the JWT path, the relay key, watch scope, fixtures, markets |
| `k8s/overlays/<instance>/harness.env` | the host harness surface: the loopback port-forwards (no pod reads it; `make port-forward` bootstraps it from the template) |

The specialization is the ONLY per-deployment source of truth. It carries no
`${VAR}` references — env indirection would make it specific to one cluster's
wiring — and it is gitignored, which is what makes it a safe home for
credentials (a keyed ZAN URL, the bridge's relay wallet): the values never
enter git and never enter a cluster Secret or ConfigMap. The pods read it as
a file. `make secrets` generates only the reth↔lighthouse JWT.

## The config layering

`config/defaults.toml` → `specialization.toml` →
`config/instances/<instance>/instance.toml` (deep merge, arrays replace,
unknown keys are errors at every level). `defaults.toml` carries the workspace
conventions every deployment shares — the service ports, the journal's home,
the engine's `@anvil` target, the endpoint-pool policy and cadences — so
`instance.toml` states only what makes one deployment different (identity,
chain endpoint references, the measured deltas; a fresh instance's file is a
dozen lines) and `specialization.toml` hardcodes the per-deployment facts the
instance only names by reference (`@evm`, `@anvil`, `@engine_api`).
Validate the result without a cluster:

```sh
make config-check
```

`scripts/config-lint.sh` also gates the model: no `${VAR}` reference in any
TOML layer, no config env or Secret in the manifests (configuration is the
mounted file; the only Secret is the generated `reth-jwt`, file-mounted), the
kustomize build fails loudly when a specialization file is absent, the two
mirrors stay structurally and TOML-shape symmetric, and the audit
(`scripts/cluster-config-audit.txt`) records every key the live pods resolve
— live-read keys must exist in the merged config, live-dead keys must not
grow back.

## Porting to another host

```sh
git clone <this repo> && cd Diorama
cp config/instances/diorama-venue/specialization.example.toml \
   config/instances/diorama-venue/specialization.toml
# hardcode the deployment's values in the specialization, then:
make up INSTANCE=diorama-venue
```

The values a porting operator replaces are all in the specialization (each
documented in its template):

- `endpoints.evm` / `endpoints.hyperliquid_*` — the venue's source and market
  planes (keyed URLs go straight into the file; it is gitignored);
- `chains.venue.fork_url` / `fork_pin` — the venue's fork source and base;
- `endpoints.*` and `services.engine.state_dir` / `source_sync.jwt_path` —
  the mainnet's in-cluster wiring;
- the bridge's `endpoints.*`, `state_dir` and `relay_key`.

The claim class (`mirror-retain`, or the bridge's `diorama-retain`) and the
lighthouse checkpoint-sync URL are manifest literals: k8s-native facts, edited
in the manifests on an empty cluster only (a bound claim's class does not
rebind). What a clone cannot carry, and where the Makefile owns it instead:
images (`make build`), chain state (`make snapshot`), keys (`make secrets` —
the JWT is generated; the relay wallet is the specialization's).

## Transition notes (the current host)

The specialization replaces the wiring/Secret env plumbing. The one-time
conversion for the live host — the live values are exactly what each
specialization.toml must hardcode:

1. **Read the live values, write the specializations.** For each instance:
   the endpoint groups from the wiring ConfigMap
   (`kubectl -n <ns> get cm <wiring-map> -o jsonpath='{.data}'`), the fork
   pin from the mounted specialization (`kubectl -n <ns> exec deploy/anvil-v
   -- cat /state/pin`), the state roots (`/state`), the JWT path
   (`/secrets/jwt.hex`), and — for the bridge — the relay wallet from the
   legacy Secret (`kubectl -n diorama-cctp get secret cctp-relay-key -o
   jsonpath='{.data.relay-key}' | base64 -d`). Write them into
   `config/instances/<instance>/specialization.toml` as literals; then
   `make config-check` must print `ok` for all three with an empty
   environment (the files are self-sustained).
2. **Apply once, per instance.** `make apply INSTANCE=<name>` — the wiring
   ConfigMap and the `*-secrets` Secret objects leave the cluster, and every
   pod re-mounts the specialization file. The resolved values are identical
   (proven by `cargo run -p diorama-config --example effective` before and
   after), so the engines resume from their persisted cursors; the anvil pods
   read their fork source from the mounted file.
3. **The mainnet's two pre-bound claims.** Their PV `volumeName` left the
   tracked manifests long ago (a fresh host must bind its own volumes), and
   an apply refuses to patch the immutable field on the live claims.
   Reconcile once with
   `kubectl apply -k k8s/overlays/diorama-mainnet --server-side` (server
   dry-run verified: the only Invalid objects are the two bound claims, whose
   spec is unchanged by this work).

## Development gates

```sh
make ci          # cargo test --workspace, then the structural gates
make lint        # clippy, all targets, warnings are errors
make ast-scope   # fn/struct size and nesting analysis
```

`scripts/config-lint.sh` (part of `make fitness`) keeps the model honest: no
`${VAR}` reference in any TOML, no config env or Secret in the manifests, the
specialization build fails loudly when absent, the two mirrors stay
structurally symmetric, no machine-specific literal survives in a tracked
manifest, and the live audit gates the merged shape.

## Ops (read-only first)

```sh
make status INSTANCE=diorama-venue   # pods, PVCs, engine status
make mirror-freshness                # every plane's lag in one view
make substrate-attest                # read-only substrate invariants
make logs-check                      # warnings/errors per component, normalized
```

Drills and repairs (re-pins, snapshots, blob pruning) are the `repin`,
`snapshot`, `prune-blobs` targets in `make help`; each is instance-parameterized
(`make repin INSTANCE=diorama-mainnet`), idempotent, and states its own
prerequisite in its header comment.
