#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mode="${1:---no-bpf}"
case "$mode" in
  --no-bpf) disable=1 ;;
  --bpf)
    disable=0
    if [[ "$(cat /sys/kernel/sched_ext/state)" != disabled ]]; then
      echo "A sched_ext scheduler is already active; run the BPF smoke test when it is idle." >&2
      exit 1
    fi
    ;;
  *) echo "Usage: $0 [--no-bpf|--bpf]" >&2; exit 2 ;;
esac
mkdir -p "$root/target"
cc -std=c11 -O2 -Wall -Wextra -Werror -pthread \
  "$root/scripts/tests/direct_api_smoke.c" -ldl -o "$root/target/direct_api_smoke"
for backend in mcs_accordin_direct mcs_tas_accordin_direct; do
  timeout 30s env \
    MCS_ACCORDIN_DIRECT_DISABLE_BPF="$disable" \
    MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF="$disable" \
    MCS_ACCORDIN_DIRECT_STATS_ONLY=0 MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY=0 \
    ACCORDIN_DISABLE_ADMISSION=0 \
    "$root/target/direct_api_smoke" "${DIRECT_LIB_DIR:-$root/target/release}/lib${backend}.so" "$backend"
done
