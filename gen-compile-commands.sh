#!/usr/bin/env bash
# Derive clangd commands from the C build, including both backend variants.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
make "$@" all
python3 - "$@" <<'PY'
import json
import shlex
import subprocess
import sys
from pathlib import Path

root = Path.cwd()
commands = subprocess.check_output(
    ['make', '--no-print-directory', '-Bn', *sys.argv[1:], 'all'], text=True
)
entries = []
for line in commands.splitlines():
    args = shlex.split(line)
    sources = [arg for arg in args if arg in ('src/direct.c', 'src/runtime.c', 'src/bpf/main.bpf.c')]
    if not sources or not any(arg in args for arg in ('-c', '-shared')):
        continue
    flags = []
    skip = False
    for arg in args:
        if skip:
            skip = False
        elif arg == '-o':
            skip = True
        elif arg not in sources and arg not in ('-c', '-shared') and not arg.startswith(('-l', '-Wl,')):
            flags.append(arg)
    for source in sources:
        entries.append({'directory': str(root), 'file': str(root / source),
                        'arguments': flags + ['-c', source]})
if not entries:
    raise SystemExit('No C compile commands found in Makefile')
(root / 'compile_commands.json').write_text(json.dumps(entries, indent=2) + '\n')
print(f'Generated compile_commands.json ({len(entries)} entries)')
PY
