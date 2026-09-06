#!/usr/bin/env bash
# Exercises the baseline algorithms that share the direct front end. None of
# them attaches a BPF scheduler, so this runs without sched_ext.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
mkdir -p obj/tests
${CC:-cc} -std=gnu11 -O2 -Wall -Werror tests/direct.c -pthread -ldl \
    -o obj/tests/direct
${CXX:-c++} -std=c++17 -O2 -Wall -Werror tests/condition-variable.cpp -pthread \
    -o obj/tests/condition-variable

algorithms=("$@")
if [[ ${#algorithms[@]} -eq 0 ]]; then
    mapfile -t algorithms < <(make --no-print-directory -f - <<'MK'
include Makefile.config
print:
	@echo $(DIRECT_ALGORITHMS)
MK
)
    read -r -a algorithms <<<"${algorithms[*]}"
fi

for backend in "${algorithms[@]}"; do
    echo "Testing $backend"
    timeout -k 5s "${LITL_TEST_TIMEOUT:-60}s" \
        bash "./lib${backend}.sh" ./obj/tests/direct "lib${backend}.so" \
        "${LITL_TEST_THREADS:-8}" "${LITL_TEST_ITERATIONS:-10000}"
    timeout -k 5s "${LITL_TEST_TIMEOUT:-60}s" \
        bash "./lib${backend}.sh" ./obj/tests/condition-variable
done
