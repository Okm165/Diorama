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

Every deployment value lives in gitignored env files per instance; everything
tracked is a template that boots unchanged on another machine. The three
instances are structurally identical — the same Makefile surface, the same
overlay shape — and a second deployment on another host is a clone plus one
edited env (see "Porting to another host").

## Boot a stack (from a fresh clone)

```sh
make up INSTANCE=diorama-venue    # the venue mirror (engine, anvil-v, venue-sim)
make up INSTANCE=diorama-mainnet  # the Ethereum mirror (reth, lighthouse, anvil-t, engine)
make up INSTANCE=diorama-cctp     # the bridge (its relay key goes in secrets.env first)
```

`make up` = wiring (`config-bootstrap`) → images (`build`: the repo's
binaries are built fresh, the pinned third-party set imported) → secrets →
chain state (`snapshot`: the venue re-forks from its source; the mainnet's
reth downloads its snapshot — the one step measured in hours, resumable) →
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
DNS and chain ids derived from the instance file, under the exact key names the
consumer harness sources. The loopback ports live in `harness.env` (see
"Deployment data"); a port collision on another machine is a wiring edit, not
a manifest one.

## Deployment data (the env files that are never committed)

`make config-bootstrap` creates each gitignored file from its tracked
`*.example` template whenever it is missing; `make apply` runs it first — a
fresh clone cannot apply a half-configured stack.

| file | what it carries |
| --- | --- |
| `k8s/overlays/<instance>/wiring.env` | the cluster wiring every pod resolves: fork source, market plane, service DNS, state roots, the claim class |
| `k8s/overlays/<instance>/secrets.env` | the keyed values (a provider URL with its key, the relay wallet): written into the instance's Secret by `make secrets`, never into a ConfigMap |
| `k8s/overlays/<instance>/harness.env` | the host harness surface: the loopback port-forwards |
| `config/instances/<instance>/specialization.toml` | the TOML mounted at `/etc/mirror`: endpoint capability claims, watch scope, fixtures, markets, the fork pin |

Secrets have exactly two homes: `secrets.env` (gitignored, → the instance
Secret) and generated ones (`make secrets` also creates the reth↔lighthouse
JWT). Never put a key in a tracked manifest, a tracked config, or `wiring.env`
(a ConfigMap is readable by anyone with `get cm`).

## The config layering

`config/defaults.toml` → `config/instances/<instance>/instance.toml` →
`specialization.toml`, with `${VAR}` references expanded from the pod's env
(the `wiring.env` ConfigMap; a key also present in the instance Secret
resolves from the Secret). `defaults.toml` carries the workspace conventions
every deployment shares — the service ports, the journal's home under the
state root, the engine's `@anvil` target, the endpoint-pool policy and
cadences — so `instance.toml` states only what makes one deployment
different (identity, chain wiring, state roots, the measured deltas; a fresh
instance's file is a dozen lines) and `specialization.toml` carries the
product's scope (endpoint groups, watchlists, markets, the fork pin).
Validate the result without a cluster:

```sh
make config-check
```

`scripts/config-lint.sh` also gates the porting surface: the audit
(`scripts/cluster-config-audit.txt`) records every key the live cluster's
ConfigMaps carried, classed live-read (the build must carry it) or live-dead
(the build must not grow it back), the two mirrors' instance files stay
TOML-shape symmetric, and every overlay builds from the tracked templates
alone.

## Porting to another host

```sh
git clone <this repo> && cd Diorama
make up INSTANCE=diorama-venue    # ends with the few values to review
```

The boot prints the env files it created; the values a porting operator
reviews are the wiring's machine choices (each documented in its template):

- `EVM_FORK_URL` / `HL_INFO_API_URL` — the venue's source plane (keyed
  providers go in `secrets.env`);
- `CHECKPOINT_SYNC_URL` / `SNAPSHOT_URL` — the mainnet's chain-state sources
  (`SNAPSHOT_URL` also accepts `file:///…` staged on the node);
- `T_RPC_URL` / `TARGET_RPC_URL` — the bridge's chains (in-cluster DNS;
  override for providers outside the cluster);
- `STORAGE_CLASS` — the claim class (`make up` creates the default; a cluster
  with its own provisioner names it here; change only on an empty cluster —
  a bound claim's class does not rebind);
- `harness.env` — the loopback forwards.

What a clone cannot carry, and where the Makefile owns it instead: images
(`make build` — one binary per manifest tag), chain state (`make snapshot` —
the reth download; the venue needs none), keys (`make secrets` — the JWT is
generated, the relay key is `secrets.env`).

## Transition notes (the current host)

The tracked tree is now portable; the live cluster carries three artifacts of
its pre-portability history. The next applies reconcile them, in this order:

1. **The keyed values move to Secrets.** `make secrets INSTANCE=diorama-venue`
   writes the keyed fork source (the URL was in `wiring.env`, readable via
   `get cm`) into `diorama-venue-secrets`; for the bridge,
   `make secrets INSTANCE=diorama-cctp` carries the relay wallet over from the
   legacy `cctp-relay-key` Secret once. The next
   `make apply INSTANCE=diorama-venue` / `INSTANCE=diorama-cctp` rolls those
   pods (anvil-v, engine, venue-sim, cctp-bridge); the resolved values are
   identical, so the follow resumes from its persisted cursor.
2. **The harness env is derived, not a ConfigMap literal.** Source
   `make endpoints INSTANCE=…` (the old `test-endpoints` block's keys, now
   rendered from the instance file) and open the forwards with
   `make port-forward INSTANCE=…` (the ports live in `harness.env`). The
   retired `*-nodeport` Service twins are not removed by apply — delete them
   once: `kubectl -n <ns> delete svc --ignore-not-found anvil-t-nodeport
   engine-nodeport` (and the venue's `anvil-v-nodeport engine-nodeport
   venue-sim-nodeport`, the bridge's `cctp-bridge-nodeport`).
3. **The mainnet's two pre-bound claims.** Their PV `volumeName` left the
   tracked manifests (a fresh host must bind its own volumes), and a
   client-side apply refuses to patch the immutable field on the live claims.
   Reconcile once with
   `kubectl apply -k k8s/overlays/diorama-mainnet --server-side` (verified by
   server dry-run: every object `serverside-applied`, nothing restarts — the
   volumes' field keeps its `k3s` manager).

## Development gates

```sh
make ci          # cargo test --workspace, then the structural gates
make lint        # clippy, all targets, warnings are errors
make ast-scope   # fn/struct size and nesting analysis
```

`scripts/config-lint.sh` (part of `make fitness`) keeps the portability story
honest: the wiring answers every `${VAR}` the instances reference, the two
mirrors stay structurally symmetric, no machine-specific literal survives in a
tracked manifest, and no deployment file is tracked by mistake.

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
