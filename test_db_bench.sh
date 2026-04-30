#!/bin/bash
set -e

LIB_PATH="/mnt/home/jz/lb_critical/target/debug/libaccordin.so"
DB_BENCH="/mnt/home/jz/test/test/leveldb/build/db_bench"

if [[ ! -f "$LIB_PATH" ]]; then
    echo "Error: $LIB_PATH not found. Run 'cargo build' first."
    exit 1
fi

if [[ ! -x "$DB_BENCH" ]]; then
    echo "Error: $DB_BENCH not found or not executable."
    exit 1
fi

echo "=== Running db_bench with accordin scheduler ==="
echo "Pinned to CPU 0, 80 threads, 50000 operations"
echo ""

sudo LD_PRELOAD="$LIB_PATH" taskset -c 0 "$DB_BENCH" \
    --benchmarks=readrandom \
    --threads=80 \
    --num=50000
