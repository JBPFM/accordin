#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: ./debug_dedup.sh LOCK [run_experiment_three_dedup.py args...]

Run experiment three's dedup benchmark at 96 threads for a single lock.

Examples:
  ./debug_dedup.sh mcs_tas_accordin
  ./debug_dedup.sh flexguard --repeats 3
  ./debug_dedup.sh accordin --force --output-root experiments/results/debug_dedup_accordin

Environment:
  PYTHON                    Python interpreter to use.
  DEBUG_DEDUP_REPEATS       Repeat count when --repeats is not passed. Default: 1.
  DEBUG_DEDUP_OUTPUT_ROOT   Output root when --output-root is not passed.
EOF
}

if [[ $# -lt 1 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

lock="$1"
shift

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
python="${PYTHON:-}"
if [[ -z "$python" ]]; then
  if [[ -x "$script_dir/bench/mutexbench/.venv/bin/python" ]]; then
    python="$script_dir/bench/mutexbench/.venv/bin/python"
  else
    python="python3"
  fi
fi

safe_lock="$(printf '%s' "$lock" | tr -c '[:alnum:]_.-' '_')"
timestamp="$(date +%Y%m%d_%H%M%S)"
output_root="${DEBUG_DEDUP_OUTPUT_ROOT:-$script_dir/experiments/results/debug_dedup_${safe_lock}_96_${timestamp}}"
repeats="${DEBUG_DEDUP_REPEATS:-1}"

output_args=(--output-root "$output_root")
repeat_args=(--repeats "$repeats")
for arg in "$@"; do
  case "$arg" in
    --output-root|--output-root=*)
      output_args=()
      ;;
    --repeats|--repeats=*)
      repeat_args=()
      ;;
  esac
done

exec "$python" "$script_dir/experiments/run_experiment_three_dedup.py" \
  --locks "$lock" \
  --threads 96 \
  "${repeat_args[@]}" \
  "${output_args[@]}" \
  "$@"
