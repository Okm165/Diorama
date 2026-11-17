#!/usr/bin/env bash
# scripts/config-lint.sh — the trackable tree's deployment gates, no cluster.
#
# The deployment story is "clone, copy one env, make up", so every rule that
# story depends on is checked here rather than discovered on a fresh machine:
#
#   1. every instance's wiring.example.env answers exactly the `${VAR}`s its
#      TOML references (a missing key is a pod that boots into a NamedError;
#      an extra key is a value no pod reads);
#   2. both mirrors are structurally identical: the same overlay files, the
#      same base layout (one service file per concern, the same inventory),
#      and the same Makefile surface for both INSTANCEs;
#   3. the wiring key sets differ only by the source facts each chain
#      genuinely has (declared below, with the reason);
#   4. no machine-specific literal survives in a tracked manifest (a PV
#      `volumeName`, a `latest` tag, an unpinned third-party image);
#   5. every `*.example.*` template has a bootstrap rule, and no live
#      deployment file is tracked by mistake.
#
# Exit: 0 clean, 1 with one line per violation.
set -euo pipefail

ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
fail=0
bad() { echo "FAIL: $*" >&2; fail=1; }

instances=(diorama-mainnet diorama-venue diorama-cctp)
keys_of() { awk -F= '$0 !~ /^[[:space:]]*#/ && $1 ~ /^[A-Z][A-Z0-9_]*$/ { print $1 }' "$1" | sort; }

# 1. The wiring (or the secrets file) answers the instance's references.
for inst in "${instances[@]}"; do
    wiring="k8s/overlays/$inst/wiring.example.env"
    secretf="k8s/overlays/$inst/secrets.example.env"
    [ -f "$wiring" ] || { bad "$wiring missing"; continue; }
    [ -f "$secretf" ] || { bad "$secretf missing"; continue; }
    declared=$(grep -rhoE '\$\{[A-Z][A-Z0-9_]*\}' "config/instances/$inst" | tr -d '${}' | sort -u)
    provided=$( { keys_of "$wiring"; keys_of "$secretf"; } | sort -u)
    for var in $declared; do
        echo "$provided" | grep -qx "$var" || bad "$inst references \${$var} but neither $wiring nor the secrets template answers it"
    done
done

# 2. Symmetry: the two mirrors' overlays carry the same files, and their bases
#    the same *shape*: the shared layer, the claims, the secrets inventory, one
#    engine and one anvil each (plus the profile's own service - the mainnet's
#    reth/lighthouse pair, the venue's simulator). The service files differ by
#    design (one binary per concern); the set of concerns never does.
a=$(cd k8s/overlays/diorama-mainnet && ls -1 | sort)
b=$(cd k8s/overlays/diorama-venue && ls -1 | sort)
[ "$a" = "$b" ] || bad "overlays differ (only in one: $(comm -3 <(echo "$a") <(echo "$b") | tr -d ' \t' | tr '\n' ' '))"
for f in kustomization.yaml pvcs.yaml engine.yaml secrets.yaml; do
    [ -f "k8s/base/diorama-mainnet/$f" ] && [ -f "k8s/base/diorama-venue/$f" ] || bad "base $f exists in only one mirror"
done
ls k8s/base/diorama-mainnet | grep -qE '^anvil-t\.yaml$' || bad "mainnet base has no anvil"
ls k8s/base/diorama-venue | grep -qE '^anvil-v\.yaml$' || bad "venue base has no anvil"
# ...and both overlays name the same Makefile targets for their INSTANCE.
for tgt in up apply diff delete secrets deps build snapshot status logs smoke port-forward endpoints; do
    grep -qE "^$tgt[^:]*:" Makefile || bad "Makefile has no instance-parameterized target: $tgt"
done

# 3. The wiring key sets differ only by the declared per-chain facts.
mainnet_only="BEACON_URL CHECKPOINT_SYNC_URL ENGINE_API_URL EVM_URL JWT_PATH SNAPSHOT_URL"
venue_only="ENGINE_ADMIN_URL EVM_FORK_URL HL_INFO_API_URL HL_WS_API_URL SIM_STATE_DIR"
a=$(keys_of k8s/overlays/diorama-mainnet/wiring.example.env)
b=$(keys_of k8s/overlays/diorama-venue/wiring.example.env)
for k in $(comm -23 <(echo "$a") <(echo "$b")); do
    echo "$mainnet_only" | grep -qw "$k" || bad "mainnet-only wiring key $k is not a declared per-chain fact"
done
for k in $(comm -13 <(echo "$a") <(echo "$b")); do
    echo "$venue_only" | grep -qw "$k" || bad "venue-only wiring key $k is not a declared per-chain fact"
done

# 4. No machine-specific literal in a tracked manifest.
if grep -rn "volumeName:" k8s/ --include="*.yaml" | grep -q .; then
    bad "a tracked PVC pins a volumeName (a volume that exists on one machine only)"
fi
if grep -rnE 'image: [^ ]*:latest' k8s/ --include="*.yaml" | grep -q .; then
    bad "a tracked manifest carries a floating ':latest' tag"
fi
while read -r ref; do
    case "$ref" in
        localhost/*|'') continue ;;
        *@sha256:*) continue ;;
        *) bad "third-party image not pinned by digest: $ref" ;;
    esac
done <<< "$(grep -rhoE 'image: [^ ]+' k8s/base k8s/bootstrap --include="*.yaml" | awk '{print $2}' | sort -u)"

# 5. Every template has a bootstrap rule, and no live file is tracked.
for kind in wiring harness secrets specialization; do
    for f in k8s/overlays/*/$kind.example.env config/instances/*/$kind.example.toml; do
        [ -f "$f" ] || continue
        live="${f/.example./.}"
        if git ls-files --error-unmatch "$live" >/dev/null 2>&1; then
            bad "$live is tracked; it is deployment data (only its .example template belongs in git)"
        fi
        # The bootstrap rule's glob, as written in the Makefile.
        glob="k8s/overlays/*/$kind.example.env"
        case "$kind" in specialization) glob="config/instances/*/$kind.example.toml" ;; esac
        grep -qF "$glob" Makefile || bad "no config-bootstrap rule covers $(basename "$f")"
    done
done

[ "$fail" -eq 0 ] && echo "config lint clean: wiring complete, mirrors symmetric, no machine literals, every template bootstrap-able"
exit "$fail"
