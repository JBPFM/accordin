#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import hashlib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from statistics import mean
from typing import Iterable

import experiment_defaults
import run_experiment_three as experiment_three


REPO_ROOT = experiment_three.REPO_ROOT
FLEXGUARD_DIR = experiment_three.FLEXGUARD_DIR
DEFAULT_CACHELIB_DIR = REPO_ROOT / "third_party" / "CacheLib"

DEFAULT_WORKLOADS = ("cachebench", "rocksdb", "memcached")
DEFAULT_ROCKSDB_BENCHMARKS = ("readrandom", "fillrandom")
DEFAULT_ROCKSDB_COMPRESSION_TYPE = "none"
DEFAULT_LOCK_PROFILE = experiment_defaults.DEFAULT_LOCK_PROFILE
DEFAULT_LOCKS = experiment_defaults.DEFAULT_LOCKS
FULL_LOCKS = experiment_defaults.FULL_LOCKS
MINIMAL_LOCKS = experiment_defaults.MINIMAL_LOCKS
DEFAULT_THREADS = experiment_defaults.DEFAULT_THREADS
DEFAULT_REPEATS = experiment_defaults.DEFAULT_REPEATS
DEFAULT_TOTAL_OPS = 1_572_864
DEFAULT_DB_NUM = 500_000
DEFAULT_CACHEBENCH_NUM_OPS = 100_000
DEFAULT_CACHEBENCH_NUM_KEYS = 1_000_000
DEFAULT_CACHEBENCH_CACHE_MB = 512
DEFAULT_CACHELIB_BUILD_JOBS = os.cpu_count() or 1
DEFAULT_MEMCACHED_HOST = "127.0.0.1"
DEFAULT_MEMCACHED_MEMORY_MB = 512
DEFAULT_MEMTIER_CLIENT_THREADS = 1
DEFAULT_MEMTIER_CLIENTS = 4
DEFAULT_MEMTIER_REQUESTS = 10_000
DEFAULT_MEMTIER_RATIO = "1:10"
DEFAULT_MEMTIER_KEY_PATTERN = "R:R"
DEFAULT_MEMTIER_DATA_SIZE = 128

SINGLE_OVERSUBSCRIBED_LOCKS = experiment_defaults.SINGLE_OVERSUBSCRIBED_LOCKS
PER_LOCK_MAX_THREADS = experiment_defaults.per_lock_max_threads_for_settings(
    SINGLE_OVERSUBSCRIBED_LOCKS,
    DEFAULT_THREADS,
)

RAW_FIELDS = (
    "workload",
    "benchmark",
    "lock",
    "threads",
    "repeat",
    "ops_per_second",
    "latency_micros_per_op",
    "wall_seconds",
    "setup_wall_seconds",
    "total_ops",
    "command_log",
    "setup_log",
    "server_log",
)
SUMMARY_FIELDS = (
    "workload",
    "benchmark",
    "lock",
    "threads",
    "mean_ops_per_second",
    "mean_latency_micros_per_op",
    "mean_wall_seconds",
    "mean_setup_wall_seconds",
    "runs",
)

DB_BENCH_LATENCY_PATTERN = re.compile(
    r"^(?P<name>[A-Za-z0-9_]+)\s+:\s+(?P<micros>\d+(?:\.\d+)?)\s+micros/op"
    r"(?:\s+(?P<ops>\d+(?:\.\d+)?)\s+ops/sec)?",
    re.MULTILINE,
)
OPS_PATTERNS = (
    re.compile(r"(?i)\bops/sec(?:ond)?\b\s*[:=]?\s*(?P<value>\d+(?:\.\d+)?)"),
    re.compile(r"(?i)\bqps\b\s*[:=]?\s*(?P<value>\d+(?:\.\d+)?)"),
    re.compile(r"(?i)\bthroughput\b.*?\b(?P<value>\d+(?:\.\d+)?)\s*(?:ops/s|ops/sec|qps)\b"),
)
MEMTIER_PROGRESS_AVG_PATTERN = re.compile(
    r"(?i)\(avg:\s*(?P<value>\d+(?:\.\d+)?)\)\s*ops/sec"
)
CACHEBENCH_THROUGHPUT_PATTERN = re.compile(
    r"^\s*(?:get|set|del|couldExist)\s*:\s*(?P<value>\d[\d,]*(?:\.\d+)?)/s\b",
    re.MULTILINE,
)
BENCHMARK_NAME_PATTERN = re.compile(r"^[A-Za-z0-9_]+$")


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be positive")
    return parsed


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("value must be non-negative")
    return parsed


def default_result_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment5_{timestamp}"


def resolve_path(path: Path) -> Path:
    return experiment_three.resolve_path(path)


def parse_csv_strings(value: str) -> tuple[str, ...]:
    return experiment_three.parse_csv_strings(value)


def parse_csv_ints(value: str) -> tuple[int, ...]:
    return experiment_three.parse_csv_ints(value)


def validate_workloads(workloads: tuple[str, ...]) -> tuple[str, ...]:
    supported = set(DEFAULT_WORKLOADS)
    normalized = tuple(workload.strip().lower() for workload in workloads)
    unsupported = [workload for workload in normalized if workload not in supported]
    if unsupported:
        raise ValueError(f"Unsupported workloads: {', '.join(unsupported)}. Supported: {', '.join(DEFAULT_WORKLOADS)}")
    return normalized


def validate_benchmark_names(benchmarks: tuple[str, ...]) -> tuple[str, ...]:
    invalid = [benchmark for benchmark in benchmarks if BENCHMARK_NAME_PATTERN.fullmatch(benchmark) is None]
    if invalid:
        raise ValueError(f"Unsupported benchmark names: {', '.join(invalid)}")
    return benchmarks


def configured_executable_path(value: Path | None, env_name: str, binary_name: str) -> Path | None:
    env_value = os.environ.get(env_name)
    candidate = value or (Path(env_value) if env_value else None)
    if candidate is not None:
        resolved = resolve_path(candidate)
        if not resolved.is_file() or not os.access(resolved, os.X_OK):
            raise RuntimeError(f"{binary_name} is missing or not executable: {resolved}")
        return resolved
    return None


def executable_path(value: Path | None, env_name: str, binary_name: str) -> Path:
    configured = configured_executable_path(value, env_name, binary_name)
    if configured is not None:
        return configured
    found = shutil.which(binary_name)
    if found is None:
        raise RuntimeError(
            f"{binary_name} was not found on PATH. Pass --{binary_name.replace('_', '-')}-bin "
            f"or set {env_name}."
        )
    return Path(found).resolve()


def default_cachelib_dir() -> Path:
    env_value = os.environ.get("CACHELIB_HOME")
    if env_value:
        return resolve_path(Path(env_value))
    return DEFAULT_CACHELIB_DIR.resolve()


def cachelib_source_is_available(cachelib_dir: Path) -> bool:
    return (
        (cachelib_dir / "README.md").is_file()
        and (cachelib_dir / "build" / "fbcode_builder" / "getdeps.py").is_file()
        and (cachelib_dir / "cachelib" / "cachebench" / "CMakeLists.txt").is_file()
    )


def cachelib_getdeps_install_dir(cachelib_dir: Path) -> Path | None:
    getdeps = cachelib_dir / "build" / "fbcode_builder" / "getdeps.py"
    if not getdeps.is_file():
        return None
    try:
        result = subprocess.run(
            ["python3", "./build/fbcode_builder/getdeps.py", "show-inst-dir", "cachelib"],
            cwd=str(cachelib_dir),
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
        )
    except OSError:
        return None
    if result.returncode != 0:
        return None
    lines = [line.strip() for line in result.stdout.splitlines() if line.strip()]
    if not lines:
        return None
    return Path(lines[-1]).expanduser()


def cachebench_candidates(cachelib_dir: Path) -> tuple[Path, ...]:
    candidates: list[Path] = [
        cachelib_dir / "opt" / "cachelib" / "bin" / "cachebench",
    ]
    getdeps_install_dir = cachelib_getdeps_install_dir(cachelib_dir)
    if getdeps_install_dir is not None:
        candidates.append(getdeps_install_dir / "bin" / "cachebench")
    candidates.extend(
        (
            cachelib_dir / "build-cachelib" / "cachebench" / "cachebench",
            cachelib_dir / "build-cachelib" / "cachelib" / "cachebench" / "cachebench",
        )
    )
    return tuple(candidates)


def find_local_cachebench(cachelib_dir: Path) -> Path | None:
    for candidate in cachebench_candidates(cachelib_dir):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate.resolve()
    return None


def cachelib_getdeps_env() -> dict[str, str]:
    env: dict[str, str] = {}
    virtual_env = os.environ.get("VIRTUAL_ENV")
    if virtual_env:
        venv_bin = str(Path(virtual_env).expanduser().resolve() / "bin")
        path_parts = [
            part
            for part in os.environ.get("PATH", "").split(os.pathsep)
            if part and str(Path(part).expanduser().resolve()) != venv_bin
        ]
        env["PATH"] = os.pathsep.join(path_parts)
        env["VIRTUAL_ENV"] = ""
    return env


def ensure_cachelib_source(
    cachelib_dir: Path,
    *,
    build_missing: bool,
    logger: experiment_three.CommandLogger,
) -> None:
    if cachelib_source_is_available(cachelib_dir):
        return
    if not build_missing:
        raise RuntimeError(
            f"CacheLib source directory is missing or incomplete: {cachelib_dir}. "
            "Rerun with --build-missing or initialize the submodule manually."
        )
    if cachelib_dir != DEFAULT_CACHELIB_DIR.resolve():
        raise RuntimeError(f"Cannot initialize a custom CacheLib source directory: {cachelib_dir}")

    logger.run(
        [
            "git",
            "submodule",
            "update",
            "--init",
            "--recursive",
            "--depth",
            "1",
            str(DEFAULT_CACHELIB_DIR.relative_to(REPO_ROOT)),
        ],
        log_name="init_cachelib_submodule.log",
        cwd=REPO_ROOT,
    )
    if not cachelib_source_is_available(cachelib_dir):
        raise RuntimeError(f"CacheLib source directory is still unavailable after submodule init: {cachelib_dir}")


def build_cachebench_from_cachelib(
    cachelib_dir: Path,
    *,
    jobs: int,
    logger: experiment_three.CommandLogger,
) -> None:
    ensure_cachelib_source(cachelib_dir, build_missing=True, logger=logger)
    getdeps = cachelib_dir / "build" / "fbcode_builder" / "getdeps.py"
    if not getdeps.is_file():
        raise RuntimeError(f"CacheLib getdeps.py was not found: {getdeps}")

    logger.run(
        [
            "python3",
            "./build/fbcode_builder/getdeps.py",
            "--allow-system-packages",
            "--num-jobs",
            str(jobs),
            "build",
            "--no-tests",
            "cachelib",
        ],
        log_name="build_cachelib_cachebench.log",
        cwd=cachelib_dir,
        env=cachelib_getdeps_env(),
    )


def ensure_cachebench_bin(
    cachebench_bin: Path | None,
    *,
    cachelib_dir: Path,
    cachelib_build_jobs: int,
    build_missing: bool,
    logger: experiment_three.CommandLogger,
) -> Path:
    configured = configured_executable_path(cachebench_bin, "CACHEBENCH_BIN", "cachebench")
    if configured is not None:
        return configured

    local = find_local_cachebench(cachelib_dir)
    if local is not None:
        return local

    found = shutil.which("cachebench")
    if found is not None:
        return Path(found).resolve()

    if not build_missing:
        raise RuntimeError(
            "cachebench was not found. Pass --cachebench-bin, set CACHEBENCH_BIN, "
            "install cachebench on PATH, or rerun with --build-missing to initialize and build "
            f"the CacheLib submodule at {DEFAULT_CACHELIB_DIR}."
        )

    build_cachebench_from_cachelib(cachelib_dir, jobs=cachelib_build_jobs, logger=logger)
    rebuilt = find_local_cachebench(cachelib_dir)
    if rebuilt is None:
        raise RuntimeError(
            "CacheLib cachebench is still unavailable after build. Expected one of: "
            + ", ".join(str(path) for path in cachebench_candidates(cachelib_dir))
        )
    return rebuilt


def default_rocksdb_dir() -> Path:
    env_value = os.environ.get("ROCKSDB_HOME")
    if env_value:
        return resolve_path(Path(env_value))

    candidates = (
        REPO_ROOT / "third_party" / "rocksdb",
        Path("/home/jz/Projects/tests/rocksdb"),
    )
    for candidate in candidates:
        if (candidate / "CMakeLists.txt").is_file() or (candidate / "Makefile").is_file():
            return candidate.resolve()
    return candidates[0].resolve()


def accordin_rocksdb_build_dir(rocksdb_dir: Path) -> Path:
    digest = hashlib.sha1(str(rocksdb_dir.resolve()).encode("utf-8")).hexdigest()[:12]
    return REPO_ROOT / ".cache" / "rocksdb_db_bench" / digest


def default_rocksdb_db_bench(rocksdb_dir: Path) -> Path | None:
    env_value = os.environ.get("ROCKSDB_DB_BENCH")
    if env_value:
        return resolve_path(Path(env_value))

    accordin_build_dir = accordin_rocksdb_build_dir(rocksdb_dir)
    for candidate in (
        rocksdb_dir / "db_bench",
        rocksdb_dir / "build" / "db_bench",
        rocksdb_dir / "build" / "tools" / "db_bench",
        accordin_build_dir / "db_bench",
        accordin_build_dir / "tools" / "db_bench",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate.resolve()

    found = shutil.which("db_bench")
    return Path(found).resolve() if found is not None else None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run or plot external workload lock sweeps: CacheBench, RocksDB db_bench, and Memcached+memtier.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"""\
Default benchmark settings:
  workloads={','.join(DEFAULT_WORKLOADS)}
  lock-profile={DEFAULT_LOCK_PROFILE}
  minimal locks={','.join(MINIMAL_LOCKS)}
  full locks={','.join(FULL_LOCKS)}
  machine-profile={experiment_defaults.ACTIVE_MACHINE_CONFIG.name} (override with {experiment_defaults.PROFILE_ENV})
  threads={','.join(str(thread) for thread in DEFAULT_THREADS)}
  repeats={DEFAULT_REPEATS}, total_ops={DEFAULT_TOTAL_OPS}
  rocksdb_benchmarks={','.join(DEFAULT_ROCKSDB_BENCHMARKS)}
  per_lock_max_threads={','.join(f"{lock}:{max_threads}" for lock, max_threads in PER_LOCK_MAX_THREADS.items())}

Examples:
  python3 experiments/run_experiment_five.py --workloads rocksdb --locks stock --threads 1 --repeats 1 --rocksdb-benchmarks fillrandom --total-ops 1000
  python3 experiments/run_experiment_five.py --workloads memcached --locks stock --threads 4 --repeats 1 --memtier-requests 1000
  python3 experiments/run_experiment_five.py --workloads cachebench --build-missing
  python3 experiments/run_experiment_five.py --workloads cachebench --cachebench-config /path/to/config.json
  python3 experiments/run_experiment_five.py --plot-only experiments/results/experiment5_manual
""",
    )
    parser.add_argument("--output-root", type=Path, default=None, help="Directory for a new run.")
    parser.add_argument("--plot-only", type=Path, default=None, metavar="RESULT_ROOT", help="Regenerate summary and plots from raw.csv.")
    parser.add_argument("--force", action="store_true", help="Allow output into an existing non-empty output root.")
    parser.add_argument(
        "--resume",
        action="store_true",
        help="Continue an existing --output-root by parsing successful command logs and skipping completed points.",
    )
    parser.add_argument("--build-missing", action="store_true", help="Build missing lock helpers, CacheBench, or RocksDB db_bench where possible.")
    parser.add_argument("--skip-plots", action="store_true", help="Write CSVs but skip PNG generation.")
    parser.add_argument("--workloads", default=",".join(DEFAULT_WORKLOADS), metavar="CSV", help="cachebench,rocksdb,memcached.")
    parser.add_argument(
        "--lock-profile",
        choices=experiment_defaults.lock_profile_names(),
        default=DEFAULT_LOCK_PROFILE,
        help=(
            "Named lock set used when --locks is omitted. "
            f"Default: {DEFAULT_LOCK_PROFILE}. "
            f"minimal={','.join(MINIMAL_LOCKS)}; full={','.join(FULL_LOCKS)}."
        ),
    )
    parser.add_argument(
        "--locks",
        default=None,
        metavar="CSV",
        help=(
            "Comma-separated lock keys. Overrides --lock-profile. Use stock for no interpose. "
            "Aliases: mcs-tas == mcstas, mcs_tse/mcs-tse == mcs_extension, "
            "accordin/mcs_accordin == mcs_tas_accordin."
        ),
    )
    parser.add_argument("--threads", default=",".join(str(thread) for thread in DEFAULT_THREADS), metavar="CSV")
    parser.add_argument("--repeats", type=positive_int, default=DEFAULT_REPEATS)
    parser.add_argument("--total-ops", type=positive_int, default=DEFAULT_TOTAL_OPS)
    parser.add_argument("--db-num", type=positive_int, default=DEFAULT_DB_NUM, help="RocksDB keyspace/DB size.")

    parser.add_argument(
        "--cachebench-bin",
        type=Path,
        default=None,
        help="Path to CacheLib cachebench. Default: CACHEBENCH_BIN, local CacheLib submodule build, or PATH.",
    )
    parser.add_argument(
        "--cachelib-dir",
        type=Path,
        default=None,
        help=f"CacheLib source directory used with --build-missing. Default: CACHELIB_HOME or {DEFAULT_CACHELIB_DIR}.",
    )
    parser.add_argument(
        "--cachelib-build-jobs",
        type=positive_int,
        default=DEFAULT_CACHELIB_BUILD_JOBS,
        help=f"Parallel jobs passed to CacheLib getdeps --num-jobs. Default: {DEFAULT_CACHELIB_BUILD_JOBS}.",
    )
    parser.add_argument("--cachebench-config", type=Path, default=None, help="CacheBench JSON config template. A small synthetic config is generated when omitted.")
    parser.add_argument("--cachebench-num-ops", type=positive_int, default=DEFAULT_CACHEBENCH_NUM_OPS)
    parser.add_argument("--cachebench-num-keys", type=positive_int, default=DEFAULT_CACHEBENCH_NUM_KEYS)
    parser.add_argument("--cachebench-cache-mb", type=positive_int, default=DEFAULT_CACHEBENCH_CACHE_MB)
    parser.add_argument("--cachebench-timeout-seconds", type=non_negative_int, default=0, help="Pass --timeout_seconds when non-zero.")
    parser.add_argument("--cachebench-extra-arg", action="append", default=[], help="Extra argument passed through to cachebench; repeatable.")

    parser.add_argument("--rocksdb-dir", type=Path, default=None, help="RocksDB source/build directory. Default: ROCKSDB_HOME or local probe.")
    parser.add_argument("--rocksdb-db-bench", type=Path, default=None, help="Path to RocksDB db_bench. Default: ROCKSDB_DB_BENCH, rocksdb build paths, or PATH.")
    parser.add_argument("--rocksdb-benchmarks", default=",".join(DEFAULT_ROCKSDB_BENCHMARKS), metavar="CSV")
    parser.add_argument("--rocksdb-fill-benchmark", default="fillrandom")
    parser.add_argument(
        "--rocksdb-compression-type",
        default=DEFAULT_ROCKSDB_COMPRESSION_TYPE,
        help="Passed to db_bench as --compression_type when non-empty. Default: none.",
    )
    parser.add_argument(
        "--rocksdb-progress-reports",
        action="store_true",
        help="Allow db_bench progress output. Default: disabled to keep long experiment logs manageable.",
    )
    parser.add_argument("--rocksdb-init-existing-benchmarks", default="readrandom,readseq,seekrandom,overwrite", metavar="CSV")
    parser.add_argument("--rocksdb-extra-arg", action="append", default=[], help="Extra argument passed through to db_bench; repeatable.")

    parser.add_argument("--memcached-bin", type=Path, default=None, help="Path to memcached. Default: MEMCACHED_BIN or PATH.")
    parser.add_argument("--memtier-bin", type=Path, default=None, help="Path to memtier_benchmark. Default: MEMTIER_BIN or PATH.")
    parser.add_argument("--memcached-host", default=DEFAULT_MEMCACHED_HOST)
    parser.add_argument("--memcached-port", type=non_negative_int, default=0, help="0 chooses a free localhost port per run.")
    parser.add_argument("--memcached-memory-mb", type=positive_int, default=DEFAULT_MEMCACHED_MEMORY_MB)
    parser.add_argument("--memcached-user", default=os.environ.get("USER", "nobody"), help="User passed with -u when memcached is launched through sudo.")
    parser.add_argument("--memcached-start-timeout", type=positive_int, default=10)
    parser.add_argument("--memtier-client-threads", type=positive_int, default=DEFAULT_MEMTIER_CLIENT_THREADS)
    parser.add_argument("--memtier-clients", type=positive_int, default=DEFAULT_MEMTIER_CLIENTS)
    parser.add_argument("--memtier-requests", type=positive_int, default=DEFAULT_MEMTIER_REQUESTS)
    parser.add_argument("--memtier-ratio", default=DEFAULT_MEMTIER_RATIO)
    parser.add_argument("--memtier-key-pattern", default=DEFAULT_MEMTIER_KEY_PATTERN)
    parser.add_argument("--memtier-data-size", type=positive_int, default=DEFAULT_MEMTIER_DATA_SIZE)
    parser.add_argument("--memtier-extra-arg", action="append", default=[], help="Extra argument passed through to memtier_benchmark; repeatable.")
    return parser.parse_args()


def init_existing_benchmarks(value: str) -> tuple[str, ...]:
    if value.strip().lower() in {"", "none"}:
        return ()
    return validate_benchmark_names(parse_csv_strings(value))


def lock_label(lock: str) -> str:
    return experiment_defaults.lock_label(lock)


def workload_label(workload: str, benchmark: str) -> str:
    if workload == "cachebench":
        return "CacheLib CacheBench"
    if workload == "rocksdb":
        return f"RocksDB {benchmark}"
    if workload == "memcached":
        return "Memcached + memtier"
    return benchmark


def lock_sort_key(lock: str) -> tuple[int, str]:
    return experiment_defaults.lock_sort_key(lock)


def workload_sort_key(workload: str) -> tuple[int, str]:
    if workload in DEFAULT_WORKLOADS:
        return (DEFAULT_WORKLOADS.index(workload), workload)
    return (len(DEFAULT_WORKLOADS), workload)


def benchmark_sort_key(workload: str, benchmark: str) -> tuple[int, str]:
    if workload == "rocksdb" and benchmark in DEFAULT_ROCKSDB_BENCHMARKS:
        return (DEFAULT_ROCKSDB_BENCHMARKS.index(benchmark), benchmark)
    return (len(DEFAULT_ROCKSDB_BENCHMARKS), benchmark)


def runnable_threads_for_lock(lock: str, threads: tuple[int, ...]) -> tuple[int, ...]:
    return experiment_defaults.runnable_threads_for_lock(lock, threads)


def per_lock_max_threads_for_settings(
    locks: tuple[str, ...],
    threads: tuple[int, ...],
) -> dict[str, int]:
    return experiment_defaults.per_lock_max_threads_for_settings(locks, threads)


def lock_command_prefix(lock: str) -> tuple[list[str], dict[str, str] | None]:
    if lock == "stock":
        return [], None
    if lock == "mcs_tas_accordin":
        return [], experiment_three.accordin_preload_env(experiment_three.ACCORDIN_PRELOAD_LIBRARY)
    if lock == "mcs_extension":
        return [], {"LD_PRELOAD": experiment_three.combine_ld_preload(experiment_three.MCS_EXTENSION_PRELOAD_LIBRARY)}
    return experiment_three.interpose_command(lock)


def merge_envs(*envs: dict[str, str] | None) -> dict[str, str] | None:
    merged: dict[str, str] = {}
    for env in envs:
        if env:
            merged.update(env)
    return merged or None


def build_lock_command(
    lock: str,
    command: list[str],
    *,
    extra_env: dict[str, str] | None = None,
) -> tuple[list[str], dict[str, str] | None]:
    prefix, env = lock_command_prefix(lock)
    full_command = [*prefix, *command]
    env = merge_envs(extra_env, env)
    return experiment_three.benchmark_command(lock, full_command, env)


def ensure_lock_helpers(
    locks: tuple[str, ...],
    *,
    build_missing: bool,
    logger: experiment_three.CommandLogger,
) -> None:
    experiment_three.ensure_interpose_helpers(locks, build_missing=build_missing, logger=logger)
    if "mcs_tas_accordin" in locks:
        experiment_three.ensure_accordin_preload(build_missing=build_missing, logger=logger)
    if "mcs_extension" in locks:
        experiment_three.ensure_mcs_extension_preload(build_missing=build_missing, logger=logger)


def ensure_rocksdb_db_bench(
    db_bench: Path | None,
    *,
    rocksdb_dir: Path,
    build_missing: bool,
    logger: experiment_three.CommandLogger,
) -> Path:
    if db_bench is not None and db_bench.is_file() and os.access(db_bench, os.X_OK):
        return db_bench
    if not build_missing:
        target = db_bench if db_bench is not None else rocksdb_dir / "db_bench"
        raise RuntimeError(f"RocksDB db_bench is missing or not executable: {target}. Rerun with --build-missing.")
    if not rocksdb_dir.is_dir():
        raise RuntimeError(f"RocksDB directory does not exist: {rocksdb_dir}")

    existing_build_dir = rocksdb_dir / "build"
    if cmake_build_dir_matches_source(existing_build_dir, rocksdb_dir):
        logger.run(
            ["cmake", "--build", str(existing_build_dir), "--target", "db_bench", "--parallel"],
            log_name="build_rocksdb_db_bench.log",
            cwd=rocksdb_dir,
        )
    else:
        build_dir = accordin_rocksdb_build_dir(rocksdb_dir)
        logger.run(
            [
                "cmake",
                "-S",
                str(rocksdb_dir),
                "-B",
                str(build_dir),
                "-DCMAKE_BUILD_TYPE=Release",
                "-DFAIL_ON_WARNINGS=OFF",
                "-DWITH_BENCHMARK_TOOLS=ON",
                "-DWITH_CORE_TOOLS=OFF",
                "-DWITH_TOOLS=OFF",
                "-DWITH_TRACE_TOOLS=OFF",
                "-DWITH_TESTS=OFF",
                "-DWITH_JEMALLOC=OFF",
                "-DWITH_LIBURING=OFF",
                "-DWITH_SNAPPY=OFF",
                "-DWITH_LZ4=OFF",
                "-DWITH_ZLIB=OFF",
                "-DWITH_ZSTD=OFF",
                "-DWITH_BZ2=OFF",
            ],
            log_name="configure_rocksdb_db_bench.log",
            cwd=REPO_ROOT,
        )
        logger.run(
            [
                "cmake",
                "--build",
                str(build_dir),
                "--target",
                "db_bench",
                "--parallel",
            ],
            log_name="build_rocksdb_db_bench.log",
            cwd=REPO_ROOT,
        )

    rebuilt = default_rocksdb_db_bench(rocksdb_dir)
    if rebuilt is None or not rebuilt.is_file() or not os.access(rebuilt, os.X_OK):
        raise RuntimeError(f"RocksDB db_bench is still unavailable after build under {rocksdb_dir}")
    return rebuilt


def cmake_build_dir_matches_source(build_dir: Path, source_dir: Path) -> bool:
    cache_path = build_dir / "CMakeCache.txt"
    if not cache_path.is_file():
        return False
    expected = source_dir.resolve()
    try:
        for line in cache_path.read_text(encoding="utf-8", errors="replace").splitlines():
            if not line.startswith("CMAKE_HOME_DIRECTORY:INTERNAL="):
                continue
            configured = Path(line.split("=", 1)[1]).expanduser().resolve()
            return configured == expected
    except OSError:
        return False
    return False


def ensure_required_binaries(
    workloads: tuple[str, ...],
    *,
    args: argparse.Namespace,
    logger: experiment_three.CommandLogger,
) -> dict[str, Path]:
    binaries: dict[str, Path] = {}
    if "cachebench" in workloads:
        cachelib_dir = resolve_path(args.cachelib_dir) if args.cachelib_dir is not None else default_cachelib_dir()
        binaries["cachelib_dir"] = cachelib_dir
        binaries["cachebench"] = ensure_cachebench_bin(
            args.cachebench_bin,
            cachelib_dir=cachelib_dir,
            cachelib_build_jobs=args.cachelib_build_jobs,
            build_missing=args.build_missing,
            logger=logger,
        )
    if "rocksdb" in workloads:
        rocksdb_dir = resolve_path(args.rocksdb_dir) if args.rocksdb_dir is not None else default_rocksdb_dir()
        db_bench = (
            resolve_path(args.rocksdb_db_bench)
            if args.rocksdb_db_bench is not None
            else default_rocksdb_db_bench(rocksdb_dir)
        )
        binaries["rocksdb_dir"] = rocksdb_dir
        binaries["rocksdb_db_bench"] = ensure_rocksdb_db_bench(
            db_bench,
            rocksdb_dir=rocksdb_dir,
            build_missing=args.build_missing,
            logger=logger,
        )
    if "memcached" in workloads:
        binaries["memcached"] = executable_path(args.memcached_bin, "MEMCACHED_BIN", "memcached")
        binaries["memtier"] = executable_path(args.memtier_bin, "MEMTIER_BIN", "memtier_benchmark")
    return binaries


def format_float(value: float) -> str:
    return experiment_three.format_float(value)


def relative_log(result_root: Path, path: Path | None) -> str:
    if path is None:
        return ""
    return str(path.relative_to(result_root))


def ceil_div(numerator: int, denominator: int) -> int:
    return -(-numerator // denominator)


def safe_name(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]+", "_", value)


def parse_db_bench_output(output: str, benchmark: str) -> tuple[float | None, float | None]:
    fallback_latency: float | None = None
    fallback_ops: float | None = None
    for match in DB_BENCH_LATENCY_PATTERN.finditer(output):
        latency = float(match.group("micros"))
        ops = float(match.group("ops")) if match.group("ops") is not None else None
        if match.group("name") == benchmark:
            return latency, ops
        if fallback_latency is None:
            fallback_latency = latency
            fallback_ops = ops
    return fallback_latency, fallback_ops


def parse_generic_ops_per_second(output: str) -> float | None:
    for pattern in OPS_PATTERNS:
        match = pattern.search(output)
        if match is not None:
            return float(match.group("value"))
    return None


def parse_memtier_ops_per_second(output: str) -> float | None:
    totals_value: float | None = None
    for line in output.splitlines():
        stripped = line.strip()
        if not stripped.startswith("Totals"):
            continue
        fields = stripped.split()
        if len(fields) >= 2:
            try:
                totals_value = float(fields[1])
            except ValueError:
                return None
    if totals_value is not None and totals_value > 0.0:
        return totals_value

    progress_values = [
        float(match.group("value"))
        for match in MEMTIER_PROGRESS_AVG_PATTERN.finditer(output)
    ]
    if progress_values:
        return progress_values[-1]
    if totals_value is not None:
        return totals_value
    return parse_generic_ops_per_second(output)


def parse_cachebench_ops_per_second(output: str) -> float | None:
    values = [
        float(match.group("value").replace(",", ""))
        for match in CACHEBENCH_THROUGHPUT_PATTERN.finditer(output)
    ]
    if values:
        return sum(values)
    return parse_generic_ops_per_second(output)


def parse_log_wall_seconds(output: str) -> float | None:
    for line in reversed(output.splitlines()):
        if not line.startswith("wall_seconds:"):
            continue
        try:
            return float(line.split(":", 1)[1].strip())
        except ValueError:
            return None
    return None


def parse_log_returncode(output: str) -> int | None:
    for line in reversed(output.splitlines()):
        if not line.startswith("returncode:"):
            continue
        try:
            return int(line.split(":", 1)[1].strip())
        except ValueError:
            return None
    return None


def record_wall_seconds(record: dict[str, object], output: str) -> float:
    wall_value = record.get("wall_seconds")
    if isinstance(wall_value, (int, float)):
        return float(wall_value)
    parsed = parse_log_wall_seconds(output)
    if parsed is None:
        raise RuntimeError("Completed command log did not contain wall_seconds.")
    return parsed


def successful_records_by_log_name(
    records: list[dict[str, object]],
) -> dict[str, tuple[dict[str, object], str, Path]]:
    by_name: dict[str, tuple[dict[str, object], str, Path]] = {}
    for record in records:
        if record.get("returncode") != 0:
            continue
        log_path_text = record.get("log_path")
        if not isinstance(log_path_text, str):
            continue
        log_path = Path(log_path_text)
        if not log_path.is_file():
            continue
        try:
            output = log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if parse_log_returncode(output) != 0:
            continue
        by_name[log_path.name] = (record, output, log_path)
    return by_name


def result_key(row: dict[str, str]) -> tuple[str, str, str, int, int] | None:
    try:
        return (
            row["workload"],
            row["benchmark"],
            row["lock"],
            int(row["threads"]),
            int(row["repeat"]),
        )
    except (KeyError, ValueError):
        return None


def target_keys(
    *,
    workloads: tuple[str, ...],
    rocksdb_benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    repeats: int,
) -> list[tuple[str, str, str, int, int]]:
    keys: list[tuple[str, str, str, int, int]] = []
    for workload in workloads:
        if workload == "cachebench":
            benchmarks = ("cachebench",)
        elif workload == "rocksdb":
            benchmarks = rocksdb_benchmarks
        elif workload == "memcached":
            benchmarks = ("memtier",)
        else:
            continue
        for benchmark in benchmarks:
            for lock in locks:
                for thread in runnable_threads_for_lock(lock, threads):
                    for repeat in range(1, repeats + 1):
                        keys.append((workload, benchmark, lock, thread, repeat))
    return keys


def ordered_completed_rows(
    rows: list[dict[str, str]],
    ordered_targets: list[tuple[str, str, str, int, int]],
) -> list[dict[str, str]]:
    target_set = set(ordered_targets)
    rows_by_key: dict[tuple[str, str, str, int, int], dict[str, str]] = {}
    for row in rows:
        key = result_key(row)
        if key in target_set:
            rows_by_key[key] = row
    return [rows_by_key[key] for key in ordered_targets if key in rows_by_key]


def completed_keys_from_rows(rows: list[dict[str, str]]) -> set[tuple[str, str, str, int, int]]:
    keys: set[tuple[str, str, str, int, int]] = set()
    for row in rows:
        key = result_key(row)
        if key is not None:
            keys.add(key)
    return keys


def rocksdb_effective_total_ops(benchmark: str, thread: int, args: argparse.Namespace) -> int:
    if benchmark.startswith("read") or benchmark.startswith("seek"):
        return ceil_div(args.total_ops, thread) * thread
    return ceil_div(args.total_ops, thread) * thread


def completed_rows_from_records(
    result_root: Path,
    records: list[dict[str, object]],
    ordered_targets: list[tuple[str, str, str, int, int]],
    args: argparse.Namespace,
) -> list[dict[str, str]]:
    records_by_log = successful_records_by_log_name(records)
    init_benchmarks = set(init_existing_benchmarks(args.rocksdb_init_existing_benchmarks))
    rows: list[dict[str, str]] = []

    for workload, benchmark, lock, thread, repeat in ordered_targets:
        if workload == "cachebench":
            log_name = f"cachebench_{safe_name(lock)}_{thread:03d}_r{repeat}.log"
            completed = records_by_log.get(log_name)
            if completed is None:
                continue
            record, output, log_path = completed
            wall_seconds = record_wall_seconds(record, output)
            ops_per_second = parse_cachebench_ops_per_second(output)
            if ops_per_second is None and wall_seconds > 0.0:
                ops_per_second = args.cachebench_num_ops / wall_seconds
            rows.append(
                {
                    "workload": workload,
                    "benchmark": benchmark,
                    "lock": lock,
                    "threads": str(thread),
                    "repeat": str(repeat),
                    "ops_per_second": "" if ops_per_second is None else format_float(ops_per_second),
                    "latency_micros_per_op": "",
                    "wall_seconds": format_float(wall_seconds),
                    "setup_wall_seconds": "",
                    "total_ops": str(args.cachebench_num_ops),
                    "command_log": relative_log(result_root, log_path),
                    "setup_log": "",
                    "server_log": "",
                }
            )
        elif workload == "rocksdb":
            log_name = f"rocksdb_{safe_name(benchmark)}_{safe_name(lock)}_{thread:03d}_r{repeat}.log"
            completed = records_by_log.get(log_name)
            if completed is None:
                continue
            record, output, log_path = completed
            setup_completed = None
            if benchmark in init_benchmarks:
                setup_log_name = f"init_rocksdb_{safe_name(benchmark)}_{safe_name(lock)}_{thread:03d}_r{repeat}.log"
                setup_completed = records_by_log.get(setup_log_name)
            wall_seconds = record_wall_seconds(record, output)
            latency, parsed_ops = parse_db_bench_output(output, benchmark)
            ops_per_second = parsed_ops
            if ops_per_second is None and latency is not None and latency > 0.0:
                ops_per_second = 1_000_000.0 / latency
            total_ops = rocksdb_effective_total_ops(benchmark, thread, args)
            if ops_per_second is None and wall_seconds > 0.0:
                ops_per_second = total_ops / wall_seconds
            setup_wall_seconds = ""
            setup_log = ""
            if setup_completed is not None:
                setup_record, setup_output, setup_path = setup_completed
                setup_wall_seconds = format_float(record_wall_seconds(setup_record, setup_output))
                setup_log = relative_log(result_root, setup_path)
            rows.append(
                {
                    "workload": workload,
                    "benchmark": benchmark,
                    "lock": lock,
                    "threads": str(thread),
                    "repeat": str(repeat),
                    "ops_per_second": "" if ops_per_second is None else format_float(ops_per_second),
                    "latency_micros_per_op": "" if latency is None else format_float(latency),
                    "wall_seconds": format_float(wall_seconds),
                    "setup_wall_seconds": setup_wall_seconds,
                    "total_ops": str(total_ops),
                    "command_log": relative_log(result_root, log_path),
                    "setup_log": setup_log,
                    "server_log": "",
                }
            )
        elif workload == "memcached":
            log_name = f"memcached_{safe_name(lock)}_{thread:03d}_r{repeat}.log"
            completed = records_by_log.get(log_name)
            if completed is None:
                continue
            record, output, log_path = completed
            wall_seconds = record_wall_seconds(record, output)
            ops_per_second = parse_memtier_ops_per_second(output)
            total_ops = args.memtier_client_threads * args.memtier_clients * args.memtier_requests
            if ops_per_second is None and wall_seconds > 0.0:
                ops_per_second = total_ops / wall_seconds
            server_log = result_root / "server_logs" / log_name
            rows.append(
                {
                    "workload": workload,
                    "benchmark": benchmark,
                    "lock": lock,
                    "threads": str(thread),
                    "repeat": str(repeat),
                    "ops_per_second": "" if ops_per_second is None else format_float(ops_per_second),
                    "latency_micros_per_op": "",
                    "wall_seconds": format_float(wall_seconds),
                    "setup_wall_seconds": "",
                    "total_ops": str(total_ops),
                    "command_log": relative_log(result_root, log_path),
                    "setup_log": "",
                    "server_log": relative_log(result_root, server_log) if server_log.is_file() else "",
                }
            )
    return rows


def write_progress_csvs(result_root: Path, rows: list[dict[str, str]]) -> None:
    write_raw_csv(result_root, rows)
    write_summary_csv(result_root, summarize_rows(rows))


def write_cachebench_config(
    destination: Path,
    *,
    template: Path | None,
    threads: int,
    args: argparse.Namespace,
) -> None:
    if template is not None:
        with template.open("r", encoding="utf-8") as f:
            config = json.load(f)
    else:
        config = {
            "cache_config": {
                "cacheSizeMB": args.cachebench_cache_mb,
                "poolRebalanceIntervalSec": 1,
                "moveOnSlabRelease": False,
                "numPools": 1,
                "poolSizes": [1.0],
            },
            "test_config": {
                "numOps": args.cachebench_num_ops,
                "numThreads": threads,
                "numKeys": args.cachebench_num_keys,
                "distribution": "range",
                "opDelayBatch": 1,
                "opDelayNs": 0,
                "keySizeRange": [1, 8, 64],
                "keySizeRangeProbability": [0.3, 0.7],
                "valSizeRange": [1, 32, 1024],
                "valSizeRangeProbability": [0.2, 0.8],
                "getRatio": 0.8,
                "setRatio": 0.15,
                "delRatio": 0.05,
            },
        }

    test_config = config.setdefault("test_config", {})
    if not isinstance(test_config, dict):
        raise RuntimeError("CacheBench config must contain an object-valued test_config.")
    test_config["numThreads"] = threads
    if args.cachebench_num_ops > 0:
        test_config["numOps"] = args.cachebench_num_ops
    if args.cachebench_num_keys > 0:
        test_config["numKeys"] = args.cachebench_num_keys

    with destination.open("w", encoding="utf-8") as f:
        json.dump(config, f, indent=2)
        f.write("\n")


def cachebench_runtime_env(cachebench_bin: Path) -> dict[str, str] | None:
    lib_dirs: list[Path] = []
    install_root = cachebench_bin.parent.parent
    installed_root = install_root.parent

    for candidate in (install_root / "lib",):
        if candidate.is_dir():
            lib_dirs.append(candidate)
    if installed_root.name == "installed" and installed_root.is_dir():
        for child in installed_root.iterdir():
            lib_dir = child / "lib"
            if lib_dir.is_dir() and any(lib_dir.glob("*.so*")):
                lib_dirs.append(lib_dir)

    existing = os.environ.get("LD_LIBRARY_PATH", "").strip()
    paths = [str(path) for path in dict.fromkeys(lib_dirs)]
    if existing:
        paths.extend(part for part in existing.split(":") if part)
    if not paths:
        return None
    return {"LD_LIBRARY_PATH": ":".join(paths)}


def run_cachebench(
    result_root: Path,
    *,
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    repeats: int,
    cachebench_bin: Path,
    args: argparse.Namespace,
    logger: experiment_three.CommandLogger,
    existing_rows: list[dict[str, str]] | None = None,
) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = existing_rows if existing_rows is not None else []
    completed_keys = completed_keys_from_rows(rows)
    config_template = resolve_path(args.cachebench_config) if args.cachebench_config is not None else None
    if config_template is not None and not config_template.is_file():
        raise RuntimeError(f"CacheBench config does not exist: {config_template}")
    runtime_env = cachebench_runtime_env(cachebench_bin)

    config_dir = result_root / "cachebench_configs"
    config_dir.mkdir(parents=True, exist_ok=True)
    for lock in locks:
        for thread in runnable_threads_for_lock(lock, threads):
            for repeat in range(1, repeats + 1):
                key = ("cachebench", "cachebench", lock, thread, repeat)
                if key in completed_keys:
                    print(f"Skipping completed cachebench/{lock}/{thread}/r{repeat}", flush=True)
                    continue
                config_path = config_dir / f"cachebench_{safe_name(lock)}_{thread:03d}_r{repeat}.json"
                write_cachebench_config(config_path, template=config_template, threads=thread, args=args)
                command = [str(cachebench_bin), "--json_test_config", str(config_path)]
                if args.cachebench_timeout_seconds:
                    command.append(f"--timeout_seconds={args.cachebench_timeout_seconds}")
                command.extend(args.cachebench_extra_arg)
                command, env = build_lock_command(lock, command, extra_env=runtime_env)
                result = logger.run(
                    command,
                    log_name=f"cachebench_{safe_name(lock)}_{thread:03d}_r{repeat}.log",
                    cwd=REPO_ROOT,
                    env=env,
                )
                ops_per_second = parse_cachebench_ops_per_second(result.output)
                if ops_per_second is None and result.wall_seconds > 0.0:
                    ops_per_second = args.cachebench_num_ops / result.wall_seconds
                rows.append(
                    {
                        "workload": "cachebench",
                        "benchmark": "cachebench",
                        "lock": lock,
                        "threads": str(thread),
                        "repeat": str(repeat),
                        "ops_per_second": "" if ops_per_second is None else format_float(ops_per_second),
                        "latency_micros_per_op": "",
                        "wall_seconds": format_float(result.wall_seconds),
                        "setup_wall_seconds": "",
                        "total_ops": str(args.cachebench_num_ops),
                        "command_log": relative_log(result_root, result.log_path),
                        "setup_log": "",
                        "server_log": "",
                    }
                )
                completed_keys.add(key)
                write_progress_csvs(result_root, rows)
    return rows


def build_db_bench_args(
    *,
    db_bench: Path,
    db_path: Path,
    benchmark: str,
    threads: int,
    num: int,
    use_existing_db: bool,
    reads: int | None,
    compression_type: str,
    progress_reports: bool,
    extra_args: list[str],
) -> list[str]:
    command = [
        str(db_bench),
        f"--benchmarks={benchmark}",
        f"--threads={threads}",
        f"--num={num}",
        f"--db={db_path}",
        f"--use_existing_db={1 if use_existing_db else 0}",
    ]
    if reads is not None:
        command.append(f"--reads={reads}")
    if compression_type:
        command.append(f"--compression_type={compression_type}")
    if not progress_reports:
        command.append("--progress_reports=false")
    command.extend(extra_args)
    return command


def cleanup_path(path: Path) -> None:
    try:
        shutil.rmtree(path)
    except FileNotFoundError:
        return
    except PermissionError as exc:
        if shutil.which("sudo") is not None:
            completed = subprocess.run(
                ["sudo", "-n", "rm", "-rf", str(path)],
                cwd=str(REPO_ROOT),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            if completed.returncode == 0:
                return
        print(f"Warning: could not remove temporary path {path}: {exc}", file=sys.stderr)


def run_rocksdb(
    result_root: Path,
    *,
    benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    repeats: int,
    db_bench: Path,
    args: argparse.Namespace,
    logger: experiment_three.CommandLogger,
    existing_rows: list[dict[str, str]] | None = None,
) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = existing_rows if existing_rows is not None else []
    completed_keys = completed_keys_from_rows(rows)
    init_benchmarks = set(init_existing_benchmarks(args.rocksdb_init_existing_benchmarks))

    for benchmark in benchmarks:
        use_existing_db = benchmark in init_benchmarks
        uses_reads = benchmark.startswith("read") or benchmark.startswith("seek")
        for lock in locks:
            for thread in runnable_threads_for_lock(lock, threads):
                if uses_reads:
                    db_bench_num = args.db_num
                    reads_per_thread = ceil_div(args.total_ops, thread)
                    effective_total_ops = reads_per_thread * thread
                else:
                    db_bench_num = ceil_div(args.total_ops, thread)
                    reads_per_thread = None
                    effective_total_ops = db_bench_num * thread
                for repeat in range(1, repeats + 1):
                    key = ("rocksdb", benchmark, lock, thread, repeat)
                    if key in completed_keys:
                        print(f"Skipping completed rocksdb/{benchmark}/{lock}/{thread}/r{repeat}", flush=True)
                        continue
                    db_path = Path(tempfile.mkdtemp(prefix="experiment5_rocksdb_"))
                    setup_result: experiment_three.CommandResult | None = None
                    try:
                        if use_existing_db:
                            setup_command = build_db_bench_args(
                                db_bench=db_bench,
                                db_path=db_path,
                                benchmark=args.rocksdb_fill_benchmark,
                                threads=1,
                                num=args.db_num,
                                use_existing_db=False,
                                reads=None,
                                compression_type=args.rocksdb_compression_type,
                                progress_reports=args.rocksdb_progress_reports,
                                extra_args=args.rocksdb_extra_arg,
                            )
                            setup_result = logger.run(
                                setup_command,
                                log_name=(
                                    f"init_rocksdb_{safe_name(benchmark)}_{safe_name(lock)}_"
                                    f"{thread:03d}_r{repeat}.log"
                                ),
                                cwd=REPO_ROOT,
                            )

                        measured = build_db_bench_args(
                            db_bench=db_bench,
                            db_path=db_path,
                            benchmark=benchmark,
                            threads=thread,
                            num=db_bench_num,
                            use_existing_db=use_existing_db,
                            reads=reads_per_thread,
                            compression_type=args.rocksdb_compression_type,
                            progress_reports=args.rocksdb_progress_reports,
                            extra_args=args.rocksdb_extra_arg,
                        )
                        measured, env = build_lock_command(lock, measured)
                        result = logger.run(
                            measured,
                            log_name=f"rocksdb_{safe_name(benchmark)}_{safe_name(lock)}_{thread:03d}_r{repeat}.log",
                            cwd=REPO_ROOT,
                            env=env,
                        )
                        latency, parsed_ops = parse_db_bench_output(result.output, benchmark)
                        ops_per_second = parsed_ops
                        if ops_per_second is None and latency is not None and latency > 0.0:
                            ops_per_second = 1_000_000.0 / latency
                        if ops_per_second is None and result.wall_seconds > 0.0:
                            ops_per_second = effective_total_ops / result.wall_seconds
                        rows.append(
                            {
                                "workload": "rocksdb",
                                "benchmark": benchmark,
                                "lock": lock,
                                "threads": str(thread),
                                "repeat": str(repeat),
                                "ops_per_second": "" if ops_per_second is None else format_float(ops_per_second),
                                "latency_micros_per_op": "" if latency is None else format_float(latency),
                                "wall_seconds": format_float(result.wall_seconds),
                                "setup_wall_seconds": "" if setup_result is None else format_float(setup_result.wall_seconds),
                                "total_ops": str(effective_total_ops),
                                "command_log": relative_log(result_root, result.log_path),
                                "setup_log": relative_log(result_root, None if setup_result is None else setup_result.log_path),
                                "server_log": "",
                            }
                        )
                        completed_keys.add(key)
                        write_progress_csvs(result_root, rows)
                    finally:
                        cleanup_path(db_path)
    return rows


def choose_free_port(host: str) -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind((host, 0))
        return int(sock.getsockname()[1])


def wait_for_tcp(host: str, port: int, timeout_seconds: int) -> None:
    deadline = time.monotonic() + timeout_seconds
    last_error: OSError | None = None
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=0.2):
                return
        except OSError as exc:
            last_error = exc
            time.sleep(0.05)
    raise RuntimeError(f"Timed out waiting for memcached on {host}:{port}: {last_error}")


def terminate_process(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def command_uses_sudo(command: list[str]) -> bool:
    return bool(command) and Path(command[0]).name == "sudo"


def run_memcached(
    result_root: Path,
    *,
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    repeats: int,
    memcached_bin: Path,
    memtier_bin: Path,
    args: argparse.Namespace,
    logger: experiment_three.CommandLogger,
    existing_rows: list[dict[str, str]] | None = None,
) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = existing_rows if existing_rows is not None else []
    completed_keys = completed_keys_from_rows(rows)
    server_log_dir = result_root / "server_logs"
    server_log_dir.mkdir(parents=True, exist_ok=True)

    for lock in locks:
        for thread in runnable_threads_for_lock(lock, threads):
            for repeat in range(1, repeats + 1):
                key = ("memcached", "memtier", lock, thread, repeat)
                if key in completed_keys:
                    print(f"Skipping completed memcached/{lock}/{thread}/r{repeat}", flush=True)
                    continue
                port = args.memcached_port or choose_free_port(args.memcached_host)
                server_command = [
                    str(memcached_bin),
                    "-l",
                    args.memcached_host,
                    "-p",
                    str(port),
                    "-U",
                    "0",
                    "-t",
                    str(thread),
                    "-m",
                    str(args.memcached_memory_mb),
                ]
                locked_server_command, server_env = build_lock_command(lock, server_command)
                if command_uses_sudo(locked_server_command):
                    server_command.extend(["-u", args.memcached_user])
                    locked_server_command, server_env = build_lock_command(lock, server_command)

                server_log = server_log_dir / f"memcached_{safe_name(lock)}_{thread:03d}_r{repeat}.log"
                with server_log.open("w", encoding="utf-8") as server_log_file:
                    server_log_file.write(f"command: {' '.join(locked_server_command)}\n\n")
                    server_log_file.flush()
                    run_env = os.environ.copy()
                    if server_env is not None:
                        run_env.update(server_env)
                    server_process = subprocess.Popen(
                        locked_server_command,
                        cwd=str(REPO_ROOT),
                        env=run_env,
                        stdout=server_log_file,
                        stderr=subprocess.STDOUT,
                        text=True,
                    )
                    try:
                        wait_for_tcp(args.memcached_host, port, args.memcached_start_timeout)
                        client_command = [
                            str(memtier_bin),
                            "-s",
                            args.memcached_host,
                            "-p",
                            str(port),
                            "--protocol=memcache_text",
                            "-t",
                            str(args.memtier_client_threads),
                            "-c",
                            str(args.memtier_clients),
                            "--requests",
                            str(args.memtier_requests),
                            "--ratio",
                            args.memtier_ratio,
                            "--key-pattern",
                            args.memtier_key_pattern,
                            "--data-size",
                            str(args.memtier_data_size),
                            "--hide-histogram",
                        ]
                        client_command.extend(args.memtier_extra_arg)
                        result = logger.run(
                            client_command,
                            log_name=f"memcached_{safe_name(lock)}_{thread:03d}_r{repeat}.log",
                            cwd=REPO_ROOT,
                        )
                        ops_per_second = parse_memtier_ops_per_second(result.output)
                        total_ops = args.memtier_client_threads * args.memtier_clients * args.memtier_requests
                        if ops_per_second is None and result.wall_seconds > 0.0:
                            ops_per_second = total_ops / result.wall_seconds
                        rows.append(
                            {
                                "workload": "memcached",
                                "benchmark": "memtier",
                                "lock": lock,
                                "threads": str(thread),
                                "repeat": str(repeat),
                                "ops_per_second": "" if ops_per_second is None else format_float(ops_per_second),
                                "latency_micros_per_op": "",
                                "wall_seconds": format_float(result.wall_seconds),
                                "setup_wall_seconds": "",
                                "total_ops": str(total_ops),
                                "command_log": relative_log(result_root, result.log_path),
                                "setup_log": "",
                                "server_log": relative_log(result_root, server_log),
                            }
                        )
                        completed_keys.add(key)
                        write_progress_csvs(result_root, rows)
                    finally:
                        terminate_process(server_process)
                        server_log_file.write(f"\nserver_returncode: {server_process.returncode}\n")

    return rows


def mean_field(rows: list[dict[str, str]], field: str) -> str:
    values = [float(row[field]) for row in rows if row[field].strip()]
    if not values:
        return ""
    return format_float(mean(values))


def summarize_rows(rows: list[dict[str, str]]) -> list[dict[str, str]]:
    groups: dict[tuple[str, str, str, int], list[dict[str, str]]] = {}
    for row in rows:
        key = (row["workload"], row["benchmark"], row["lock"], int(row["threads"]))
        groups.setdefault(key, []).append(row)

    summary_rows: list[dict[str, str]] = []
    for workload, benchmark, lock, thread_count in sorted(
        groups.keys(),
        key=lambda item: (workload_sort_key(item[0]), benchmark_sort_key(item[0], item[1]), lock_sort_key(item[2]), item[3]),
    ):
        group_rows = groups[(workload, benchmark, lock, thread_count)]
        summary_rows.append(
            {
                "workload": workload,
                "benchmark": benchmark,
                "lock": lock,
                "threads": str(thread_count),
                "mean_ops_per_second": mean_field(group_rows, "ops_per_second"),
                "mean_latency_micros_per_op": mean_field(group_rows, "latency_micros_per_op"),
                "mean_wall_seconds": mean_field(group_rows, "wall_seconds"),
                "mean_setup_wall_seconds": mean_field(group_rows, "setup_wall_seconds"),
                "runs": str(len(group_rows)),
            }
        )
    return summary_rows


def write_raw_csv(result_root: Path, rows: list[dict[str, str]]) -> Path:
    path = result_root / "raw.csv"
    with path.open("w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=RAW_FIELDS)
        writer.writeheader()
        writer.writerows(rows)
    return path


def load_raw_rows(result_root: Path) -> list[dict[str, str]]:
    path = result_root / "raw.csv"
    if not path.is_file():
        raise RuntimeError(f"raw.csv was not found under {result_root}")
    with path.open("r", encoding="utf-8", newline="") as f:
        reader = csv.DictReader(f)
        if reader.fieldnames is None:
            raise RuntimeError(f"raw.csv is missing a header: {path}")
        missing = [field for field in RAW_FIELDS if field not in reader.fieldnames]
        if missing:
            raise RuntimeError(f"raw.csv is missing required columns: {', '.join(missing)}")
        return [dict(row) for row in reader]


def write_summary_csv(result_root: Path, rows: list[dict[str, str]]) -> Path:
    path = result_root / "summary.csv"
    with path.open("w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=SUMMARY_FIELDS)
        writer.writeheader()
        writer.writerows(rows)
    return path


def unique_threads(summary_rows: list[dict[str, str]], workload: str, benchmark: str) -> list[int]:
    return sorted(
        {
            int(row["threads"])
            for row in summary_rows
            if row["workload"] == workload and row["benchmark"] == benchmark
        }
    )


def plot_ops(
    summary_rows: list[dict[str, str]],
    *,
    workload: str,
    benchmark: str,
    output_path: Path,
) -> None:
    try:
        import matplotlib
    except ModuleNotFoundError as exc:
        raise RuntimeError("matplotlib is required to generate plots. Use --skip-plots to only write CSVs.") from exc

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.ticker import ScalarFormatter

    rows = [
        row
        for row in summary_rows
        if row["workload"] == workload and row["benchmark"] == benchmark and row["mean_ops_per_second"].strip()
    ]
    if not rows:
        raise RuntimeError(f"No summary rows available for {workload}/{benchmark}.")

    fig, ax = plt.subplots(figsize=(9.5, 5.5))
    thread_values = unique_threads(summary_rows, workload, benchmark)
    lock_keys = sorted({row["lock"] for row in rows}, key=lock_sort_key)
    for lock in lock_keys:
        points = [
            (int(row["threads"]), float(row["mean_ops_per_second"]))
            for row in rows
            if row["lock"] == lock
        ]
        if not points:
            continue
        points.sort()
        ax.plot(
            [thread for thread, _ in points],
            [value for _, value in points],
            marker="o",
            linewidth=1.8,
            markersize=4,
            label=lock_label(lock),
        )

    ax.set_title(f"Throughput vs Threads: {workload_label(workload, benchmark)}")
    ax.set_xlabel("Threads")
    ax.set_ylabel("Mean throughput (ops/s, higher is better)")
    experiment_three.add_thread_axis_formatting(ax, thread_values)
    ax.xaxis.set_major_formatter(ScalarFormatter())
    ax.grid(True, axis="y", alpha=0.28)
    ax.grid(True, axis="x", which="major", alpha=0.16)
    ax.legend(frameon=False)
    fig.tight_layout()
    fig.savefig(output_path, dpi=180)
    plt.close(fig)


def write_plots(result_root: Path, summary_rows: list[dict[str, str]], *, skip_plots: bool) -> list[Path]:
    if skip_plots:
        return []
    plot_paths: list[Path] = []
    targets = sorted(
        {(row["workload"], row["benchmark"]) for row in summary_rows},
        key=lambda item: (workload_sort_key(item[0]), benchmark_sort_key(item[0], item[1])),
    )
    for workload, benchmark in targets:
        output_path = result_root / f"ops_vs_threads_{safe_name(workload)}_{safe_name(benchmark)}.png"
        plot_ops(summary_rows, workload=workload, benchmark=benchmark, output_path=output_path)
        plot_paths.append(output_path)
    return plot_paths


def write_settings(
    result_root: Path,
    *,
    workloads: tuple[str, ...],
    rocksdb_benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    binaries: dict[str, Path],
    args: argparse.Namespace,
) -> None:
    settings = {
        "workloads": list(workloads),
        "rocksdb_benchmarks": list(rocksdb_benchmarks),
        "locks": [{"key": lock, "label": lock_label(lock)} for lock in locks],
        "lock_profile": args.lock_profile,
        "lock_profile_source": "manual" if args.locks is not None else "profile",
        "threads": list(threads),
        "machine_profile": experiment_defaults.ACTIVE_MACHINE_CONFIG.name,
        "machine_profile_env": experiment_defaults.PROFILE_ENV,
        "single_oversubscribed_locks": list(SINGLE_OVERSUBSCRIBED_LOCKS),
        "per_lock_max_threads": per_lock_max_threads_for_settings(locks, threads),
        "runnable_threads_by_lock": {lock: list(runnable_threads_for_lock(lock, threads)) for lock in locks},
        "repeats": args.repeats,
        "total_ops": args.total_ops,
        "db_num": args.db_num,
        "build_missing": args.build_missing,
        "binaries": {key: str(value) for key, value in binaries.items()},
        "flexguard_dir": str(FLEXGUARD_DIR),
        "machine_core_count": experiment_defaults.MACHINE_CORE_COUNT,
        "cachebench": {
            "cachelib_dir": str(binaries.get("cachelib_dir", "")),
            "cachelib_build_jobs": args.cachelib_build_jobs,
            "config": "" if args.cachebench_config is None else str(resolve_path(args.cachebench_config)),
            "num_ops": args.cachebench_num_ops,
            "num_keys": args.cachebench_num_keys,
            "cache_mb": args.cachebench_cache_mb,
            "timeout_seconds": args.cachebench_timeout_seconds,
            "extra_args": list(args.cachebench_extra_arg),
        },
        "rocksdb": {
            "fill_benchmark": args.rocksdb_fill_benchmark,
            "compression_type": args.rocksdb_compression_type,
            "progress_reports": args.rocksdb_progress_reports,
            "init_existing_benchmarks": list(init_existing_benchmarks(args.rocksdb_init_existing_benchmarks)),
            "extra_args": list(args.rocksdb_extra_arg),
        },
        "memcached": {
            "host": args.memcached_host,
            "port": args.memcached_port,
            "memory_mb": args.memcached_memory_mb,
            "memtier_client_threads": args.memtier_client_threads,
            "memtier_clients": args.memtier_clients,
            "memtier_requests": args.memtier_requests,
            "memtier_ratio": args.memtier_ratio,
            "memtier_key_pattern": args.memtier_key_pattern,
            "memtier_data_size": args.memtier_data_size,
            "memtier_extra_args": list(args.memtier_extra_arg),
        },
    }
    with (result_root / "settings.json").open("w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")


def print_outputs(result_root: Path, raw_path: Path, summary_path: Path, plot_paths: Iterable[Path]) -> None:
    print(f"Result root: {result_root}")
    print(f"Raw CSV: {raw_path}")
    print(f"Summary CSV: {summary_path}")
    for plot_path in plot_paths:
        print(f"Plot: {plot_path}")


def main() -> int:
    args = parse_args()
    try:
        if args.output_root is not None and args.plot_only is not None:
            print("--output-root cannot be used together with --plot-only.", file=sys.stderr)
            return 2
        if args.resume and args.plot_only is not None:
            print("--resume cannot be used together with --plot-only.", file=sys.stderr)
            return 2
        if args.resume and args.output_root is None:
            print("--resume requires --output-root.", file=sys.stderr)
            return 2

        workloads = validate_workloads(parse_csv_strings(args.workloads))
        rocksdb_benchmarks = validate_benchmark_names(parse_csv_strings(args.rocksdb_benchmarks))
        locks = experiment_defaults.resolve_locks(
            profile=args.lock_profile,
            locks=None if args.locks is None else parse_csv_strings(args.locks),
        )
        threads = parse_csv_ints(args.threads)
        validate_benchmark_names((args.rocksdb_fill_benchmark,))
        init_existing_benchmarks(args.rocksdb_init_existing_benchmarks)

        if args.plot_only is not None:
            result_root = resolve_path(args.plot_only)
            if not result_root.is_dir():
                print(f"Plot-only result root does not exist: {result_root}", file=sys.stderr)
                return 2
            raw_rows = load_raw_rows(result_root)
            raw_path = result_root / "raw.csv"
            summary_rows = summarize_rows(raw_rows)
            summary_path = write_summary_csv(result_root, summary_rows)
            plot_paths = write_plots(result_root, summary_rows, skip_plots=args.skip_plots)
            print_outputs(result_root, raw_path, summary_path, plot_paths)
            return 0

        result_root = resolve_path(args.output_root) if args.output_root is not None else default_result_root()
        experiment_three.ensure_output_root(result_root, args.force, args.resume)
        logger = experiment_three.CommandLogger(result_root, resume=args.resume)

        ensure_lock_helpers(locks, build_missing=args.build_missing, logger=logger)
        binaries = ensure_required_binaries(workloads, args=args, logger=logger)
        write_settings(
            result_root,
            workloads=workloads,
            rocksdb_benchmarks=rocksdb_benchmarks,
            locks=locks,
            threads=threads,
            binaries=binaries,
            args=args,
        )

        ordered_targets = target_keys(
            workloads=workloads,
            rocksdb_benchmarks=rocksdb_benchmarks,
            locks=locks,
            threads=threads,
            repeats=args.repeats,
        )
        existing_rows: list[dict[str, str]] = []
        if args.resume:
            if (result_root / "raw.csv").is_file():
                existing_rows.extend(load_raw_rows(result_root))
            existing_rows.extend(completed_rows_from_records(result_root, logger.records, ordered_targets, args))
            existing_rows = ordered_completed_rows(existing_rows, ordered_targets)
        raw_rows: list[dict[str, str]] = list(existing_rows)
        if raw_rows:
            write_progress_csvs(result_root, raw_rows)
        if "cachebench" in workloads:
            run_cachebench(
                result_root,
                locks=locks,
                threads=threads,
                repeats=args.repeats,
                cachebench_bin=binaries["cachebench"],
                args=args,
                logger=logger,
                existing_rows=raw_rows,
            )
        if "rocksdb" in workloads:
            run_rocksdb(
                result_root,
                benchmarks=rocksdb_benchmarks,
                locks=locks,
                threads=threads,
                repeats=args.repeats,
                db_bench=binaries["rocksdb_db_bench"],
                args=args,
                logger=logger,
                existing_rows=raw_rows,
            )
        if "memcached" in workloads:
            run_memcached(
                result_root,
                locks=locks,
                threads=threads,
                repeats=args.repeats,
                memcached_bin=binaries["memcached"],
                memtier_bin=binaries["memtier"],
                args=args,
                logger=logger,
                existing_rows=raw_rows,
            )

        raw_path = write_raw_csv(result_root, raw_rows)
        summary_rows = summarize_rows(raw_rows)
        summary_path = write_summary_csv(result_root, summary_rows)
        plot_paths = write_plots(result_root, summary_rows, skip_plots=args.skip_plots)
        print_outputs(result_root, raw_path, summary_path, plot_paths)
        return 0
    except experiment_three.CommandError as exc:
        print(str(exc), file=sys.stderr)
        print(f"Command log: {exc.log_path}", file=sys.stderr)
        return exc.returncode
    except Exception as exc:
        print(str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
