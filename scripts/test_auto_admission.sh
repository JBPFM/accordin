#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib/direct_test.sh"
wait_idle 0
build_test auto_admission
for backend in "${backends[@]}"; do
    backend_setup "$backend"
    timeout -k 2s 30s env "${backend_env[@]}" \
        ACCORDIN_AUTO_ADMISSION=1 \
        ACCORDIN_CV_CUSTODY=1 ACCORDIN_CV_CUSTODY_MS=20 \
        "$root/target/auto_admission" "$backend_library" "$backend_prefix"
done
