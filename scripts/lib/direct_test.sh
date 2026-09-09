# SPDX-License-Identifier: GPL-2.0-only
# Common ground for the tests that drive a direct backend: where the libraries
# are, how a test program is built, what every backend is run with, and how to
# tell that no other sched_ext scheduler is in the way. Sourced by callers that
# set -euo pipefail; nothing here changes shell options.

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
libdir="${DIRECT_LIB_DIR:-$root/target/release}"
backends=(mcs_accordin_direct mcs_tas_accordin_direct)

# One test program from the source of the same name under scripts/tests.
build_test() {
    mkdir -p "$root/target"
    cc -std=c11 -O2 -Wall -Wextra -Werror -pthread \
        "$root/scripts/tests/$1.c" -ldl -o "$root/target/$1"
}

# The library, the symbol prefix and the settings one backend runs under. The
# runtime reads its switches by prefix, so both names are always passed and the
# backend in use picks up its own.
backend_setup() {
    local backend="$1" disable="${2:-0}"
    backend_library="$libdir/lib$backend.so"
    backend_prefix="$backend"
    backend_env=(
        "MCS_ACCORDIN_DIRECT_DISABLE_BPF=$disable"
        "MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF=$disable"
        MCS_ACCORDIN_DIRECT_STATS_ONLY=0
        MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY=0
        ACCORDIN_DISABLE_ADMISSION=0
    )
}

# A test attaches its own scheduler, so any other one has to be gone first. The
# argument is how many seconds to wait for that; zero refuses to wait at all. A
# host that serializes scheduler loads through a lock takes it around the whole
# script rather than inside it.
wait_idle() {
    local limit="${1:-0}" waited=0
    while [[ "$(cat /sys/kernel/sched_ext/state)" != disabled ]]; do
        if (( waited >= limit * 10 )); then
            echo "A sched_ext scheduler is active; run this test when it is idle." >&2
            exit 1
        fi
        sleep 0.1
        waited=$((waited + 1))
    done
}
