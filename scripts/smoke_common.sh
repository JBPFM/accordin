#!/usr/bin/env bash
# Shared setup for the direct and fullhook smoke scripts: repository root,
# mode parsing with the sched_ext idle guard, the environment both runs use,
# and reuse of the smoke binaries the build already produced.
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bindir="${SMOKE_BIN_DIR:-$root/target/release}"
smoke_env=()

smoke_setup() {
  local disable
  case "${1:---no-bpf}" in
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
  smoke_env=(
    MCS_ACCORDIN_DIRECT_DISABLE_BPF="$disable"
    MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF="$disable"
    MCS_ACCORDIN_DIRECT_STATS_ONLY=0
    MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY=0
    ACCORDIN_DISABLE_ADMISSION=0
  )
}

# Compile only when the build has not already produced a current binary.
smoke_binary() {
  local name="$1" source="$2"
  shift 2
  local binary="$bindir/$name"
  if [[ ! -x "$binary" || "$source" -nt "$binary" ]]; then
    mkdir -p "$bindir"
    "$@" "$source" -ldl -o "$binary"
  fi
  printf '%s\n' "$binary"
}
