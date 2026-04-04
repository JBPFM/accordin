#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

"$SCRIPT_DIR/gen-compile-commands.sh" >/dev/null
"$SCRIPT_DIR/update-clangd.sh" >/dev/null

ROOT="$SCRIPT_DIR" python3 - <<'PY'
import json
import os
from pathlib import Path

root = Path(os.environ["ROOT"])
entries = json.loads((root / "compile_commands.json").read_text())
clangd = (root / ".clangd").read_text()

main_entries = [entry for entry in entries if entry["file"].endswith("/src/bpf/main.bpf.c")]
flexguard_entries = [entry for entry in entries if entry["file"].endswith("/src/bpf/flexguard.bpf.c")]

assert any("/build/mcs_tas_simple-" in entry["command"] for entry in main_entries), (
    "main.bpf.c should be tracked with mcs_tas_simple generated headers"
)
assert any("/build/libflexguard-" in entry["command"] for entry in main_entries), (
    "main.bpf.c should be tracked with libflexguard generated headers"
)
assert any("/build/libflexguard-" in entry["command"] for entry in flexguard_entries), (
    "flexguard.bpf.c should be tracked with libflexguard generated headers"
)
assert "scx_utils-bpf_h" not in clangd, ".clangd should not pin a single generated-header directory"
PY
