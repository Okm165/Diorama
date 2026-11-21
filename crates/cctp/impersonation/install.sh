#!/usr/bin/env bash
# Install the CCTP V2 impersonation at a chain's transmitter address.
#
# The mirror chain presents the same observable contract as a production CCTP
# V2 deployment (getters, packed messages, attester semantics). This script is
# the mechanical half: write the compiled code at the deployment's transmitter
# address, clear the previous fixture's storage, initialize the deployment
# facts, and verify the surface a relay asserts at boot.
#
# Every input is an environment fact — nothing about a host lives here:
#
#   RPC_URL              the chain's operator RPC (cheatcodes must be allowed)
#   TARGET_ADDRESS       the transmitter address the deployment already records
#   LOCAL_DOMAIN         the chain's CCTP domain (0 Ethereum, 19 HyperEVM, …)
#   LOCAL_TOKEN          the token depositForBurn burns and delivery pays out
#   ATTESTER_MANAGER     the address a relay impersonates to install its attester
#   PEER_DOMAIN          the opposite chain's domain
#   PEER_MESSENGER       the opposite chain's messenger address (bytes32-encoded)
#   SENDER               an account the RPC accepts without a key (auto-impersonate)
#
# Usage: RPC_URL=… TARGET_ADDRESS=… scripts/install.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FOUNDRY_BIN="${FOUNDRY_BIN:-$HOME/.foundry/bin}"
export PATH="$FOUNDRY_BIN:$PATH"

for var in RPC_URL TARGET_ADDRESS LOCAL_DOMAIN LOCAL_TOKEN ATTESTER_MANAGER PEER_DOMAIN PEER_MESSENGER SENDER; do
  [ -n "${!var:-}" ] || { echo "ERROR: $var is unset" >&2; exit 2; }
done

echo "== build"
forge build --root "$HERE" >/dev/null
CODE="$(forge inspect --root "$HERE" MirrorCctpV2 deployedBytecode)"
case "$CODE" in 0x*) ;; *) echo "ERROR: no deployed bytecode" >&2; exit 1 ;; esac

echo "== clear the previous fixture's storage (slots 0..16)"
ZERO="0x$(printf '0%.0s' $(seq 64))"
for slot in $(seq 0 16); do
  cast rpc anvil_setStorageAt "$TARGET_ADDRESS" "0x$(printf '%064x' "$slot")" "$ZERO" --rpc-url "$RPC_URL" >/dev/null
done

echo "== write the impersonation code at $TARGET_ADDRESS"
cast rpc anvil_setCode "$TARGET_ADDRESS" "$CODE" --rpc-url "$RPC_URL" >/dev/null

echo "== initialize the deployment facts"
cast send "$TARGET_ADDRESS" \
  "initialize(uint32,address,address,uint32[],bytes32[])" \
  "$LOCAL_DOMAIN" "$LOCAL_TOKEN" "$ATTESTER_MANAGER" \
  "[$PEER_DOMAIN]" "[$PEER_MESSENGER]" \
  --unlocked --from "$SENDER" --rpc-url "$RPC_URL" >/dev/null

echo "== verify the surface a relay asserts at boot"
check() { # check <label> <want> <signature> [args…]
  local label="$1" want="$2" sig="$3"; shift 3
  local got
  got="$(cast call "$TARGET_ADDRESS" "$sig" "$@" --rpc-url "$RPC_URL")"
  if [ "$(echo "$got" | tr 'A-F' 'a-f')" != "$(echo "$want" | tr 'A-F' 'a-f')" ]; then
    echo "FAIL $label: want $want, got $got" >&2
    exit 1
  fi
  printf '  %-26s %s\n' "$label" "$got"
}

check "localDomain()"            "$LOCAL_DOMAIN"                     "localDomain()(uint32)"
check "version()"                "1"                                "version()(uint32)"
check "messageBodyVersion()"     "1"                                "messageBodyVersion()(uint32)"
check "localMessageTransmitter()" "$TARGET_ADDRESS"                 "localMessageTransmitter()(address)"
check "attesterManager()"        "$ATTESTER_MANAGER"                "attesterManager()(address)"
check "signatureThreshold()"     "1"                                "signatureThreshold()(uint256)"
check "remoteTokenMessengers($PEER_DOMAIN)" "$PEER_MESSENGER"       "remoteTokenMessengers(uint32)(bytes32)" "$PEER_DOMAIN"
echo "installed: $TARGET_ADDRESS"
