#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
mode="${1:---no-bpf}"
case "$mode" in
    --no-bpf) disable=1 ;;
    --bpf)
        disable=0
        if [[ "$(cat /sys/kernel/sched_ext/state)" != disabled ]]; then
            echo "A sched_ext scheduler is already active; run tests when it is idle." >&2
            exit 1
        fi
        ;;
    *) echo "Usage: $0 [--no-bpf|--bpf]" >&2; exit 2 ;;
esac
mkdir -p obj/tests
root="${ACCORDIN_ROOT:-$(cd ../.. && pwd)}"
libdir="${ACCORDIN_LIB_DIR:-$root/target/release}"

# Under the scheduler an untimed wait is held until it is notified or its
# custody expires. The short limit keeps expiry close enough to cover whatever
# the flush does not reach; the long one puts expiry out of reach, so the
# release deadlines the suite asserts can only be met by the flush itself.
if [[ "$disable" == 0 ]]; then
    counters="${ACCORDIN_CV_COUNTERS:-1}"
    if [[ -n "${ACCORDIN_CV_CUSTODY_MS:-}" ]]; then
        custody_limits=("$ACCORDIN_CV_CUSTODY_MS")
    else
        custody_limits=(1 1000)
    fi
else
    counters="${ACCORDIN_CV_COUNTERS:-}"
    custody_limits=("${ACCORDIN_CV_CUSTODY_MS:-}")
fi
custody_ms="${custody_limits[0]}"

# Admission carries the request the scheduler reads, so no wait can be held
# without it.
if [[ "${ACCORDIN_DISABLE_ADMISSION:-0}" =~ ^([1]|[Tt]rue|[Yy]es|[Oo]n|TRUE|YES|ON)$ ]]; then
    admission=0
else
    admission=1
fi

# The runtime reads custody as enabled unless it is explicitly denied.
if [[ "${ACCORDIN_CV_CUSTODY:-}" =~ ^([0]|[Ff]alse|[Nn]o|[Oo]ff|FALSE|NO|OFF)$ ]]; then
    custody=0
else
    custody=1
fi

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

run_case() {
    local status=0
    set +e
    timeout -k 5s "${LITL_TEST_TIMEOUT:-60}s" env \
        LD_PRELOAD="${preload:-}" \
        MCS_ACCORDIN_DIRECT_DISABLE_BPF="$disable" \
        MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF="$disable" \
        MCS_ACCORDIN_DIRECT_STATS_ONLY=0 MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY=0 \
        ACCORDIN_DISABLE_ADMISSION="$((1 - admission))" \
        ACCORDIN_CV_CUSTODY="${ACCORDIN_CV_CUSTODY:-}" \
        ACCORDIN_CV_CUSTODY_MS="$custody_ms" \
        ACCORDIN_CV_COUNTERS="$counters" \
        ACCORDIN_OWN_LIMIT="${ACCORDIN_OWN_LIMIT:-}" \
        ACCORDIN_OWN_SLACK_US="${ACCORDIN_OWN_SLACK_US:-}" \
        ACCORDIN_GROUP_SIZE="${ACCORDIN_GROUP_SIZE:-}" \
        "$@" >"$log" 2>&1
    status=$?
    set -e
    cat "$log"
    [[ $status -eq 0 ]] || exit "$status"
}

# Every park leaves custody exactly once, so the totals must add up. The first
# argument is the number of parks the case is expected to reach at least.
check_counters() {
    [[ "$disable" == 0 && "$counters" == 1 ]] || return 0
    local least="${1:-0}" line
    # A run without admission publishes no request for the scheduler to read.
    [[ "$admission" == 1 ]] || least=0
    line="$(grep -m1 '^\[accordin_cv\]' "$log" || true)"
    if [[ -z "$line" ]]; then
        echo "missing [accordin_cv] counters" >&2
        exit 1
    fi
    awk -v line="$line" -v custody="$custody" -v least="$least" 'BEGIN {
        split(line, fields, " ");
        for (i in fields) {
            split(fields[i], pair, "=");
            value[pair[1]] = pair[2] + 0;
        }
        total = value["flushed"] + value["expired"] + value["drained"] + value["parked_now"];
        if (value["parked"] != total) {
            printf "custody counters do not balance: %s\n", line > "/dev/stderr";
            exit 1;
        }
        if (!custody) {
            if (value["parked"] != 0) {
                printf "custody is off but waits were parked: %s\n", line > "/dev/stderr";
                exit 1;
            }
            exit 0;
        }
        if (value["parked"] < least) {
            printf "no wait reached custody: %s\n", line > "/dev/stderr";
            exit 1;
        }
        # Notification hands the wait to the flush, so a case that parks at all
        # must release through it rather than through expiry alone.
        if (least > 0 && value["flushed"] < 1) {
            printf "no wait left custody through a flush: %s\n", line > "/dev/stderr";
            exit 1;
        }
    }'
}

${CC:-cc} -std=gnu11 -O2 -Wall -Werror tests/accordin.c -pthread -ldl -o obj/tests/accordin
${CXX:-c++} -std=c++17 -O2 -Wall -Werror tests/condition-variable.cpp -pthread \
    -o obj/tests/condition-variable
${CC:-cc} -std=gnu11 -O2 -Wall -Werror -fPIC -shared tests/early-lock.c -ldl \
    -o obj/tests/libearlylock.so
for backend in mcsaccordin_original mcstasaccordin_original; do
    echo "Testing $backend ($mode)"
    for custody_ms in "${custody_limits[@]}"; do
        if [[ "$disable" == 0 ]]; then
            echo "  custody limit ${custody_ms} ms"
        fi
        run_case bash "./lib${backend}.sh" ./obj/tests/accordin "lib${backend}.so" \
            "${LITL_TEST_THREADS:-8}" "${LITL_TEST_ITERATIONS:-10000}"
        check_counters 1
    done
    custody_ms="${custody_limits[0]}"
    # A thread whose first lock lands while the scheduler library is still
    # loading has no registry to publish its admission word into yet. Left
    # unpublished it reads as idle to the scheduler, is never admitted, and
    # waits for a grant that cannot arrive.
    if [[ "$disable" == 0 ]]; then
        echo "  lock taken during the library load"
        preload="$PWD/obj/tests/libearlylock.so" \
        run_case bash "./lib${backend}.sh" ./obj/tests/accordin "lib${backend}.so" \
            "${LITL_TEST_THREADS:-8}" "${LITL_TEST_ITERATIONS:-10000}"
        check_counters 1
    fi
    # A CPU allowed a single grant from its own queue has to serve its topology
    # group for every other grant, so this case drives the group probe.
    if [[ "$disable" == 0 ]]; then
        echo "  own-queue grants bounded at one"
        ACCORDIN_OWN_LIMIT=1 \
        run_case bash "./lib${backend}.sh" ./obj/tests/accordin "lib${backend}.so" \
            "${LITL_TEST_THREADS:-8}" "${LITL_TEST_ITERATIONS:-10000}"
        check_counters 1
    fi
    # With no slack and no count bound a CPU serves its own queue only while
    # its head is the oldest of the group, so this case drives the age order.
    if [[ "$disable" == 0 ]]; then
        echo "  strict age order inside the group"
        ACCORDIN_OWN_SLACK_US=0 ACCORDIN_OWN_LIMIT=0 \
        run_case bash "./lib${backend}.sh" ./obj/tests/accordin "lib${backend}.so" \
            "${LITL_TEST_THREADS:-8}" "${LITL_TEST_ITERATIONS:-10000}"
        check_counters 1
    fi
    # A single worker may find its predicate already true and never wait, so
    # this case only has to balance.
    run_case bash "./lib${backend}.sh" ./obj/tests/condition-variable
    check_counters
    if [[ "$disable" == 0 && "$(cat /sys/kernel/sched_ext/state)" != disabled ]]; then
        echo "sched_ext is still active after $backend exited" >&2
        exit 1
    fi
done

# This focused test is independent of scheduling and also checks NDEBUG builds.
for backend in mcs mcstas; do
    if [[ "$backend" == mcs ]]; then
        define=MCSACCORDIN
        direct=mcs_accordin_direct
    else
        define=MCSTASACCORDIN
        direct=mcs_tas_accordin_direct
    fi
    ${CC:-cc} -std=gnu11 -O2 -Wall -Werror -DNDEBUG -D"$define" \
        -DFCT_LINK_SUFFIX=test -Iinclude -I"$root/include" \
        tests/no-shadow.c src/accordin-cond.c -L"$libdir" \
        -Wl,-z,now -Wl,-rpath,"$libdir" -l"$direct" -pthread \
        -o "obj/tests/no-shadow-$backend"
    timeout -k 5s 10s env MCS_ACCORDIN_DIRECT_DISABLE_BPF=1 \
        MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF=1 "./obj/tests/no-shadow-$backend"
done
