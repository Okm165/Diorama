#!/usr/bin/env bash
# Regenerate docs/img/*.png - the rendered views of the ASCII diagrams.
# The docs' fenced art blocks are the single source; pango-view rasterizes
# them with a real monospace font. Run after editing any diagram.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v pango-view >/dev/null; then
  echo "pango-view not found (install pango / pango-tools)" >&2
  exit 1
fi

mkdir -p docs/img
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

render() { # <source.md> <marker in block> <out.png>
  python3 - "$1" "$2" "$3" "$tmp" <<'PY'
import re, subprocess, sys, pathlib
src, marker, out, tmp = sys.argv[1:5]
s = pathlib.Path(src).read_text()
for m in re.finditer(r"```\n(.*?)```", s, re.S):
    if marker in m.group(1):
        txt = pathlib.Path(tmp) / "block.txt"
        txt.write_text(m.group(1))
        subprocess.run(["pango-view", "--no-display", "--font", "DejaVu Sans Mono 14",
                        "--margin", "8", "--background", "white", "-o", out, str(txt)],
                       check=True)
        print(f"  {out}")
        break
else:
    raise SystemExit(f"no art block matching {marker!r} in {src}")
PY
}

render README.md             "reth + Lighthouse"     docs/img/readme-overview.png
render docs/HOW-IT-WORKS.md  "Lighthouse"            docs/img/how-topology.png
render docs/HOW-IT-WORKS-VENUE.md "the HL public API" docs/img/how-topology-venue.png
render docs/HOW-IT-WORKS.md  "parent diff for key K" docs/img/how-cow.png
render docs/ARCHITECTURE.md  "anvil T"               docs/img/arch-data-flow.png
render docs/ARCHITECTURE.md  "no prepend"            docs/img/arch-prepend.png
render docs/SPINE.md         "the spine (crates/devnet)" docs/img/spine-overview.png
echo "7 diagrams regenerated from the docs"
