#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
libdir="${DIRECT_LIB_DIR:-$root/target/release}"
cpus="${ACCORDIN_CLAIM_CPUS:-0,1}"

mkdir -p "$root/target"
cc -std=c11 -O2 -Wall -Wextra -Werror -pthread \
    "$root/scripts/tests/user_claim.c" -ldl -o "$root/target/user_claim"

context=""
line=""
renews=0 claims=0 undone=0 queued=0 adopted=0 swept=0 slots_left=0 demand=0

# Each scenario attaches its own scheduler, so the previous one has to be gone.
# A host that serializes scheduler loads through a lock takes it around the
# whole script rather than inside it.
wait_idle() {
    local attempt
    for attempt in $(seq 1 600); do
        [[ "$(cat /sys/kernel/sched_ext/state)" == disabled ]] && return 0
        sleep 0.1
    done
    echo "A sched_ext scheduler stayed active; run this test when it is idle." >&2
    exit 1
}

# One scenario under one backend. The counters are printed by the runtime when
# the scheduler detaches, so they can only be read from the finished process.
run() {
    local backend="$1" mode="$2" claim="$3" log
    context="$backend $mode ACCORDIN_USER_CLAIM=$claim"
    log="$root/target/user_claim.$backend.$mode.claim$claim.log"
    wait_idle
    if ! timeout -k 2s 90s taskset -c "$cpus" env \
        MCS_ACCORDIN_DIRECT_DISABLE_BPF=0 MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF=0 \
        MCS_ACCORDIN_DIRECT_STATS_ONLY=0 MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY=0 \
        ACCORDIN_DISABLE_ADMISSION=0 ACCORDIN_CV_COUNTERS=1 \
        ACCORDIN_USER_CLAIM="$claim" \
        "$root/target/user_claim" "$libdir/lib$backend.so" "$backend" "$mode" \
        2>"$log"; then
        cat "$log" >&2
        echo "FAIL $context: the run did not complete" >&2
        exit 1
    fi
    line="$(grep '^\[accordin_claim\]' "$log" | tail -1 || true)"
    if [[ -z "$line" ]]; then
        cat "$log" >&2
        echo "FAIL $context: no [accordin_claim] line" >&2
        exit 1
    fi
    read -r _ renews claims undone queued adopted swept slots_left demand <<<"$line"
    renews=${renews#*=} claims=${claims#*=} undone=${undone#*=} queued=${queued#*=}
    adopted=${adopted#*=} swept=${swept#*=} slots_left=${slots_left#*=}
    demand=${demand#*=}
    echo "$context: $line"
}

expect() {
    if (( $1 )); then
        return 0
    fi
    printf 'FAIL %s: expected %s\n  %s\n' "$context" "$1" "$line" >&2
    exit 1
}

for backend in mcs_accordin_direct mcs_tas_accordin_direct; do
    # Two threads with a pause between episodes keep both queues empty, so a
    # contender takes its own slot. The first contention on a shared CPU has to
    # queue once and a demand blip adds a few more, so the shortcut is required
    # to carry the bulk rather than all of it.
    run "$backend" low 1
    expect "claims + renews > 0"
    expect "queued < (claims + renews) / 2"
    expect "slots_left == 0"

    # Four threads without a pause keep the bank non-empty, which is where the
    # queue order has to be respected.
    run "$backend" overload 1
    expect "queued > 0"
    expect "slots_left == 0"

    # The same load with the shortcut off is the path the scheduler always took.
    run "$backend" overload 0
    expect "claims == 0 && renews == 0 && undone == 0"
    expect "queued > 0"

    # A thread leaving on a slot it took itself. The tick may adopt the entry
    # and release it before the thread dies, so the sweep is one of two legal
    # outcomes; the table has to end empty either way.
    run "$backend" exit 1
    expect "slots_left == 0"
    expect "swept > 0 || adopted > 0"
done
echo "user claim ok: low contention, overload with and without the switch, exit reclaim"
