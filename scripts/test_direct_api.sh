#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/smoke_common.sh"
smoke_setup "${1:---no-bpf}"
smoke="$(smoke_binary direct_api_smoke "$root/scripts/tests/direct_api_smoke.c" \
  "${SMOKE_CC:-cc}" -std=c11 -O2 -Wall -Wextra -Werror -pthread)"
for backend in mcs_accordin_direct mcs_tas_accordin_direct; do
  timeout 30s env "${smoke_env[@]}" \
    "$smoke" "${DIRECT_LIB_DIR:-$root/target/release}/lib${backend}.so" "$backend"
done
