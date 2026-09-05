#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
libdir="${1:-$root/target/release}"
for backend in mcs_accordin_direct mcs_tas_accordin_direct; do
  symbols="$(nm -D --defined-only "$libdir/lib${backend}.so" | awk '{print $NF}')"
  prefixes=("$backend")
  if [[ "$backend" = mcs_accordin_direct ]]; then
    prefixes+=(mcs_tas_accordin_direct)
  fi
  for prefix in "${prefixes[@]}"; do
    for op in create destroy lock trylock unlock; do
      symbol="${prefix}_mutex_${op}"
      if ! grep -Fxq "$symbol" <<< "$symbols"; then
        echo "missing direct ABI symbol in $backend: $symbol" >&2
        exit 1
      fi
    done
  done
  if grep -Eq '^(pthread_(mutex|cond)_|accordin_dynamic_cpu_affinity_|mcs(_tas)?_accordin_direct_(cond_|writer_event_))' <<< "$symbols"; then
    echo "unexpected removed API symbol in $backend" >&2
    exit 1
  fi
done
echo "Both direct ABIs verified."
