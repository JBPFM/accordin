#!/usr/bin/env bash
# Exercises the baseline algorithms that share the direct front end.
#
# With no argument it runs every algorithm that needs no BPF scheduler, so it
# is safe on a machine where sched_ext belongs to someone else. FlexGuard
# attaches its own BPF program and is therefore only run when named
# explicitly.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
mkdir -p obj/tests
${CC:-cc} -std=gnu11 -O2 -Wall -Werror tests/direct.c -pthread -ldl \
    -o obj/tests/direct
${CXX:-c++} -std=c++17 -O2 -Wall -Werror tests/condition-variable.cpp -pthread \
    -o obj/tests/condition-variable

# GCR's admission thresholds, its head back-off and its environment overrides
# are checked against the lock alone, without the interposition layer.
${CC:-cc} -std=gnu11 -O2 -Wall -Werror -Iinclude -c src/gcrmcs.c \
    -o obj/tests/gcrmcs.o
gcr_tests=(
    gcr-empty-active-release
    gcr-rejoin-below-threshold
    gcr-head-backoff
    gcr-env-config
)
for gcr_test in "${gcr_tests[@]}"; do
    ${CC:-cc} -std=gnu11 -O2 -Wall -Werror -Iinclude "tests/${gcr_test}.c" \
        obj/tests/gcrmcs.o -pthread -o "obj/tests/${gcr_test}"
done

for gcr_test in gcr-empty-active-release gcr-rejoin-below-threshold \
        gcr-head-backoff; do
    "./obj/tests/${gcr_test}"
    echo "PASS ${gcr_test}"
done

# The GCR environment is read once per process, so every override case gets its
# own run with the variables named on the command line.  The default case runs
# with the overrides removed so a caller's environment cannot leak into it.
run_env_case() {
    local case_name=$1
    shift
    env -u GCR_MCS_ACTIVE_LIMIT -u GCR_MCS_SIGNAL_PERIOD -u GCR_MCS_PASSIVE_SPINS \
        "$@" ./obj/tests/gcr-env-config "${case_name}"
    echo "PASS gcr-env-config ${case_name}"
}
run_env_case defaults
run_env_case valid GCR_MCS_ACTIVE_LIMIT=9 GCR_MCS_SIGNAL_PERIOD=0x100 \
    GCR_MCS_PASSIVE_SPINS=77
run_env_case malformed GCR_MCS_ACTIVE_LIMIT=-1 GCR_MCS_SIGNAL_PERIOD=12x \
    GCR_MCS_PASSIVE_SPINS=0x
run_env_case zero GCR_MCS_ACTIVE_LIMIT=0 GCR_MCS_SIGNAL_PERIOD=0x0 \
    GCR_MCS_PASSIVE_SPINS=0
run_env_case explicit GCR_MCS_ACTIVE_LIMIT=3 GCR_MCS_SIGNAL_PERIOD=5 \
    GCR_MCS_PASSIVE_SPINS=11

algorithms=("$@")
if [[ ${#algorithms[@]} -eq 0 ]]; then
    listed=$(make --no-print-directory -f - <<'MK'
include Makefile.config
print:
	@echo $(DIRECT_ALGORITHMS)
MK
)
    for candidate in $listed; do
        [[ "$candidate" == flexguard_original ]] && continue
        algorithms+=("$candidate")
    done
fi

for backend in "${algorithms[@]}"; do
    echo "Testing $backend"
    timeout -k 5s "${LITL_TEST_TIMEOUT:-60}s" \
        bash "./lib${backend}.sh" ./obj/tests/direct "lib${backend}.so" \
        "${LITL_TEST_THREADS:-8}" "${LITL_TEST_ITERATIONS:-10000}"
    timeout -k 5s "${LITL_TEST_TIMEOUT:-60}s" \
        bash "./lib${backend}.sh" ./obj/tests/condition-variable
done
