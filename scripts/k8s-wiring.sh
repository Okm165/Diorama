#!/usr/bin/env bash
# Fitness function: k8s-wiring.
#
# The instance files and the manifests are two halves of one wiring contract.
# An instance's `${VAR}` placeholder must be provided by the pod env (the
# tree's `mirror-config` ConfigMap, or an inline env entry in one of the
# tree's manifests — the bridge's relay key rides a Secret that way), and
# every ConfigMap key must be consumed: named by the instance the pods mount,
# or substituted (`$(KEY)`) by a manifest that `envFrom`s the ConfigMap. A key
# left behind after a feature moves is dead wiring that still shows up in
# `kubectl describe` (measured: `CHECKPOINT_SYNC_FALLBACK`, read by nothing;
# `MAINNET_RPC`, overridden inline by the self-test Job).
#
# The overlay's resource list is the tree's manifest set: the venue overlay
# includes the cross-mirror bridge's base, the mainnet overlay does not. The
# same file also carries the harness's `test-endpoints` env: every
# `localhost:PORT` there must be a NodePort the overlay declares, or the
# harness dials a port nothing serves.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 - <<'PY'
import glob, re, sys

def placeholders(path):
    out = set()
    for line in open(path):
        code = line.split("#", 1)[0]
        out |= set(re.findall(r"\$\{([A-Z_]+)\}", code))
    return out

def cm_keys(path):
    return set(re.findall(r"^  ([A-Z_]+):", open(path).read(), re.M))

def env_names(path):
    text = open(path).read()
    block = set(re.findall(r"^\s*- name:\s*([A-Z_]+)\s*$", text, re.M))
    inline = set(re.findall(r"name:\s*([A-Z_]+)\s*[,}]", text))
    return block | inline

bad = 0
for tree in ("mainnet-mirror", "venue-mirror"):
    inst = f"config/instances/{tree}/instance.toml"
    cm = cm_keys(f"k8s/base/{tree}/mirror-config.yaml")
    manifests = sorted(glob.glob(f"k8s/base/{tree}/*.yaml"))
    if tree == "venue-mirror":
        manifests += sorted(glob.glob("k8s/base/cctp-bridge/*.yaml"))

    provided = set(cm)
    for m in manifests:
        provided |= env_names(m)
    for k in sorted(placeholders(inst) - provided):
        print(f"VIOLATION: {tree}: the instance names ${{{k}}} but no manifest provides it")
        bad = 1

    consumed = placeholders(inst)
    for m in manifests:
        consumed |= set(re.findall(r"\$\(([A-Z_]+)\)", open(m).read()))
    for k in sorted(cm - consumed):
        print(f"VIOLATION: {tree}: ConfigMap key {k} is consumed by nothing (dead wiring)")
        bad = 1

# The harness env's ports must be NodePorts the same overlay declares.
for tree in ("mainnet-mirror", "venue-mirror"):
    path = f"k8s/overlays/{tree}/nodeports.yaml"
    text = open(path).read()
    declared = set(re.findall(r"nodePort:\s*(\d+)", text))
    env_block = text.split("test-endpoints.env: |", 1)[-1]
    for port in sorted(set(re.findall(r"localhost:(\d+)", env_block))):
        if port not in declared:
            print(f"VIOLATION: {tree}: test-endpoints names localhost:{port}, which no Service in the overlay declares")
            bad = 1

if not bad:
    print("k8s-wiring: OK (placeholders provided, ConfigMap keys consumed, harness ports declared)")
sys.exit(bad)
PY
