#!/usr/bin/env bash
# spec-citations: every code identifier the spine doc names in backticks must
# exist in the workspace. docs/SPINE.md's tables claim behaviors by symbol
# (`Bundle`, `check_account`, `validate_trigger_side`); a rename that leaves
# the doc behind would make the claim unverifiable, so the citation itself is
# checked. The specs' own vocabulary (field and method names quoted from
# EIP-7928 / EIP-8379, which the code reaches through alloy's types) is
# allow-listed with its source, below.
set -euo pipefail
cd "$(dirname "$0")/.."

# The specs' names, quoted by the doc but not ours: EIP-7928's fields and
# engine-API names, EIP-7843's (slotNumber, SLOTNUM, the V5 methods),
# EIP-4844's (the blob constants and the engine parameter), the beacon-APIs'
# version enum (deneb/electra), the execution-apis' BlockTag and Prague
# parameter, EIP-8379's method and status, and the Hyperliquid docs'
# MAX_DECIMALS (the tick rule's constant; ours is the perp value, 6).
spec_vocabulary="block_access_index storage_reads MISSING_STATE engine_getSyncStatus MAX_DECIMALS HISTORY_PRUNE_EPOCHS blockAccessList ExecutionPayloadV4 newPayloadV5 getPayloadV6 forkchoiceUpdatedV4 slotNumber SLOTNUM MIN_EPOCHS_FOR_BLOB_SIDECARS_REQUESTS expectedBlobVersionedHashes parentBeaconBlockRoot executionRequests deneb electra BlockTag"

tokens=$(python3 -c '
import re
s = open("docs/SPINE.md").read()
for t in sorted({x.strip() for x in re.findall(r"`([^`\n]+)`", s)}):
    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)?", t):
        print(t)
')

fail=0
checked=0
for tok in $tokens; do
  name="${tok##*::}"
  case " $spec_vocabulary " in
    *" $name "*) continue ;;
  esac
  checked=$((checked + 1))
  if ! grep -rq -- "$name" crates/ bins/; then
    echo "MISSING (SPINE.md names it, the code does not): $tok"
    fail=1
  fi
done
echo "spec-citations: $checked symbols checked against crates/ + bins/"
exit $fail
