SHELL := /bin/bash

# INSTANCE selects the mirror: the namespace, the kustomize overlay, the
# instance TOML and the engine image all follow from it.
INSTANCE       ?= diorama-mainnet
NS             := $(INSTANCE)
OVERLAY        := k8s/overlays/$(INSTANCE)
BOOTSTRAP      := k8s/bootstrap

# Machine plumbing, overridable on another box (a fresh clone boots with the
# defaults; only a differently-installed cluster needs these).
#   KUBECTL     the cluster client
#   RUNTIME     the local container runtime (podman/docker) that builds+saves
#               the images `$(CTR)` imports
#   CTR         the cluster's image importer (k3s' containerd here; a
#               non-k3s cluster overrides this — e.g. `docker` needs no import
#               and `make import` can be a no-op)
#   SUDO        how privileged commands are issued (empty for root)
#   K3S_STORAGE the node's PVC backing dir (the prune-size probe)
KUBECTL        ?= kubectl
SUDO           ?= sudo
RUNTIME        ?= podman
CTR            ?= $(SUDO) k3s ctr
K3S_STORAGE    ?= /var/lib/rancher/k3s/storage
KUBECTL_NS     := $(KUBECTL) -n $(NS)
IMG_TAR        := target/diorama-img.tar

# The engine image/tag of the selected instance, scraped from the manifest
# that deploys it - the deployed manifest stays the single source of truth.
# diorama-mainnet runs the ethereum engine, diorama-venue the venue engine: one
# deployment shape per profile, one image per binary.
ENGINE_MANIFEST := k8s/base/$(INSTANCE)/engine.yaml
ENGINE_REF      := $(shell grep -ohE 'image: localhost/[^ ]+' $(ENGINE_MANIFEST) | head -1 | awk '{print $$2}')
ENGINE_IMAGE    := $(word 1,$(subst :, ,$(ENGINE_REF)))
ENGINE_TAG      := $(lastword $(subst :, ,$(ENGINE_REF)))
ENGINE_BIN      := $(notdir $(ENGINE_IMAGE))

# The anvil components of the selected instance (pin-show).
ANVILS := $(if $(filter diorama-mainnet,$(INSTANCE)),anvil-t,anvil-v)

# The selected instance's Namespace manifest: the overlay owns it (instance
# identity, not a service template) - one rule for both mirrors.
NS_MANIFEST := $(OVERLAY)/namespace.yaml

.DEFAULT_GOAL := help

##@ Cluster

up: ## One command from a fresh clone: make up INSTANCE=diorama-venue (also: diorama-mainnet, diorama-cctp)
	@case $(INSTANCE) in \
	  diorama-mainnet) tgt=mainnet-bootstrap ;; \
	  diorama-cctp) tgt=cctp-bootstrap ;; \
	  *) tgt=venue-bootstrap ;; \
	esac; \
	echo "== make $$tgt (INSTANCE=$(INSTANCE))"; \
	$(MAKE) --no-print-directory $$tgt

ns: ## Create namespace + StorageClass (idempotent)
	$(KUBECTL) apply -f $(NS_MANIFEST)
	$(KUBECTL) apply -f k8s/base/shared/storageclass.yaml

mainnet-secrets: ## Create the reth<->lighthouse JWT secret if missing (mainnet namespace)
	@NS=diorama-mainnet; \
	if $(KUBECTL) -n $$NS get secret reth-jwt >/dev/null 2>&1; then \
	  echo "secret reth-jwt already exists in $$NS (left untouched)"; \
	else \
	  $(KUBECTL) -n $$NS create secret generic reth-jwt --from-literal=jwt.hex="$$(openssl rand -hex 32)" >/dev/null; \
	  echo "created secret reth-jwt in $$NS"; \
	fi

CCTP_NS      ?= diorama-cctp
CCTP_OVERLAY := k8s/overlays/diorama-cctp

venue-secrets: ## Create the venue-side secrets (no CCTP key: the bridge owns its own)
	@NS=diorama-venue; \
	if $(KUBECTL) -n $$NS get secret cctp-relay-key >/dev/null 2>&1; then \
	  echo "legacy cctp-relay-key still exists in $$NS (the standalone bridge uses $(CCTP_NS))"; \
	fi

cctp-secrets: ## Create the bridge's relay-wallet secret (CCTP_RELAY_KEY=0x…; CCTP_NS)
	@NS=$(CCTP_NS); \
	if $(KUBECTL) -n $$NS get secret cctp-relay-key >/dev/null 2>&1; then \
	  echo "secret cctp-relay-key already exists in $$NS (left untouched)"; \
	else \
	  test -n "$$CCTP_RELAY_KEY" || { echo "set CCTP_RELAY_KEY - the dedicated relay wallet hex key" >&2; exit 1; }; \
	  $(KUBECTL) -n $$NS create secret generic cctp-relay-key --from-literal=relay-key="$$CCTP_RELAY_KEY" >/dev/null; \
	  echo "created secret cctp-relay-key in $$NS"; \
	fi

apply: config-bootstrap ## Apply the selected instance's stack (INSTANCE selects the overlay)
	$(KUBECTL) apply -k $(OVERLAY)

diff: ## Show what `make apply` would change
	$(KUBECTL) diff -k $(OVERLAY) || true

delete: ## Delete the namespace (data survives: StorageClass is reclaimPolicy Retain)
	$(KUBECTL) delete namespace $(NS) --ignore-not-found

##@ Bootstrap

mainnet-snapshot: ## Download the reth snapshot into the reth PVC (scales reth down first)
	$(KUBECTL) -n diorama-mainnet scale deploy/reth --replicas=0 2>/dev/null || true
	$(KUBECTL) -n diorama-mainnet delete job reth-snapshot --ignore-not-found
	$(KUBECTL) apply -f $(BOOTSTRAP)/snapshot-job.yaml
	@# The wait bound is the Job's own spec.activeDeadlineSeconds, not a second
	@# hardcoded number: the manifest already owns how long the download may run.
	@deadline=$$(kubectl -n diorama-mainnet get job reth-snapshot -o jsonpath='{.spec.activeDeadlineSeconds}'); \
	 test -n "$$deadline" || { echo "reth-snapshot Job declares no activeDeadlineSeconds" >&2; exit 1; }; \
	 echo "waiting for snapshot to finish (Job deadline $${deadline}s; resumable)..."; \
	 $(KUBECTL) -n diorama-mainnet wait --for=condition=complete job/reth-snapshot --timeout="$${deadline}s" || \
		{ echo "not complete within the Job's own deadline ($${deadline}s) - check: make mainnet-snapshot-status"; exit 1; }

mainnet-snapshot-status: ## Show snapshot job status + progress
	@$(KUBECTL_NS) get job reth-snapshot 2>/dev/null || echo "no snapshot job"
	@$(KUBECTL_NS) logs job/reth-snapshot --tail=3 2>/dev/null || true

mainnet-bootstrap: ns mainnet-secrets config-bootstrap images ## First-time setup (mainnet): namespace, secrets, images, snapshot, full stack
	$(KUBECTL) apply -f k8s/base/diorama-mainnet/pvcs.yaml
	$(MAKE) mainnet-snapshot
	$(MAKE) apply
	@echo "bootstrap complete. watch: make status"

##@ Images

# Every image is pinned by full version tag (see k8s/base/**/*.yaml). Pre-importing
# them into the k3s containerd store keeps pod start-up independent of registry
# DNS - measured: containerd's resolver can fail transiently while podman's
# works, which turned a deploy into ImagePullBackOff.
# Single source of truth: the manifests. LOCAL_IMAGES are the images this repo
# builds (one binary per image, `--build-arg BIN`); IMAGES are the third-party
# references, pinned by tag *and* digest. Bumping a version means editing the
# manifest once.
LOCAL_IMAGES := $(shell grep -rhoE 'image: localhost/[^ ]+' k8s/ | awk '{print $$2}' | sort -u)
IMAGES := $(shell grep -rhoE 'image: [^ ]+' k8s/ | awk '{print $$2}' \
            | grep -v '^localhost/' | sort -u)

check-pins: ## Verify every image reference is pinned by tag+digest and present locally
	@fail=0; \
	for ref in $$(grep -rhoE 'image: [^ ]+' k8s/ | awk '{print $$2}' | sort -u); do \
	  case "$$ref" in \
	    localhost/*) \
	      printf "  %-62s local build, tag pinned\n" "$$ref" ;; \
	    *@sha256:*) \
	      if $(CTR) images ls -q 2>/dev/null | grep -qF "$${ref%%:*}@$${ref##*@}"; then \
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

images: ## Build + import the local images, then pull + import the pinned third-party images
	@for ref in $(LOCAL_IMAGES); do \
	  bin=$${ref##*/}; bin=$${bin%%:*}; \
	  if [ ! -f "bins/$$bin/src/main.rs" ]; then \
	    printf "  %-62s manifest only (bins/%s has no source yet)\n" "$$ref" "$$bin"; continue; \
	  fi; \
	  printf "== %s (build)\n" "$$ref"; \
	  $(RUNTIME) build -q -f Dockerfile --build-arg BIN=$$bin -t "$$ref" . || exit 1; \
	  rm -f "$(IMG_TAR)"; \
	  $(RUNTIME) save -o "$(IMG_TAR)" "$$ref" || exit 1; \
	  $(CTR) images import "$(IMG_TAR)" >/dev/null || exit 1; \
	done
	@for img in $(IMAGES); do \
		printf "== %s\n" "$$img"; \
		$(RUNTIME) pull -q "$$img" || exit 1; \
		rm -f "$(IMG_TAR)"; \
		$(RUNTIME) save -o "$(IMG_TAR)" "$$img" || exit 1; \
		$(CTR) images import "$(IMG_TAR)" >/dev/null || exit 1; \
	done
	@rm -f "$(IMG_TAR)"
	@echo "local: $(LOCAL_IMAGES)"
	@echo "imported: $(IMAGES)"
	@$(MAKE) --no-print-directory check-pins

##@ Engine

# --no-cache is deliberate: a cached layer once survived a source change and
# the old binary shipped under the pinned tag (measured 2026-11-12 — two
# builds of the same tag printed one digest while crates/anvil differed). A
# deploy image must be reproducible from the tree, never "probably the tree";
# the dependency layer costs ~30 s uncached.
build: ## Build the selected instance's engine image fresh (no layer cache)
	$(RUNTIME) build --no-cache -f Dockerfile --build-arg BIN=$(ENGINE_BIN) -t $(ENGINE_IMAGE):$(ENGINE_TAG) .

import: build ## Import the engine image into the k3s containerd store
	$(RUNTIME) save -o $(IMG_TAR) $(ENGINE_IMAGE):$(ENGINE_TAG)
	$(CTR) images import $(IMG_TAR)
	rm -f $(IMG_TAR)
	@id=$$($(RUNTIME) images --format "{{.ID}}" $(ENGINE_IMAGE):$(ENGINE_TAG)); \
	  echo "imported $$id -> $(ENGINE_IMAGE):$(ENGINE_TAG)"

deploy-engine: import ## Rebuild + redeploy the selected instance's engine
	$(KUBECTL_NS) rollout restart deploy/engine
	@# Bound the wait by the Deployment's own progressDeadlineSeconds (the
	@# Kubernetes default is 600s when unset), not a second hardcoded timeout.
	@deadline=$$(kubectl -n $(NS) get deploy/engine -o jsonpath='{.spec.progressDeadlineSeconds}' 2>/dev/null); \
	 $(KUBECTL_NS) rollout status deploy/engine --timeout="$${deadline:-600}s"

venue-build: ## Build the venue binaries on the host (cargo: diorama-venue-engine + diorama-venue-sim)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo build -p diorama-venue-engine -p diorama-venue-sim

mainnet-build: ## Build the mainnet engine on the host (cargo: diorama-mainnet-engine)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo build -p diorama-mainnet-engine

cctp-build: ## Build the CCTP bridge on the host (cargo: diorama-cctp-bridge)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo build -p diorama-cctp-bridge

test-engine: ## Run workspace unit tests on the host (cargo)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo test --workspace

config-bootstrap: ## Copy each tracked project template to the gitignored live file when absent
	@for f in config/instances/*/specialization.example.toml; do \
	  d=$${f%/*}; \
	  if [ ! -f "$$d/specialization.toml" ]; then cp "$$f" "$$d/specialization.toml"; echo "created $$d/specialization.toml from template"; fi; \
	done
	@for f in k8s/overlays/*/wiring.example.env; do \
	  d=$${f%/*}; \
	  if [ ! -f "$$d/wiring.env" ]; then cp "$$f" "$$d/wiring.env"; echo "created $$d/wiring.env from template"; fi; \
	done
	@echo "deployment data present (specialization.toml, wiring.env) - the tracked tree stays template-only"

config-check: config-bootstrap ## Validate every instance file through the layered loader
	@for f in config/instances/*/instance.toml; do \
	  PATH="$$HOME/.cargo/bin:$$PATH" cargo run -q -p diorama-config -- check "$$f" || exit 1; \
	done

fitness: lint design-assertions config-check ast-scope ## Run the structural gates: lint, design assertions, config check, AST scope

ci: ## Run the exact CI gates locally: workspace tests, then fitness
	PATH="$$HOME/.cargo/bin:$$PATH" cargo test --workspace --locked
	$(MAKE) fitness

lint: ## lint-gate: clippy, all targets, warnings are errors
	PATH="$$HOME/.cargo/bin:$$PATH" cargo clippy --workspace --all-targets -- -D warnings

design-assertions: ## Public types keep Send + Sync (rule 11)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo test --workspace design_assertions

ast-scope: ## AST scope analysis: fn/struct size and nesting depth (syn, the compiler's parser)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo run -q -p diorama-ast-scope -- crates bins

##@ Ops

status: ## Pods, PVCs, engine status
	@$(KUBECTL_NS) get pods -o wide
	@$(KUBECTL_NS) get pvc
	@echo "--- engine status ---"
	@$(KUBECTL_NS) exec deploy/engine -- curl -sf http://localhost:8080/status | jq . 2>/dev/null || echo "(engine not ready)"

logs: ## Tail logs of all pods of the selected instance (follow)
	$(KUBECTL_NS) logs -f -l app.kubernetes.io/part-of=$(NS) --max-log-requests=10 --prefix

logs-%: ## Tail logs of one component, e.g. make logs-engine / logs-reth / logs-anvil-t
	$(KUBECTL_NS) logs -f deploy/$* --tail=100

# The mirror-specific targets pin their own namespace (the shared targets
# follow INSTANCE): a `mainnet-*` target is about the mainnet mirror, a
# `venue-*` target about the venue's.
mainnet-smoke: ## End-to-end smoke test (mainnet)
	./scripts/smoke.sh --side mainnet

venue-smoke: ## End-to-end smoke test of the Venue Mirror (V, engine, sim, bridge)
	./scripts/smoke.sh --side venue

venue-bootstrap: ns venue-secrets config-bootstrap images ## First-time setup (venue): namespace, secrets, images, full stack
	$(KUBECTL) apply -f k8s/base/diorama-venue/pvcs.yaml
	$(MAKE) INSTANCE=diorama-venue apply
	@echo "venue bootstrap complete. watch: make INSTANCE=diorama-venue status"

cctp-ns: ## Create the bridge namespace + StorageClass
	$(KUBECTL) apply -f $(CCTP_OVERLAY)/namespace.yaml
	$(KUBECTL) apply -f k8s/base/shared/storageclass.yaml

cctp-apply: config-bootstrap ## Apply the standalone CCTP bridge instance
	$(KUBECTL) apply -k $(CCTP_OVERLAY)

cctp-diff: ## Show what `make cctp-apply` would change
	$(KUBECTL) diff -k $(CCTP_OVERLAY) || true

cctp-status: ## Pods, PVC and health of the standalone bridge
	@$(KUBECTL) -n $(CCTP_NS) get pods,pvc
	@$(KUBECTL) -n $(CCTP_NS) exec deploy/cctp-bridge -- curl -sf localhost:8096/health | jq . 2>/dev/null || echo "(bridge not ready)"

cctp-logs: ## Tail the standalone bridge's logs
	$(KUBECTL) -n $(CCTP_NS) logs -f deploy/cctp-bridge --tail=100

cctp-bootstrap: cctp-ns cctp-secrets config-bootstrap images ## First-time setup: namespace, secret, images, bridge
	$(MAKE) cctp-apply
	@echo "cctp bootstrap complete. watch: make cctp-status"

venue-repin: ## Re-pin V to the fork source's latest block (the engine's /admin/epoch)
	$(KUBECTL) -n diorama-venue exec deploy/engine -- curl -sf -m 90 -X POST localhost:8080/admin/epoch

mirror-freshness: ## One view of the execution, market-data, venue and relay planes
	./scripts/mirror-freshness.sh

substrate-attest: ## Read-only substrate invariants: cursors, pins, contract code, bridge, DB, host
	./scripts/substrate-attest.sh

logs-check: ## Deep log scan: warnings/errors per component (last 30 min, normalized)
	@for c in reth lighthouse anvil-t engine; do \
	  printf "== %s\n" "$$c"; \
	  $(KUBECTL) -n $(NS) logs deploy/$$c --since=30m 2>/dev/null \
	    | grep -aE "WARN|ERROR|\"level\":\"(WARN|ERROR)\"" \
	    | sed -E 's/\"timestamp\":\"[^\"]*\"/TS/; s/[0-9a-fx]{8,}/HEX/g; s/[0-9]{4,}/N/g' \
	    | sort | uniq -c | sort -rn | head -6; \
	done

mainnet-backfill: ## Show the CL backfill progress and the lighthouse datadir sizes
	@$(KUBECTL_NS) logs deploy/lighthouse --tail=200 2>/dev/null \
	  | jq -r 'select(.msg != null and (.msg|test("historical";"i"))) | "backfill: distance=\(.distance // "?") speed=\(.speed // "?") est=\(.est_time // "?")"' 2>/dev/null | tail -1
	@$(SUDO) sh -c 'du -sh $(K3S_STORAGE)/pvc-*_$(NS)_lighthouse-data/beacon/* 2>/dev/null | sort -h | tail -4'

mainnet-prune-blobs: ## Reclaim backfilled blob storage (needs the CL stopped; safe for the mirror)
	@echo "scaling lighthouse down (the prune needs the database lock)..."
	$(KUBECTL) -n $(NS) scale deploy/lighthouse --replicas=0
	$(KUBECTL) -n $(NS) wait --for=delete pod -l app.kubernetes.io/name=lighthouse --timeout=180s || true
	$(KUBECTL) -n $(NS) delete job lighthouse-prune-blobs --ignore-not-found
	$(KUBECTL) apply -f $(BOOTSTRAP)/prune-blobs-job.yaml
	$(KUBECTL) -n $(NS) wait --for=condition=complete job/lighthouse-prune-blobs --timeout=3600s || true
	$(KUBECTL_NS) logs job/lighthouse-prune-blobs --tail=20 || true
	$(KUBECTL) -n $(NS) delete job lighthouse-prune-blobs --ignore-not-found
	$(KUBECTL) -n $(NS) scale deploy/lighthouse --replicas=1
	$(KUBECTL) -n $(NS) rollout status deploy/lighthouse --timeout=300s
	@$(MAKE) --no-print-directory mainnet-backfill

pin-show: ## Show the resolved fork pins of the instance's anvils
	@for c in $(ANVILS); do \
		printf "%s pin: " $$c; $(KUBECTL_NS) exec deploy/$$c -- cat /state/pin 2>/dev/null || echo "(not resolved yet)"; \
	done

mainnet-repin: ## Re-pin drill: wipe the pin, restart the anvil (engine auto-rebases; a new epoch starts)
	@cm=$$(kubectl -n diorama-mainnet get deploy anvil-t -o jsonpath='{.spec.template.spec.volumes[?(@.name=="instance")].configMap.name}'); \
	pin=$$(kubectl -n diorama-mainnet get cm "$$cm" -o go-template='{{index .data "specialization.toml"}}' 2>/dev/null | sed -n 's/^fork_pin[[:space:]]*=[[:space:]]*\([0-9_]*\).*/\1/p' | tr -d '_'); \
	if [ -n "$$pin" ]; then \
	  echo "the mounted specialization still pins fork_pin=$$pin; remove it first (a changed pin is a new epoch and clears the shadow registry)." >&2; \
	  exit 1; \
	fi
	$(KUBECTL) -n diorama-mainnet scale deploy/anvil-t --replicas=0
	# Wait until the pod is really gone before wiping: the wipe must not
	# race a container that is still shutting down (its init container
	# re-resolves the pin on the next start).
	$(KUBECTL) -n diorama-mainnet wait --for=delete pod -l app.kubernetes.io/name=anvil-t --timeout=180s || true
	$(KUBECTL) -n diorama-mainnet delete job repin-wipe --ignore-not-found
	$(KUBECTL) apply -f $(BOOTSTRAP)/repin-job.yaml
	$(KUBECTL) -n diorama-mainnet wait --for=condition=complete job/repin-wipe --timeout=300s
	$(KUBECTL) -n diorama-mainnet delete job repin-wipe --ignore-not-found
	$(KUBECTL) -n diorama-mainnet scale deploy/anvil-t --replicas=1
	$(KUBECTL) -n $(NS) rollout status deploy/anvil-t --timeout=600s
	@echo "re-pinned. engine will rebase automatically on its next poll."

##@ Help

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage: make \033[36m<target>\033[0m\n"} \
		/^[a-zA-Z0-9_-]+:.*?##/ { printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2 } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)
	@echo

.PHONY: ci fitness lint design-assertions config-check config-bootstrap ast-scope
.PHONY: ns mainnet-secrets venue-secrets apply diff delete \
	mainnet-snapshot mainnet-snapshot-status mainnet-bootstrap venue-bootstrap \
	images build venue-build mainnet-build cctp-build import deploy-engine test-engine \
	status logs logs-check check-pins mainnet-smoke venue-smoke venue-repin \
	mainnet-backfill mainnet-prune-blobs pin-show mainnet-repin mirror-freshness help \
	substrate-attest up
