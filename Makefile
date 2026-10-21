SHELL := /bin/bash

NS             := mainnet-mirror
ENGINE_IMAGE   := localhost/mirror-engine
# Single source of truth: the tag in the manifest we deploy.
ENGINE_TAG     := $(shell grep -o 'mirror-engine:[^@ ]*' k8s/base/engine.yaml | cut -d: -f2)
OVERLAY        := k8s/overlays/local
BOOTSTRAP      := k8s/bootstrap
KUBECTL        := kubectl -n $(NS)

.DEFAULT_GOAL := help

##@ Cluster

ns: ## Create namespace + StorageClass (idempotent)
	kubectl apply -f k8s/base/namespace.yaml
	kubectl apply -f k8s/base/storageclass.yaml

secrets: ## Create the reth<->lighthouse JWT secret if missing
	NS=$(NS) ./scripts/gen-secret.sh

apply: ## Apply the full mirror stack (kustomize build k8s/overlays/local)
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

##@ Engine

build: ## Build the engine image with podman
	cd engine && podman build -t $(ENGINE_IMAGE):$(ENGINE_TAG) .

import: build ## Import the engine image into the k3s containerd store
	podman save -o /tmp/mirror-engine.tar $(ENGINE_IMAGE):$(ENGINE_TAG)
	sudo k3s ctr images import /tmp/mirror-engine.tar
	rm -f /tmp/mirror-engine.tar

deploy-engine: import ## Rebuild + redeploy the engine
	$(KUBECTL) rollout restart deploy/engine
	$(KUBECTL) rollout status deploy/engine --timeout=120s

test-engine: ## Run engine unit tests on the host (cargo)
	cd engine && PATH="$$HOME/.cargo/bin:$$PATH" cargo test

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

selftest: ## Run the engine selftest Job (public RPC + scratch anvil; no reth needed)
	kubectl -n $(NS) delete job engine-selftest --ignore-not-found
	kubectl apply -f $(BOOTSTRAP)/selftest-job.yaml
	kubectl -n $(NS) wait --for=condition=complete job/engine-selftest --timeout=900s || true
	$(KUBECTL) logs job/engine-selftest

pin-show: ## Show the resolved fork pins of anvil M and T
	@for c in anvil-m anvil-t; do \
		printf "%s pin: " $$c; $(KUBECTL) exec deploy/$$c -- cat /state/pin 2>/dev/null || echo "(not resolved yet)"; \
	done

repin: ## Re-pin drill: wipe pins/state dumps, restart anvils (engine auto-rebases)
	kubectl -n $(NS) scale deploy/anvil-m deploy/anvil-t --replicas=0
	# anvil dumps state on SIGTERM, so the wipe must wait until the pods are
	# really gone - otherwise the dump lands after the wipe and a fresh pod
	# restores a stale chain (head ahead of the follow head, empty blocks).
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
	test-engine status logs smoke selftest pin-show repin help
