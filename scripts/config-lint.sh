#!/usr/bin/env bash
# scripts/config-lint.sh — the trackable tree's deployment gates, no cluster.
#
# The configuration model is single-artifact: the gitignored specialization
# file is the ONLY per-deployment source of truth, hardcoded, mounted into
# the pods as a file. No `${VAR}` references anywhere, no config envs and no
# config Secrets on the k8s side. The gates:
#
#   1. no `${VAR}` reference survives in any TOML layer (defaults, instance,
#      specialization — example or live): an env indirection would make the
#      specialization specific to one cluster's wiring, and the values must
#      be hardcoded because specializations are not committed;
#   2. no config-bearing env or Secret in the manifests: no `envFrom`, no
#      `configMapKeyRef`, no `secretKeyRef` anywhere under k8s/; the only
#      Secret material allowed is the generated `reth-jwt`, mounted as a
#      FILE (a credential between two of this repo's own pods, never config);
#   3. the instance units' ConfigMap generator carries the specialization as
#      a FILE and fails the build loudly when the file is absent (the
#      no-silent-gap behavior — a generator silently skipping a missing
#      source is exactly how a fresh clone loses a live fact);
#   4. the two mirrors are structurally identical (the same overlay files,
#      the same base shape, the same Makefile surface) and their instance
#      files are TOML-shape symmetric beyond the declared per-profile facts;
#   5. no machine-specific literal in a tracked manifest (a PV `volumeName`,
#      a `latest` tag, an unpinned third-party image); every template has
#      its bootstrap rule (harness) or its named remedy (specialization),
#      and no live deployment file is tracked by mistake;
#   6. the live audit, re-pointed: `scripts/cluster-config-audit.txt` names
#      every key the live pods resolve, as a dotted path into the merged
#      config. live-read: the key must exist in the merged
#      defaults+specialization+instance (a fresh clone with the template
#      must carry the fact). live-dead: the key must NOT exist (a leak the
#      audit removed must not grow back). host-fact: the pods read it, but
#      its presence is the *deployment's* state, not the tree's — a fresh
#      clone is correct to omit it (an epoch pin is the case: a pin the
#      fresh node's state does not agree with trips the engine's rollback
#      guard, fail-closed), so neither presence nor absence is a finding.
#      The other direction — every specialization key has a
#      reader — is enforced by the typed loader's `deny_unknown_fields` at
#      every level: `make config-check` refuses a key nobody reads, so the
#      specialization cannot grow dead weight;
#   7. the two cluster facts a move changes are single-valued: the
#      StorageClass name and the repo images' registry path. Every overlay
#      includes both components; every claim in the build names the class the
#      build ships (no second literal, no class the tree does not create);
#      the only k3s-specific literal in the tree is that class's provisioner;
#      every `localhost/…` image is mapped in the registry component and every
#      mapping is used; each overlay's namespace transform names the Namespace
#      object it ships, and no rendered object carries another namespace.
#
# Exit: 0 clean, 1 with one line per violation.
set -euo pipefail

ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
fail=0
bad() { echo "FAIL: $*" >&2; fail=1; }

# Discovered, not listed: a host that adds an instance
# (`config/instances/<name>/instance.toml` plus its overlay) gets it built and
# checked without editing this script. The *symmetry* comparison below stays
# pinned to the two product mirrors, because "the mirrors have the same shape"
# is a product property, not a discovery result.
mapfile -t instances < <(cd config/instances && printf '%s\n' */instance.toml | sed 's#/instance.toml##' | sort)

# 1. No `${VAR}` reference in any TOML layer (comments excluded: the rule
#    itself is documented inside the files).
if grep -rn '^\s*[^#].*\${' config/defaults.toml config/instances/*/*.toml \
    | grep -q .; then
    grep -rn '^\s*[^#].*\${' config/defaults.toml config/instances/*/*.toml >&2
    bad "a \${VAR} reference survives in a TOML layer (the specialization is self-sustained: values are hardcoded)"
fi

# 2. No config-bearing env or Secret in the manifests.
if grep -rn "envFrom:" k8s/ --include="*.yaml" | grep -q .; then
    grep -rn "envFrom:" k8s/ --include="*.yaml" >&2
    bad "envFrom in a manifest (configuration is the mounted specialization file, never pod env)"
fi
if grep -rn "configMapKeyRef\|secretKeyRef" k8s/ --include="*.yaml" | grep -q .; then
    grep -rn "configMapKeyRef\|secretKeyRef" k8s/ --include="*.yaml" >&2
    bad "a keyRef in a manifest (configuration is file-mounted, never injected as env)"
fi
while read -r ref; do
    [ -n "$ref" ] || continue
    case "$ref" in *secretName:\ reth-jwt*) continue ;; esac
    bad "a Secret volume other than the generated reth-jwt: $ref"
done < <(grep -rhoE 'secretName: [a-z0-9-]+' k8s/ --include="*.yaml" | sort -u)
for f in k8s/overlays/*/wiring.env k8s/overlays/*/wiring.example.env \
         k8s/overlays/*/secrets.env k8s/overlays/*/secrets.example.env; do
    [ -e "$f" ] && bad "$f exists (the wiring/secrets plumbing is gone: the specialization is the only config source)"
done

# 3. The instance units build from the mounted files, and the build fails
#    loudly when the specialization is absent.
command -v "${KUBECTL:-kubectl}" >/dev/null 2>&1 \
    || { bad "no kubectl on PATH; the build gates cannot run"; }
if command -v "${KUBECTL:-kubectl}" >/dev/null 2>&1; then
    tmpdir=$(mktemp -d)
    trap 'rm -rf "$tmpdir"' EXIT
    cp -a k8s config "$tmpdir/"
    # The harness surface is the operator's (no pod reads it): a fresh-clone
    # build uses the tracked template, the same copy `make port-forward` makes.
    # Iterate the *templates*: on a fresh clone no live harness.env exists, so
    # iterating those would expand to nothing and the build would fail on a
    # missing source instead of using the template.
    for template in "$tmpdir"/k8s/overlays/*/harness.example.env; do
        [ -f "$template" ] || continue
        f="${template%.example.env}.env"
        [ -f "$f" ] || cp "$template" "$f"
    done
    # A fresh clone has no specialization (it is deployment data): the build
    # gate uses the tracked template, exactly like the harness above and like
    # the loader's own fallback. A *host's* checkout keeps its live file and
    # this loop does nothing. The negative test below still proves a missing
    # source fails the build loudly.
    for d in "$tmpdir"/config/instances/*/; do
        live="$d/specialization.toml"
        [ -f "$live" ] && continue
        [ -f "$d/specialization.example.toml" ] && cp "$d/specialization.example.toml" "$live"
    done
    for inst in "${instances[@]}"; do
        if "${KUBECTL:-kubectl}" kustomize "$tmpdir/k8s/overlays/$inst" > "$tmpdir/$inst.yaml" 2>"$tmpdir/$inst.err"; then
            echo "  build ok: $inst ($(grep -c '^kind:' "$tmpdir/$inst.yaml") objects)"
        else
            bad "kustomize build for $inst failed: $(head -c 200 "$tmpdir/$inst.err")"
        fi
    done
    # 7a. Storage: exactly one class object, and every claim names it. The
    #     replacement is what normalizes the claims, so this asserts the fan-out
    #     (one object, every claim on it) - and the failure it guards is the one
    #     the bridge hit: a claim on `diorama-retain`, a class the tree no longer
    #     creates, which a fresh host would leave Pending forever.
    sc_name=$(awk '/^metadata:/{m=1;next} m && /^  name:/{print $2; exit}' k8s/base/shared/storageclass.yaml)
    [ -n "$sc_name" ] || bad "no StorageClass name in k8s/base/shared/storageclass.yaml"
    for inst in "${instances[@]}"; do
        f="$tmpdir/$inst.yaml"
        [ -s "$f" ] || continue
        classes=$(grep -c '^kind: StorageClass' "$f")
        [ "$classes" = "1" ] || bad "$inst renders $classes StorageClass objects (one class per cluster)"
        claims=$(grep -c '^  storageClassName:' "$f")
        on_class=$(grep -c "^  storageClassName: $sc_name\$" "$f")
        [ "$claims" = "$on_class" ] || bad "$inst: $((claims - on_class)) of $claims claims do not name the rendered class '$sc_name'"
    done
    # 7b. The only k3s-specific literal left is the class's own provisioner.
    while read -r hit; do
        case "$hit" in k8s/base/shared/storageclass.yaml:*provisioner:*) continue ;; esac
        bad "a k3s/local-path literal in a tracked manifest line: $hit"
    done < <(grep -rnE '^[^#]*(local-path|k3s|/var/lib/rancher)' k8s/ --include="*.yaml")
    # 7c. Every repo image is mapped in the one registry component, and every
    #     mapping names an image a manifest actually uses.
    for ref in $(grep -rhoE 'image: localhost/[^ ]+' k8s/base k8s/bootstrap --include="*.yaml" | awk '{print $2}' | sed 's/:.*//' | sort -u); do
        grep -qE "^  - name: $ref\$" k8s/components/registry/kustomization.yaml \
            || bad "image $ref is used by a manifest but not mapped in k8s/components/registry/kustomization.yaml"
    done
    while read -r mapped; do
        grep -rqE "image: $mapped:" k8s/base k8s/bootstrap --include="*.yaml" \
            || bad "k8s/components/registry maps $mapped but no manifest uses it"
    done < <(awk '/^  - name: /{print $3}' k8s/components/registry/kustomization.yaml)
    # 7d. The namespace is instance-parameterized: the overlay's transform
    #     names the Namespace it ships, no manifest in the base pins one, and
    #     every namespaced object of the build lands in it.
    for inst in "${instances[@]}"; do
        ns=$(awk '/^metadata:/{m=1;next} m && /^  name:/{print $2; exit}' "k8s/overlays/$inst/namespace.yaml")
        transform=$(awk '/^namespace:/{print $2}' "k8s/overlays/$inst/kustomization.yaml")
        [ "$ns" = "$transform" ] \
            || bad "$inst: namespace.yaml ships '$ns' but the overlay transforms to '$transform'"
        grep -q 'components/storage' "k8s/overlays/$inst/kustomization.yaml" \
            || bad "$inst: overlay does not include the storage component (its claims would keep a second literal)"
        grep -q 'components/registry' "k8s/overlays/$inst/kustomization.yaml" \
            || bad "$inst: overlay does not include the registry component (its images would keep the local-build prefix)"
        f="$tmpdir/$inst.yaml"
        [ -s "$f" ] || continue
        stray=$(grep -E '^  namespace: ' "$f" | grep -vc "namespace: $ns\$" || true)
        [ "$stray" = "0" ] || bad "$inst: $stray rendered object(s) carry a namespace other than '$ns'"
    done
    if grep -rn '^  namespace:' k8s/base --include="*.yaml" | grep -q .; then
        grep -rn '^  namespace:' k8s/base --include="*.yaml" >&2
        bad "a base manifest pins a namespace (the overlay's transform is the one place)"
    fi
    # The negative: a missing specialization.toml must FAIL the build, never
    # silently skip the generator entry.
    rm -f "$tmpdir/config/instances/diorama-cctp/specialization.toml"
    if "${KUBECTL:-kubectl}" kustomize "$tmpdir/k8s/overlays/diorama-cctp" >/dev/null 2>&1; then
        bad "the build SUCCEEDED without config/instances/diorama-cctp/specialization.toml (a missing source file must fail loudly)"
    fi
fi

# 4. Symmetry: the two mirrors' overlays carry the same files, their bases
#    the same *shape*, and their instance files the same TOML shape beyond
#    the declared per-profile facts (the specialization files differ by
#    product scope; the instance files are one deployment grammar).
a=$(cd k8s/overlays/diorama-mainnet && ls -1 | sort)
b=$(cd k8s/overlays/diorama-venue && ls -1 | sort)
[ "$a" = "$b" ] || bad "overlays differ (only in one: $(comm -3 <(echo "$a") <(echo "$b") | tr -d ' \t' | tr '\n' ' '))"
for f in kustomization.yaml pvcs.yaml engine.yaml; do
    [ -f "k8s/base/diorama-mainnet/$f" ] && [ -f "k8s/base/diorama-venue/$f" ] || bad "base $f exists in only one mirror"
done
ls k8s/base/diorama-mainnet | grep -qE '^anvil-t\.yaml$' || bad "mainnet base has no anvil"
ls k8s/base/diorama-venue | grep -qE '^anvil-v\.yaml$' || bad "venue base has no anvil"
for tgt in up apply diff delete secrets deps build snapshot status logs smoke port-forward endpoints; do
    grep -qE "^$tgt[^:]*:" Makefile || bad "Makefile has no instance-parameterized target: $tgt"
done
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
fi
# The instance units are one shape: the same tracked files per instance.
for d in config/instances/*/; do
    for f in kustomization.yaml instance.toml specialization.example.toml; do
        [ -f "$d$f" ] || bad "$d$f missing (the instance units are one shape)"
    done
done

# 5. No machine-specific literal in a tracked manifest; templates bootstrap
#    (harness) or name their remedy (specialization); no live file tracked.
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
# The specialization is deliberately NOT auto-copied (`make up` names the
# remedy instead - its values are the deployment's facts, not boilerplate),
# but no live file may be tracked, and the tracked template must exist.
for f in config/instances/*/specialization.example.toml; do
    [ -f "$f" ] || bad "$f missing (the tracked template is the copy source)"
    live="${f/.example./.}"
    if git ls-files --error-unmatch "$live" >/dev/null 2>&1; then
        bad "$live is tracked; it is deployment data - keyed endpoints and wallet keys never enter git"
    fi
done
for f in k8s/overlays/*/harness.example.env; do
    [ -f "$f" ] || continue
    live="${f/.example./.}"
    if git ls-files --error-unmatch "$live" >/dev/null 2>&1; then
        bad "$live is tracked; it is deployment data (only its .example template belongs in git)"
    fi
    grep -qF "harness.example.env" Makefile || bad "no harness bootstrap rule covers $(basename "$f")"
done

# 6. The live audit, re-pointed: dotted paths into the merged config. The
#    merged shape is computed from defaults + (live specialization if
#    present, else the tracked example) + instance — the same deep merge the
#    loader runs, so a live-read key the tree lost fails here.
if [ ! -f scripts/cluster-config-audit.txt ]; then
    bad "scripts/cluster-config-audit.txt missing (the live audit is the porting gate)"
fi
audit_check() {
    python3 - "$1" "$2" <<'PYEOF'
import sys, tomllib

instance_path, audit_path = sys.argv[1], sys.argv[2]
from pathlib import Path
d = Path(instance_path).parent

def load(p):
    with open(p, "rb") as f:
        return tomllib.load(f)

def merge(base, over):
    for k, v in over.items():
        if isinstance(v, dict) and isinstance(base.get(k), dict):
            merge(base[k], v)
        else:
            base[k] = v

merged = load(Path("config/defaults.toml"))
spec = d / "specialization.toml"
if not spec.exists():
    spec = d / "specialization.example.toml"
merge(merged, load(spec))
merge(merged, load(instance_path))

def lookup(path):
    cur = merged
    for seg in path.split("."):
        if seg.endswith("]"):
            name, idx = seg[:-1].split("[")
            cur = cur.get(name)
            if cur is None:
                return None
            i = int(idx)
            cur = cur[i] if isinstance(cur, list) and i < len(cur) else None
        else:
            cur = cur.get(seg) if isinstance(cur, dict) else None
        if cur is None:
            return None
    return cur

status = 0
for line in open(audit_path):
    line = line.strip()
    if not line or line.startswith("#"):
        continue
    ns, key, cls = (part.strip() for part in line.split("|"))
    if ns != d.name:
        continue
    present = lookup(key) is not None
    if cls == "live-read" and not present:
        print(f"FAIL: {ns} lost a live-read key: {key} (a fresh clone loses a live fact)", file=sys.stderr)
        status = 1
    elif cls == "live-dead" and present:
        print(f"FAIL: {ns} grew a live-dead key back: {key} (the audit cut it: no reader)", file=sys.stderr)
        status = 1
    elif cls == "host-fact":
        # Presence is the deployment's state, not the tree's: a fresh clone
        # omitting it is correct, and so is a live deployment that pins it.
        continue
    elif cls not in ("live-read", "live-dead", "host-fact"):
        print(f"FAIL: {audit_path}: unknown class '{cls}' for {ns}/{key}", file=sys.stderr)
        status = 1
sys.exit(status)
PYEOF
}
for inst in "${instances[@]}"; do
    audit_check "config/instances/$inst/instance.toml" scripts/cluster-config-audit.txt || fail=1
done

[ "$fail" -eq 0 ] && echo "config lint clean: no \${VAR} in any TOML, no config env/Secret in the manifests, the specialization build fails loudly when absent, mirrors symmetric, no machine literals, the live audit gates the merged shape"
exit "$fail"
