SHELL := /bin/bash

NS             := mainnet-mirror
ENGINE_IMAGE   := localhost/mirror-engine
# Single source of truth: the tag in the manifest we deploy.
ENGINE_TAG     := $(shell grep -o 'mirror-engine:[^@ ]*' k8s/base/engine.yaml | cut -d: -f2)
OVERLAY        := k8s/overlays/mainnet-mirror
BOOTSTRAP      := k8s/bootstrap
KUBECTL        := kubectl -n $(NS)

.DEFAULT_GOAL := help

##@ Cluster

ns: ## Create namespace + StorageClass (idempotent)
	kubectl apply -f k8s/base/namespace.yaml
	kubectl apply -f k8s/base/storageclass.yaml

secrets: ## Create the reth<->lighthouse JWT secret if missing
	NS=$(NS) ./scripts/gen-secret.sh

apply: ## Apply the full mirror stack (kustomize build k8s/overlays/mainnet-mirror)
	kubectl apply -k $(OVERLAY)

diff: ## Show what `make apply` would change
	kubectl diff -k $(OVERLAY) || true

delete: ## Delete the namespace (data survives: StorageClass is reclaimPolicy Retain)
	kubectl delete namespace $(NS) --ignore-not-found

##@ Bootstrap

snapshot: ## Download the reth snapshot into the reth PVC (scales reth down first)
	kubectl -n $(NS) scale deploy/reth --replicas=0 2>/dev/null || true
	kubectl -n $(NS) delete job reth-snapshot --ignore-not-found
	kubectl apply -f $(BOOTSTRAP)/snapshot-job.yaml
	@echo "waiting for snapshot to finish (up to 2h; resumable)..."
	kubectl -n $(NS) wait --for=condition=complete job/reth-snapshot --timeout=7200s || \
		{ echo "still running - check: make snapshot-status"; exit 1; }

snapshot-status: ## Show snapshot job status + progress
	@$(KUBECTL) get job reth-snapshot 2>/dev/null || echo "no snapshot job"
	@$(KUBECTL) logs job/reth-snapshot --tail=3 2>/dev/null || true

bootstrap: ns secrets images ## First-time setup: namespace, secrets, images, snapshot, full stack
	kubectl apply -f k8s/base/pvcs.yaml
	$(MAKE) snapshot
	$(MAKE) apply
	@echo "bootstrap complete. watch: make status"

##@ Images

# Every image is pinned by full version tag (see k8s/base/*.yaml). Pre-importing
# them into the k3s containerd store keeps pod start-up independent of registry
# DNS - measured: containerd's resolver can fail transiently while podman's
# works, which turned a deploy into ImagePullBackOff.
# Single source of truth: every third-party image reference in the manifests,
# pinned by tag *and* digest. Bumping a version means editing the manifest once.
IMAGES := $(shell grep -rhoE 'image: [^ ]+' k8s/ | awk '{print $$2}' \
            | grep -v 'mirror-engine' | sort -u)

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

images: ## Pull + import the pinned third-party images into k3s containerd
	@for img in $(IMAGES); do \
		printf "== %s\n" "$$img"; \
		podman pull -q "$$img" || exit 1; \
		rm -f /tmp/mirror-img.tar; \
		podman save -o /tmp/mirror-img.tar "$$img" || exit 1; \
		sudo k3s ctr images import /tmp/mirror-img.tar >/dev/null || exit 1; \
	done
	@rm -f /tmp/mirror-img.tar
	@echo "imported: $(IMAGES)"
	@$(MAKE) --no-print-directory check-pins

##@ Engine

build: ## Build the engine image with podman (Dockerfile at the workspace root)
	podman build -f Dockerfile --build-arg BIN=mirror-engine -t $(ENGINE_IMAGE):$(ENGINE_TAG) .

import: build ## Import the engine image into the k3s containerd store
	podman save -o /tmp/mirror-engine.tar $(ENGINE_IMAGE):$(ENGINE_TAG)
	sudo k3s ctr images import /tmp/mirror-engine.tar
	rm -f /tmp/mirror-engine.tar

deploy-engine: import ## Rebuild + redeploy the engine
	$(KUBECTL) rollout restart deploy/engine
	$(KUBECTL) rollout status deploy/engine --timeout=120s

test-engine: ## Run workspace unit tests on the host (cargo)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo test --workspace

config-check: ## Validate the instance config through the layered loader
	PATH="$$HOME/.cargo/bin:$$PATH" cargo run -q -p mirror-config -- check config/instances/mainnet-mirror.toml

fitness: lint design-assertions closure-matrix no-orphans ## Run every structural fitness function (plan §11.3)

lint: ## lint-gate: clippy, all targets, warnings are errors
	PATH="$$HOME/.cargo/bin:$$PATH" cargo clippy --workspace --all-targets -- -D warnings

design-assertions: ## Public types keep Send + Sync (rule 11)
	PATH="$$HOME/.cargo/bin:$$PATH" cargo test --workspace design_assertions

closure-matrix: ## Every internal dependency edge is declared (plan §11.3)
	./scripts/closure-matrix.sh

no-orphans: ## No utils/common/helpers crate or module (Lesson 101)
	./scripts/no-orphans.sh

##@ Ops

status: ## Pods, PVCs, engine status
	@$(KUBECTL) get pods -o wide
	@$(KUBECTL) get pvc
	@echo "--- engine status ---"
	@$(KUBECTL) exec deploy/engine -- curl -sf http://localhost:8080/status | jq . 2>/dev/null || echo "(engine not ready)"

logs: ## Tail logs of all pods (follow)
	$(KUBECTL) logs -f -l app.kubernetes.io/part-of=mainnet-mirror --max-log-requests=10 --prefix

logs-%: ## Tail logs of one component, e.g. make logs-engine / logs-reth / logs-anvil-m
	$(KUBECTL) logs -f deploy/$* --tail=100

smoke: ## End-to-end smoke test
	NS=$(NS) ./scripts/smoke.sh

scenario: ## Scenario test: self-containment, live parity (M+T), devnet workflow
	NS=$(NS) ./scripts/scenario.sh

proof: ## Capability proof: live fork, mint funds, real Aave/Uniswap/liquidation interaction
	NS=$(NS) ./scripts/capability-proof.sh

product-sim: ## Product simulation: levered position, live-data sampling, bot liquidation on T
	NS=$(NS) ./scripts/product-sim.sh

closure-check: ## Audit the watchlist closure: every storage dep of every market-data view
	NS=$(NS) ./scripts/closure-check.sh

diagrams: ## Regenerate docs/img renders from the ASCII diagram blocks (needs pango-view)
	./scripts/render-diagrams.sh

selftest: ## Run the engine selftest Job (public RPC + scratch anvil; no reth needed)
	kubectl -n $(NS) delete job engine-selftest --ignore-not-found
	kubectl apply -f $(BOOTSTRAP)/selftest-job.yaml
	kubectl -n $(NS) wait --for=condition=complete job/engine-selftest --timeout=900s || true
	$(KUBECTL) logs job/engine-selftest

logs-check: ## Deep log scan: warnings/errors per component (last 30 min, normalized)
	@for c in reth lighthouse anvil-m anvil-t engine; do \
	  printf "== %s\n" "$$c"; \
	  kubectl -n $(NS) logs deploy/$$c --since=30m 2>/dev/null \
	    | grep -aE "WARN|ERROR|\"level\":\"(WARN|ERROR)\"" \
	    | sed -E 's/\"timestamp\":\"[^\"]*\"/TS/; s/[0-9a-fx]{8,}/HEX/g; s/[0-9]{4,}/N/g' \
	    | sort | uniq -c | sort -rn | head -6; \
	done

backfill: ## Show the CL backfill progress and the lighthouse datadir sizes
	@$(KUBECTL) logs deploy/lighthouse --tail=200 2>/dev/null \
	  | jq -r 'select(.msg != null and (.msg|test("historical";"i"))) | "backfill: distance=\(.distance // "?") speed=\(.speed // "?") est=\(.est_time // "?")"' 2>/dev/null | tail -1
	@sudo sh -c 'du -sh /var/lib/rancher/k3s/storage/pvc-*_$(NS)_lighthouse-data/beacon/* 2>/dev/null | sort -h | tail -4'

prune-blobs: ## Reclaim backfilled blob storage (needs the CL stopped; safe for the mirror)
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

pin-show: ## Show the resolved fork pins of anvil M and T
	@for c in anvil-m anvil-t; do \
		printf "%s pin: " $$c; $(KUBECTL) exec deploy/$$c -- cat /state/pin 2>/dev/null || echo "(not resolved yet)"; \
	done

repin: ## Re-pin drill: wipe the pins, restart the anvils (engine auto-rebases; a new epoch starts)
	kubectl -n $(NS) scale deploy/anvil-m deploy/anvil-t --replicas=0
	# Wait until the pods are really gone before wiping: the wipe must not
	# race a container that is still shutting down (its init container
	# re-resolves the pin on the next start).
	kubectl -n $(NS) wait --for=delete pod -l app.kubernetes.io/name=anvil-m --timeout=180s || true
	kubectl -n $(NS) wait --for=delete pod -l app.kubernetes.io/name=anvil-t --timeout=180s || true
	kubectl -n $(NS) delete job repin-wipe --ignore-not-found
	kubectl apply -f $(BOOTSTRAP)/repin-job.yaml
	kubectl -n $(NS) wait --for=condition=complete job/repin-wipe --timeout=300s
	kubectl -n $(NS) delete job repin-wipe --ignore-not-found
	kubectl -n $(NS) scale deploy/anvil-m deploy/anvil-t --replicas=1
	kubectl -n $(NS) rollout status deploy/anvil-m --timeout=600s
	kubectl -n $(NS) rollout status deploy/anvil-t --timeout=600s
	@echo "re-pinned. engine will rebase automatically on its next poll."

##@ Help

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage: make \033[36m<target>\033[0m\n"} \
		/^[a-zA-Z0-9_-]+:.*?##/ { printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2 } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)
	@echo

.PHONY: ns secrets apply diff delete snapshot snapshot-status bootstrap build import deploy-engine \
	test-engine config-check status logs smoke selftest pin-show repin help
