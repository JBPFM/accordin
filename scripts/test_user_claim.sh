#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib/direct_test.sh"

build_test user_claim

context=""
line=""
declare -A count

# One scenario under one backend. The counters are printed by the runtime when
# the scheduler detaches, so they can only be read from the finished process.
run() {
    local backend="$1" mode="$2" claim="$3" rseq="$4" log token
    context="$backend $mode claim=$claim rseq=$rseq"
    log="$root/target/user_claim.$backend.$mode.claim$claim.rseq$rseq.log"
    backend_setup "$backend"
    wait_idle 60
    if ! timeout -k 2s 90s env "${backend_env[@]}" \
        ACCORDIN_CV_COUNTERS=1 \
        ACCORDIN_USER_CLAIM="$claim" ACCORDIN_USER_RSEQ="$rseq" \
        "$root/target/user_claim" "$backend_library" "$backend_prefix" "$mode" \
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
    count=()
    for token in ${line#*]}; do
        count["${token%%=*}"]="${token#*=}"
    done
    echo "$context: $line"
}

# Assertions are written in the names the line carries, so neither their order
# nor how many of them there are matters. A name the runtime no longer prints
# leaves the lookup unbound, which stops the script rather than reading as zero.
expect() {
    local resolved="$1" name
    for name in $(grep -oE '[a-z_]+' <<<"$1"); do
        resolved="${resolved//"$name"/${count[$name]}}"
    done
    if (( resolved )); then
        return 0
    fi
    printf 'FAIL %s: expected %s\n  %s\n' "$context" "$1" "$line" >&2
    exit 1
}

for backend in "${backends[@]}"; do
    # Two threads with a pause between episodes keep both queues empty, so a
    # contender takes its own slot. The first contention on a shared CPU has to
    # queue once and a demand blip adds a few more, so the shortcut is required
    # to carry the bulk rather than all of it.
    run "$backend" low 1 1
    expect "claims + renews > 0"
    expect "queued < (claims + renews) / 2"
    expect "slots_left == 0"
    # Slots are sticky, so the entry a worker's last contended acquisition took
    # is still in the table when the thread dies. The tick may adopt that entry
    # and release it first, so the sweep is one of two legal outcomes; the table
    # has to end empty either way.
    expect "swept > 0 || adopted > 0"

    # The same shape without a restartable sequence puts the claim on the
    # exchange, which reads the CPU back afterwards and withdraws a ticket a
    # migration left on the wrong entry.
    run "$backend" low 1 0
    expect "claims + renews > 0"
    expect "slots_left == 0"

    # Four threads without a pause keep the bank non-empty, which is where the
    # queue order has to be respected.
    run "$backend" overload 1 1
    expect "queued > 0"
    expect "slots_left == 0"

    # The same load with the shortcut off is the path the scheduler always took.
    run "$backend" overload 0 1
    expect "claims == 0 && renews == 0 && undone == 0"
    expect "queued > 0"
done
echo "user claim ok: low contention with and without the sequence, overload with and without the switch"
