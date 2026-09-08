#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "$(cat /sys/kernel/sched_ext/state)" != disabled ]]; then
    echo "A sched_ext scheduler is already active; run this test when it is idle." >&2
    exit 1
fi
mkdir -p "$root/target"
cc -std=c11 -O2 -Wall -Wextra -Werror -pthread \
    "$root/scripts/tests/auto_admission.c" -ldl -o "$root/target/auto_admission"
for backend in mcs_accordin_direct mcs_tas_accordin_direct; do
    timeout -k 2s 30s env \
        MCS_ACCORDIN_DIRECT_DISABLE_BPF=0 MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF=0 \
        MCS_ACCORDIN_DIRECT_STATS_ONLY=0 MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY=0 \
        ACCORDIN_DISABLE_ADMISSION=0 ACCORDIN_AUTO_ADMISSION=1 \
        ACCORDIN_CV_CUSTODY=1 ACCORDIN_CV_CUSTODY_MS=20 \
        "$root/target/auto_admission" \
        "${DIRECT_LIB_DIR:-$root/target/release}/lib${backend}.so" "$backend"
done
