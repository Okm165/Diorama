#!/usr/bin/env bash
# scripts/check-secrets.sh — verify an instance's secret inventory is whole.
#
# Runs after `make secrets`. Three facts, all cheap, all fail-closed:
#   1. the instance Secret the manifests mount exists (a missing Secret is a
#      pod stuck in ContainerCreating naming a key, not a diagnosis);
#   2. every `${VAR}` the instance's TOML references is answered by the wiring
#      (ConfigMap) or the Secret, or the pod refuses to boot naming it;
#   3. the secrets file has no empty key that the instance references (a
#      template default left unfilled is a keyless boot).
#
# Usage: NS=diorama-venue INSTANCE=diorama-venue scripts/check-secrets.sh
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
export REPO_ROOT
# shellcheck source=lib-secrets.sh
. "$REPO_ROOT/scripts/lib-secrets.sh"

NS="${NS:?NS must be set (the instance namespace)}"
INSTANCE="${INSTANCE:?INSTANCE must be set}"
KUBECTL="${KUBECTL:-kubectl}"
SHORT="${INSTANCE#diorama-}"
SECRET="$INSTANCE-secrets"
SECRETS="$(instance_env "$SHORT" secrets)"

fail=0
if "$KUBECTL" -n "$NS" get secret "$SECRET" >/dev/null 2>&1; then
    echo "  secret $SECRET present in $NS"
else
    echo "  secret $SECRET MISSING in $NS (run: make secrets INSTANCE=$INSTANCE)" >&2
    fail=1
fi

# Every `${VAR}` the instance's own TOML files name must be answered by the
# wiring file or the Secret's keys - one rule, both instances, no per-pod list.
declared=$(grep -rhoE '\$\{[A-Z][A-Z0-9_]*\}' "$REPO_ROOT/config/instances/$INSTANCE" | tr -d '${}' | sort -u)
for var in $declared; do
    if [ -n "$(instance_value "$SHORT" wiring "$var")" ]; then
        continue
    fi
    if "$KUBECTL" -n "$NS" get secret "$SECRET" -o jsonpath="{.data.$var}" 2>/dev/null | grep -q .; then
        continue
    fi
    # The secrets file may carry the value without `make secrets` having run
    # yet; a filled file with a missing Secret is the same boot-time gap.
    if [ -n "$(instance_value "$SHORT" secrets "$var")" ]; then
        echo "  wiring gap: \${$var} is in secrets.env but the Secret is missing/stale (run: make secrets INSTANCE=$INSTANCE)" >&2
        fail=1
        continue
    fi
    echo "  wiring gap: $INSTANCE references \${$var} but neither wiring.env nor secret $SECRET answers it" >&2
    fail=1
done

[ "$fail" -eq 0 ] && echo "secrets complete for $INSTANCE"
exit "$fail"
