SHELL := /bin/bash

# INSTANCE selects the mirror: the namespace, the kustomize overlay, the
# instance TOML and the engine image all follow from it (plan §19.8).
INSTANCE       ?= mainnet-mirror
NS             := $(INSTANCE)
OVERLAY        := k8s/overlays/$(INSTANCE)
BOOTSTRAP      := k8s/bootstrap
KUBECTL        := kubectl -n $(NS)

# The engine image/tag of the selected instance, scraped from the manifest
# that deploys it - the deployed manifest stays the single source of truth.
# mainnet-mirror runs the ethereum engine, venue-mirror the venue engine: one
# deployment shape per profile, one image per binary.
ENGINE_MANIFEST := k8s/base/$(INSTANCE)/engine.yaml
ENGINE_REF      := $(shell grep -ohE 'image: localhost/[^ ]+' $(ENGINE_MANIFEST) | head -1 | awk '{print $$2}')
ENGINE_IMAGE    := $(word 1,$(subst :, ,$(ENGINE_REF)))
ENGINE_TAG      := $(lastword $(subst :, ,$(ENGINE_REF)))
ENGINE_BIN      := $(notdir $(ENGINE_IMAGE))

# The anvil components of the selected instance (pin-show).
ANVILS := $(if $(filter mainnet-mirror,$(INSTANCE)),anvil-t,anvil-v)

# The selected instance's Namespace manifest: the overlay owns it (instance
# identity, not a service template) - one rule for both mirrors.
NS_MANIFEST := $(OVERLAY)/namespace.yaml

.DEFAULT_GOAL := help

##@ Cluster

ns: ## Create namespace + StorageClass (idempotent)
	kubectl apply -f $(NS_MANIFEST)
	kubectl apply -f k8s/base/shared/storageclass.yaml

mainnet-secrets: ## Create the reth<->lighthouse JWT secret if missing (mainnet namespace)
	NS=mainnet-mirror ./scripts/mainnet-secrets.sh

venue-secrets: ## Create the CCTP relay-wallet secret (CCTP_RELAY_KEY=0x…; venue namespace)
	NS=venue-mirror ./scripts/venue-secrets.sh

apply: ## Apply the selected instance's stack (INSTANCE selects the overlay)
	kubectl apply -k $(OVERLAY)

diff: ## Show what `make apply` would change
	kubectl diff -k $(OVERLAY) || true

delete: ## Delete the namespace (data survives: StorageClass is reclaimPolicy Retain)
	kubectl delete namespace $(NS) --ignore-not-found

##@ Bootstrap

mainnet-snapshot: ## Download the reth snapshot into the reth PVC (scales reth down first)
	kubectl -n mainnet-mirror scale deploy/reth --replicas=0 2>/dev/null || true
	kubectl -n mainnet-mirror delete job reth-snapshot --ignore-not-found
	kubectl apply -f $(BOOTSTRAP)/snapshot-job.yaml
	@echo "waiting for snapshot to finish (up to 2h; resumable)..."
	kubectl -n mainnet-mirror wait --for=condition=complete job/reth-snapshot --timeout=7200s || \
		{ echo "still running - check: make mainnet-snapshot-status"; exit 1; }

mainnet-snapshot-status: ## Show snapshot job status + progress
	@$(KUBECTL) get job reth-snapshot 2>/dev/null || echo "no snapshot job"
	@$(KUBECTL) logs job/reth-snapshot --tail=3 2>/dev/null || true

mainnet-bootstrap: ns mainnet-secrets images ## First-time setup (mainnet): namespace, secrets, images, snapshot, full stack
	kubectl apply -f k8s/base/mainnet-mirror/pvcs.yaml
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
	      if sudo k3s ctr images ls -q 2>/dev/null | grep -qF "$${ref%%:*}@$${ref##*@}"; then \
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
	  podman build -q -f Dockerfile --build-arg BIN=$$bin -t "$$ref" . || exit 1; \
	  rm -f /tmp/mirror-img.tar; \
	  podman save -o /tmp/mirror-img.tar "$$ref" || exit 1; \
	  sudo k3s ctr images import /tmp/mirror-img.tar >/dev/null || exit 1; \
	done
	@for img in $(IMAGES); do \
		printf "== %s\n" "$$img"; \
		podman pull -q "$$img" || exit 1; \
		rm -f /tmp/mirror-img.tar; \
		podman save -o /tmp/mirror-img.tar "$$img" || exit 1; \
		sudo k3s ctr images import /tmp/mirror-img.tar >/dev/null || exit 1; \
	done
	@rm -f /tmp/mirror-img.tar
	@echo "local: $(LOCAL_IMAGES)"
	@echo "imported: $(IMAGES)"
	@$(MAKE) --no-print-directory check-pins

##@ Engine

build: ## Build the selected instance's engine image with podman (Dockerfile at the workspace root)
	podman build -f Dockerfile --build-arg BIN=$(ENGINE_BIN) -t $(ENGINE_IMAGE):$(ENGINE_TAG) .

import: build ## Import the engine image into the k3s containerd store
	podman save -o /tmp/engine-image.tar $(ENGINE_IMAGE):$(ENGINE_TAG)
	sudo k3s ctr images import /tmp/engine-image.tar
	rm -f /tmp/engine-image.tar

deploy-engine: import ## Rebuild + redeploy the selected instance's engine
	$(KUBECTL) rollout restart deploy/engine
	$(KUBECTL) rollout status deploy/engine --timeout=120s

venue-build: ## Build the venue binaries on the host (cargo: venue-engine + venue-sim)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo build -p venue-engine -p venue-sim

mainnet-build: ## Build the mainnet engine on the host (cargo: mainnet-engine)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo build -p mainnet-engine

test-engine: ## Run workspace unit tests on the host (cargo)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo test --workspace

config-check: ## Validate every instance file through the layered loader (structural fitness)
	@for f in config/instances/*/instance.toml; do \
	  PATH="$$HOME/.cargo/bin:$$PATH" cargo run -q -p mirror-config -- check "$$f" || exit 1; \
	done

fitness: lint design-assertions closure-matrix no-orphans docs-metrics trust-audit clock-labels no-panics config-check docker-members k8s-wiring spec-citations spine-purity facts-published metrics-written ast-scope ## Run every structural fitness function (plan §11.3)

lint: ## lint-gate: clippy, all targets, warnings are errors
	PATH="$$HOME/.cargo/bin:$$PATH" cargo clippy --workspace --all-targets -- -D warnings

design-assertions: ## Public types keep Send + Sync (rule 11)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo test --workspace design_assertions

closure-matrix: ## Every internal dependency edge is declared (plan §11.3)
	./scripts/closure-matrix.sh

no-orphans: ## No utils/common/helpers crate or module (Lesson 101)
	./scripts/no-orphans.sh

docs-metrics: ## docs<->metrics: every metric the docs name exists in the code
	./scripts/docs-metrics.sh

trust-audit: ## Every `// TRUST:` marker names its guard (doctrine rule 6)
	./scripts/trust-audit.sh

clock-labels: ## Every logged block label names its clock (doctrine `block-numbering`)
	./scripts/clock-labels.sh

no-panics: ## No panic-family site in production code outside the named allow-list
	./scripts/no-panics.sh

docker-members: ## Dockerfile<->workspace: every member is copied and stubbed
	./scripts/docker-members.sh

k8s-wiring: ## instances<->manifests: every placeholder provided, every key consumed
	./scripts/k8s-wiring.sh

spec-citations: ## SPINE.md<->code: every symbol the spine doc names exists
	./scripts/spec-citations.sh

spine-purity: ## the spine's code names no mirror (seams are data, not names)
	./scripts/spine-purity.sh

facts-published: ## every Fact variant the event vocabulary declares is published
	./scripts/facts-published.sh

metrics-written: ## every metric series a registry declares is written by real code
	./scripts/metrics-written.sh

ast-scope: ## AST scope analysis: fn/struct size and nesting depth (syn, the compiler's parser)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo run -q -p ast-scope -- crates bins

##@ Ops

status: ## Pods, PVCs, engine status
	@$(KUBECTL) get pods -o wide
	@$(KUBECTL) get pvc
	@echo "--- engine status ---"
	@$(KUBECTL) exec deploy/engine -- curl -sf http://localhost:8080/status | jq . 2>/dev/null || echo "(engine not ready)"

logs: ## Tail logs of all pods of the selected instance (follow)
	$(KUBECTL) logs -f -l app.kubernetes.io/part-of=$(NS) --max-log-requests=10 --prefix

logs-%: ## Tail logs of one component, e.g. make logs-engine / logs-reth / logs-anvil-t
	$(KUBECTL) logs -f deploy/$* --tail=100

# The mirror-specific targets pin their own namespace (the shared targets
# follow INSTANCE): a `mainnet-*` target is about the mainnet mirror, a
# `venue-*` target about the venue's.
mainnet-smoke: ## End-to-end smoke test (mainnet)
	NS=mainnet-mirror ./scripts/mainnet-smoke.sh

venue-smoke: ## End-to-end smoke test of the Venue Mirror (V, engine, sim, bridge)
	NS=venue-mirror ./scripts/venue-smoke.sh

venue-follow-local: venue-build ## Native local proof of the source follow (two anvils; no cluster/Docker)
	./scripts/venue-follow-local.sh

venue-bootstrap: ns venue-secrets images ## First-time setup (venue): namespace, secrets, images, full stack
	kubectl apply -f k8s/base/venue-mirror/pvcs.yaml
	$(MAKE) INSTANCE=venue-mirror apply
	@echo "venue bootstrap complete. watch: make INSTANCE=venue-mirror status"

venue-repin: ## Re-pin V to the fork source's latest block (the engine's /admin/epoch)
	kubectl -n venue-mirror exec deploy/engine -- curl -sf -m 90 -X POST localhost:8080/admin/epoch

venue-drill: ## The venue epoch drill (same-pin restore, new-pin epoch, driver liveness)
	NS=venue-mirror ./scripts/venue-drill.sh

venue-proof: ## The venue capability proof (mint, sim deal, crash⇒liquidation, bridge round-trip, block production)
	NS=venue-mirror ./scripts/venue-proof.sh

venue-order-proof: ## The venue order-lane proof (fills vs the real book, the §4.5 impact invariants, hand-back)
	NS=venue-mirror ./scripts/venue-order-proof.sh

venue-product-sim: ## The daloc use-case simulation against the venue stack (endpoints via env)
	python3 scripts/daloc-sim.py

cctp-smoke: ## The CCTP bridge smoke (health, store coherence, metric set)
	NS=venue-mirror ./scripts/cctp-smoke.sh

cctp-roundtrip: ## The CCTP round trip (burn -> mint, exact delta; SOURCE/DESTINATION/AMOUNT env)
	NS=venue-mirror ./scripts/cctp-roundtrip.sh

cctp-margin-cycle: ## The crosschain unified-margin cycle (T mint -> CCTP -> venue collateral -> trade -> CCTP back)
	NS=venue-mirror ./scripts/cctp-margin-cycle.sh

engine-symmetry: ## Both engines' shared HTTP verbs must answer identically (fitness function)
	./scripts/engine-symmetry.sh

mainnet-feed-bridge: ## Feed-bridge helpers (list/pause/resume/override); ARGS="list"
	./scripts/mainnet-feed-bridge.sh $(ARGS)

mainnet-scenario: ## Scenario test: self-containment, live parity (T vs reth), devnet workflow
	NS=mainnet-mirror ./scripts/mainnet-scenario.sh

mainnet-proof: ## Capability proof: live fork, mint funds, real Aave/Uniswap/liquidation interaction
	NS=mainnet-mirror ./scripts/mainnet-proof.sh

mainnet-feed-local: ## Local feed-bridge proof (live parent answers -> mock aggregators, scenario pause/resume)
	./scripts/mainnet-feed-local.sh

mainnet-product-sim: ## Product simulation: levered position, live-data sampling, bot liquidation on T
	NS=mainnet-mirror ./scripts/mainnet-product-sim.sh

mainnet-closure-check: ## Audit the watchlist closure: every storage dep of every market-data view
	NS=mainnet-mirror ./scripts/mainnet-closure-check.sh

diagrams: ## Regenerate docs/img renders from the ASCII diagram blocks (needs pango-view)
	./scripts/render-diagrams.sh

mainnet-selftest: ## Run the engine selftest Job (public RPC + scratch anvil; no reth needed)
	kubectl -n mainnet-mirror delete job engine-selftest --ignore-not-found
	kubectl apply -f $(BOOTSTRAP)/selftest-job.yaml
	kubectl -n mainnet-mirror wait --for=condition=complete job/engine-selftest --timeout=900s || true
	$(KUBECTL) logs job/engine-selftest

mainnet-selftest-local: mainnet-build ## Native local selftest (two anvils; no reth/public RPC/cluster)
	./scripts/mainnet-selftest-local.sh

mainnet-engine-local: mainnet-build ## Native local proof of the engine UX (mirror/modify/mint/measure; two anvils)
	./scripts/mainnet-engine-local.sh

daloc-local: venue-build mainnet-build ## The daloc use-case arc against a local scratch stack (live HL market data)
	./scripts/daloc-local.sh

container-smoke: ## The mainnet-engine image, run for real (host anvils; needs podman)
	podman build -q -f Dockerfile --build-arg BIN=mainnet-engine -t localhost/mainnet-engine:v1 .
	./scripts/container-smoke.sh

logs-check: ## Deep log scan: warnings/errors per component (last 30 min, normalized)
	@for c in reth lighthouse anvil-t engine; do \
	  printf "== %s\n" "$$c"; \
	  kubectl -n $(NS) logs deploy/$$c --since=30m 2>/dev/null \
	    | grep -aE "WARN|ERROR|\"level\":\"(WARN|ERROR)\"" \
	    | sed -E 's/\"timestamp\":\"[^\"]*\"/TS/; s/[0-9a-fx]{8,}/HEX/g; s/[0-9]{4,}/N/g' \
	    | sort | uniq -c | sort -rn | head -6; \
	done

mainnet-backfill: ## Show the CL backfill progress and the lighthouse datadir sizes
	@$(KUBECTL) logs deploy/lighthouse --tail=200 2>/dev/null \
	  | jq -r 'select(.msg != null and (.msg|test("historical";"i"))) | "backfill: distance=\(.distance // "?") speed=\(.speed // "?") est=\(.est_time // "?")"' 2>/dev/null | tail -1
	@sudo sh -c 'du -sh /var/lib/rancher/k3s/storage/pvc-*_$(NS)_lighthouse-data/beacon/* 2>/dev/null | sort -h | tail -4'

mainnet-prune-blobs: ## Reclaim backfilled blob storage (needs the CL stopped; safe for the mirror)
	@echo "scaling lighthouse down (the prune needs the database lock)..."
	kubectl -n $(NS) scale deploy/lighthouse --replicas=0
	kubectl -n $(NS) wait --for=delete pod -l app.kubernetes.io/name=lighthouse --timeout=180s || true
	kubectl -n $(NS) delete job lighthouse-prune-blobs --ignore-not-found
	kubectl apply -f $(BOOTSTRAP)/prune-blobs-job.yaml
	kubectl -n $(NS) wait --for=condition=complete job/lighthouse-prune-blobs --timeout=3600s || true
	$(KUBECTL) logs job/lighthouse-prune-blobs --tail=20 || true
	kubectl -n $(NS) delete job lighthouse-prune-blobs --ignore-not-found
	kubectl -n $(NS) scale deploy/lighthouse --replicas=1
	kubectl -n $(NS) rollout status deploy/lighthouse --timeout=300s
	@$(MAKE) --no-print-directory backfill

pin-show: ## Show the resolved fork pins of the instance's anvils
	@for c in $(ANVILS); do \
		printf "%s pin: " $$c; $(KUBECTL) exec deploy/$$c -- cat /state/pin 2>/dev/null || echo "(not resolved yet)"; \
	done

mainnet-repin: ## Re-pin drill: wipe the pin, restart the anvil (engine auto-rebases; a new epoch starts)
	kubectl -n mainnet-mirror scale deploy/anvil-t --replicas=0
	# Wait until the pod is really gone before wiping: the wipe must not
	# race a container that is still shutting down (its init container
	# re-resolves the pin on the next start).
	kubectl -n mainnet-mirror wait --for=delete pod -l app.kubernetes.io/name=anvil-t --timeout=180s || true
	kubectl -n mainnet-mirror delete job repin-wipe --ignore-not-found
	kubectl apply -f $(BOOTSTRAP)/repin-job.yaml
	kubectl -n mainnet-mirror wait --for=condition=complete job/repin-wipe --timeout=300s
	kubectl -n mainnet-mirror delete job repin-wipe --ignore-not-found
	kubectl -n mainnet-mirror scale deploy/anvil-t --replicas=1
	kubectl -n $(NS) rollout status deploy/anvil-t --timeout=600s
	@echo "re-pinned. engine will rebase automatically on its next poll."

##@ Help

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage: make \033[36m<target>\033[0m\n"} \
		/^[a-zA-Z0-9_-]+:.*?##/ { printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2 } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)
	@echo

.PHONY: fitness lint design-assertions closure-matrix no-orphans docs-metrics \
	trust-audit clock-labels no-panics config-check docker-members k8s-wiring ast-scope \
	spec-citations facts-published metrics-written
.PHONY: ns mainnet-secrets venue-secrets apply diff delete \
	mainnet-snapshot mainnet-snapshot-status mainnet-bootstrap venue-bootstrap \
	images build venue-build import deploy-engine test-engine config-check \
	status logs mainnet-smoke venue-smoke mainnet-feed-bridge venue-repin venue-drill \
	venue-proof venue-order-proof venue-product-sim cctp-smoke cctp-roundtrip cctp-margin-cycle engine-symmetry \
	mainnet-scenario mainnet-proof mainnet-product-sim mainnet-closure-check mainnet-feed-local \
	mainnet-selftest mainnet-backfill mainnet-prune-blobs pin-show mainnet-repin help \
	venue-follow-local mainnet-selftest-local mainnet-engine-local daloc-local container-smoke
