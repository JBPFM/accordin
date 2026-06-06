#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

PYTHON=${PYTHON:-python3}
LOCKS=${LOCKS:-mcs_tse,mcs-tp,mcstas}
EXPERIMENTS=${EXPERIMENTS:-6,7,8,9}
RESULT_BASE=${RESULT_BASE:-experiments/results}
STAMP=${STAMP:-$(date +%Y%m%d_%H%M%S)}
SUDO_MODE=${SUDO_MODE:-auto}
BUILD_MISSING=${BUILD_MISSING:-1}

SMOKE_THREADS=${SMOKE_THREADS:-4}
SMOKE_DURATION_MS=${SMOKE_DURATION_MS:-100}
SMOKE_WARMUP_DURATION_MS=${SMOKE_WARMUP_DURATION_MS:-0}
SMOKE_REPEATS=${SMOKE_REPEATS:-1}
SMOKE_CRITICAL_NS=${SMOKE_CRITICAL_NS:-300}
SMOKE_CPU_FRACTIONS=${SMOKE_CPU_FRACTIONS:-1}

MODE=${MODE:-run}

usage() {
  cat <<'EOF'
Usage: ./extra.sh [--smoke|--dry-run]

Runs experiments 6-9 for the extra FlexGuard locks:
  mcs_tse,mcs-tp,mcstas

Environment overrides:
  LOCKS              comma-separated locks
  EXPERIMENTS        comma-separated experiment numbers, default 6,7,8,9
  RESULT_BASE        result parent directory, default experiments/results
  STAMP              output suffix, default current timestamp
  SUDO_MODE          runner sudo mode, default auto
  BUILD_MISSING      pass --build-missing to exp9, default 1
  SMOKE_THREADS      smoke thread count, default 4
  SMOKE_DURATION_MS  smoke duration, default 100
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --smoke|smoke)
      MODE=smoke
      ;;
    --dry-run|dry-run)
      MODE=dry-run
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

mkdir -p "$RESULT_BASE"

run_cmd() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  "$@"
}

has_experiment() {
  local needle="$1"
  case ",$EXPERIMENTS," in
    *,"$needle",*) return 0 ;;
    *) return 1 ;;
  esac
}

mode_args_common() {
  if [[ "$MODE" == "dry-run" ]]; then
    printf '%s\n' --dry-run
  fi
  if [[ "$MODE" == "smoke" || "$MODE" == "dry-run" ]]; then
    printf '%s\n' --skip-plots
    printf '%s\n' --duration-ms "$SMOKE_DURATION_MS"
    printf '%s\n' --warmup-duration-ms "$SMOKE_WARMUP_DURATION_MS"
    printf '%s\n' --repeats "$SMOKE_REPEATS"
    printf '%s\n' --sudo-mode none
  else
    printf '%s\n' --sudo-mode "$SUDO_MODE"
  fi
}

run_exp6() {
  local root="${EXP6_OUTPUT_ROOT:-$RESULT_BASE/experiment6_extra_$STAMP}"
  local args=(
    experiments/run_experiment_six.py
    --locks "$LOCKS"
    --output-root "$root"
  )
  if [[ "$MODE" == "smoke" || "$MODE" == "dry-run" ]]; then
    args+=(--threads "$SMOKE_THREADS")
  fi
  mapfile -t common < <(mode_args_common)
  run_cmd "$PYTHON" "${args[@]}" "${common[@]}"
}

run_exp7() {
  local root="${EXP7_OUTPUT_ROOT:-$RESULT_BASE/experiment7_extra_$STAMP}"
  local args=(
    experiments/run_experiment_seven.py
    --locks "$LOCKS"
    --output-root "$root"
  )
  if [[ "$MODE" == "smoke" || "$MODE" == "dry-run" ]]; then
    args+=(--threads "$SMOKE_THREADS" --critical-ns "$SMOKE_CRITICAL_NS")
  fi
  mapfile -t common < <(mode_args_common)
  run_cmd "$PYTHON" "${args[@]}" "${common[@]}"
}

run_exp8() {
  local root="${EXP8_OUTPUT_ROOT:-$RESULT_BASE/experiment8_extra_$STAMP}"
  local args=(
    experiments/run_experiment_eight.py
    --locks "$LOCKS"
    --output-root "$root"
  )
  if [[ "$MODE" == "smoke" || "$MODE" == "dry-run" ]]; then
    args+=(--threads "$SMOKE_THREADS" --cpu-fractions "$SMOKE_CPU_FRACTIONS")
  fi
  mapfile -t common < <(mode_args_common)
  run_cmd "$PYTHON" "${args[@]}" "${common[@]}"
}

run_exp9() {
  local root="${EXP9_OUTPUT_ROOT:-$RESULT_BASE/experiment9_extra_$STAMP}"
  local args=(
    experiments/run_experiment_nine.py
    --locks "$LOCKS"
    --output-root "$root"
  )
  if [[ "$BUILD_MISSING" == "1" ]]; then
    args+=(--build-missing)
  fi
  if [[ "$MODE" == "smoke" || "$MODE" == "dry-run" ]]; then
    args+=(--threads "$SMOKE_THREADS")
  fi
  mapfile -t common < <(mode_args_common)
  run_cmd "$PYTHON" "${args[@]}" "${common[@]}"
}

has_experiment 6 && run_exp6
has_experiment 7 && run_exp7
has_experiment 8 && run_exp8
has_experiment 9 && run_exp9
