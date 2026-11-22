#!/usr/bin/env bash
# A **clean** restart of the venue chain node, before its state-dump memory
# growth reaches the limit.
#
# Measured 2026-11-22: anvil-v's RSS grows ~268 MB/min at the mirror's ~1.2
# blocks/s — almost exactly one full state dump (1.55 GB of JSON) per
# `--state-interval`, i.e. the periodic serialization is what the process
# keeps. At the 40 Gi limit that is an OOM every ~2 h, and an OOM is an
# *unclean* death: the node comes back from the last dump and rewinds up to one
# interval of chain state — which is what parks CCTP transfers and strands
# settlements (the 2026-11-22 05:46 park was one).
#
# A SIGTERM is the clean death: anvil flushes the state file at termination and
# the chain height never dips (nothing to rewind, nothing for the CCTP watchers
# to re-reconcile). Proven live 11:03.
#
# **Scale to zero, never delete the pod** (measured 2026-11-22 11:40). A pod
# delete lets the Deployment's Recreate/RollingUpdate create the replacement
# immediately: it mounted the same RWO volume, read the outgoing pod's
# *mid-write* dump, parked it as torn, and exited 1 — and the container's next
# start found the file *absent* and forked a fresh history from the pin,
# replacing 298 k blocks. At 170 s the 1.6 GB dump was also killed mid-write.
# Scaling to zero waits for the old pod (and its dump) to be gone before the
# replacement is created; the node's own startup guard refuses a torn/absent
# file, so a failed dump stays a loud unavailable deployment instead of a silent
# rewind. The grace is 600 s (the pod spec carries it; `kubectl delete` is not
# used here at all).
set -euo pipefail

NS="${ANVIL_NAMESPACE:-diorama-venue}"
DEPLOY="${ANVIL_DEPLOYMENT:-anvil-v}"
SELECTOR="${ANVIL_SELECTOR:-app.kubernetes.io/name=anvil-v}"

pods=$(kubectl -n "$NS" get pods -l "$SELECTOR" --no-headers 2>/dev/null | wc -l)
if [ "$pods" = "0" ]; then
  echo "anvil-clean-restart: no pod matches ${SELECTOR} in ${NS}; nothing to restart" >&2
  exit 1
fi

before=$(kubectl -n "$NS" get pods -l "$SELECTOR" -o jsonpath='{.items[0].status.startTime}')
echo "anvil-clean-restart: scaling ${DEPLOY} to zero (old pod start ${before}; SIGTERM -> state dump)"
kubectl -n "$NS" scale "deployment/${DEPLOY}" --replicas=0 >/dev/null
kubectl -n "$NS" wait --for=delete pod -l "$SELECTOR" --timeout=600s

echo "anvil-clean-restart: old pod gone; starting the replacement"
kubectl -n "$NS" scale "deployment/${DEPLOY}" --replicas=1 >/dev/null
kubectl -n "$NS" rollout status "deployment/${DEPLOY}" --timeout=600s

after=$(kubectl -n "$NS" get pods -l "$SELECTOR" -o jsonpath='{.items[0].status.startTime}')
if [ "$after" = "$before" ]; then
  echo "anvil-clean-restart: the pod did not change (${after}); the restart did not happen" >&2
  exit 1
fi
echo "anvil-clean-restart: done (new pod start ${after})"
