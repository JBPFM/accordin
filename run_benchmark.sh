if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
else
  SUDO="sudo"
fi

$SUDO LD_PRELOAD="./target/release/libaccordin.so" ~/Projects/tests/leveldb/build/db_bench --benchmarks=readrandom --threads=256 --num=$1
