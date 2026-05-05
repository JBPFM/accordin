#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import hashlib
import json
import math
import os
import re
import signal
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from statistics import mean, pstdev
from typing import Iterable

import experiment_defaults
import experiment_failures
import run_experiment_three as experiment_three


REPO_ROOT = experiment_three.REPO_ROOT
FLEXGUARD_DIR = experiment_three.FLEXGUARD_DIR
DEFAULT_CACHELIB_DIR = REPO_ROOT / "third_party" / "CacheLib"
DEFAULT_CACHELIB_LIBURING_PREFIX = REPO_ROOT / ".cache" / "experiment5_liburing"
DEFAULT_ROCKSDB_DIR = REPO_ROOT / "third_party" / "rocksdb"
ROCKSDB_GIT_URL = "https://github.com/facebook/rocksdb.git"

DEFAULT_WORKLOADS = ("cachebench", "rocksdb", "memcached")
DEFAULT_ROCKSDB_BENCHMARKS = ("readrandom", "fillrandom")
DEFAULT_ROCKSDB_COMPRESSION_TYPE = "none"
DEFAULT_ROCKSDB_EXTRA_ARGS = ("--disable_auto_compactions=true",)
DEFAULT_LOCK_PROFILE = experiment_defaults.DEFAULT_LOCK_PROFILE
DEFAULT_LOCKS = experiment_defaults.DEFAULT_LOCKS
FULL_LOCKS = experiment_defaults.FULL_LOCKS
MINIMAL_LOCKS = experiment_defaults.MINIMAL_LOCKS
DEFAULT_THREADS = experiment_defaults.DEFAULT_THREADS
DEFAULT_REPEATS = experiment_defaults.DEFAULT_REPEATS
DEFAULT_TOTAL_OPS = 1_572_864
DEFAULT_DB_NUM = 500_000
DEFAULT_CACHEBENCH_NUM_OPS = 500_000
DEFAULT_CACHEBENCH_NUM_KEYS = 1_000_000
DEFAULT_CACHEBENCH_CACHE_MB = 512
DEFAULT_CACHEBENCH_POOL_REBALANCE_INTERVAL_SEC = 0
DEFAULT_CACHEBENCH_COMMAND_TIMEOUT_SECONDS = 300
DEFAULT_COMMAND_TIMEOUT_SECONDS = 300
COMMAND_TIMEOUT_KILL_AFTER_SECONDS = 60
DEFAULT_CACHELIB_BUILD_JOBS = os.cpu_count() or 1
DEFAULT_MEMCACHED_HOST = "127.0.0.1"
DEFAULT_MEMCACHED_MEMORY_MB = 512
DEFAULT_MEMCACHED_CONN_LIMIT = 0
DEFAULT_MEMTIER_CLIENT_THREADS = 48
DEFAULT_MEMTIER_CLIENTS = 32
DEFAULT_MEMTIER_REQUESTS = 10_000
DEFAULT_MEMTIER_RATIO = "1:10"
DEFAULT_MEMTIER_KEY_PATTERN = "R:R"
DEFAULT_MEMTIER_FILL_KEY_PATTERN = "P:P"
DEFAULT_MEMTIER_KEY_MINIMUM = 1
DEFAULT_MEMTIER_KEY_MAXIMUM = 10_000
DEFAULT_MEMTIER_WARMUP_REQUESTS = 1_000
DEFAULT_MEMTIER_DATA_SIZE = 128
MEMCACHED_SYSTEM_PACKAGES = {
    "memcached": "memcached",
    "memtier_benchmark": "memtier-benchmark",
}

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
OUTLIER_FIELDS = (
    "kind",
    "workload",
    "benchmark",
    "lock",
    "threads",
    "score",
    "direction",
    "mean_ops_per_second",
    "cv_percent",
    "runs",
    "sample_ops_per_second",
    "sample_wall_seconds",
    "command_logs",
    "setup_logs",
    "server_logs",
    "neighbor_threads",
    "neighbor_ops_per_second",
    "note",
)

REPEAT_OUTLIER_MIN_RUNS = 3
REPEAT_OUTLIER_CV_THRESHOLD_PERCENT = 15.0
REPEAT_OUTLIER_MAX_MIN_RATIO_THRESHOLD = 2.0
LOCAL_SHAPE_RATIO_THRESHOLD = 2.0

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


def root_command(command: list[str], *, env: dict[str, str] | None = None) -> list[str]:
    full_command = command
    if env:
        full_command = ["env", *(f"{key}={value}" for key, value in env.items()), *command]
    if hasattr(os, "geteuid") and os.geteuid() == 0:
        return full_command
    sudo = shutil.which("sudo")
    if sudo is None:
        raise RuntimeError(f"Cannot run as root because sudo is unavailable: {' '.join(command)}")
    return [sudo, "-n", *full_command]


def install_system_packages(
    packages: tuple[str, ...],
    *,
    log_prefix: str,
    logger: experiment_three.CommandLogger,
) -> None:
    packages = tuple(dict.fromkeys(package for package in packages if package))
    if not packages:
        return

    apt_get = shutil.which("apt-get")
    if apt_get is None:
        raise RuntimeError(
            "Cannot install missing system packages automatically because apt-get was not found: "
            + ", ".join(packages)
        )

    env = {"DEBIAN_FRONTEND": "noninteractive"}
    logger.run(
        root_command([apt_get, "update"], env=env),
        log_name=f"{log_prefix}_apt_update.log",
        cwd=REPO_ROOT,
        timeout_seconds=0,
    )
    logger.run(
        root_command([apt_get, "install", "-y", *packages], env=env),
        log_name=f"{log_prefix}_apt_install.log",
        cwd=REPO_ROOT,
        timeout_seconds=0,
    )


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
    getdeps_build_dir = cachelib_getdeps_build_dir(cachelib_dir, "cachelib")
    if getdeps_build_dir is not None:
        candidates.append(getdeps_build_dir / "cachebench" / "cachebench")
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
    env.update(cachelib_compiler_env())
    existing_ldflags = os.environ.get("LDFLAGS", "")
    if "-latomic" not in existing_ldflags.split():
        env["LDFLAGS"] = " ".join(part for part in (existing_ldflags, "-latomic") if part)
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

    include_paths: list[str] = []
    liburing_prefix = cachelib_local_liburing_prefix()
    if liburing_prefix is not None:
        include_paths.append(str(liburing_prefix / "include"))
    include_override = cachelib_include_override_dir()
    if include_override is not None:
        include_paths.append(str(include_override))
    if include_paths:
        for key in ("C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH"):
            existing = env.get(key, os.environ.get(key, ""))
            parts = [
                *include_paths,
                *(part for part in existing.split(os.pathsep) if part and part not in include_paths),
            ]
            env[key] = os.pathsep.join(parts)

    library_paths = []
    if liburing_prefix is not None:
        library_paths.append(str(liburing_prefix / "lib"))
    library_paths.append("/usr/lib/x86_64-linux-gnu")
    for key in ("LIBRARY_PATH", "LD_LIBRARY_PATH"):
        existing = env.get(key, os.environ.get(key, ""))
        parts = [
            *library_paths,
            *(part for part in existing.split(os.pathsep) if part and part not in library_paths),
        ]
        env[key] = os.pathsep.join(parts)
    return env


def cachelib_compiler_env() -> dict[str, str]:
    clang = shutil.which("clang")
    clangxx = shutil.which("clang++")
    if clang is None or clangxx is None:
        return {}

    env: dict[str, str] = {}
    if "CC" not in os.environ:
        env["CC"] = clang
    if "CXX" not in os.environ:
        env["CXX"] = clangxx
    return env


def cachelib_local_liburing_prefix() -> Path | None:
    prefix = DEFAULT_CACHELIB_LIBURING_PREFIX
    if (
        (prefix / "include" / "liburing.h").is_file()
        and (prefix / "include" / "liburing" / "io_uring.h").is_file()
        and (prefix / "lib" / "liburing.so").is_file()
    ):
        return prefix
    return None


def cachelib_extra_cmake_defines() -> dict[str, str]:
    defines = {
        "CMAKE_C_COMPILER": os.environ.get("CC", shutil.which("clang") or ""),
        "CMAKE_CXX_COMPILER": os.environ.get("CXX", shutil.which("clang++") or ""),
    }
    defines = {key: value for key, value in defines.items() if value}

    liburing_prefix = cachelib_local_liburing_prefix()
    if liburing_prefix is not None:
        defines.update(
            {
                "LIBURING_INCLUDE_DIR": str(liburing_prefix / "include"),
                "LIBURING_LIBRARY": str(liburing_prefix / "lib" / "liburing.so"),
            }
        )
    return defines


def cachelib_getdeps_build_dir(cachelib_dir: Path, dependency: str) -> Path | None:
    getdeps = cachelib_dir / "build" / "fbcode_builder" / "getdeps.py"
    if not getdeps.is_file():
        return None
    try:
        result = subprocess.run(
            ["python3", "./build/fbcode_builder/getdeps.py", "show-build-dir", dependency],
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


def remove_stale_cachelib_cmake_build(
    cachelib_dir: Path,
    dependency: str,
    *,
    expected_defines: dict[str, str],
) -> None:
    build_dir = cachelib_getdeps_build_dir(cachelib_dir, dependency)
    if build_dir is None:
        return
    cmake_cache = build_dir / "CMakeCache.txt"
    if not cmake_cache.is_file():
        return
    try:
        cache_text = cmake_cache.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return

    stale = False
    for key, expected_value in expected_defines.items():
        define_pattern = re.compile(rf"^{re.escape(key)}:[^=]*={re.escape(expected_value)}$", re.MULTILINE)
        if not define_pattern.search(cache_text):
            stale = True
            break

    if stale:
        shutil.rmtree(build_dir)


def cachelib_include_override_dir() -> Path | None:
    local_jemalloc = Path("/usr/local/include/jemalloc/jemalloc.h")
    system_jemalloc = Path("/usr/include/jemalloc/jemalloc.h")
    try:
        if (
            not local_jemalloc.is_file()
            or "MALLCTL_ARENAS_ALL" in local_jemalloc.read_text(encoding="utf-8", errors="replace")
            or "MALLCTL_ARENAS_ALL" not in system_jemalloc.read_text(encoding="utf-8", errors="replace")
        ):
            return None
    except OSError:
        return None

    include_dir = REPO_ROOT / ".cache" / "experiment5_include_overrides"
    override_header = include_dir / "jemalloc" / "jemalloc.h"
    override_header.parent.mkdir(parents=True, exist_ok=True)
    if override_header.exists() or override_header.is_symlink():
        override_header.unlink()
    try:
        override_header.symlink_to(system_jemalloc)
    except OSError:
        shutil.copy2(system_jemalloc, override_header)
    return include_dir


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
        timeout_seconds=0,
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

    command = [
        "python3",
        "./build/fbcode_builder/getdeps.py",
        "--allow-system-packages",
        "--num-jobs",
        str(jobs),
        "build",
    ]
    extra_cmake_defines = cachelib_extra_cmake_defines()
    remove_stale_cachelib_cmake_build(cachelib_dir, "folly", expected_defines=extra_cmake_defines)
    remove_stale_cachelib_cmake_build(cachelib_dir, "cachelib", expected_defines=extra_cmake_defines)
    if extra_cmake_defines:
        command.extend(["--extra-cmake-defines", json.dumps(extra_cmake_defines, sort_keys=True)])
    command.extend(["--cmake-target", "cachebench", "--no-tests", "cachelib"])
    logger.run(
        command,
        log_name="build_cachelib_cachebench.log",
        cwd=cachelib_dir,
        env=cachelib_getdeps_env(),
        timeout_seconds=0,
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
        DEFAULT_ROCKSDB_DIR,
        Path("/home/jz/Projects/tests/rocksdb"),
    )
    for candidate in candidates:
        if (candidate / "CMakeLists.txt").is_file() or (candidate / "Makefile").is_file():
            return candidate.resolve()
    return candidates[0].resolve()


def rocksdb_source_is_available(rocksdb_dir: Path) -> bool:
    return (
        (rocksdb_dir / "CMakeLists.txt").is_file()
        and (
            (rocksdb_dir / "tools" / "db_bench_tool.cc").is_file()
            or (rocksdb_dir / "tools" / "db_bench.cc").is_file()
        )
    )


def ensure_rocksdb_source(
    rocksdb_dir: Path,
    *,
    build_missing: bool,
    logger: experiment_three.CommandLogger,
) -> None:
    if rocksdb_source_is_available(rocksdb_dir):
        return
    if not build_missing:
        raise RuntimeError(
            f"RocksDB source directory is missing or incomplete: {rocksdb_dir}. "
            "Rerun with --build-missing or set ROCKSDB_HOME/--rocksdb-dir."
        )
    if rocksdb_dir != DEFAULT_ROCKSDB_DIR.resolve():
        raise RuntimeError(f"Cannot initialize a custom RocksDB source directory: {rocksdb_dir}")

    rocksdb_dir.parent.mkdir(parents=True, exist_ok=True)
    logger.run(
        [
            "git",
            "clone",
            "--depth",
            "1",
            ROCKSDB_GIT_URL,
            str(DEFAULT_ROCKSDB_DIR.relative_to(REPO_ROOT)),
        ],
        log_name="init_rocksdb_source.log",
        cwd=REPO_ROOT,
        timeout_seconds=0,
    )
    if not rocksdb_source_is_available(rocksdb_dir):
        raise RuntimeError(f"RocksDB source directory is still unavailable after clone: {rocksdb_dir}")


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
  cachebench_pool_rebalance_interval_sec={DEFAULT_CACHEBENCH_POOL_REBALANCE_INTERVAL_SEC}
  rocksdb_benchmarks={','.join(DEFAULT_ROCKSDB_BENCHMARKS)}
  rocksdb_extra_args={' '.join(DEFAULT_ROCKSDB_EXTRA_ARGS)}
  memtier=-t {DEFAULT_MEMTIER_CLIENT_THREADS} -c {DEFAULT_MEMTIER_CLIENTS} --requests {DEFAULT_MEMTIER_REQUESTS}
  memtier_key_range={DEFAULT_MEMTIER_KEY_MINIMUM}..{DEFAULT_MEMTIER_KEY_MAXIMUM}, warmup_requests={DEFAULT_MEMTIER_WARMUP_REQUESTS}
  per_lock_max_threads={','.join(f"{lock}:{max_threads}" for lock, max_threads in PER_LOCK_MAX_THREADS.items())}

Examples:
  python3 experiments/run_experiment_five.py --workloads rocksdb --locks stock --threads 1 --repeats 1 --rocksdb-benchmarks fillrandom --total-ops 1000
  python3 experiments/run_experiment_five.py --workloads memcached --locks stock --threads 4 --repeats 1 --memtier-requests 1000
  python3 experiments/run_experiment_five.py --workloads cachebench,rocksdb,memcached --build-missing
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
    parser.add_argument(
        "--build-missing",
        action="store_true",
        help="Build/install missing lock helpers, CacheBench, RocksDB db_bench, or memcached/memtier where possible.",
    )
    parser.add_argument("--skip-plots", action="store_true", help="Write CSVs but skip PNG generation.")
    parser.add_argument(
        "--command-timeout-seconds",
        type=non_negative_int,
        default=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        help=(
            "Outer timeout for each non-CacheBench workload command. 0 disables it. "
            f"Default: {DEFAULT_COMMAND_TIMEOUT_SECONDS}."
        ),
    )
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
    parser.add_argument(
        "--cachebench-pool-rebalance-interval-sec",
        type=non_negative_int,
        default=DEFAULT_CACHEBENCH_POOL_REBALANCE_INTERVAL_SEC,
        help=(
            "Value written to cache_config.poolRebalanceIntervalSec in generated CacheBench JSON. "
            f"Default: {DEFAULT_CACHEBENCH_POOL_REBALANCE_INTERVAL_SEC}."
        ),
    )
    parser.add_argument("--cachebench-timeout-seconds", type=non_negative_int, default=0, help="Pass --timeout_seconds when non-zero.")
    parser.add_argument(
        "--cachebench-command-timeout-seconds",
        type=non_negative_int,
        default=DEFAULT_CACHEBENCH_COMMAND_TIMEOUT_SECONDS,
        help=(
            "Outer timeout for each cachebench command. 0 disables it. "
            "Timed-out commands are accepted only when final CacheBench throughput results were already printed."
        ),
    )
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
    parser.add_argument(
        "--rocksdb-extra-arg",
        action="append",
        default=list(DEFAULT_ROCKSDB_EXTRA_ARGS),
        help=(
            "Extra argument passed through to db_bench; repeatable. "
            f"Default: {' '.join(DEFAULT_ROCKSDB_EXTRA_ARGS)}."
        ),
    )

    parser.add_argument("--memcached-bin", type=Path, default=None, help="Path to memcached. Default: MEMCACHED_BIN or PATH.")
    parser.add_argument("--memtier-bin", type=Path, default=None, help="Path to memtier_benchmark. Default: MEMTIER_BIN or PATH.")
    parser.add_argument("--memcached-host", default=DEFAULT_MEMCACHED_HOST)
    parser.add_argument("--memcached-port", type=non_negative_int, default=0, help="0 chooses a free localhost port per run.")
    parser.add_argument("--memcached-memory-mb", type=positive_int, default=DEFAULT_MEMCACHED_MEMORY_MB)
    parser.add_argument(
        "--memcached-conn-limit",
        type=non_negative_int,
        default=DEFAULT_MEMCACHED_CONN_LIMIT,
        help="Passed to memcached as -c. Default 0 auto-sizes from server threads.",
    )
    parser.add_argument("--memcached-user", default=os.environ.get("USER", "nobody"), help="User passed with -u when memcached is launched through sudo.")
    parser.add_argument("--memcached-start-timeout", type=positive_int, default=10)
    parser.add_argument("--memtier-client-threads", type=positive_int, default=DEFAULT_MEMTIER_CLIENT_THREADS)
    parser.add_argument("--memtier-clients", type=positive_int, default=DEFAULT_MEMTIER_CLIENTS)
    parser.add_argument("--memtier-requests", type=positive_int, default=DEFAULT_MEMTIER_REQUESTS)
    parser.add_argument("--memtier-ratio", default=DEFAULT_MEMTIER_RATIO)
    parser.add_argument("--memtier-key-pattern", default=DEFAULT_MEMTIER_KEY_PATTERN)
    parser.add_argument("--memtier-key-minimum", type=non_negative_int, default=DEFAULT_MEMTIER_KEY_MINIMUM)
    parser.add_argument("--memtier-key-maximum", type=non_negative_int, default=DEFAULT_MEMTIER_KEY_MAXIMUM)
    parser.add_argument("--memtier-no-fill", action="store_true", help="Skip the SET-only key fill before measured memtier runs.")
    parser.add_argument(
        "--memtier-fill-requests",
        type=non_negative_int,
        default=0,
        help="SET-only fill requests per memtier client. Default 0 covers the configured key range once.",
    )
    parser.add_argument("--memtier-fill-key-pattern", default=DEFAULT_MEMTIER_FILL_KEY_PATTERN)
    parser.add_argument(
        "--memtier-warmup-requests",
        type=non_negative_int,
        default=DEFAULT_MEMTIER_WARMUP_REQUESTS,
        help="Unmeasured warmup requests per memtier client after fill. 0 disables warmup.",
    )
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
    if lock == "stock":
        return command, extra_env
    if lock == "mcs_tas_accordin":
        env = merge_envs(extra_env, experiment_three.accordin_preload_env(experiment_three.ACCORDIN_PRELOAD_LIBRARY))
        return experiment_three.benchmark_command(lock, command, env)
    if lock == "mcs_extension":
        env = merge_envs(
            extra_env,
            {"LD_PRELOAD": experiment_three.combine_ld_preload(experiment_three.MCS_EXTENSION_PRELOAD_LIBRARY)},
        )
        return experiment_three.benchmark_command(lock, command, env)

    prefix, env = experiment_three.interpose_command(lock, extra_env)
    return [*prefix, *command], env


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
    ensure_rocksdb_source(rocksdb_dir, build_missing=build_missing, logger=logger)

    existing_build_dir = rocksdb_dir / "build"
    if cmake_build_dir_matches_source(existing_build_dir, rocksdb_dir):
        logger.run(
            ["cmake", "--build", str(existing_build_dir), "--target", "db_bench", "--parallel"],
            log_name="build_rocksdb_db_bench.log",
            cwd=rocksdb_dir,
            timeout_seconds=0,
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
            timeout_seconds=0,
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
            timeout_seconds=0,
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


def path_executable(binary_name: str) -> Path | None:
    found = shutil.which(binary_name)
    return Path(found).resolve() if found is not None else None


def ensure_memcached_binaries(
    args: argparse.Namespace,
    *,
    build_missing: bool,
    logger: experiment_three.CommandLogger,
) -> tuple[Path, Path]:
    configured_memcached = configured_executable_path(args.memcached_bin, "MEMCACHED_BIN", "memcached")
    configured_memtier = configured_executable_path(args.memtier_bin, "MEMTIER_BIN", "memtier_benchmark")

    memcached_bin = configured_memcached or path_executable("memcached")
    memtier_bin = configured_memtier or path_executable("memtier_benchmark")
    missing_packages: list[str] = []
    if memcached_bin is None and configured_memcached is None:
        missing_packages.append(MEMCACHED_SYSTEM_PACKAGES["memcached"])
    if memtier_bin is None and configured_memtier is None:
        missing_packages.append(MEMCACHED_SYSTEM_PACKAGES["memtier_benchmark"])

    if missing_packages:
        if not build_missing:
            missing = []
            if memcached_bin is None:
                missing.append("memcached")
            if memtier_bin is None:
                missing.append("memtier_benchmark")
            raise RuntimeError(
                f"{', '.join(missing)} missing from PATH. Pass explicit paths, install them, "
                "or rerun with --build-missing."
            )
        install_system_packages(
            tuple(missing_packages),
            log_prefix="install_memcached_deps",
            logger=logger,
        )
        memcached_bin = configured_memcached or path_executable("memcached")
        memtier_bin = configured_memtier or path_executable("memtier_benchmark")

    if memcached_bin is None or memtier_bin is None:
        missing = []
        if memcached_bin is None:
            missing.append("memcached")
        if memtier_bin is None:
            missing.append("memtier_benchmark")
        raise RuntimeError(f"Missing memcached workload binaries after install: {', '.join(missing)}")
    return memcached_bin, memtier_bin


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
        memcached_bin, memtier_bin = ensure_memcached_binaries(
            args,
            build_missing=args.build_missing,
            logger=logger,
        )
        binaries["memcached"] = memcached_bin
        binaries["memtier"] = memtier_bin
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


def cachebench_printed_final_results(output: str) -> bool:
    return "== Throughput Stats ==" in output and parse_cachebench_ops_per_second(output) is not None


def wrap_command_timeout(command: list[str], timeout_seconds: int) -> list[str]:
    if timeout_seconds <= 0:
        return command
    return [
        "timeout",
        "-k",
        f"{COMMAND_TIMEOUT_KILL_AFTER_SECONDS}s",
        f"{timeout_seconds}s",
        *command,
    ]


def cachebench_timeout_returncode(returncode: int) -> bool:
    return returncode in {-9, 124, 137}


def accepted_timed_out_cachebench_result(
    exc: experiment_three.CommandError,
    *,
    timeout_seconds: int,
) -> experiment_three.CommandResult | None:
    if not cachebench_timeout_returncode(exc.returncode):
        return None
    try:
        output = exc.log_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    if not cachebench_printed_final_results(output):
        return None
    wall_seconds = parse_log_wall_seconds(output)
    if wall_seconds is None:
        wall_seconds = float(timeout_seconds)
    print(
        f"Accepted timed-out CacheBench command because final throughput results were printed: {exc.log_path}",
        flush=True,
    )
    return experiment_three.CommandResult(
        log_path=exc.log_path,
        output=output,
        wall_seconds=wall_seconds,
    )


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
            total_ops = memtier_total_requests(args)
            if ops_per_second is None and wall_seconds > 0.0:
                ops_per_second = total_ops / wall_seconds
            server_log = result_root / "server_logs" / log_name
            setup_log_names: list[str] = []
            if not args.memtier_no_fill:
                setup_log_names.append(f"fill_memcached_{safe_name(lock)}_{thread:03d}_r{repeat}.log")
            if args.memtier_warmup_requests > 0:
                setup_log_names.append(f"warmup_memcached_{safe_name(lock)}_{thread:03d}_r{repeat}.log")
            setup_wall_seconds = 0.0
            setup_logs: list[str] = []
            for setup_log_name in setup_log_names:
                setup_completed = records_by_log.get(setup_log_name)
                if setup_completed is None:
                    continue
                setup_record, setup_output, setup_path = setup_completed
                setup_wall_seconds += record_wall_seconds(setup_record, setup_output)
                setup_logs.append(relative_log(result_root, setup_path))
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
                    "setup_wall_seconds": format_float(setup_wall_seconds) if setup_wall_seconds > 0.0 else "",
                    "total_ops": str(total_ops),
                    "command_log": relative_log(result_root, log_path),
                    "setup_log": ";".join(setup_logs),
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
                "poolRebalanceIntervalSec": 0,
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

    cache_config = config.setdefault("cache_config", {})
    if not isinstance(cache_config, dict):
        raise RuntimeError("CacheBench config must contain an object-valued cache_config.")
    cache_config["poolRebalanceIntervalSec"] = args.cachebench_pool_rebalance_interval_sec

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
    if installed_root.name == "build" and (installed_root.parent / "installed").is_dir():
        for child in (installed_root.parent / "installed").iterdir():
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
    failures: list[dict[str, str]],
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
                    continue
                config_path = config_dir / f"cachebench_{safe_name(lock)}_{thread:03d}_r{repeat}.json"
                write_cachebench_config(config_path, template=config_template, threads=thread, args=args)
                command = [str(cachebench_bin), "--json_test_config", str(config_path)]
                if args.cachebench_timeout_seconds:
                    command.append(f"--timeout_seconds={args.cachebench_timeout_seconds}")
                command.extend(args.cachebench_extra_arg)
                command, env = build_lock_command(lock, command, extra_env=runtime_env)
                log_name = f"cachebench_{safe_name(lock)}_{thread:03d}_r{repeat}.log"
                try:
                    result = logger.run(
                        wrap_command_timeout(command, args.cachebench_command_timeout_seconds),
                        log_name=log_name,
                        cwd=REPO_ROOT,
                        env=env,
                        timeout_seconds=0,
                    )
                except experiment_three.CommandError as exc:
                    accepted = accepted_timed_out_cachebench_result(
                        exc,
                        timeout_seconds=args.cachebench_command_timeout_seconds,
                    )
                    if accepted is None:
                        experiment_failures.append_command_failure(
                            failures,
                            result_root=result_root,
                            experiment="experiment5",
                            workload="cachebench",
                            benchmark="cachebench",
                            lock=lock,
                            threads=thread,
                            repeat=repeat,
                            exc=exc,
                        )
                        experiment_failures.write_failures_csv(result_root, failures)
                        continue
                    experiment_failures.append_failure(
                        failures,
                        result_root=result_root,
                        experiment="experiment5",
                        workload="cachebench",
                        benchmark="cachebench",
                        lock=lock,
                        threads=thread,
                        repeat=repeat,
                        stage="exit_after_results",
                        status="timeout_after_results",
                        returncode=exc.returncode,
                        command_log=exc.log_path,
                        message=str(exc),
                    )
                    experiment_failures.write_failures_csv(result_root, failures)
                    result = accepted
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
    writes: int | None,
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
    if writes is not None:
        command.append(f"--writes={writes}")
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
    failures: list[dict[str, str]],
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
                    writes_per_thread = None
                    effective_total_ops = reads_per_thread * thread
                else:
                    db_bench_num = args.db_num
                    reads_per_thread = None
                    writes_per_thread = ceil_div(args.total_ops, thread)
                    effective_total_ops = writes_per_thread * thread
                for repeat in range(1, repeats + 1):
                    key = ("rocksdb", benchmark, lock, thread, repeat)
                    if key in completed_keys:
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
                                writes=None,
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
                            writes=writes_per_thread,
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
                    except experiment_three.CommandError as exc:
                        stage = "setup" if use_existing_db and setup_result is None else "run"
                        experiment_failures.append_command_failure(
                            failures,
                            result_root=result_root,
                            experiment="experiment5",
                            workload="rocksdb",
                            benchmark=benchmark,
                            lock=lock,
                            threads=thread,
                            repeat=repeat,
                            stage=stage,
                            exc=exc,
                        )
                        experiment_failures.write_failures_csv(result_root, failures)
                        continue
                    finally:
                        cleanup_path(db_path)
    return rows


def choose_free_port(host: str) -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind((host, 0))
        return int(sock.getsockname()[1])


def tail_file(path: Path, max_lines: int = 20) -> str:
    try:
        return "\n".join(path.read_text(encoding="utf-8", errors="replace").splitlines()[-max_lines:])
    except OSError:
        return ""


def wait_for_tcp(
    host: str,
    port: int,
    timeout_seconds: int,
    *,
    process: subprocess.Popen[str] | None = None,
    log_path: Path | None = None,
) -> None:
    deadline = time.monotonic() + timeout_seconds
    last_error: OSError | None = None
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            detail = (
                f"memcached exited before accepting connections on {host}:{port} "
                f"(returncode {process.returncode})"
            )
            if log_path is not None:
                detail += f"; server log: {log_path}"
                log_tail = tail_file(log_path)
                if log_tail:
                    detail += f"\nserver log tail:\n{log_tail}"
            raise RuntimeError(detail)
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


def terminate_process_group(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    try:
        process_group_id = os.getpgid(process.pid)
    except ProcessLookupError:
        return

    try:
        os.killpg(process_group_id, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=5)
        return
    except subprocess.TimeoutExpired:
        pass

    try:
        os.killpg(process_group_id, signal.SIGKILL)
    except ProcessLookupError:
        return
    process.wait(timeout=5)


def command_uses_sudo(command: list[str]) -> bool:
    return bool(command) and Path(command[0]).name == "sudo"


def memcached_conn_limit_for_thread(thread: int, args: argparse.Namespace) -> int:
    if args.memcached_conn_limit > 0:
        return args.memcached_conn_limit
    client_connections = args.memtier_client_threads * args.memtier_clients
    return max(1024, thread * 16, client_connections * 4)


def memtier_client_count(args: argparse.Namespace) -> int:
    return args.memtier_client_threads * args.memtier_clients


def memtier_total_requests(args: argparse.Namespace) -> int:
    return memtier_client_count(args) * args.memtier_requests


def memtier_key_count(args: argparse.Namespace) -> int:
    return args.memtier_key_maximum - args.memtier_key_minimum + 1


def memtier_fill_requests(args: argparse.Namespace) -> int:
    if args.memtier_fill_requests > 0:
        return args.memtier_fill_requests
    return ceil_div(memtier_key_count(args), memtier_client_count(args))


def memtier_command(
    memtier_bin: Path,
    *,
    host: str,
    port: int,
    client_threads: int,
    clients: int,
    requests: int,
    ratio: str,
    key_pattern: str,
    args: argparse.Namespace,
) -> list[str]:
    command = [
        str(memtier_bin),
        "-s",
        host,
        "-p",
        str(port),
        "--protocol=memcache_text",
        "-t",
        str(client_threads),
        "-c",
        str(clients),
        "--requests",
        str(requests),
        "--ratio",
        ratio,
        "--key-pattern",
        key_pattern,
        "--key-minimum",
        str(args.memtier_key_minimum),
        "--key-maximum",
        str(args.memtier_key_maximum),
        "--data-size",
        str(args.memtier_data_size),
        "--hide-histogram",
    ]
    command.extend(args.memtier_extra_arg)
    return command


def run_memtier_setup(
    result_root: Path,
    *,
    lock: str,
    thread: int,
    repeat: int,
    port: int,
    memtier_bin: Path,
    args: argparse.Namespace,
    logger: experiment_three.CommandLogger,
) -> tuple[float, str]:
    setup_wall_seconds = 0.0
    setup_logs: list[str] = []

    if not args.memtier_no_fill:
        result = logger.run(
            memtier_command(
                memtier_bin,
                host=args.memcached_host,
                port=port,
                client_threads=args.memtier_client_threads,
                clients=args.memtier_clients,
                requests=memtier_fill_requests(args),
                ratio="1:0",
                key_pattern=args.memtier_fill_key_pattern,
                args=args,
            ),
            log_name=f"fill_memcached_{safe_name(lock)}_{thread:03d}_r{repeat}.log",
            cwd=REPO_ROOT,
        )
        setup_wall_seconds += result.wall_seconds
        setup_logs.append(relative_log(result_root, result.log_path))

    if args.memtier_warmup_requests > 0:
        result = logger.run(
            memtier_command(
                memtier_bin,
                host=args.memcached_host,
                port=port,
                client_threads=args.memtier_client_threads,
                clients=args.memtier_clients,
                requests=args.memtier_warmup_requests,
                ratio=args.memtier_ratio,
                key_pattern=args.memtier_key_pattern,
                args=args,
            ),
            log_name=f"warmup_memcached_{safe_name(lock)}_{thread:03d}_r{repeat}.log",
            cwd=REPO_ROOT,
        )
        setup_wall_seconds += result.wall_seconds
        setup_logs.append(relative_log(result_root, result.log_path))

    return setup_wall_seconds, ";".join(setup_logs)


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
    failures: list[dict[str, str]],
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
                    "-c",
                    str(memcached_conn_limit_for_thread(thread, args)),
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
                        start_new_session=True,
                    )
                    try:
                        wait_for_tcp(
                            args.memcached_host,
                            port,
                            args.memcached_start_timeout,
                            process=server_process,
                            log_path=server_log,
                        )
                        setup_wall_seconds, setup_log = run_memtier_setup(
                            result_root,
                            lock=lock,
                            thread=thread,
                            repeat=repeat,
                            port=port,
                            memtier_bin=memtier_bin,
                            args=args,
                            logger=logger,
                        )
                        client_command = memtier_command(
                            memtier_bin,
                            host=args.memcached_host,
                            port=port,
                            client_threads=args.memtier_client_threads,
                            clients=args.memtier_clients,
                            requests=args.memtier_requests,
                            ratio=args.memtier_ratio,
                            key_pattern=args.memtier_key_pattern,
                            args=args,
                        )
                        result = logger.run(
                            client_command,
                            log_name=f"memcached_{safe_name(lock)}_{thread:03d}_r{repeat}.log",
                            cwd=REPO_ROOT,
                        )
                        ops_per_second = parse_memtier_ops_per_second(result.output)
                        total_ops = memtier_total_requests(args)
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
                                "setup_wall_seconds": format_float(setup_wall_seconds) if setup_wall_seconds > 0.0 else "",
                                "total_ops": str(total_ops),
                                "command_log": relative_log(result_root, result.log_path),
                                "setup_log": setup_log,
                                "server_log": relative_log(result_root, server_log),
                            }
                        )
                        completed_keys.add(key)
                        write_progress_csvs(result_root, rows)
                    except experiment_three.CommandError as exc:
                        experiment_failures.append_command_failure(
                            failures,
                            result_root=result_root,
                            experiment="experiment5",
                            workload="memcached",
                            benchmark="memtier",
                            lock=lock,
                            threads=thread,
                            repeat=repeat,
                            exc=exc,
                        )
                        experiment_failures.write_failures_csv(result_root, failures)
                        continue
                    except RuntimeError as exc:
                        experiment_failures.append_failure(
                            failures,
                            result_root=result_root,
                            experiment="experiment5",
                            workload="memcached",
                            benchmark="memtier",
                            lock=lock,
                            threads=thread,
                            repeat=repeat,
                            stage="server_start",
                            status="failed",
                            command_log=server_log,
                            message=str(exc),
                        )
                        experiment_failures.write_failures_csv(result_root, failures)
                        continue
                    finally:
                        terminate_process_group(server_process)
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


def optional_float(value: str) -> float | None:
    stripped = value.strip()
    if not stripped:
        return None
    try:
        return float(stripped)
    except ValueError:
        return None


def optional_int(value: str) -> int | None:
    stripped = value.strip()
    if not stripped:
        return None
    try:
        return int(stripped)
    except ValueError:
        return None


def outlier_sort_key(row: dict[str, str]) -> tuple[tuple[int, str], tuple[int, str], tuple[int, str], int, str]:
    thread = optional_int(row["threads"])
    return (
        workload_sort_key(row["workload"]),
        benchmark_sort_key(row["workload"], row["benchmark"]),
        lock_sort_key(row["lock"]),
        -1 if thread is None else thread,
        row["kind"],
    )


def raw_group_key(row: dict[str, str]) -> tuple[str, str, str, int] | None:
    thread = optional_int(row["threads"])
    if thread is None:
        return None
    return (row["workload"], row["benchmark"], row["lock"], thread)


def sample_field(rows: list[dict[str, str]], field: str) -> str:
    samples: list[str] = []
    for row in sorted(rows, key=lambda item: optional_int(item["repeat"]) or 0):
        value = row[field].strip()
        if not value:
            continue
        repeat = optional_int(row["repeat"])
        prefix = f"r{repeat}" if repeat is not None else "r?"
        samples.append(f"{prefix}:{value}")
    return "; ".join(samples)


def log_field(rows: list[dict[str, str]], field: str) -> str:
    values = []
    seen = set()
    for row in sorted(rows, key=lambda item: optional_int(item["repeat"]) or 0):
        value = row[field].strip()
        if not value or value in seen:
            continue
        seen.add(value)
        values.append(value)
    return "; ".join(values)


def raw_groups_by_point(raw_rows: list[dict[str, str]]) -> dict[tuple[str, str, str, int], list[dict[str, str]]]:
    groups: dict[tuple[str, str, str, int], list[dict[str, str]]] = {}
    for row in raw_rows:
        key = raw_group_key(row)
        if key is not None:
            groups.setdefault(key, []).append(row)
    return groups


def detect_repeat_outliers(
    raw_groups: dict[tuple[str, str, str, int], list[dict[str, str]]],
) -> list[dict[str, str]]:
    outliers: list[dict[str, str]] = []
    for workload, benchmark, lock, thread in sorted(
        raw_groups.keys(),
        key=lambda item: (workload_sort_key(item[0]), benchmark_sort_key(item[0], item[1]), lock_sort_key(item[2]), item[3]),
    ):
        group_rows = raw_groups[(workload, benchmark, lock, thread)]
        samples = [
            (row, value)
            for row in group_rows
            if (value := optional_float(row["ops_per_second"])) is not None
        ]
        if len(samples) < REPEAT_OUTLIER_MIN_RUNS:
            continue

        values = [value for _, value in samples]
        mean_value = mean(values)
        if mean_value <= 0.0:
            continue
        cv_percent = pstdev(values) / mean_value * 100.0 if len(values) > 1 else 0.0
        positive_values = [value for value in values if value > 0.0]
        max_min_ratio = (
            max(positive_values) / min(positive_values)
            if len(positive_values) >= 2 and min(positive_values) > 0.0
            else 1.0
        )
        if (
            cv_percent < REPEAT_OUTLIER_CV_THRESHOLD_PERCENT
            and max_min_ratio < REPEAT_OUTLIER_MAX_MIN_RATIO_THRESHOLD
        ):
            continue

        score = max(
            cv_percent / REPEAT_OUTLIER_CV_THRESHOLD_PERCENT,
            max_min_ratio / REPEAT_OUTLIER_MAX_MIN_RATIO_THRESHOLD,
        )
        outliers.append(
            {
                "kind": "repeat_variation",
                "workload": workload,
                "benchmark": benchmark,
                "lock": lock,
                "threads": str(thread),
                "score": format_float(score),
                "direction": "variable",
                "mean_ops_per_second": format_float(mean_value),
                "cv_percent": format_float(cv_percent),
                "runs": str(len(samples)),
                "sample_ops_per_second": sample_field(group_rows, "ops_per_second"),
                "sample_wall_seconds": sample_field(group_rows, "wall_seconds"),
                "command_logs": log_field(group_rows, "command_log"),
                "setup_logs": log_field(group_rows, "setup_log"),
                "server_logs": log_field(group_rows, "server_log"),
                "neighbor_threads": "",
                "neighbor_ops_per_second": "",
                "note": f"repeat CV >= {REPEAT_OUTLIER_CV_THRESHOLD_PERCENT:g}% or max/min >= {REPEAT_OUTLIER_MAX_MIN_RATIO_THRESHOLD:g}x; max/min={max_min_ratio:.2f}x",
            }
        )
    return outliers


def detect_local_shape_outliers(
    summary_rows: list[dict[str, str]],
    raw_groups: dict[tuple[str, str, str, int], list[dict[str, str]]],
) -> list[dict[str, str]]:
    curves: dict[tuple[str, str, str], dict[int, float]] = {}
    for row in summary_rows:
        value = optional_float(row["mean_ops_per_second"])
        thread = optional_int(row["threads"])
        if value is None or thread is None or value <= 0.0:
            continue
        curves.setdefault((row["workload"], row["benchmark"], row["lock"]), {})[thread] = value

    outliers: list[dict[str, str]] = []
    for workload, benchmark, lock in sorted(
        curves.keys(),
        key=lambda item: (workload_sort_key(item[0]), benchmark_sort_key(item[0], item[1]), lock_sort_key(item[2])),
    ):
        points = curves[(workload, benchmark, lock)]
        thread_values = sorted(points)
        if len(thread_values) < 3:
            continue
        for index in range(1, len(thread_values) - 1):
            prev_thread = thread_values[index - 1]
            thread = thread_values[index]
            next_thread = thread_values[index + 1]
            prev_value = points[prev_thread]
            value = points[thread]
            next_value = points[next_thread]
            if prev_value <= 0.0 or next_value <= 0.0:
                continue
            expected = math.sqrt(prev_value * next_value)
            if expected <= 0.0:
                continue
            ratio = value / expected
            if ratio < LOCAL_SHAPE_RATIO_THRESHOLD and ratio > 1.0 / LOCAL_SHAPE_RATIO_THRESHOLD:
                continue

            group_rows = raw_groups.get((workload, benchmark, lock, thread), [])
            direction = "peak" if ratio >= LOCAL_SHAPE_RATIO_THRESHOLD else "dip"
            score = ratio if ratio >= 1.0 else 1.0 / ratio
            neighbor_ops = (
                f"{prev_thread}:{format_float(prev_value)}; "
                f"{thread}:{format_float(value)}; "
                f"{next_thread}:{format_float(next_value)}"
            )
            outliers.append(
                {
                    "kind": "local_shape",
                    "workload": workload,
                    "benchmark": benchmark,
                    "lock": lock,
                    "threads": str(thread),
                    "score": format_float(score),
                    "direction": direction,
                    "mean_ops_per_second": format_float(value),
                    "cv_percent": "",
                    "runs": str(len(group_rows)),
                    "sample_ops_per_second": sample_field(group_rows, "ops_per_second"),
                    "sample_wall_seconds": sample_field(group_rows, "wall_seconds"),
                    "command_logs": log_field(group_rows, "command_log"),
                    "setup_logs": log_field(group_rows, "setup_log"),
                    "server_logs": log_field(group_rows, "server_log"),
                    "neighbor_threads": f"{prev_thread}; {thread}; {next_thread}",
                    "neighbor_ops_per_second": neighbor_ops,
                    "note": f"{direction} is {score:.2f}x away from geometric interpolation of adjacent thread points",
                }
            )
    return outliers


def detect_outliers(raw_rows: list[dict[str, str]], summary_rows: list[dict[str, str]]) -> list[dict[str, str]]:
    raw_groups = raw_groups_by_point(raw_rows)
    outliers = [
        *detect_repeat_outliers(raw_groups),
        *detect_local_shape_outliers(summary_rows, raw_groups),
    ]
    return sorted(outliers, key=outlier_sort_key)


def write_outlier_csv(result_root: Path, outlier_rows: list[dict[str, str]]) -> Path:
    path = result_root / "outliers.csv"
    with path.open("w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=OUTLIER_FIELDS)
        writer.writeheader()
        writer.writerows(outlier_rows)
    return path


def markdown_cell(value: str) -> str:
    return value.replace("|", "\\|").replace("\n", "<br>")


def write_outlier_markdown(result_root: Path, outlier_rows: list[dict[str, str]]) -> Path:
    path = result_root / "outliers.md"
    lines = [
        "# Experiment 5 Outlier Tracking",
        "",
        "Generated from raw.csv and summary.csv during plot generation.",
        "",
        "Rules:",
        f"- repeat_variation: at least {REPEAT_OUTLIER_MIN_RUNS} repeats and CV >= {REPEAT_OUTLIER_CV_THRESHOLD_PERCENT:g}% or max/min >= {REPEAT_OUTLIER_MAX_MIN_RATIO_THRESHOLD:g}x.",
        f"- local_shape: point is >= {LOCAL_SHAPE_RATIO_THRESHOLD:g}x above or below the geometric interpolation of adjacent thread points on the same workload/benchmark/lock curve.",
        "",
    ]
    if not outlier_rows:
        lines.append("No outliers detected.")
    else:
        lines.extend(
            [
                f"Detected {len(outlier_rows)} outlier records.",
                "",
                "| kind | workload | benchmark | lock | threads | direction | score | mean_ops_per_second | cv_percent | sample_ops_per_second | command_logs | note |",
                "| --- | --- | --- | --- | ---: | --- | ---: | ---: | ---: | --- | --- | --- |",
            ]
        )
        for row in outlier_rows:
            lines.append(
                "| "
                + " | ".join(
                    markdown_cell(value)
                    for value in (
                        row["kind"],
                        row["workload"],
                        row["benchmark"],
                        lock_label(row["lock"]),
                        row["threads"],
                        row["direction"],
                        row["score"],
                        row["mean_ops_per_second"],
                        row["cv_percent"],
                        row["sample_ops_per_second"],
                        row["command_logs"],
                        row["note"],
                    )
                )
                + " |"
            )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def write_outlier_artifacts(
    result_root: Path,
    raw_rows: list[dict[str, str]],
    summary_rows: list[dict[str, str]],
) -> list[Path]:
    outlier_rows = detect_outliers(raw_rows, summary_rows)
    csv_path = write_outlier_csv(result_root, outlier_rows)
    markdown_path = write_outlier_markdown(result_root, outlier_rows)
    return [csv_path, markdown_path]


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
        "command_timeout_seconds": args.command_timeout_seconds,
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
            "pool_rebalance_interval_sec": args.cachebench_pool_rebalance_interval_sec,
            "timeout_seconds": args.cachebench_timeout_seconds,
            "command_timeout_seconds": args.cachebench_command_timeout_seconds,
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
            "conn_limit": args.memcached_conn_limit,
            "effective_conn_limits_by_thread": {
                str(thread): memcached_conn_limit_for_thread(thread, args)
                for thread in threads
            },
            "memtier_client_threads": args.memtier_client_threads,
            "memtier_clients": args.memtier_clients,
            "memtier_requests": args.memtier_requests,
            "memtier_ratio": args.memtier_ratio,
            "memtier_key_pattern": args.memtier_key_pattern,
            "memtier_key_minimum": args.memtier_key_minimum,
            "memtier_key_maximum": args.memtier_key_maximum,
            "memtier_fill_enabled": not args.memtier_no_fill,
            "memtier_fill_requests": None if args.memtier_no_fill else memtier_fill_requests(args),
            "memtier_fill_key_pattern": args.memtier_fill_key_pattern,
            "memtier_warmup_requests": args.memtier_warmup_requests,
            "memtier_data_size": args.memtier_data_size,
            "memtier_extra_args": list(args.memtier_extra_arg),
        },
    }
    with (result_root / "settings.json").open("w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")


def print_outputs(
    result_root: Path,
    raw_path: Path,
    summary_path: Path,
    plot_paths: Iterable[Path],
    outlier_paths: Iterable[Path],
) -> None:
    print(f"Result root: {result_root}")
    print(f"Raw CSV: {raw_path}")
    print(f"Summary CSV: {summary_path}")
    for plot_path in plot_paths:
        print(f"Plot: {plot_path}")
    for outlier_path in outlier_paths:
        print(f"Outliers: {outlier_path}")


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
        if args.memtier_key_maximum < args.memtier_key_minimum:
            print("--memtier-key-maximum must be greater than or equal to --memtier-key-minimum.", file=sys.stderr)
            return 2

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
            outlier_paths = write_outlier_artifacts(result_root, raw_rows, summary_rows)
            print_outputs(result_root, raw_path, summary_path, plot_paths, outlier_paths)
            return 0

        result_root = resolve_path(args.output_root) if args.output_root is not None else default_result_root()
        experiment_three.ensure_output_root(result_root, args.force, args.resume)
        logger = experiment_three.CommandLogger(
            result_root,
            resume=args.resume,
            command_timeout_seconds=args.command_timeout_seconds,
            echo_output=False,
        )

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
        failures: list[dict[str, str]] = []
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
                failures=failures,
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
                failures=failures,
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
                failures=failures,
                existing_rows=raw_rows,
            )

        raw_path = write_raw_csv(result_root, raw_rows)
        summary_rows = summarize_rows(raw_rows)
        summary_path = write_summary_csv(result_root, summary_rows)
        plot_paths = write_plots(result_root, summary_rows, skip_plots=args.skip_plots)
        outlier_paths = write_outlier_artifacts(result_root, raw_rows, summary_rows)
        print_outputs(result_root, raw_path, summary_path, plot_paths, outlier_paths)
        failures_path = experiment_failures.write_failures_csv(result_root, failures)
        experiment_failures.print_failure_summary(failures, failures_path)
        return 1 if failures else 0
    except experiment_three.CommandError as exc:
        print(str(exc), file=sys.stderr)
        print(f"Command log: {exc.log_path}", file=sys.stderr)
        return exc.returncode
    except Exception as exc:
        print(str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
