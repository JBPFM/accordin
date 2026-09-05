#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
libdir="${1:-$root/target/release}"
hook_symbols="$(printf '%s\n' \
  pthread_cond_broadcast pthread_cond_destroy pthread_cond_init \
  pthread_cond_signal pthread_cond_timedwait pthread_cond_wait \
  pthread_mutex_destroy pthread_mutex_init pthread_mutex_lock \
  pthread_mutex_timedlock pthread_mutex_trylock pthread_mutex_unlock | sort)"

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

for hook in mcs_accordin_fullhook mcs_tas_accordin_fullhook; do
  symbols="$(nm -D --defined-only "$libdir/lib${hook}.so" | awk '{print $NF}' | sort)"
  if [[ "$symbols" != "$hook_symbols" ]]; then
    echo "unexpected exported symbol set in $hook:" >&2
    diff <(echo "$hook_symbols") <(echo "$symbols") >&2 || true
    exit 1
  fi
  if grep -Eq '^mcs(_tas)?_accordin_direct_' <<< "$symbols"; then
    echo "hook library $hook must not export the direct ABI" >&2
    exit 1
  fi
done
echo "All four libraries verified."
