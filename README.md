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
tracked is a template that boots unchanged on another machine, with two
cluster facts named in one file each (the StorageClass and the image
registry — see "Porting to another host"). The three instances are
structurally identical — the same Makefile surface, the same overlay shape —
and a second deployment on another host is a clone plus one edited
specialization; **moving this stack**, data and all, is the runbook under
"Moving the stack to another cluster".

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

The services are ClusterIP by design (one exception: the venue info listener,
NodePort 31090, below): the anvil cheatcode set and the `/admin`+`/control` APIs
must never bind a host interface (a NodePort exposes them to the LAN). The host
uses loopback port-forwards, and the harness's env is derived, never
hand-written:

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

### Place orders in the simulated venue (testers)

The venue simulator is the trading surface. The `venue-sim-nodeport` Service
publishes the **info listener** (`/info`, `/exchange`, `/ws`) on node port
**31090**, so a tester does not need a port-forward. From another machine the
same API is published on the tailnet Funnel:

```sh
# this host (NodePort)
export DALOC_VENUE_INFO_URL=http://<node-ip>:31090
# another machine on the tailnet
export DALOC_VENUE_INFO_URL=https://venue.<tailnet>.ts.net
```

Orders are signed Hyperliquid actions posted to `$DALOC_VENUE_INFO_URL/exchange`
(the account is the margin agent's EVM address; a devnet envelope may carry
`user` explicitly). Spot is served symmetrically: `spotMeta` /
`spotMetaAndAssetCtxs` / `l2Book` for a pair name, `allMids` carries `@N` and
`PURR/USDC`, and a spot order's asset id is `10000 + pair.index` (for example
`PURR/USDC` = `10000`, `@142` = `10142`). Spot fills settle tokens in
`spotClearinghouseState` — no funding, no leverage.

The in-cluster name (`http://venue-sim.diorama-venue.svc.cluster.local:8090`)
and a loopback forward (`kubectl -n diorama-venue port-forward svc/venue-sim
18100:8090`) still work. There is no order route on the consumer API by
design — DALOC reads the venue's account and book state; it does not proxy
trades.

```sh
# the market list (the asset index `a` in an order comes from here)
curl -s -X POST localhost:18100/info -d '{"type":"meta"}'

# the account the venue credits: the margin agent's EVM address, spot USDC
curl -s -X POST localhost:18100/info \
  -d '{"type":"spotClearinghouseState","user":"0x<margin-agent>"}'

# a market order (IOC) in the venue's own exchange envelope
curl -s -X POST localhost:18100/exchange -d '{
  "action":{"type":"order","orders":[
    {"a":<asset-index>,"b":true,"p":"<limit-price>","s":"<size>","r":false,
     "t":{"limit":{"tif":"Ioc"}}}]},
  "user":"0x<margin-agent>","nonce":<unix-ms>
}'
```

- **The acting account is `user`** (or `vaultAddress`); the order book, fills,
  fees and funding all land on that account.
- **Signatures are accepted, never verified** on the devnet — the simulator has
  no key material (the anvil-impersonation analogue). A signature field is
  optional; `nonce` is milliseconds and is consumed per account, so a retry
  needs a fresh nonce.
- **`clearinghouseState` is the perp view**; the USDC a unified book deposited
  is spot — read `spotClearinghouseState` for it.
- **Every HIP-3 dex is orderable.** An order's `a` resolves in the venue's own
  namespace: `a < 10000` is the default dex's universe index and
  `a >= 100000` is `100000 + perpDexIndex * 10000 + indexInThatDexUniverse`,
  with `perpDexIndex` the position in the upstream `perpDexs` list (`[0]` is
  `null`, so `xyz` is `1` → `110000` is `xyz` universe index 0). `meta?dex=xyz`
  returns that dex's universe; the account plane validates and margins every
  configured `dex:coin` by name. Per-dex balances, per-dex fees/OI caps and a
  multi-dex `sendAsset` are still unmodeled.
- The simulator models `order`, `cancel`, `cancelByCloid`, `modify`,
  `batchModify`, `updateLeverage`, `updateIsolatedMargin`, `scheduleCancel`
  and `noop`. An unmodeled tag is refused by name in the venue's own error
  envelope (HTTP 200, the shape their clients parse); TP/SL orders must be
  reduce-only.

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
mirrors stay structurally and TOML-shape symmetric, no machine literal
survives in a tracked manifest, the audit
(`scripts/cluster-config-audit.txt`) records every key the live pods resolve
— live-read keys must exist in the merged config, live-dead keys must not
grow back — and the two facts a move changes are single-valued: every rendered
claim names the one StorageClass object the build ships (no second literal, no
class the tree does not create) and every repo image is mapped in the one
registry component.

## Porting to another host (a new deployment)

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
- `chains.venue.fork_url` — the venue's fork source (keyless default; a keyed
  archive goes here, never into a cluster Secret);
- `chains.l1.fork_pin` — **the source host's measurement, not a default.** The
  mainnet template pins the epoch it measured (`26437369`); a copied pin is only
  valid if it is still inside reth's state window (`reth.toml` state history,
  10 064 blocks). Past that, every unwritten read fails with `pruned history
  unavailable` and chain 1 freezes while the block head still advances. A new
  host resolves its own with `make mainnet-repin` (a new epoch), or omits the
  line to resolve a fresh pin at start-up. `chains.venue.fork_pin` is omitted
  from the template for the mirror-image reason: the keyless public fork source
  serves `latest` only, and a pin the fresh node's state does not agree with
  trips the engine's epoch-rollback guard.
  **Measured 2026-11-19:** the pin was 12 045 blocks old — 1 981 past the
  10 064-block window. The anvil fell back to the public archive in
  `endpoints.evm_archive`, whose free tier returned HTTP 429 on every uncached
  slot; the applier stalled, the mirror head timestamp fell five hours behind
  wall time, and the feed bridge/heartbeat gates refused chain-1 prices. The
  engine's `POST /admin/time` offset restored a current clock (and made the
  feed repair stick), but that is a compensating control: a pin outside the
  state window still needs a re-pin plus the full protocol redeploy, not a
  public archive on the read path;
- `endpoints.*` and `services.engine.state_dir` / `source_sync.jwt_path` —
  the mainnet's in-cluster wiring;
- the bridge's `endpoints.*`, `state_dir` and `relay_key`.

Two cluster facts are deliberately NOT in the specialization — they are
Kubernetes objects, and both are create-time facts on an empty cluster:

- **the storage class** — `k8s/base/shared/storageclass.yaml`'s
  `metadata.name` + `provisioner`. Every claim of every instance takes that
  name through `k8s/components/storage`, so renaming the class in that one file
  re-points all eight claims (no claim carries a second literal; the rendered
  build names one class object and nothing else). The default is
  `mirror-retain` on `rancher.io/local-path` (k3s' node-local provisioner). A
  cluster with its own storage edits the two lines: `ebs.csi.aws.com`,
  `local.csi.azure.com`, `standard` (kind), or Rancher's local-path chart on
  any distro (which keeps the default). A bound claim's class never rebinds and
  a StorageClass's provisioner cannot be changed, so this is an empty-cluster
  edit — on a host that already has the class, rename it (both fields).
- **the repo images' registry path** — `k8s/components/registry`, four
  `newName`s, one per image this repo builds (the build *tag* stays in the
  manifest beside the image; the component carries no version). The default
  `localhost/…` is k3s' node-store prefix: the images `make build` imports, no
  registry contacted. A registry host edits the four names and runs
  `make registry-push`, which tags and pushes exactly those refs.

The lighthouse checkpoint-sync URL is the one remaining third-party literal in
a manifest (a provider choice, not a cluster fact; the known-good alternative
is in `OPERATIONS`, "sync stuck"). The Makefile's machine plumbing
(`KUBECTL`, `CTR`, `SUDO`, `RUNTIME`, `K3S_STORAGE`, all documented at the top
of the Makefile) overrides the k3s defaults. What a clone cannot carry, and
where the Makefile owns it instead: images (`make build`), chain state
(`make snapshot`), keys (`make secrets` — the JWT is generated; the relay
wallet is the specialization's).

## Moving the stack to another cluster

This is the audited runbook (measured 2026-11-17: three instances, eight
claims). The source host is a single-node k3s; the target may be anything. Two
facts decide everything: which volume content is irreproducible, and which
cluster facts are create-time only.

### The eight claims: re-derive or copy

`request` is the manifest's claim size; `data` is what the live volume holds.
The whole **must-copy set is ~300 MiB** — the two big claims are public chain
data that any host can re-download.

| claim | request | data | written by | its content | move = |
| --- | --- | --- | --- | --- | --- |
| `diorama-mainnet/reth-data` | 400Gi | 293Gi | `reth` Deployment, `reth-snapshot` Job | public mainnet execution data, pruned to the mirror's window (`reth.toml`: 10k state, 20k bodies/receipts) | **re-derive**: `make snapshot INSTANCE=diorama-mainnet` — 165 GB compressed, `.part`-resumable, measured in hours |
| `diorama-mainnet/lighthouse-data` | 1500Gi | 329Gi | `lighthouse` Deployment, `lighthouse-prune-blobs` Job | public consensus data: beacon DB, the 18-day column window, retained payloads | **re-derive**: checkpoint sync reaches head in minutes, the backfill takes hours and peaks ~900 Gi before the first prune. Copy 329Gi only to skip the backfill |
| `diorama-mainnet/anvil-t-state` | 40Gi | 663Mi | `anvil-t` (init resolves the pin, anvil dumps `--state` every 300 s) | `/state/pin`, `anvil-state.json` (27 Mi), `.foundry` fork cache (69 Mi), five pre-epoch backups | **copy** (~100 Mi w/o backups): the pin-epoch's deployed contracts + T's writes. Re-derive = a fresh pin + the full protocol re-deploy (proven path, today's bootstrap) |
| `diorama-mainnet/engine-state` | 10Gi | 7.5Mi | mainnet engine | `cursor.json`, `journal/` (rewind), `shadow.json` (the COW record of T's writes), `drift.jsonl` | **copy** (7.5 Mi): exactness for ~0 cost. Re-derive = re-follow from the pin + re-scan T's own blocks into the shadow registry (the rewind window is lost) |
| `diorama-venue/anvil-v-state` | 20Gi | 1.4Gi | `anvil-v` | `/state/pin` (51273700), `anvil-state.json` (257 Mi), fork cache, seven pre-epoch backups | **copy** (~260 Mi w/o backups): V's epoch state. Re-derive = a fresh pin; the venue's writes are gone |
| `diorama-venue/engine-state` | 10Gi | 5.8Mi | venue engine | `cursor.json`, `shadow.json` (5.8 Mi — the venue's write record), `drift.jsonl`; the journal is empty | **copy** (5.8 Mi): without it a same-pin re-fork cannot restore the venue's writes (ADR 0003) and the write-backs are only recoverable from a re-scan |
| `diorama-venue/venue-sim-state` | 5Gi | 120Ki | `venue-sim` | `sim.json` (the market/account scenario), `feed.json`, dated backups | **copy** (120Ki). Re-derive = `/control/reset` blanks the account plane and the sim re-seeds from its config |
| `diorama-cctp/cctp-state` | 5Gi | 760Ki | `cctp-bridge` | `cctp.json` (watcher cursors + in-flight ledger), `retired/` | **copy** (760Ki). Re-derive = the watcher re-scans both chains; a burn in flight at the cut needs re-attesting |

Total claimed: 1990 GiB. Total live data: ~624 GiB. `k3s`' local-path
provisioner does not enforce claim sizes (the live lighthouse PV is 500Gi under
a 1500Gi request, and the resize never happened) — on a CSI provisioner they
are real reservations, so lower the two mirror claims to the disk you actually
plan (400Gi + 400Gi is honest for a copy-only move; 1500Gi is the CL backfill
peak).

Outside this tree, the protocol deployment's `*-deployed-env` volume is
**re-seeded, not copied**: its deployment tree's `scripts/post-deploy.sh`
rebuilds it and rolls its consumers. Its *input* is not the chain: it merges the
protocol checkout's `contracts/deployments/*.env`, which are host-local and
gitignored exactly like the specializations — carry those five `.env` files
(and their `.json`) with the copy set, or re-run the contract deploy (the
records are its output). The protocol deployment's own postgres volume is that
platform's state — copy it with the same procedure or re-index; it is not this
environment's.

### Ordered steps

0. **Preflight the target.** The pod requests sum to 18.9 GiB (~0.55 CPU),
   so ≥32 GiB allocatable covers the steady state — but the measured peaks are
   higher (reth's 24 GiB limit, the venue anvil's 40 GiB fork-materialization
   ceiling; the source host has 94 GiB), so a 64 GiB node keeps today's
   headroom. ≥2 TB of volume space for the requested claims (or the lowered
   sizes above), ports 9000/tcp and 9000/udp free (lighthouse's P2P
   `hostPort` — a busy port keeps the pod from starting; the CL dials out, so
   dropping the `hostPort` lines is a legitimate one-line edit on a busy host),
   and the deploy host needs `kubectl`, `make`, `bash`, `jq`, `openssl`, a
   container runtime and the Rust toolchain (`make endpoints`/`config-check`
   run `cargo run`). Permissions: there is no RBAC object in this tree (no
   ServiceAccount, Role, ClusterRole or binding — every workload runs as the
   namespace default), so a namespace-scoped installer needs only the three
   namespaces, the objects in them, and permission to create the one
   cluster-scoped StorageClass. On a cluster that forbids creating a class,
   point `k8s/components/storage` at the operator's existing class (it is one
   value in one file) instead of shipping one.
1. **Read the source's deployment data.** The three
   `config/instances/*/specialization.toml` and the three `harness.env` are
   gitignored: copy them by hand (they carry the keyed endpoints, the relay
   key, the fork sources). Verify the pins now — the anvil state and the
   specialization must agree, or the copied dump is loaded onto a different
   base: `kubectl -n <ns> exec deploy/anvil-t -c anvil -- cat /state/pin`
   (measured: T 26437369, V 51273700, equal to both files' `fork_pin`). If the
   target renames a namespace or uses a non-`cluster.local` DNS domain, the
   edits are in the specializations (the bridge reaches T as
   `http://anvil-t.diorama-mainnet.svc.cluster.local:8545`) and in the
   harness's loopback ports — never in the manifests.
2. **Gate the target's tree before touching anything:**
   `make config-check` (all three `ok`, empty environment), `make config-lint`
   (builds every overlay; the single class and single registry point, the
   namespace transform, the audit), and the rendered proof:
   ```sh
   for i in diorama-mainnet diorama-venue diorama-cctp; do
     kubectl kustomize k8s/overlays/$i > /tmp/$i.yaml
     printf '%s: %s class, %s claims on it, %s k3s literals\n' "$i" \
       "$(grep -c '^kind: StorageClass' /tmp/$i.yaml)" \
       "$(grep -c '^  storageClassName: mirror-retain$' /tmp/$i.yaml)" \
       "$(grep -vE '^provisioner:' /tmp/$i.yaml | grep -cE 'local-path|k3s|/var/lib/rancher')"
   done   # expect: 1 class, every claim on it, 0 k3s literals
   ```
   then the server dry-run — `kubectl apply -k k8s/overlays/<i> --server-side
   --dry-run=server` — which on an empty target reports nothing Invalid (a
   fresh host binds its own volumes; the two pre-bound claims only exist on the
   source — see "the source host's one-time reconciles").
3. **Quiesce the source.** `kubectl -n <ns> scale deploy --all --replicas=0`
   for all three instances, then wait for the pods to be gone. The anvil
   dumps its state on SIGTERM (the dump is what you copy); the engine persists
   per pass; the bridge's ledger is written per message. Copying a live
   datadir is how you get a half-written `anvil-state.json`.
4. **Stage the copy set.** The source node's directories are
   `/var/lib/rancher/k3s/storage/<pvc-uid>_<namespace>_<claim>/`. The trailing
   pair is frozen at creation and can predate a rename — the live reth and
   lighthouse directories still say `mainnet-mirror` — so read the mapping from
   the cluster, never from the name: `kubectl get pv -o
   custom-columns=PATH:.spec.hostPath.path` (or join it with
   `kubectl get pvc -o jsonpath` per claim). Example:
   ```sh
   sudo rsync -aHAX --numeric-ids --info=progress2 \
     /var/lib/rancher/k3s/storage/pvc-<uid>_diorama-mainnet_anvil-t-state/ \
     /srv/move/mainnet/anvil-t-state/
   ```
   Copy the live files only (`pin`, `anvil-state.json`, `.foundry/`), not the
   pre-epoch backups. Ownership: the anvil's files are uid 1000 (`foundry`),
   the engines' and the PVC root are root — `--numeric-ids` keeps them, the
   PVC root is 0777 (k3s' local-path mode) so the unprivileged processes can
   rewrite their state.
5. **Bind the target's claims** (the namespace and the claims only — no
   workload starts):
   ```sh
   make bind-claims INSTANCE=diorama-mainnet
   ```
   `WaitForFirstConsumer` only binds a claim when a pod consumes it, so the
   target creates them plus one `claim-binder` pod that mounts every claim;
   the target prints each claim's PV path. The pod stays so a pod-mediated
   copy has a writer; `make unbind-claims INSTANCE=…` releases the claims.
6. **Copy in.** Node-local provisioner: `rsync -aHAX --numeric-ids --delete
   /srv/move/mainnet/anvil-t-state/ <target path>/` (delete is safe — the
   target volume is empty by construction). Any provisioner, no node access:
   stream through the binder pod, `tar -C <dir> -cf - . | kubectl -n <ns> exec
   -i claim-binder -- tar -C /mnt/<claim> -xf -` (rsync over `kubectl
   exec` works the same way). Do **not** copy `reth-data`/`lighthouse-data`
   this way unless the link beats the snapshot source: 622 GiB.
7. **Start each instance.** `make unbind-claims INSTANCE=…` releases the
   claims, then: for a stack whose chain state was copied,
   `SKIP_SNAPSHOT=1 make up INSTANCE=diorama-mainnet` — plain `make up` would
   find reth down and re-download 165 GB over the datadir just copied;
   `SKIP_SNAPSHOT=1` is exactly `build secrets apply`. Re-derived state needs
   nothing special (`make up` sources it).
8. **Gate the move.** Per instance: `make status` (every pod Ready, every
   claim Bound to a *new* PV), `make pin-show` (the pins are the ones copied),
   `make logs-check` (no new WARN/ERROR classes), `make smoke INSTANCE=…` (the
   venue and the mainnet suites), `make mirror-freshness` (all four planes lag
   in seconds), `make substrate-attest` (read-only invariants: cursors, pins,
   contract code, bridge, DB, host). The contract-level proof is the protocol
   side's `preflight-all.sh` (both chains' catalog + signer bindings).
9. **The protocol deployment, in this order** (nothing the other way: its env
   points *into* these services and no manifest here points at it — the only
   tie is the endpoint values in its env files):
   1. the three instances are up **with their pins set** (a re-fork at a live
      head is a new epoch and clears the shadow registry — never deploy onto an
      unpinned anvil);
   2. deploy the contracts onto the pinned chains (the protocol checkout's
      `just deploy-mirror-evm 1` / `just deploy-mirror-hyperevm`);
   3. seed the registry: its `k8s/scripts/post-deploy.sh` writes
      `chain-registry.env` + the per-chain records into its deployed-env PVC
      and rolls the consumers;
   4. project it into the env manifest:
      its `k8s/scripts/sync-deployed-env.sh --write`;
   5. its `k8s/apply.sh` (or `kubectl apply -f deploy`), then its
      `preflight-all.sh`.

   If the anvil state was copied, the contracts are already deployed at the
   pin, so 9.2 is a re-deploy over the existing timelock (or nothing) and 9.3
   re-seeds the PVC from the carried `contracts/deployments/*.env` records —
   either way the PVC registry is the fresh host's to rebuild, never a volume
   to copy.

### The source host's one-time reconciles (not needed on the target)

A fresh host creates its claims and binds its own volumes, so none of this
applies there. It is the *source* host's cleanup if it keeps running:

1. **The mainnet's two pre-bound claims** (`reth-data`, `lighthouse-data`).
   Their `volumeName` left the tracked manifests long ago; the live claims were
   created when it was still there, so a server-side apply tries to unset a
   field the old manager owns and refuses an immutable change. Measured with
   `kubectl apply -k k8s/overlays/diorama-mainnet --server-side
   --dry-run=server`: the only two Invalid objects are those claims; the other
   twelve apply, and the venue's three claims (never pre-bound) apply clean. A
   host that has them reconciles once by rewriting the claims' stored
   definitions (delete the claim objects — the PVs are `Retain` — and re-apply;
   WaitForFirstConsumer rebinds the retained volumes).
2. **The bridge's claim class** (`diorama-cctp/cctp-state`). It was created
   when the tree's class was still named `diorama-retain`; the mirrors were
   renamed to `mirror-retain` (162bd86) and the bridge's claim was left on the
   old name, which no tracked object creates any more — so a fresh host would
   have left that one claim Pending forever. The tree now ships the one class
   and every claim names it (measured: server dry-run's only new Invalid object
   on the source host is `cctp-state`, `StorageClassName: diorama-retain ->
   mirror-retain`). On the source host a bound claim's class is immutable:
   scale the bridge to 0, delete the claim (760Ki of cursors; the volume stays
   `Retain`), re-apply — or leave it bound and accept one Invalid object on the
   next `make apply INSTANCE=diorama-cctp`.
3. **NodePorts.** `venue-sim-nodeport` (31090) is the one intentional NodePort:
   the trading surface has to be reachable without a port-forward. The rest of
   the venue services are ClusterIP by design (the harness uses loopback
   forwards). The old generic `-nodeport` twins (`anvil-t-nodeport`,
   `engine-nodeport`, …) are not in the manifests; they may still exist on an
   older host as ClusterIP objects and are not carried by a move — delete them
   once or let `make delete` take the namespace.

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
manifest, the live audit gates the merged shape, and the move's two cluster
facts stay single-valued (one StorageClass object fanned out to every claim,
one registry component for the four repo images).

**The gates run on a fresh clone.** `make config-check` and `make config-lint`
both fall back to the tracked `*.example` templates for the files a host owns
(`specialization.toml`, `harness.env`) and warn when they do; a checkout that
has them validates the live files instead. That is what makes `make fitness`
usable in CI, where the gitignored deployment data does not exist — and it is
the property to keep: a gate that needs a file the repository deliberately does
not ship is a gate nobody else can run.

### The two-repo gate pair

Diorama owns the mirrors; the DALOC monorepo owns the product's config. Each
repository answers the same two questions with its own command, and a host runs
both before it trusts a deployment:

| Question | Diorama | DALOC monorepo |
|---|---|---|
| Is the tracked config self-consistent? | `make config-check`, `make config-lint` | `just config-check-self` |
| Has *this* host replaced the host facts? | the template fallback warns; `make up` refuses to run on it | `just config-preflight` (the `DALOC_PROFILE_ORIGIN` sentinel) |

Diorama's host facts are in-cluster DNS names its own manifests create, so a
copied template is already correct on any cluster running those manifests.
DALOC's host facts are endpoints, ports and keys outside both repositories,
which is why it carries an explicit sentinel for "edited for this host".

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
