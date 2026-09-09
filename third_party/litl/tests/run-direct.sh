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
gcr_tests=(
    gcr-empty-active-release
    gcr-rejoin-below-threshold
    gcr-head-backoff
    gcr-env-config
)
for gcr_test in "${gcr_tests[@]}"; do
    ${CC:-cc} -std=gnu11 -O2 -Wall -Werror -Iinclude "tests/${gcr_test}.c" \
        src/gcrmcs.c -pthread -o "obj/tests/${gcr_test}"
    "./obj/tests/${gcr_test}"
    echo "PASS ${gcr_test}"
done

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
