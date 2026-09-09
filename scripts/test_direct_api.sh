#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib/direct_test.sh"
mode="${1:---no-bpf}"
case "$mode" in
  --no-bpf) disable=1 ;;
  --bpf) disable=0; wait_idle 0 ;;
  *) echo "Usage: $0 [--no-bpf|--bpf]" >&2; exit 2 ;;
esac
build_test direct_api_smoke
for backend in "${backends[@]}"; do
  backend_setup "$backend" "$disable"
  timeout 30s env "${backend_env[@]}" \
    ACCORDIN_CV_CUSTODY="${ACCORDIN_CV_CUSTODY:-}" \
    ACCORDIN_CV_CUSTODY_MS="${ACCORDIN_CV_CUSTODY_MS:-5}" \
    "$root/target/direct_api_smoke" "$backend_library" "$backend_prefix"
done
