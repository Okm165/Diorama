#!/usr/bin/env bash
# Fitness function: docker-members.
#
# The image build's dependency layer copies every workspace member's manifest
# (cargo resolves the whole workspace before building one package) and creates
# a stub source for each. A member added or renamed without updating the
# Dockerfile breaks `make build` at COPY — minutes into a podman build, with a
# message about a path that no longer exists. Measured drift this gate exists
# for: the ethereum/hyperliquid crate names survived the rename to
# mainnet/venue, and the ast-scope bin was missing entirely.
set -euo pipefail
cd "$(dirname "$0")/.."

members=$(for d in crates/*/ bins/*/; do [ -f "${d}Cargo.toml" ] && echo "${d%/}"; done | sort)
copied=$(grep -oE '^COPY (crates|bins)/[A-Za-z0-9_-]+/Cargo\.toml' Dockerfile | awk '{print $2}' | sed 's#/Cargo.toml##' | sort)
stubbed=$(grep -oE 'for d in [^;]+; do' Dockerfile | sed 's/^for d in //; s/; do$//' | tr ' ' '\n' | sort -u)

bad=0
while IFS= read -r m; do
  [ -n "$m" ] || continue
  grep -qx "$m" <<<"$copied"  || { echo "VIOLATION: workspace member $m has no COPY line in the Dockerfile"; bad=1; }
  grep -qx "$m" <<<"$stubbed" || { echo "VIOLATION: workspace member $m has no stub entry in the Dockerfile"; bad=1; }
done <<<"$members"
while IFS= read -r c; do
  [ -n "$c" ] || continue
  grep -qx "$c" <<<"$members" || { echo "VIOLATION: the Dockerfile copies $c, which is not a workspace member"; bad=1; }
done <<<"$copied"
while IFS= read -r s; do
  [ -n "$s" ] || continue
  grep -qx "$s" <<<"$members" || { echo "VIOLATION: the Dockerfile stubs $s, which is not a workspace member"; bad=1; }
done <<<"$stubbed"

if [ "$bad" -eq 0 ]; then
  echo "docker-members: OK ($(grep -c . <<<"$members") members, every one COPYed and stubbed)"
fi
exit $bad
