#!/usr/bin/env bash
# heredoc-audit: the scripts that generate config files must not lose text
# silently. An unquoted heredoc expands its body: a backtick in a comment
# runs as a command (measured: `balanceAndBlacklistStates` in
# venue-selftest-local's generated instance.toml printed "command not found"
# and the word vanished from the file), and an unset `$var` expands to
# nothing. Both are audited here; a quoted delimiter (`<<'EOF'`) opts out.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import pathlib
import re
import sys

bad = 0
for path in sorted(pathlib.Path("scripts").glob("*.sh")):
    text = path.read_text()
    # Assignments may share a line (`A=1; B=2`); `export` counts too.
    defined = set(re.findall(r"(?:^|[;\s])(?:export\s+)?([A-Za-z_]\w*)=", text))
    in_here = None
    for i, line in enumerate(text.splitlines(), 1):
        if in_here is None:
            m = re.search(r"<<-?(\w+)", line)
            if m:
                after = line.split("<<", 1)[1].lstrip("-")
                in_here = None if after[:1] in ("'", '"') else m.group(1)
        elif line.strip() == in_here:
            in_here = None
        else:
            if "`" in line:
                print(
                    f"VIOLATION: {path}:{i}: a backtick in <<{in_here} runs as a "
                    f"command: {line.strip()}"
                )
                bad = 1
            for tok in re.findall(r"\$\{?(\w+)\}?", line):
                if tok not in defined and not tok.isdigit():
                    print(
                        f"VIOLATION: {path}:{i}: ${tok} is not set in the script "
                        f"(expands to empty): {line.strip()}"
                    )
                    bad = 1
if bad == 0:
    print("heredoc-audit: OK (no unquoted heredoc loses text: no backticks, no unset expansions)")
sys.exit(bad)
PY
