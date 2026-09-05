#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/smoke_common.sh"
smoke_setup "${1:---no-bpf}"
programs=(
  "$(smoke_binary fullhook_smoke "$root/scripts/tests/fullhook_smoke.c" \
    "${SMOKE_CC:-cc}" -std=c11 -O2 -Wall -Wextra -Werror -pthread)"
  "$(smoke_binary fullhook_cxx_smoke "$root/scripts/tests/fullhook_cxx_smoke.cc" \
    "${SMOKE_CXX:-c++}" -std=c++17 -O2 -Wall -Wextra -Werror -pthread)"
)
for hook in mcs_accordin_fullhook mcs_tas_accordin_fullhook; do
  for program in "${programs[@]}"; do
    timeout 60s env "${smoke_env[@]}" \
      LD_PRELOAD="${FULLHOOK_LIB_DIR:-$root/target/release}/lib${hook}.so" "$program"
  done
done
