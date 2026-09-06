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
${CC:-cc} -std=gnu11 -O2 -Wall -Werror tests/accordin.c -pthread -ldl -o obj/tests/accordin
${CXX:-c++} -std=c++17 -O2 -Wall -Werror tests/condition-variable.cpp -pthread \
    -o obj/tests/condition-variable
for backend in mcsaccordin_original mcstasaccordin_original; do
    echo "Testing $backend ($mode)"
    timeout -k 5s "${LITL_TEST_TIMEOUT:-60}s" env \
        MCS_ACCORDIN_DIRECT_DISABLE_BPF="$disable" \
        MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF="$disable" \
        MCS_ACCORDIN_DIRECT_STATS_ONLY=0 MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY=0 \
        ACCORDIN_DISABLE_ADMISSION=0 \
        bash "./lib${backend}.sh" ./obj/tests/accordin "lib${backend}.so" \
        "${LITL_TEST_THREADS:-8}" "${LITL_TEST_ITERATIONS:-10000}"
    timeout -k 5s "${LITL_TEST_TIMEOUT:-60}s" env \
        MCS_ACCORDIN_DIRECT_DISABLE_BPF="$disable" \
        MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF="$disable" \
        MCS_ACCORDIN_DIRECT_STATS_ONLY=0 MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY=0 \
        ACCORDIN_DISABLE_ADMISSION=0 \
        bash "./lib${backend}.sh" ./obj/tests/condition-variable
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
