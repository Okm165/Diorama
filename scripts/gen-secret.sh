#!/usr/bin/env bash
# Create the shared reth<->lighthouse engine-API JWT secret (idempotent).
set -euo pipefail

NS="${NS:-mainnet-mirror}"

if kubectl -n "$NS" get secret reth-jwt >/dev/null 2>&1; then
  echo "secret reth-jwt already exists in $NS (left untouched)"
  exit 0
fi

JWT="$(openssl rand -hex 32)"
kubectl -n "$NS" create secret generic reth-jwt --from-literal=jwt.hex="$JWT" >/dev/null
echo "created secret reth-jwt in $NS"
