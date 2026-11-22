#!/usr/bin/env bash
# A **clean** restart of the venue chain node, before its state-dump memory
# growth reaches the limit.
#
# Measured 2026-11-22: anvil-v's RSS grows ~268 MB/min at the mirror's ~1.2
# blocks/s — almost exactly one full state dump (1.55 GB of JSON) per
# `--state-interval 300`, i.e. the periodic serialization is what the process
# keeps. At the 40 Gi limit that is an OOM every ~2 h, and an OOM is an
# *unclean* death: the node comes back from the last 5-minute dump and rewinds
# up to five minutes of chain state — which is what parks CCTP transfers and
# strands settlements (the 2026-11-22 05:46 park was one).
#
# A SIGTERM is the clean death, proven live (11:03): anvil flushed the state
# file at termination (`state_mtime` moved to the restart second), the chain
# height never dipped below the pre-restart mark (nothing to rewind, nothing
# for the CCTP watchers to re-reconcile), and the replacement node came back at
# ~2.3 GiB RSS. Scheduled hourly at minute 40 (root crontab, DALOC-ANVIL-RESTART),
# which keeps the peak around ~20 GiB — an hour of growth on the fresh base.
set -euo pipefail

NS="${ANVIL_NAMESPACE:-diorama-venue}"
SELECTOR="${ANVIL_SELECTOR:-app.kubernetes.io/name=anvil-v}"

pod=$(kubectl -n "$NS" get pods -l "$SELECTOR" -o jsonpath='{.items[0].metadata.name}')
if [ -z "${pod}" ]; then
  echo "anvil-clean-restart: no pod matches ${SELECTOR} in ${NS}" >&2
  exit 1
fi

echo "anvil-clean-restart: terminating ${pod} gracefully (SIGTERM -> state dump)"
kubectl -n "$NS" delete pod "$pod" --grace-period=170 --wait=false >/dev/null
kubectl -n "$NS" wait --for=delete "pod/${pod}" --timeout=240s >/dev/null 2>&1 || true

echo "anvil-clean-restart: waiting for the replacement"
kubectl -n "$NS" rollout status deployment/anvil-v --timeout=300s
