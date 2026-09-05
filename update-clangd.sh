#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
bash gen-compile-commands.sh "$@"
cat > .clangd <<'CONFIG'
CompileFlags:
  CompilationDatabase: .
Index:
  Background: Build
CONFIG
