#!/usr/bin/env bash
# scripts/sync-instance-secrets.sh — write an instance's secret values into
# its Secret, and create the Secret when it is absent.
#
# The secrets file (k8s/overlays/<instance>/secrets.env, gitignored) carries
# what a provider keys - a keyed RPC URL, a relay wallet. The values must not
# land in a ConfigMap, where anyone with `get cm` reads them: this script
# copies each key into the `<instance>-secrets` Secret under the env name the
# pod reads (`CCTP_RELAY_KEY` -> key `relay-key`, everything else under its
# own name), and the manifests' `optional: true` secretKeyRef shadows the
# wiring ConfigMap's public default.
#
# Usage: scripts/sync-instance-secrets.sh <instance>   (e.g. diorama-venue)
#
# Idempotent: an unchanged value rewrites the same Secret (no rollout - the
# pods read it at container start); the pre-split `cctp-relay-key` Secret is
# migrated once so an apply never leaves the bridge keyless.
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
export REPO_ROOT
# shellcheck source=lib-secrets.sh
. "$REPO_ROOT/scripts/lib-secrets.sh"

INSTANCE="${1:?usage: sync-instance-secrets.sh <instance>}"
KUBECTL="${KUBECTL:-kubectl}"
NS="${NS:-$INSTANCE}"
SHORT="${INSTANCE#diorama-}"
SECRETS="$(instance_env "$SHORT" secrets)"
SECRET="$INSTANCE-secrets"

if [ ! -f "$SECRETS" ]; then
    echo "secrets fault: $SECRETS does not exist (run: make config-bootstrap)" >&2
    exit 1
fi

args=()
while IFS= read -r var; do
    [ -n "$var" ] || continue
    value=$(instance_value "$SHORT" secrets "$var")

    # Transition: the bridge's relay wallet predates the secrets file (created
    # by the old `make cctp-secrets CCTP_RELAY_KEY=0x…`). An unset file value
    # carries the existing Secret's key over once, so the first post-split
    # apply cannot leave the bridge keyless.
    if [ -z "$value" ] && [ "$var" = "CCTP_RELAY_KEY" ]; then
        legacy=$("$KUBECTL" -n "$NS" get secret cctp-relay-key \
            -o jsonpath='{.data.relay-key}' 2>/dev/null | base64 -d 2>/dev/null || true)
        if [ -n "$legacy" ]; then
            value="$legacy"
            echo "  $var: carried over from the pre-split secret cctp-relay-key"
        fi
    fi

    [ -n "$value" ] || continue
    # The Secret's key is the env name the pod reads (envFrom injects keys
    # verbatim; the legacy `relay-key` spelling is read during the carry-over
    # below, never written again).
    args+=("--from-literal=$var=$value")
done <<< "$(awk -F= '$0 !~ /^[[:space:]]*#/ && $1 ~ /^[A-Z][A-Z0-9_]*$/ { print $1 }' "$SECRETS")"

if "$KUBECTL" -n "$NS" get secret "$SECRET" >/dev/null 2>&1; then
    if [ "${#args[@]}" -gt 0 ]; then
        "$KUBECTL" -n "$NS" create secret generic "$SECRET" "${args[@]}" \
            --dry-run=client -o yaml | "$KUBECTL" apply -f - >/dev/null
        echo "secret $SECRET synced from secrets.env (${#args[@]} key(s)) in $NS"
    else
        echo "secret $SECRET exists in $NS (no values in secrets.env; left untouched)"
    fi
else
    "$KUBECTL" -n "$NS" create secret generic "$SECRET" ${args[@]+"${args[@]}"} >/dev/null
    if [ "${#args[@]}" -gt 0 ]; then
        echo "created secret $SECRET in $NS with ${#args[@]} key(s) from secrets.env"
    else
        echo "created empty secret $SECRET in $NS"
    fi
fi
