#!/usr/bin/env bash
# Create the CCTP bridge's relay-wallet secret (idempotent). The value is a
# dedicated hex key - never the deployer's - and on the devnet the bridge
# self-funds the wallet at boot (anvil stipend), so it needs no balance yet.
set -euo pipefail

NS="${NS:-venue-mirror}"

if kubectl -n "$NS" get secret cctp-relay-key >/dev/null 2>&1; then
  echo "secret cctp-relay-key already exists in $NS (left untouched)"
  exit 0
fi

KEY="${CCTP_RELAY_KEY:?set CCTP_RELAY_KEY - the dedicated relay wallet hex key}"
kubectl -n "$NS" create secret generic cctp-relay-key --from-literal=relay-key="$KEY" >/dev/null
echo "created secret cctp-relay-key in $NS"
