SHELL := /bin/bash

# INSTANCE selects one deployable unit: its namespace, kustomize overlay,
# instance TOML, images and wiring all follow from it. The three units are
# structurally identical: the two mirrors (diorama-mainnet, diorama-venue) and
# the standalone bridge (diorama-cctp).
#
#   make up INSTANCE=diorama-venue
#
# Every target below that acts on a cluster takes INSTANCE, so a second
# deployment on the same host is `make up INSTANCE=<name>` with its own
# directory (see "Horizontal duplication" in the README).
INSTANCES    := diorama-mainnet diorama-venue diorama-cctp
INSTANCE     ?= diorama-mainnet
NS           := $(INSTANCE)
OVERLAY      := k8s/overlays/$(INSTANCE)
BOOTSTRAP    := k8s/bootstrap

# Machine plumbing, overridable on another box (a fresh clone boots with the
# defaults; only a differently-installed cluster needs these).
#   KUBECTL     the cluster client (pass --context/--kubeconfig through it)
#   RUNTIME     the local container runtime (podman/docker) that builds+saves
#               the images `$(CTR)` imports
#   CTR         the cluster's image importer (k3s' containerd here; a
#               non-k3s cluster overrides this - e.g. `docker` needs no import
#               and `make import` can be a no-op)
#   SUDO        how privileged commands are issued (empty for root)
#   K3S_STORAGE the node's PVC backing dir (the prune-size probe)
KUBECTL      ?= kubectl
SUDO         ?= sudo
RUNTIME      ?= podman
CTR          ?= $(SUDO) k3s ctr
K3S_STORAGE  ?= /var/lib/rancher/k3s/storage
KUBECTL_NS   := $(KUBECTL) -n $(NS)
IMG_TAR      := target/diorama-img.tar
PATH         := $(HOME)/.cargo/bin:$(PATH)
export PATH

# The images a manifest of the selected instance pins (`localhost/…` are this
# repo's builds, one binary per image, `--build-arg BIN=<binary>`).
INSTANCE_MANIFESTS := k8s/base/$(INSTANCE) $(OVERLAY)
ENGINE_MANIFEST    := k8s/base/$(INSTANCE)/engine.yaml
ENGINE_REF         := $(shell grep -ohE 'image: localhost/[^ ]+' $(ENGINE_MANIFEST) 2>/dev/null | head -1 | awk '{print $$2}')
ENGINE_IMAGE       := $(word 1,$(subst :, ,$(ENGINE_REF)))
ENGINE_TAG         := $(lastword $(subst :, ,$(ENGINE_REF)))
ENGINE_BIN         := $(notdir $(ENGINE_IMAGE))
INSTANCE_IMAGES    := $(shell grep -rhoE 'image: [^ ]+' $(INSTANCE_MANIFESTS) 2>/dev/null | awk '{print $$2}' | sort -u)
LOCAL_IMAGES       := $(filter localhost/%,$(INSTANCE_IMAGES))
PINNED_IMAGES      := $(filter-out localhost/%,$(INSTANCE_IMAGES))

# The selected instance's Namespace manifest: the overlay owns it (instance
# identity, not a service template) - one rule for every unit.
NS_MANIFEST := $(OVERLAY)/namespace.yaml

.DEFAULT_GOAL := help

# The deploy chain (spec present -> images -> secrets -> snapshot -> apply) is
# order-sensitive; never let a parallel make interleave it.
.NOTPARALLEL:

# The one deployment input: the gitignored specialization. `make up` does not
# create it (its values are this deployment's facts, not boilerplate); a
# missing file is named here - with the remedy - before the kustomize build
# refuses it loudly.
spec-present:
	@if [ ! -f config/instances/$(INSTANCE)/specialization.toml ]; then \
	  echo "config/instances/$(INSTANCE)/specialization.toml is missing." >&2; \
	  echo "  copy the tracked template:" >&2; \
	  echo "    cp config/instances/$(INSTANCE)/specialization.example.toml config/instances/$(INSTANCE)/specialization.toml" >&2; \
	  echo "  then hardcode this deployment's values (endpoints, fork facts, state roots)" >&2; \
	  echo "  - the file must carry no \$${VAR} references; it is the only config source." >&2; \
	  exit 1; \
	fi

# The host harness surface: the gitignored copy of harness.example.env. It is
# not configuration (no pod reads it; the operator's loopback forwards do), so
# a fresh clone bootstraps it silently.
harness-env:
	@for f in k8s/overlays/*/harness.example.env; do \
	  d=$${f%/*}; \
	  if [ ! -f "$$d/harness.env" ]; then cp "$$f" "$$d/harness.env"; echo "created $$d/harness.env from template"; fi; \
	done

##@ Deploy (any instance)

# The one command from a fresh clone: specialization (the operator's one
# file) -> images (built + imported) -> secrets (the reth JWT) -> snapshot (a
# mirror needs chain state; the first one takes hours) -> apply. `build` runs
# `deps` itself, so the pinned third-party set is imported without a second
# pull.
up: spec-present build secrets snapshot-$(INSTANCE) apply ## Build + import the images, create the reth JWT, source chain state, apply
	@echo "== $(INSTANCE) applied"
	@echo "   watch:      make status INSTANCE=$(INSTANCE)"
	@echo "   host reach: make port-forward INSTANCE=$(INSTANCE)"
	@echo "   endpoints:  make endpoints INSTANCE=$(INSTANCE)"

apply: spec-present secrets ## Apply the selected instance's stack
	$(KUBECTL) apply -k $(OVERLAY)
	@$(MAKE) --no-print-directory endpoints INSTANCE=$(INSTANCE)

diff: spec-present ## Show what `make apply` would change
	$(KUBECTL) diff -k $(OVERLAY) || true

delete: ## Delete the namespace (data survives: the StorageClass is Retain)
	$(KUBECTL) delete namespace $(NS) --ignore-not-found

##@ Images (a machine cannot clone these: they are built here)

# Every image is pinned by full version tag (see k8s/base/**/*.yaml). Pre-importing
# them into the k3s containerd store keeps pod start-up independent of registry
# DNS - measured: containerd's resolver can fail transiently while podman's
# works, which turned a deploy into ImagePullBackOff.
# Single source of truth: the manifests. LOCAL_IMAGES are the images this repo
# builds (one binary per image, `--build-arg BIN`); PINNED_IMAGES are the
# third-party references, pinned by tag *and* digest. Bumping a version means
# editing the manifest once.
deps: ## Pull + import the selected instance's pinned third-party images (no build)
	@for img in $(PINNED_IMAGES); do \
	  printf "== %s\n" "$$img"; \
	  $(RUNTIME) pull -q "$$img" || exit 1; \
	  rm -f "$(IMG_TAR)"; \
	  $(RUNTIME) save -o "$(IMG_TAR)" "$$img" || exit 1; \
	  $(CTR) images import "$(IMG_TAR)" >/dev/null || exit 1; \
	done
	@rm -f "$(IMG_TAR)"
	@echo "third-party images imported: $(PINNED_IMAGES)"

# --no-cache is deliberate: a cached layer once survived a source change and
# the old binary shipped under the pinned tag (measured 2026-11-12 - two
# builds of the same tag printed one digest while crates/anvil differed). A
# deploy image must be reproducible from the tree, never "probably the tree";
# the dependency layer costs ~30 s uncached.
build: ## Build + import every image the selected instance needs (local builds fresh, no layer cache)
	@for ref in $(LOCAL_IMAGES); do \
	  bin=$${ref##*/}; bin=$${bin%%:*}; \
	  if [ ! -f "bins/$$bin/src/main.rs" ]; then \
	    printf "  %-62s manifest only (bins/%s has no source yet)\n" "$$ref" "$$bin"; continue; \
	  fi; \
	  printf "== %s (build)\n" "$$ref"; \
	  $(RUNTIME) build --no-cache -q -f Dockerfile --build-arg BIN=$$bin -t "$$ref" . || exit 1; \
	  rm -f "$(IMG_TAR)"; \
	  $(RUNTIME) save -o "$(IMG_TAR)" "$$ref" || exit 1; \
	  $(CTR) images import "$(IMG_TAR)" >/dev/null || exit 1; \
	done
	@rm -f "$(IMG_TAR)"
	@echo "local images built + imported: $(LOCAL_IMAGES)"
	@$(MAKE) --no-print-directory deps INSTANCE=$(INSTANCE)
	@$(MAKE) --no-print-directory check-pins INSTANCE=$(INSTANCE)

import: build ## Alias of `build` (kept for the documented pipeline: build + import)

check-pins: ## Verify every image reference is pinned by tag+digest and present locally
	@fail=0; \
	for ref in $(INSTANCE_IMAGES); do \
	  case "$$ref" in \
	    localhost/*) \
	      printf "  %-62s local build, tag pinned\n" "$$ref" ;; \
	    *@sha256:*) \
	      if $(CTR) images ls -q 2>/dev/null | grep -qF "$${ref%%@*}@$${ref##*@}"; then \
	        printf "  %-62s pinned + present locally\n" "$${ref%%@*}"; \
	      else \
	        printf "  %-62s PINNED BUT MISSING locally\n" "$${ref%%@*}"; fail=1; \
	      fi ;; \
	    *) \
	      printf "  %-62s NOT PINNED (no digest)\n" "$$ref"; fail=1 ;; \
	  esac; \
	done; \
	if grep -rqE 'image: [^ ]*:latest' k8s/; then echo "  'latest' tag found in k8s/"; fail=1; fi; \
	if [ $$fail -eq 0 ]; then echo "all images pinned by tag+digest and present"; \
	else echo "pin check FAILED"; exit 1; fi

deploy-engine: build ## Rebuild + redeploy the selected instance's engine
	$(KUBECTL_NS) rollout restart deploy/engine
	@# Bound the wait by the Deployment's own progressDeadlineSeconds (the
	@# Kubernetes default is 600s when unset), not a second hardcoded timeout.
	@deadline=$($(KUBECTL) -n $(NS) get deploy/engine -o jsonpath='{.spec.progressDeadlineSeconds}' 2>/dev/null); \
	 $(KUBECTL_NS) rollout status deploy/engine --timeout="$${deadline:-600}s"

##@ Secrets (a machine has no key: created here, never tracked)

# The only k8s-native credentials left: the reth<->lighthouse JWT (a random
# shared secret between two of this repo's own pods, useless outside the
# cluster) and the namespace itself. Every configuration value - keyed
# endpoints, the relay wallet - lives in the gitignored specialization file,
# which is mounted into the pods as a file.
secrets: ## Create the namespace + the reth JWT (idempotent; an existing JWT is never rotated)
	@$(KUBECTL) apply -f $(NS_MANIFEST) >/dev/null
	@if [ "$(INSTANCE)" = "diorama-mainnet" ]; then \
	  if $(KUBECTL) -n $(NS) get secret reth-jwt >/dev/null 2>&1; then \
	    echo "secret reth-jwt already exists in $(NS) (left untouched)"; \
	  else \
	    $(KUBECTL) -n $(NS) create secret generic reth-jwt --from-literal=jwt.hex="$$(openssl rand -hex 32)" >/dev/null; \
	    echo "created secret reth-jwt in $(NS)"; \
	  fi; \
	fi

##@ Chain state (a machine has no state: a fresh chain starts at a pin)

# A mirror's anvil re-forks from its source at the pin and needs no imported
# state (the fork materializes it). The mainnet mirror's reth and lighthouse
# do: they follow the real chain from a snapshot (reth) and a checkpoint sync
# (lighthouse), and that download is the one part of a fresh deploy measured
# in hours. The bridge needs no state at all.
snapshot-diorama-mainnet: ## Download the reth snapshot into the reth PVC (scales reth down first; skipped while reth serves)
	@if [ "$$($(KUBECTL) -n diorama-mainnet get deploy/reth -o jsonpath='{.status.readyReplicas}' 2>/dev/null)" = "1" ]; then \
	  echo "diorama-mainnet: reth is up (state already sourced; a re-download is explicit: make snapshot-force INSTANCE=diorama-mainnet)"; \
	else \
	  $(MAKE) --no-print-directory snapshot-force INSTANCE=diorama-mainnet; \
	fi

snapshot-force: ## Force the snapshot (re)download: scales reth down first
	$(KUBECTL) -n $(NS) apply -f $(NS_MANIFEST) >/dev/null 2>&1 || true
	$(KUBECTL) -n $(NS) scale deploy/reth --replicas=0 2>/dev/null || true
	$(KUBECTL) -n $(NS) delete job reth-snapshot --ignore-not-found
	$(KUBECTL) -n $(NS) apply -f $(BOOTSTRAP)/snapshot-job.yaml
	@# The wait bound is the Job's own spec.activeDeadlineSeconds, not a second
	@# hardcoded number: the manifest already owns how long the download may run.
	@deadline=$(KUBECTL) -n $(NS) get job reth-snapshot -o jsonpath='{.spec.activeDeadlineSeconds}'; \
	 test -n "$$deadline" || { echo "reth-snapshot Job declares no activeDeadlineSeconds" >&2; exit 1; }; \
	 echo "waiting for snapshot to finish (Job deadline $${deadline}s; resumable)..."; \
	 $(KUBECTL) -n $(NS) wait --for=condition=complete job/reth-snapshot --timeout="$${deadline}s" || \
		{ echo "not complete within the Job's own deadline ($${deadline}s) - check: make snapshot-status INSTANCE=$(INSTANCE)"; exit 1; }

snapshot-diorama-venue: ## No import needed: anvil-v re-forks from its source at the pin
	@echo "diorama-venue: no chain state to import (anvil-v forks from EVM_FORK_URL at the pin)"

snapshot-diorama-cctp: ## No import needed: the bridge carries cursors only
	@echo "diorama-cctp: no chain state to import (the bridge scans the live anvils)"

snapshot: ## Source the selected instance's chain state (wrapper; hours on a fresh mainnet)
	@$(MAKE) --no-print-directory snapshot-$(INSTANCE) INSTANCE=$(INSTANCE)

snapshot-status: ## Show the reth snapshot job status + progress (mainnet)
	@$(KUBECTL) -n $(NS) get job reth-snapshot 2>/dev/null || echo "no snapshot job"

reset: ## Delete the selected instance's PVCs so the next apply re-forks fresh (DESTRUCTIVE)
	@read -r -p "delete every PVC of $(NS) (data is lost; volumes are Retained) [y/N] " a; [ "$$a" = y ] || exit 1
	$(KUBECTL_NS) delete pvc --all

##@ Host access (loopback only: the services bind ClusterIP by design)

# Exactly the forwards `harness.env` names, in the background. The anvil
# cheatcode set and /admin/* must never be a NodePort (they bind every host
# interface); a ClusterIP + a loopback forward is the only reachable shape.
port-forward: harness-env ## Open the instance's loopback port-forwards (FORWARD_* in harness.env)
	@set -a; . $(OVERLAY)/harness.env; set +a; \
	pids=""; \
	for pair in $$(env | grep -E '^FORWARD_[A-Z0-9_]+=' | sort); do \
	  var=$${pair%%=*}; spec="$${pair#*=}"; svc=$$(echo "$$var" | sed -E 's/^FORWARD_//; s/_/-/g' | tr 'A-Z' 'a-z'); \
	  echo "  $$svc: $$spec"; \
	  $(KUBECTL_NS) port-forward --address 127.0.0.1 "svc/$$svc" $$spec & pids="$$pids $$!"; \
	done; \
	trap 'kill'$$pids' 2>/dev/null' EXIT INT TERM; wait

endpoints: spec-present harness-env ## Render the consumer harness env for the selected instance (in-cluster DNS)
	@# The block `make port-forward` complements: every chain id and service
	@# name derived from the merged instance config, never repeated in a
	@# manifest. The specialization is self-sustained - every value resolves
	@# from the files alone, with no pod environment at all.
	@cargo run -q -p diorama-config -- export-env config/instances/$(INSTANCE)/instance.toml --section client

##@ Operate (any instance)

status: ## Pods, PVCs, engine status
	@$(KUBECTL_NS) get pods -o wide
	@$(KUBECTL_NS) get pvc
	@echo "--- engine status ---"
	@$(KUBECTL_NS) exec deploy/engine -- curl -sf http://localhost:8080/status | jq . 2>/dev/null || echo "(no engine in $(NS), or not ready)"

logs: ## Tail logs of all pods of the selected instance (follow)
	$(KUBECTL_NS) logs -f -l app.kubernetes.io/part-of=$(NS) --max-log-requests=10 --prefix

logs-%: ## Tail logs of one component, e.g. make logs-engine / logs-reth / logs-anvil-v
	$(KUBECTL_NS) logs -f deploy/$* --tail=100

smoke: ## End-to-end smoke test of the selected mirror (mainnet, venue)
	@case $(INSTANCE) in \
	  diorama-mainnet) ./scripts/smoke.sh --side mainnet ;; \
	  diorama-venue) ./scripts/smoke.sh --side venue ;; \
	  *) echo "$(INSTANCE) has no smoke suite (TESTING.md: one row per reason)" ;; \
	esac

##@ Forensics (read-only)

mirror-freshness: ## One view of the execution, market-data, venue and relay planes
	./scripts/mirror-freshness.sh

substrate-attest: ## Read-only substrate invariants: cursors, pins, contract code, bridge, DB, host
	./scripts/substrate-attest.sh

logs-check: ## Deep log scan: warnings/errors per component (last 30 min, normalized)
	@for c in $$($(KUBECTL_NS) get deploy -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null); do \
	  printf "== %s\n" "$$c"; \
	  $(KUBECTL) -n $(NS) logs deploy/$$c --since=30m 2>/dev/null \
	    | grep -aE "WARN|ERROR|\"level\":\"(WARN|ERROR)\"" \
	    | sed -E 's/\"timestamp\":\"[^\"]*\"/TS/; s/[0-9a-fx]{8,}/HEX/g; s/[0-9]{4,}/N/g' \
	    | sort | uniq -c | sort -rn | head -6; \
	done

# The selected instance's anvils (one per mirror; the bridge has none), read
# lazily: the recipes that need them query the cluster when they run, never at
# parse time (an offline `make help` must not hit the cluster).
ANVILS = $(shell $(KUBECTL_NS) get deploy -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null | grep -- '-v$$\|-t$$')

##@ Drills (a mirror's own levers)

pin-show: ## Show the resolved fork pins of the instance's anvils
	@for c in $(ANVILS); do \
		printf "%s pin: " $$c; $(KUBECTL_NS) exec deploy/$$c -- cat /state/pin 2>/dev/null || echo "(not resolved yet)"; \
	done

repin: ## Re-pin the instance's anvil: wipe the pin, restart (engine auto-rebases; new epoch)
	@anvil=$(ANVILS); anvil=$${anvil%% *}; \
	[ -n "$$anvil" ] || { echo "$(NS) has no anvil deployment" >&2; exit 1; }; \
	cm=$$($(KUBECTL) -n $(NS) get deploy "$$anvil" -o jsonpath='{.spec.template.spec.volumes[?(@.name=="instance")].configMap.name}'); \
	pin=$$($(KUBECTL) -n $(NS) get cm "$$cm" -o go-template='{{index .data "specialization.toml"}}' 2>/dev/null | sed -n 's/^fork_pin[[:space:]]*=[[:space:]]*\([0-9_]*\).*/\1/p' | tr -d '_'); \
	if [ -n "$$pin" ]; then \
	  echo "the mounted specialization still pins fork_pin=$$pin; remove it first (a changed pin is a new epoch and clears the shadow registry)." >&2; \
	  exit 1; \
	fi; \
	echo "re-pinning $$anvil in $(NS)"; \
	$(KUBECTL) -n $(NS) scale deploy/$$anvil --replicas=0; \
	# Wait until the pod is really gone before wiping: the wipe must not
	# race a container that is still shutting down (its init container
	# re-resolves the pin on the next start).
	$(KUBECTL) -n $(NS) wait --for=delete pod -l app.kubernetes.io/name=$$anvil --timeout=180s || true; \
	$(KUBECTL) -n $(NS) delete job repin-wipe --ignore-not-found; \
	$(KUBECTL) -n $(NS) apply -f <(sed "s/claimName: anvil-t-state/claimName: $$anvil-state/" $(BOOTSTRAP)/repin-job.yaml); \
	$(KUBECTL) -n $(NS) wait --for=condition=complete job/repin-wipe --timeout=300s; \
	$(KUBECTL) -n $(NS) delete job repin-wipe --ignore-not-found; \
	$(KUBECTL) -n $(NS) scale deploy/$$anvil --replicas=1; \
	$(KUBECTL) -n $(NS) rollout status deploy/$$anvil --timeout=1200s; \
	echo "re-pinned. engine will rebase automatically on its next poll."

venue-repin: ## Re-pin V to the fork source's latest block (the engine's /admin/epoch)
	$(KUBECTL) -n diorama-venue exec deploy/engine -- curl -sf -m 90 -X POST localhost:8080/admin/epoch

backfill: ## Show the CL backfill progress and the lighthouse datadir sizes (mainnet)
	@$(KUBECTL) -n $(NS) logs deploy/lighthouse --tail=200 2>/dev/null \
	  | jq -r 'select(.msg != null and (.msg|test("historical";"i"))) | "backfill: distance=\(.distance // "?") speed=\(.speed // "?") est=\(.est_time // "?")"' 2>/dev/null | tail -1
	@$(SUDO) sh -c 'du -sh $(K3S_STORAGE)/pvc-*_$(NS)_lighthouse-data/beacon/* 2>/dev/null | sort -h | tail -4'

prune-blobs: ## Reclaim backfilled blob storage (needs the CL stopped; safe for the mirror)
	@echo "scaling lighthouse down (the prune needs the database lock)..."
	$(KUBECTL) -n $(NS) scale deploy/lighthouse --replicas=0
	$(KUBECTL) -n $(NS) wait --for=delete pod -l app.kubernetes.io/name=lighthouse --timeout=180s || true
	$(KUBECTL) -n $(NS) delete job lighthouse-prune-blobs --ignore-not-found
	$(KUBECTL) -n $(NS) apply -f $(BOOTSTRAP)/prune-blobs-job.yaml
	$(KUBECTL) -n $(NS) wait --for=condition=complete job/lighthouse-prune-blobs --timeout=3600s || true
	$(KUBECTL_NS) logs job/lighthouse-prune-blobs --tail=20 || true
	$(KUBECTL) -n $(NS) delete job lighthouse-prune-blobs --ignore-not-found
	$(KUBECTL) -n $(NS) scale deploy/lighthouse --replicas=1
	$(KUBECTL) -n $(NS) rollout status deploy/lighthouse --timeout=300s
	@$(MAKE) --no-print-directory backfill INSTANCE=$(NS)

##@ Develop

test-engine: ## Run workspace unit tests on the host (cargo)
	cargo test --workspace

config-check: ## Validate every instance file through the layered loader (every specialization must be present)
	@for f in config/instances/*/instance.toml; do \
	  d=$${f%/*}; \
	  if [ ! -f "$$d/specialization.toml" ]; then \
	    echo "missing $$d/specialization.toml (copy $$d/specialization.example.toml and hardcode the deployment's values)" >&2; exit 1; \
	  fi; \
	done
	@for f in config/instances/*/instance.toml; do \
	  cargo run -q -p diorama-config -- check "$$f" || exit 1; \
	done

config-lint: ## Structural gates over the tracked tree (wiring keys, templates, image pins)
	@./scripts/config-lint.sh

fitness: lint design-assertions config-check ast-scope config-lint ## Run the structural gates: lint, design assertions, config check, AST scope, config lint

ci: ## Run the exact CI gates locally: workspace tests, then fitness
	cargo test --workspace --locked
	$(MAKE) fitness

lint: ## lint-gate: clippy, all targets, warnings are errors
	cargo clippy --workspace --all-targets -- -D warnings

design-assertions: ## Public types keep Send + Sync (rule 11)
	cargo test --workspace design_assertions

ast-scope: ## AST scope analysis: fn/struct size and nesting depth (syn, the compiler's parser)
	cargo run -q -p diorama-ast-scope -- crates bins

##@ Help

# The instance list, spelled for the usage line.
space := $(subst ,, )
INSTANCE_LIST := $(subst $(space),|,$(strip $(INSTANCES)))

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage: make \033[36m<target>\033[0m INSTANCE={$(INSTANCE_LIST)}\n"} \
		/^[a-zA-Z0-9_-]+:.*?##/ { printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2 } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)
	@echo

.PHONY: up spec-present harness-env apply diff delete deps build import check-pins deploy-engine \
	secrets snapshot snapshot-force snapshot-diorama-mainnet snapshot-diorama-venue snapshot-diorama-cctp \
	snapshot-status reset port-forward endpoints status logs logs-check smoke \
	mirror-freshness substrate-attest pin-show repin venue-repin backfill prune-blobs \
	test-engine config-check config-lint fitness ci lint design-assertions ast-scope help
