#!/bin/sh
# Read one instance's deployment values, split by sensitivity.
#
# The two env files per overlay (gitignored, copied from the tracked
# *.example templates by `make config-bootstrap`):
#   wiring.env   - every ConfigMap-safe value (the pods' envFrom ConfigMap)
#   secrets.env  - the keyed values (a provider URL with its key, a wallet
#                  key): `make secrets` writes them into the instance Secret,
#                  and they never enter a ConfigMap or git
#
# Usage (sourced by scripts/*.sh):
#   . lib-secrets.sh
#   v=$(instance_value venue secrets EVM_FORK_URL)
set -eu

# The overlay's env path (`mainnet`, `venue`, `cctp`, kind `wiring`|`secrets`).
instance_env() { # instance_env <instance-short> <kind>
    printf '%s/k8s/overlays/diorama-%s/%s.env\n' "${REPO_ROOT:?REPO_ROOT must be set}" "$1" "$2"
}

# Read one env-file key (the last assignment wins; comments skipped; the
# empty-string default of a template key reads as unset).
instance_value() { # instance_value <instance-short> <kind> <KEY>
    f=$(instance_env "$1" "$2")
    [ -f "$f" ] || return 0
    awk -v k="$3" '$0 !~ /^[[:space:]]*#/ && index($0, k "=") == 1 { sub(/^[^=]*=/, ""); v = $0 } END { if (v != "") print v }' "$f"
}
