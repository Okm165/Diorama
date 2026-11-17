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
#      audit removed must not grow back). The other direction — every
#      specialization key has a reader — is enforced by the typed loader's
#      `deny_unknown_fields` at every level: `make config-check` refuses a
#      key nobody reads, so the specialization cannot grow dead weight.
#
# Exit: 0 clean, 1 with one line per violation.
set -euo pipefail

ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
fail=0
bad() { echo "FAIL: $*" >&2; fail=1; }

instances=(diorama-mainnet diorama-venue diorama-cctp)

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
    for f in "$tmpdir"/k8s/overlays/*/harness.env; do
        [ -f "$f" ] || continue
        template="${f%.env}.example.env"
        [ -f "$template" ] && cp "$template" "$f"
    done
    for inst in "${instances[@]}"; do
        if "${KUBECTL:-kubectl}" kustomize "$tmpdir/k8s/overlays/$inst" > "$tmpdir/$inst.yaml" 2>"$tmpdir/$inst.err"; then
            echo "  build ok: $inst ($(grep -c '^kind:' "$tmpdir/$inst.yaml") objects)"
        else
            bad "kustomize build for $inst failed: $(head -c 200 "$tmpdir/$inst.err")"
        fi
    done
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
    elif cls not in ("live-read", "live-dead"):
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
