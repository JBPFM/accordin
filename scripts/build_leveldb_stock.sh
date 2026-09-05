#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_repo="$root/third_party/leveldb-1.23"
stage="$root/target/leveldb-stock"
src="$stage/src"
build="$stage/build"

# The checkout carries the accordin LevelDB patch; export the pristine commit
# instead so db_bench uses the stock std::mutex / std::condition_variable port.
rm -rf "$src"
mkdir -p "$src"
git -C "$source_repo" archive HEAD | tar -x -C "$src"

# git archive omits submodule contents; unit tests are off, so only copy them
# when a populated working tree has them.
for module in benchmark googletest; do
  if [[ -e "$source_repo/third_party/$module/CMakeLists.txt" ]]; then
    cp -a "$source_repo/third_party/$module/." "$src/third_party/$module/"
  fi
done

cmake -S "$src" -B "$build" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_CXX_STANDARD=17 \
  -DCMAKE_CXX_STANDARD_REQUIRED=ON \
  -DLEVELDB_BUILD_BENCHMARKS=ON \
  -DLEVELDB_BUILD_TESTS=OFF
cmake --build "$build" --target db_bench -j "$(nproc)"

if [[ "$(nm -C "$build/db_bench" | grep -c condition_variable_any || true)" != 0 ]]; then
  echo "db_bench still carries the patched port layer" >&2
  exit 1
fi
"$build/db_bench" --benchmarks=fillseq --num=1000 --db="$(mktemp -d)" > /dev/null
echo "Stock db_bench built at $build/db_bench"
