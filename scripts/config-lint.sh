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
#      deployment file is tracked by mistake;
#   6. the live audit is a gate: `scripts/cluster-config-audit.txt` records
#      every key the live cluster's ConfigMaps carried, classed live-read or
#      live-dead, and the build must carry exactly those - a live fact the
#      repo lost is a porting gap, a dead key the repo grew back is a
#      regression of the leak the audit removed;
#   7. the two mirrors' instance files are TOML-shape symmetric: the same
#      sections, and the same keys in every shared section, beyond the
#      declared per-profile facts.
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

# 6. The live audit as a gate: every live-read key exists in the build, and
#    no live-dead key grew back.
if [ ! -f scripts/cluster-config-audit.txt ]; then
    bad "scripts/cluster-config-audit.txt missing (the live audit is the porting gate)"
fi
for inst in "${instances[@]}"; do
    for f in "k8s/overlays/$inst/wiring.example.env" "k8s/overlays/$inst/secrets.example.env" \
             "config/instances/$inst/specialization.example.toml"; do
        [ -f "$f" ] || bad "$f missing (the audit gate builds each instance from the tracked templates)"
    done
done
if command -v "${KUBECTL:-kubectl}" >/dev/null 2>&1; then
    tmpdir=$(mktemp -d)
    trap 'rm -rf "$tmpdir"' EXIT
    # The gate reads the tracked tree only: every overlay is built on a
    # scratch copy in which each gitignored wiring/specialization file is the
    # tracked `.example` template (an absent file is silently skipped by the
    # generator, and a local live value is not what a clone would produce).
    cp -a k8s config "$tmpdir/"
    for f in "$tmpdir"/k8s/overlays/*/wiring.env "$tmpdir"/k8s/overlays/*/secrets.env; do
        [ -f "$f" ] || continue
        template="${f%.env}.example.env"
        [ -f "$template" ] && cp "$template" "$f"
    done
    for f in "$tmpdir"/config/instances/*/specialization.toml; do
        [ -f "$f" ] || continue
        template="${f%.toml}.example.toml"
        [ -f "$template" ] && cp "$template" "$f"
    done
    for inst in "${instances[@]}"; do
        "${KUBECTL:-kubectl}" kustomize "$tmpdir/k8s/overlays/$inst" > "$tmpdir/$inst.yaml" 2>"$tmpdir/$inst.err" \
            || bad "kustomize build for $inst failed: $(head -c 200 "$tmpdir/$inst.err")"
    done
    # The build's ConfigMap (name, data key) pairs: one pass over each
    # overlay's build output. The generator emits `data:` before `metadata:`,
    # so each document is buffered and flushed whole.
    built_keys_of() {
        awk '
            function flush() {
                if (iscm) for (i = 1; i <= nk; i++) print cname " " keys[i];
            }
            function reset() {
                iscm=0; inmeta=0; top=0; indata=0; cname=""; nk=0;
                split("", keys);
            }
            /^---$/ { flush(); reset(); next }
            /^[^[:space:]]/ { top=1; inmeta=0; indata=0 }
            top && /^kind: ConfigMap/ { iscm=1; next }
            top && /^metadata:/ { inmeta=1; next }
            inmeta && /^  name:/ && cname == "" { cname=$2; next }
            top && /^data:/ { indata=1; next }
            indata && /^  [A-Za-z0-9._-]+:/ {
                line=$0; sub(/^  /, "", line); sub(/:.*/, "", line);
                if (line !~ /^metadata$/) keys[++nk]=line;
            }
            END { flush() }
        ' "$1" | sort -u
    }
    for inst in "${instances[@]}"; do
        [ -s "$tmpdir/$inst.yaml" ] || continue
        built=$(built_keys_of "$tmpdir/$inst.yaml")
        while IFS='|' read -r ns cm key class; do
            case "$ns" in ''|'#'*) continue ;; esac
            [ "$ns" = "$inst" ] || continue
            # The built map: its name is the base name, with or without the
            # generator's hash suffix.
            matches=$(printf '%s\n' "$built" | awk -v cm="$cm" -v key="$key" \
                '$1 == cm || $1 ~ "^"cm"-" { if ($2 == key) print "hit"; } ')
            case "$class" in
                live-read)
                    [ -n "$matches" ] || bad "$inst build dropped a live-read key: $cm/$key (a fresh clone loses a live fact)"
                    ;;
                live-dead)
                    [ -z "$matches" ] || bad "$inst build grew a live-dead key back: $cm/$key (the audit cut it: no reader)"
                    ;;
                *)
                    bad "scripts/cluster-config-audit.txt: unknown class '$class' for $ns/$cm/$key"
                    ;;
            esac
        done < scripts/cluster-config-audit.txt
    done
elif [ "$fail" -eq 0 ]; then
    bad "no kubectl on PATH; the audit gate cannot build the overlays"
fi

# 7. TOML shape symmetry: the two mirrors' instance files declare the same
#    sections, and the same keys in every shared section. The per-profile
#    facts are declared here, with the reason, the same way the wiring key
#    deltas are (rule 3).
toml_keys() {
    awk '
        /^[[:space:]]*#/  { next }
        /^[[:space:]]*$/  { next }
        /^\[\[/           { next }
        /^\[/             { section=$0; gsub(/[][]/,"",section); next }
        /=/               { split($0, pair, "="); gsub(/[[:space:]]/, "", pair[1]); print section ":" pair[1] }
    ' "$1" | sort
}
mainnet_instance=config/instances/diorama-mainnet/instance.toml
venue_instance=config/instances/diorama-venue/instance.toml
if [ -f "$mainnet_instance" ] && [ -f "$venue_instance" ]; then
    mainnet_only_sections="chains.l1 services.engine.source_sync services.engine.beacon"
    venue_only_sections="chains.venue services.venue_sim"
    # The key spelling is the extractor's `section:key` (the section path and
    # the key, colon-joined).
    mainnet_only_keys=""
    venue_only_keys="services.engine:boot_timeout_secs services.engine.loops:poll_interval_ms"
    a=$(toml_keys "$mainnet_instance")
    b=$(toml_keys "$venue_instance")
    for k in $(comm -23 <(echo "$a") <(echo "$b")); do
        section=${k%%:*}
        echo "$mainnet_only_sections" | grep -qw "$section" && continue
        echo "$mainnet_only_keys" | grep -qw "$k" && continue
        bad "mainnet instance carries $k and the venue's does not (undeclared per-profile delta)"
    done
    for k in $(comm -13 <(echo "$a") <(echo "$b")); do
        section=${k%%:*}
        echo "$venue_only_sections" | grep -qw "$section" && continue
        echo "$venue_only_keys" | grep -qw "$k" && continue
        bad "venue instance carries $k and the mainnet's does not (undeclared per-profile delta)"
    done
    # The instance units are one shape: the same tracked files per instance.
    for d in config/instances/*/; do
        for f in kustomization.yaml instance.toml specialization.example.toml; do
            [ -f "$d$f" ] || bad "$d$f missing (the instance units are one shape)"
        done
    done
fi

[ "$fail" -eq 0 ] && echo "config lint clean: wiring complete, mirrors symmetric, no machine literals, every template bootstrap-able, the live audit gates the build"
exit "$fail"
