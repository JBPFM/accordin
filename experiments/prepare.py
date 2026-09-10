#!/usr/bin/env python3
"""Fetch pinned inputs and build private application/lock copies; no root needed."""
import argparse
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import urllib.request

import litl_locks
from litl_locks import sha256

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
FG = ROOT / "bench/flexguard"
LEVELDB_REV = "99b3c03b3284f5886f9ef9a4ef703d57373e61be"  # 1.23
ROCKSDB_REV = "ae8fb3e5000e46d8d4c9dbf3a36019c0aaceebff"  # v9.10.0
FG_REV = "951c9417393574d5918c08b5f54ecca0df18d872"


def manifest_digests(artifacts, drivers, digest=sha256):
    """Measured artifacts and the driver scripts that produced them hash apart."""
    return {"sha256": {str(p): digest(p) for p in artifacts},
            "drivers_sha256": {str(p): digest(p) for p in drivers}}


def replace(path, old, new, count=1):
    data = path.read_text()
    if data.count(old) != count:
        raise RuntimeError(f"patch mismatch {path}: expected {count} occurrences of {old!r}")
    path.write_text(data.replace(old, new))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", type=Path, default=ROOT / "target/experiments")
    parser.add_argument("--jobs", type=int, default=8)
    args = parser.parse_args()
    out = args.build.resolve()
    out.mkdir(parents=True, exist_ok=True)
    logs = out / "build-logs"
    logs.mkdir(exist_ok=True)
    def run(command, cwd=ROOT, label="prepare"):
        print(f"[{label}] {' '.join(map(str, command))}", flush=True)
        with (logs / f"{label}.log").open("a") as log:
            log.write(repr(list(map(str, command))) + "\n"); log.flush()
            subprocess.run(list(map(str, command)), cwd=cwd, stdout=log,
                           stderr=subprocess.STDOUT, check=True)

    # An incomplete build is resumable. Source copies are immutable after patching.
    def copy(src, dest):
        if dest.exists():
            return False
        shutil.copytree(src, dest, ignore=shutil.ignore_patterns(
            ".git", "build", "*.o", "*.a", "*.so*", "out-static", "out-shared", ".output"))
        return True

    if not (FG / "ext/kyotocabinet/kccachedb.h").is_file():
        run(["git", "submodule", "update", "--init", "--recursive", "bench/flexguard"], label="fetch-flexguard")
    fg_rev = subprocess.check_output(["git", "-C", str(FG), "rev-parse", "HEAD"], text=True).strip()
    if fg_rev != FG_REV:
        raise RuntimeError(f"FlexGuard source revision must be {FG_REV}, found {fg_rev}")

    ldb = out / "leveldb"
    if not ldb.exists():
        source = ROOT / "third_party/leveldb-1.23"
        if source.exists() and subprocess.check_output(
                ["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip() == LEVELDB_REV:
            copy(source, ldb)
        else:
            run(["git", "clone", "--depth", "1", "--branch", "1.23", "https://github.com/google/leveldb.git", ldb], label="fetch-leveldb")
    if not (ldb / ".experiment-patched").exists():
        p = ldb / "benchmarks/db_bench.cc"
        original = ldb / ".db_bench.original"
        if original.exists():
            shutil.copy2(original, p)
        else:
            shutil.copy2(p, original)
        replace(p, "static int FLAGS_reads = -1;", "static int FLAGS_reads = -1;\nstatic int FLAGS_total_ops = -1;")
        replace(p, '    } else if (sscanf(argv[i], "--threads=%d%c", &n, &junk) == 1) {',
                '    } else if (sscanf(argv[i], "--total_ops=%d%c", &n, &junk) == 1) {\n      FLAGS_total_ops = n;\n    } else if (sscanf(argv[i], "--threads=%d%c", &n, &junk) == 1) {')
        replace(p, "    if (done_ >= next_report_) {", "    if (false && done_ >= next_report_) {")
        replace(p, "    if (FLAGS_histogram) {\n      std::fprintf(stdout,", '    std::fprintf(stdout, "EXP_RESULT name=%s ops=%d seconds=%.9f\\n",\n                 name.ToString().c_str(), done_, (finish_ - start_) * 1e-6);\n    if (FLAGS_histogram) {\n      std::fprintf(stdout,')
        replace(p, "    for (int i = 0; i < num_; i += entries_per_batch_) {\n      batch.Clear();",
                "    const int count = FLAGS_total_ops < 0 ? num_ : FLAGS_total_ops / FLAGS_threads + (thread->tid < FLAGS_total_ops % FLAGS_threads);\n    for (int i = 0; i < count; i += entries_per_batch_) {\n      batch.Clear();", 2)
        replace(p, "  void ReadRandom(ThreadState* thread) {\n    ReadOptions options;", "  void ReadRandom(ThreadState* thread) {\n    const int count = FLAGS_total_ops < 0 ? reads_ : FLAGS_total_ops / FLAGS_threads + (thread->tid < FLAGS_total_ops % FLAGS_threads);\n    ReadOptions options;")
        # Patch only ReadRandom; other benchmark modes retain their upstream semantics.
        start = p.read_text().index("  void ReadRandom(ThreadState* thread)")
        data = p.read_text(); end = data.index("  void ReadMissing", start)
        chunk = data[start:end].replace("i < reads_", "i < count")
        chunk = chunk.replace("        found++;", '        found++;\n      } else {\n        std::fprintf(stderr, "random read failed\\n"); std::exit(2);')
        chunk = chunk.replace("found, num_", "found, count")
        p.write_text(data[:start] + chunk + data[end:])
        (ldb / ".experiment-patched").touch()
    p = ldb / "benchmarks/db_bench.cc"
    if 'name == Slice("readseq")) {\n        method' in p.read_text():
        replace(p, 'name == Slice("readseq")) {\n        method',
                'name == Slice("readseq")) {\n        num_threads = 1;  // Untimed cache warmup.\n        method')
    # LevelDB 1.23 hardcodes C++11; the installed googletest headers need C++17.
    cmake = ldb / "CMakeLists.txt"
    if "set(CMAKE_CXX_STANDARD 11)" in cmake.read_text():
        replace(cmake, "set(CMAKE_CXX_STANDARD 11)", "set(CMAKE_CXX_STANDARD 17)")
    run(["cmake", "-S", ldb, "-B", out / "leveldb-build", "-DCMAKE_BUILD_TYPE=Release",
         "-DLEVELDB_BUILD_TESTS=OFF", "-DLEVELDB_BUILD_BENCHMARKS=ON", "-DHAVE_SNAPPY=OFF"], label="leveldb")
    run(["cmake", "--build", out / "leveldb-build", "--target", "db_bench", f"-j{args.jobs}"], label="leveldb")

    rocks = out / "rocksdb"
    if not rocks.exists():
        cached = ROOT / "target/rocksdb-fetch-v9.10.0"
        if cached.exists():
            run(["git", "clone", "--shared", cached, rocks], label="fetch-rocksdb")
        else:
            run(["git", "clone", "--depth", "1", "--branch", "v9.10.0", "https://github.com/facebook/rocksdb.git", rocks], label="fetch-rocksdb")
        run(["git", "checkout", ROCKSDB_REV], rocks, "fetch-rocksdb")
    if not (rocks / ".experiment-patched").exists():
        original = rocks / ".db_bench_tool.original"
        p = rocks / "tools/db_bench_tool.cc"
        if original.exists():
            shutil.copy2(original, p)
        else:
            shutil.copy2(p, original)
        replace(rocks / "tools/db_bench_tool.cc", "    double throughput = (double)done_ / elapsed;",
                '    double throughput = (double)done_ / elapsed;\n    fprintf(stdout, "EXP_RESULT name=%s ops=%llu seconds=%.9f\\n", name.ToString().c_str(), (unsigned long long)done_, elapsed);')
        (rocks / ".experiment-patched").touch()
    # RocksDB v9.10.0 reaches uint64_t through headers that recent C++ libraries no
    # longer include transitively.
    run(["cmake", "-S", rocks, "-B", out / "rocksdb-build", "-DCMAKE_BUILD_TYPE=Release",
         "-DCMAKE_CXX_FLAGS=-include cstdint", "-DWITH_TESTS=OFF", "-DWITH_BENCHMARK_TOOLS=ON", "-DWITH_TOOLS=OFF", "-DWITH_GFLAGS=ON",
         "-DWITH_SNAPPY=OFF", "-DWITH_LZ4=OFF", "-DWITH_ZSTD=OFF", "-DWITH_ZLIB=OFF",
         "-DWITH_BZ2=OFF", "-DFAIL_ON_WARNINGS=OFF", "-DPORTABLE=ON"], label="rocksdb")
    run(["cmake", "--build", out / "rocksdb-build", "--target", "db_bench", f"-j{args.jobs}"], label="rocksdb")

    sc = out / "streamcluster"
    if copy(FG / "ext/parsec-benchmark/pkgs/kernels/streamcluster/src", sc):
        p = sc / "streamcluster.cpp"
        replace(p, "__builtin_ia32_rdtsc()", "experiment_nanoseconds()", 3)
        p.write_text('#include <time.h>\nstatic unsigned long long experiment_nanoseconds() { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (unsigned long long)t.tv_sec * 1000000000ULL + t.tv_nsec; }\n' + p.read_text())
    run(["make", f"-j{args.jobs}", "version=pthreads", "CXXFLAGS=-O3 -DNDEBUG -DENABLE_THREADS -pthread"], sc, "streamcluster")
    ray = out / "raytrace"
    copy(FG / "ext/parsec-benchmark/ext/splash2x/apps/raytrace/src", ray)
    macro = FG / "ext/parsec-benchmark/pkgs/libs/parmacs/src/m4/parmacs.pthreads.c.m4"
    run(["make", f"-j{args.jobs}", "version=pthreads", "HOSTCC=gcc", f"MACROS={macro}", "M4=m4",
         "CFLAGS=-O3 -DNDEBUG -DENABLE_THREADS -pthread -fcommon -Wno-implicit-function-declaration -Wno-implicit-int -Wno-int-conversion -Wno-incompatible-pointer-types"], ray, "raytrace")
    inputs = out / "inputs/raytrace"
    if not inputs.exists():
        cached = ROOT / "target/flexguard-suite-20260905/inputs/raytrace"
        if cached.is_dir():
            shutil.copytree(cached, inputs)
        else:
            archive = out / "parsec-3.0-input-sim.tar.gz"
            if not archive.exists():
                print("Downloading PARSEC simulation inputs", flush=True)
                urllib.request.urlretrieve("https://github.com/cirosantilli/parsec-benchmark/releases/download/3.0/parsec-3.0-input-sim.tar.gz", str(archive) + ".part")
                Path(str(archive) + ".part").rename(archive)
            with tarfile.open(archive) as outer:
                member = next(m for m in outer if "splash2x/apps/raytrace/inputs/input_simsmall" in m.name and m.isfile())
                inputs.mkdir(parents=True)
                with tarfile.open(fileobj=outer.extractfile(member)) as inner:
                    inner.extractall(inputs, filter="data")

    kyoto = out / "kyotocabinet"
    copy(FG / "ext/kyotocabinet", kyoto)
    if not (kyoto / ".configured").exists():
        run(["./configure", "--disable-shared", "--enable-static"], kyoto, "kyoto")
        (kyoto / ".configured").touch()
    run(["make", f"-j{args.jobs}", "libkyotocabinet.a"], kyoto, "kyoto")
    run(["g++", "-O3", "-DNDEBUG", "-std=c++17", "-pthread", f"-I{kyoto}", HERE / "sources/kyoto_cachedb.cc",
         kyoto / "libkyotocabinet.a", "-lz", "-lrt", "-o", out / "kyoto_cachedb"], label="kyoto-driver")

    run(["make", f"-j{args.jobs}", f"OUT={out}/accordin", "all"], label="accordin")
    # FlexGuard contributes the runtime archive its LiTL algorithm links; the lock,
    # its queue nodes and its BPF program all live in that archive.
    flexguard = out / "flexguard"
    if not (flexguard / ".sources-ready").exists():
        shutil.rmtree(flexguard, ignore_errors=True)
        flexguard.mkdir(parents=True)
        for name in ["include", "src", "bmarks"]:
            shutil.copytree(FG / name, flexguard / name)
        for name in ["Makefile", "interpose.in"]:
            shutil.copy2(FG / name, flexguard / name)
        run(["patch", "-p1", "-i", HERE / "patches/flexguard-arm.patch"], flexguard, "flexguard")
        # The patch reaches none of these, so they stay links into the checkout.
        for name in ["libbpf", "bpftool", "vmlinux"]:
            (flexguard / name).symlink_to(FG / name, target_is_directory=True)
        (flexguard / ".sources-ready").touch()

    litl = out / "litl"
    litl.mkdir(exist_ok=True)
    for name in ["src", "include"]:
        shutil.copytree(ROOT / "third_party/litl" / name, litl / name, dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns("topology.h"))
    for name in ["Makefile", "Makefile.config"]:
        shutil.copy2(ROOT / "third_party/litl" / name, litl / name)
    algorithms = sorted({litl_locks.litl_algorithm(lock) for lock in litl_locks.EXPERIMENT_LOCKS})
    run(["make", f"-j{args.jobs}", f"ACCORDIN_ROOT={ROOT}", f"ACCORDIN_LIB_DIR={out}/accordin",
         f"FLEXGUARD_DIR={flexguard}", "FLEXGUARD=1", "ALGORITHMS=" + " ".join(algorithms), "all"], litl, "litl")
    locks = litl_locks.manifest_locks(litl)
    for lock, library in locks.items():
        if not Path(library).is_file():
            raise RuntimeError(f"LiTL build did not produce the library for {lock}: {library}")
    run(["gcc", "-O2", "-pthread", HERE / "sources/lock_probe.c", "-ldl", "-o", out / "lock_probe"], label="probe")
    run(["g++", "-O3", "-std=c++20", f"-I{litl}/include", HERE / "sources/tse_probe.cc",
         "-o", out / "tse_probe"], label="tse-probe")

    binaries = {"leveldb": out / "leveldb-build/db_bench", "rocksdb": out / "rocksdb-build/db_bench",
                "streamcluster": sc / "streamcluster", "raytrace": ray / "raytrace", "kyoto-cachedb": out / "kyoto_cachedb"}
    linked = list(binaries.values()) + [Path(p) for p in locks.values()]
    output = subprocess.check_output(["ldd", *map(str, linked)], text=True)
    dependencies = sorted(set(re.findall(r"(?:=>\s+|^\s*)(/[^\s]+)\s+\(", output, re.M)))
    files = linked + list((out / "accordin").glob("*.so"))
    files += list((out / "rocksdb-build").glob("*.so*")) + [out / "tse_probe", out / "lock_probe"]
    files += list(inputs.glob("car.*")) + list((HERE / "sources").glob("*")) + list((HERE / "patches").glob("*"))
    files += [Path(p) for p in dependencies]
    manifest = {"schema": 1, "architecture": platform.machine(), "build": str(out),
                "revisions": {"leveldb": LEVELDB_REV, "rocksdb": ROCKSDB_REV, "flexguard": fg_rev,
                              "accordin": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()},
                "git_status": subprocess.check_output(["git", "status", "--short"], text=True),
                "binaries": {k: str(v) for k, v in binaries.items()},
                "locks": locks,
                **manifest_digests(files, sorted(HERE.glob("*.py")))}
    # Record the exact source snapshot, including pre-existing local edits.
    sources = list((ROOT / "src").rglob("*.c")) + list((ROOT / "src").rglob("*.h"))
    sources += [p for base in [ldb, rocks / "tools", sc, ray, kyoto,
                               flexguard / "src", flexguard / "include",
                               ROOT / "third_party/litl/src", ROOT / "third_party/litl/include"]
                for p in base.rglob("*") if p.is_file() and p.suffix in {".cc", ".cpp", ".c", ".C", ".h", ".H", ".hpp"}]
    manifest["source_sha256"] = {str(p): sha256(p) for p in sources}
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Ready: {out / 'manifest.json'}", flush=True)


if __name__ == "__main__":
    main()
