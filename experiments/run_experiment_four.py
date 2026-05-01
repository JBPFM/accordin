#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import os
import re
import shutil
import sys
import tempfile
from pathlib import Path
from statistics import mean
from typing import Iterable

import run_experiment_one as experiment_one
import run_experiment_three as experiment_three
import machine_config


REPO_ROOT = experiment_three.REPO_ROOT
FLEXGUARD_DIR = experiment_three.FLEXGUARD_DIR
LEVELDB_VERSION = "1.23"
DEFAULT_LEVELDB_DIR = REPO_ROOT / "third_party" / f"leveldb-{LEVELDB_VERSION}"
DEFAULT_DB_BENCH = DEFAULT_LEVELDB_DIR / "build" / "db_bench"

DEFAULT_BENCHMARKS = ("readrandom", "fillrandom")
DEFAULT_LOCKS = experiment_three.DEFAULT_LOCKS
DEFAULT_THREADS = experiment_one.THREADS
DEFAULT_REPEATS = experiment_three.DEFAULT_REPEATS
DEFAULT_NUM = 500_000
DEFAULT_TOTAL_OPS = 1_572_864
DEFAULT_FILL_BENCHMARK = "fillseq"
DEFAULT_INIT_EXISTING_BENCHMARKS = ("readrandom", "readseq", "overwrite")
DEFAULT_JEMALLOC_CANDIDATES = (
    Path("/usr/local/lib/libjemalloc.so.2"),
    Path("/lib/x86_64-linux-gnu/libjemalloc.so.2"),
)
PER_LOCK_MAX_THREADS = {
    "mcs": machine_config.LEVELDB_PER_LOCK_MAX_THREADS,
    "mcstp": machine_config.LEVELDB_PER_LOCK_MAX_THREADS,
    "mcs_extension": machine_config.LEVELDB_PER_LOCK_MAX_THREADS,
}

RAW_FIELDS = (
    "benchmark",
    "lock",
    "threads",
    "repeat",
    "num",
    "total_ops",
    "db_bench_num",
    "reads_per_thread",
    "effective_total_ops",
    "use_existing_db",
    "fill_benchmark",
    "init_wall_seconds",
    "latency_micros_per_op",
    "wall_seconds",
    "init_command_log",
    "command_log",
)
SUMMARY_FIELDS = (
    "benchmark",
    "lock",
    "threads",
    "mean_latency_micros_per_op",
    "mean_ops_per_second",
    "mean_init_wall_seconds",
    "mean_wall_seconds",
    "runs",
)
LATENCY_PATTERN = re.compile(
    r"^(?P<name>\w+)\s+:\s+(?P<micros>\d+(?:\.\d+)?)\s+micros/op;",
    re.MULTILINE,
)
BENCHMARK_NAME_PATTERN = re.compile(r"^[A-Za-z0-9_]+$")


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be positive")
    return parsed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run or plot the LevelDB lock experiment sweep.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"""\
Default benchmark settings:
  benchmarks={','.join(DEFAULT_BENCHMARKS)}
  locks={','.join(DEFAULT_LOCKS)}
  machine-profile={machine_config.ACTIVE_MACHINE_CONFIG.name} (override with {machine_config.PROFILE_ENV})
  threads={','.join(str(thread) for thread in DEFAULT_THREADS)}
  repeats={DEFAULT_REPEATS}, num={DEFAULT_NUM}, total_ops={DEFAULT_TOTAL_OPS}
  leveldb={DEFAULT_LEVELDB_DIR} (tag {LEVELDB_VERSION})
  db_bench={DEFAULT_DB_BENCH}
  fill_benchmark={DEFAULT_FILL_BENCHMARK}
  init_existing_benchmarks={','.join(DEFAULT_INIT_EXISTING_BENCHMARKS)}
  jemalloc=auto; disabled when FlexGuard pthread interpose locks are present, otherwise first existing candidate from {','.join(str(path) for path in DEFAULT_JEMALLOC_CANDIDATES)}
  per_lock_max_threads={','.join(f"{lock}:{max_threads}" for lock, max_threads in PER_LOCK_MAX_THREADS.items())}

Examples:
  python3 experiments/run_experiment_four.py
  python3 experiments/run_experiment_four.py --locks stock --threads 1 --repeats 1 --benchmarks fillseq --num 1000
  {machine_config.PROFILE_ENV}=original python3 experiments/run_experiment_four.py
  python3 experiments/run_experiment_four.py --plot-only experiments/results/experiment4_manual
""",
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        default=None,
        help="Directory for a new run. Default: experiments/results/experiment4_<timestamp>.",
    )
    parser.add_argument(
        "--plot-only",
        type=Path,
        default=None,
        metavar="RESULT_ROOT",
        help="Skip benchmark execution and regenerate summary.csv and PNGs from RESULT_ROOT/raw.csv.",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Allow benchmark output into an existing non-empty output root.",
    )
    parser.add_argument(
        "--build-missing",
        action="store_true",
        help="Build missing interpose helpers or db_bench before running.",
    )
    parser.add_argument(
        "--benchmarks",
        default=",".join(DEFAULT_BENCHMARKS),
        metavar="CSV",
        help=f"Comma-separated LevelDB db_bench benchmark names. Default: {','.join(DEFAULT_BENCHMARKS)}.",
    )
    parser.add_argument(
        "--locks",
        default=",".join(DEFAULT_LOCKS),
        metavar="CSV",
        help=(
            "Comma-separated lock keys. "
            f"Default: {','.join(DEFAULT_LOCKS)}. "
            "Use stock to run without interpose. "
            "Aliases: mcs-tas == mcstas."
        ),
    )
    parser.add_argument(
        "--threads",
        default=",".join(str(thread) for thread in DEFAULT_THREADS),
        metavar="CSV",
        help=f"Comma-separated thread counts. Default: {','.join(str(thread) for thread in DEFAULT_THREADS)}.",
    )
    parser.add_argument(
        "--repeats",
        type=positive_int,
        default=DEFAULT_REPEATS,
        help=f"Number of repeats per benchmark/lock/thread point. Default: {DEFAULT_REPEATS}.",
    )
    parser.add_argument(
        "--num",
        type=positive_int,
        default=DEFAULT_NUM,
        help=(
            "LevelDB --num value for DB size/keyspace in initialization and measured read runs; "
            "db_bench also uses it as the fallback operation target when --reads is absent. "
            f"Default: {DEFAULT_NUM}."
        ),
    )
    parser.add_argument(
        "--total-ops",
        type=positive_int,
        default=DEFAULT_TOTAL_OPS,
        help=(
            "Target total operations per benchmark/lock/thread point. "
            "Measured runs divide this across threads. "
            f"Default: {DEFAULT_TOTAL_OPS}."
        ),
    )
    parser.add_argument(
        "--db-bench",
        type=Path,
        default=None,
        help="Path to db_bench. Default: <leveldb-dir>/build/db_bench.",
    )
    parser.add_argument(
        "--leveldb-dir",
        type=Path,
        default=DEFAULT_LEVELDB_DIR,
        help=(
            "LevelDB source directory used when --db-bench is omitted or --build-missing builds db_bench. "
            f"Default: {DEFAULT_LEVELDB_DIR}."
        ),
    )
    parser.add_argument(
        "--fill-benchmark",
        default=DEFAULT_FILL_BENCHMARK,
        help=f"LevelDB benchmark used to initialize existing DB workloads. Default: {DEFAULT_FILL_BENCHMARK}.",
    )
    parser.add_argument(
        "--init-existing-benchmarks",
        default=",".join(DEFAULT_INIT_EXISTING_BENCHMARKS),
        metavar="CSV",
        help=(
            "Benchmarks that should run against a freshly initialized DB. "
            "Use none to disable initialization. "
            f"Default: {','.join(DEFAULT_INIT_EXISTING_BENCHMARKS)}."
        ),
    )
    parser.add_argument(
        "--jemalloc-library",
        default=None,
        metavar="PATH",
        help=(
            "jemalloc library to LD_PRELOAD for LevelDB runs. "
            "Default: disabled when FlexGuard pthread interpose locks are present; otherwise first existing candidate from "
            f"{','.join(str(path) for path in DEFAULT_JEMALLOC_CANDIDATES)}. "
            "Use none to disable."
        ),
    )
    return parser.parse_args()


def parse_csv_strings(value: str) -> tuple[str, ...]:
    return experiment_three.parse_csv_strings(value)


def parse_csv_ints(value: str) -> tuple[int, ...]:
    return experiment_three.parse_csv_ints(value)


def validate_benchmark_names(benchmarks: tuple[str, ...]) -> tuple[str, ...]:
    invalid = [benchmark for benchmark in benchmarks if BENCHMARK_NAME_PATTERN.fullmatch(benchmark) is None]
    if invalid:
        raise ValueError(f"Unsupported benchmark names: {', '.join(invalid)}")
    return benchmarks


def parse_init_existing_benchmarks(value: str) -> tuple[str, ...]:
    if value.strip().lower() in {"", "none"}:
        return ()
    return validate_benchmark_names(parse_csv_strings(value))


def resolve_path(path: Path) -> Path:
    return experiment_three.resolve_path(path)


def validate_jemalloc_library(path: Path) -> Path:
    resolved = resolve_path(path)
    if not resolved.is_file():
        raise RuntimeError(
            f"jemalloc is enabled, but the library was not found: {resolved}. "
            "Use --jemalloc-library PATH or --jemalloc-library none."
        )
    if not os.access(resolved, os.R_OK):
        raise RuntimeError(
            f"jemalloc is enabled, but the library is not readable: {resolved}. "
            "Use --jemalloc-library PATH or --jemalloc-library none."
        )
    return resolved


def resolve_jemalloc_library(value: str | None) -> Path | None:
    if value is not None:
        stripped = value.strip()
        if stripped.lower() == "none":
            return None
        if not stripped:
            raise RuntimeError("--jemalloc-library must be a path or none.")
        return validate_jemalloc_library(Path(stripped))

    for candidate in DEFAULT_JEMALLOC_CANDIDATES:
        if candidate.exists():
            return validate_jemalloc_library(candidate)

    candidates = ", ".join(str(path) for path in DEFAULT_JEMALLOC_CANDIDATES)
    raise RuntimeError(
        "jemalloc is enabled by default, but no candidate library was found. "
        f"Checked: {candidates}. Use --jemalloc-library PATH or --jemalloc-library none."
    )


def uses_flexguard_pthread_interpose(locks: tuple[str, ...]) -> bool:
    preload_only_locks = {"stock", "accordin", "mcs_extension", "mcs_accordin"}
    return any(lock not in preload_only_locks for lock in locks)


def merge_ld_preload_entries(*libraries: Path | None) -> str | None:
    entries = [str(library) for library in libraries if library is not None]
    return ":".join(entries) if entries else None


def preload_env(*libraries: Path | None) -> dict[str, str]:
    ld_preload = merge_ld_preload_entries(*libraries)
    if ld_preload is None:
        return {"LD_PRELOAD": ""}
    return {"LD_PRELOAD": ld_preload}


def accordin_preload_env(preload_library: Path, jemalloc_library: Path | None) -> dict[str, str]:
    env = preload_env(preload_library, jemalloc_library)
    if "ACCORDIN_CPU_MASK_K" in os.environ:
        env["ACCORDIN_CPU_MASK_K"] = os.environ["ACCORDIN_CPU_MASK_K"]
    if "K" in os.environ:
        env["K"] = os.environ["K"]
    return env


def default_result_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment4_{timestamp}"


def lock_label(lock: str) -> str:
    return experiment_three.lock_label(lock)


def lock_sort_key(lock: str) -> tuple[int, str]:
    return experiment_three.lock_sort_key(lock)


def runnable_threads_for_lock(lock: str, threads: tuple[int, ...]) -> tuple[int, ...]:
    max_threads = PER_LOCK_MAX_THREADS.get(lock)
    if max_threads is None:
        return threads
    return tuple(thread for thread in threads if thread <= max_threads)


def ceil_div(numerator: int, denominator: int) -> int:
    return -(-numerator // denominator)


def benchmark_label(benchmark: str) -> str:
    return f"LevelDB {benchmark}"


def benchmark_sort_key(benchmark: str) -> tuple[int, str]:
    if benchmark in DEFAULT_BENCHMARKS:
        return (DEFAULT_BENCHMARKS.index(benchmark), benchmark)
    return (len(DEFAULT_BENCHMARKS), benchmark)


def safe_name(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]+", "_", value)


def default_db_bench_for_leveldb(leveldb_dir: Path) -> Path:
    return leveldb_dir / "build" / "db_bench"


def ensure_leveldb_source(
    leveldb_dir: Path,
    *,
    build_missing: bool,
    logger: experiment_three.CommandLogger,
) -> None:
    if (leveldb_dir / "CMakeLists.txt").is_file():
        return
    if not build_missing:
        raise RuntimeError(
            f"LevelDB source directory is missing or incomplete: {leveldb_dir}. "
            "Rerun with --build-missing or initialize the submodule manually."
        )
    if leveldb_dir != DEFAULT_LEVELDB_DIR:
        raise RuntimeError(f"Cannot initialize a custom LevelDB source directory: {leveldb_dir}")

    logger.run(
        [
            "git",
            "submodule",
            "update",
            "--init",
            "--recursive",
            "--depth",
            "1",
            str(leveldb_dir.relative_to(REPO_ROOT)),
        ],
        log_name="init_leveldb_submodule.log",
        cwd=REPO_ROOT,
    )
    if not (leveldb_dir / "CMakeLists.txt").is_file():
        raise RuntimeError(f"LevelDB source directory is still unavailable after submodule init: {leveldb_dir}")


def ensure_db_bench(
    db_bench: Path,
    *,
    leveldb_dir: Path,
    build_missing: bool,
    logger: experiment_three.CommandLogger,
) -> None:
    if db_bench.is_file() and os.access(db_bench, os.X_OK):
        return
    if not build_missing:
        raise RuntimeError(f"LevelDB db_bench is missing or not executable: {db_bench}. Rerun with --build-missing.")
    if db_bench != default_db_bench_for_leveldb(leveldb_dir):
        raise RuntimeError(f"Cannot build a custom db_bench path: {db_bench}")

    ensure_leveldb_source(leveldb_dir, build_missing=build_missing, logger=logger)
    logger.run(
        [
            "cmake",
            "-S",
            str(leveldb_dir),
            "-B",
            str(leveldb_dir / "build"),
            "-DCMAKE_BUILD_TYPE=Release",
            "-DCMAKE_CXX_STANDARD=17",
            "-DCMAKE_CXX_STANDARD_REQUIRED=ON",
            "-DLEVELDB_BUILD_TESTS=OFF",
            "-DLEVELDB_BUILD_BENCHMARKS=ON",
        ],
        log_name="configure_leveldb.log",
        cwd=REPO_ROOT,
    )
    logger.run(
        [
            "cmake",
            "--build",
            str(leveldb_dir / "build"),
            "--target",
            "db_bench",
            "--parallel",
        ],
        log_name="build_leveldb_db_bench.log",
        cwd=REPO_ROOT,
    )
    if not db_bench.is_file() or not os.access(db_bench, os.X_OK):
        raise RuntimeError(f"LevelDB db_bench is still unavailable after build: {db_bench}")


def ensure_lock_helpers(
    locks: tuple[str, ...],
    *,
    build_missing: bool,
    logger: experiment_three.CommandLogger,
) -> None:
    experiment_three.ensure_interpose_helpers(locks, build_missing=build_missing, logger=logger)
    if "accordin" in locks:
        experiment_three.ensure_accordin_preload(build_missing=build_missing, logger=logger)
    if "mcs_extension" in locks:
        experiment_three.ensure_mcs_extension_preload(build_missing=build_missing, logger=logger)
    if "mcs_accordin" in locks:
        experiment_three.ensure_mcs_accordin_preload(build_missing=build_missing, logger=logger)


def lock_command_prefix(lock: str, jemalloc_library: Path | None) -> tuple[list[str], dict[str, str] | None]:
    if lock == "stock":
        return [], preload_env(jemalloc_library)
    if lock == "accordin":
        return [], accordin_preload_env(experiment_three.ACCORDIN_PRELOAD_LIBRARY, jemalloc_library)
    if lock == "mcs_extension":
        return [], preload_env(experiment_three.MCS_EXTENSION_PRELOAD_LIBRARY, jemalloc_library)
    if lock == "mcs_accordin":
        return [], accordin_preload_env(experiment_three.MCS_ACCORDIN_PRELOAD_LIBRARY, jemalloc_library)
    return experiment_three.interpose_command(lock, env=preload_env(jemalloc_library))


def build_db_bench_args(
    *,
    db_bench: Path,
    db_path: Path,
    benchmark: str,
    threads: int,
    num: int,
    use_existing_db: bool,
    reads: int | None = None,
) -> list[str]:
    args = [
        str(db_bench),
        f"--benchmarks={benchmark}",
        f"--threads={threads}",
        f"--num={num}",
        f"--db={db_path}",
        f"--use_existing_db={1 if use_existing_db else 0}",
    ]
    if reads is not None:
        args.append(f"--reads={reads}")
    return args


def build_measured_command(
    *,
    lock: str,
    db_bench: Path,
    db_path: Path,
    benchmark: str,
    threads: int,
    num: int,
    use_existing_db: bool,
    jemalloc_library: Path | None,
    reads: int | None = None,
) -> tuple[list[str], dict[str, str] | None]:
    cmd, env = lock_command_prefix(lock, jemalloc_library)
    cmd.extend(
        build_db_bench_args(
            db_bench=db_bench,
            db_path=db_path,
            benchmark=benchmark,
            threads=threads,
            num=num,
            reads=reads,
            use_existing_db=use_existing_db,
        )
    )
    return experiment_three.benchmark_command(lock, cmd, env)


def build_init_command(
    *,
    db_bench: Path,
    db_path: Path,
    fill_benchmark: str,
    num: int,
) -> list[str]:
    return build_db_bench_args(
        db_bench=db_bench,
        db_path=db_path,
        benchmark=fill_benchmark,
        threads=1,
        num=num,
        use_existing_db=False,
    )


def parse_latency(output: str, benchmark: str) -> float:
    fallback: float | None = None
    for match in LATENCY_PATTERN.finditer(output):
        latency = float(match.group("micros"))
        if match.group("name") == benchmark:
            return latency
        if fallback is None:
            fallback = latency
    if fallback is not None:
        return fallback
    raise RuntimeError(f"db_bench output did not contain a latency line for {benchmark}.")


def format_float(value: float) -> str:
    return experiment_three.format_float(value)


def relative_log(result_root: Path, path: Path | None) -> str:
    if path is None:
        return ""
    return str(path.relative_to(result_root))


def cleanup_db_path(path: Path) -> None:
    try:
        shutil.rmtree(path)
    except FileNotFoundError:
        return
    except PermissionError as exc:
        print(f"Warning: could not remove temporary DB path {path}: {exc}", file=sys.stderr)


def run_benchmarks(
    result_root: Path,
    *,
    benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    repeats: int,
    num: int,
    total_ops: int,
    db_bench: Path,
    fill_benchmark: str,
    init_existing_benchmarks: tuple[str, ...],
    logger: experiment_three.CommandLogger,
    jemalloc_library: Path | None,
) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = []
    init_benchmarks = set(init_existing_benchmarks)
    init_env = preload_env(jemalloc_library)

    for benchmark in benchmarks:
        use_existing_db = benchmark in init_benchmarks
        uses_reads = benchmark.startswith("read")
        for lock in locks:
            for thread in runnable_threads_for_lock(lock, threads):
                if uses_reads:
                    db_bench_num = num
                    reads_per_thread = ceil_div(total_ops, thread)
                    effective_total_ops = reads_per_thread * thread
                else:
                    db_bench_num = ceil_div(total_ops, thread)
                    reads_per_thread = None
                    effective_total_ops = db_bench_num * thread
                for repeat in range(1, repeats + 1):
                    db_path = Path(tempfile.mkdtemp(prefix="experiment4_leveldb_"))
                    init_result: experiment_three.CommandResult | None = None
                    try:
                        if use_existing_db:
                            init_cmd = build_init_command(
                                db_bench=db_bench,
                                db_path=db_path,
                                fill_benchmark=fill_benchmark,
                                num=num,
                            )
                            init_result = logger.run(
                                init_cmd,
                                log_name=(
                                    f"init_{safe_name(benchmark)}_{safe_name(lock)}_"
                                    f"{thread:03d}_r{repeat}.log"
                                ),
                                cwd=REPO_ROOT,
                                env=init_env,
                            )

                        cmd, env = build_measured_command(
                            lock=lock,
                            db_bench=db_bench,
                            db_path=db_path,
                            benchmark=benchmark,
                            threads=thread,
                            num=db_bench_num,
                            reads=reads_per_thread,
                            use_existing_db=use_existing_db,
                            jemalloc_library=jemalloc_library,
                        )
                        result = logger.run(
                            cmd,
                            log_name=f"{safe_name(benchmark)}_{safe_name(lock)}_{thread:03d}_r{repeat}.log",
                            cwd=REPO_ROOT,
                            env=env,
                        )
                        latency = parse_latency(result.output, benchmark)
                        rows.append(
                            {
                                "benchmark": benchmark,
                                "lock": lock,
                                "threads": str(thread),
                                "repeat": str(repeat),
                                "num": str(num),
                                "total_ops": str(total_ops),
                                "db_bench_num": str(db_bench_num),
                                "reads_per_thread": (
                                    "" if reads_per_thread is None else str(reads_per_thread)
                                ),
                                "effective_total_ops": str(effective_total_ops),
                                "use_existing_db": "1" if use_existing_db else "0",
                                "fill_benchmark": fill_benchmark if use_existing_db else "",
                                "init_wall_seconds": (
                                    "" if init_result is None else format_float(init_result.wall_seconds)
                                ),
                                "latency_micros_per_op": format_float(latency),
                                "wall_seconds": format_float(result.wall_seconds),
                                "init_command_log": relative_log(
                                    result_root,
                                    None if init_result is None else init_result.log_path,
                                ),
                                "command_log": relative_log(result_root, result.log_path),
                            }
                        )
                    finally:
                        cleanup_db_path(db_path)

    return rows


def write_settings(
    result_root: Path,
    *,
    benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    repeats: int,
    num: int,
    total_ops: int,
    db_bench: Path,
    leveldb_dir: Path,
    fill_benchmark: str,
    init_existing_benchmarks: tuple[str, ...],
    build_missing: bool,
    jemalloc_library: Path | None,
) -> None:
    settings = {
        "benchmarks": [{"key": benchmark, "label": benchmark_label(benchmark)} for benchmark in benchmarks],
        "locks": [{"key": lock, "label": lock_label(lock)} for lock in locks],
        "threads": list(threads),
        "machine_profile": machine_config.ACTIVE_MACHINE_CONFIG.name,
        "machine_profile_env": machine_config.PROFILE_ENV,
        "per_lock_max_threads": {
            lock: max_threads for lock, max_threads in PER_LOCK_MAX_THREADS.items() if lock in locks
        },
        "runnable_threads_by_lock": {lock: list(runnable_threads_for_lock(lock, threads)) for lock in locks},
        "repeats": repeats,
        "num": num,
        "total_ops": total_ops,
        "build_missing": build_missing,
        "db_bench": str(db_bench),
        "leveldb_dir": str(leveldb_dir),
        "leveldb_version": LEVELDB_VERSION if leveldb_dir == DEFAULT_LEVELDB_DIR else "custom",
        "fill_benchmark": fill_benchmark,
        "init_existing_benchmarks": list(init_existing_benchmarks),
        "jemalloc": {
            "enabled": jemalloc_library is not None,
            "library": None if jemalloc_library is None else str(jemalloc_library),
        },
        "flexguard_dir": str(FLEXGUARD_DIR),
        "machine_core_count": experiment_three.MACHINE_CORE_COUNT,
    }
    with (result_root / "settings.json").open("w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")


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
        required_fields = [field for field in RAW_FIELDS if field not in {
            "total_ops",
            "db_bench_num",
            "reads_per_thread",
            "effective_total_ops",
        }]
        missing = [field for field in required_fields if field not in reader.fieldnames]
        if missing:
            raise RuntimeError(f"raw.csv is missing required columns: {', '.join(missing)}")
        rows = []
        for row in reader:
            rows.append({field: row.get(field, "") for field in RAW_FIELDS})
        return rows


def mean_field(rows: list[dict[str, str]], field: str) -> str:
    values = [float(row[field]) for row in rows if row[field].strip()]
    if not values:
        return ""
    return format_float(mean(values))


def row_ops_per_second(row: dict[str, str]) -> float | None:
    effective_total_ops = row["effective_total_ops"].strip()
    wall_seconds = row["wall_seconds"].strip()
    if effective_total_ops and wall_seconds:
        wall = float(wall_seconds)
        if wall > 0:
            return float(effective_total_ops) / wall

    latency = row["latency_micros_per_op"].strip()
    if latency:
        latency_micros = float(latency)
        if latency_micros > 0:
            return 1_000_000.0 / latency_micros
    return None


def mean_ops_per_second(rows: list[dict[str, str]]) -> str:
    values = [ops for row in rows if (ops := row_ops_per_second(row)) is not None]
    if not values:
        return ""
    return format_float(mean(values))


def summarize_rows(rows: list[dict[str, str]]) -> list[dict[str, str]]:
    groups: dict[tuple[str, str, int], list[dict[str, str]]] = {}
    for row in rows:
        key = (row["benchmark"], row["lock"], int(row["threads"]))
        groups.setdefault(key, []).append(row)

    summary_rows: list[dict[str, str]] = []
    for benchmark, lock, thread_count in sorted(
        groups.keys(),
        key=lambda item: (benchmark_sort_key(item[0]), lock_sort_key(item[1]), item[2]),
    ):
        group_rows = groups[(benchmark, lock, thread_count)]
        summary_rows.append(
            {
                "benchmark": benchmark,
                "lock": lock,
                "threads": str(thread_count),
                "mean_latency_micros_per_op": mean_field(group_rows, "latency_micros_per_op"),
                "mean_ops_per_second": mean_ops_per_second(group_rows),
                "mean_init_wall_seconds": mean_field(group_rows, "init_wall_seconds"),
                "mean_wall_seconds": mean_field(group_rows, "wall_seconds"),
                "runs": str(len(group_rows)),
            }
        )
    return summary_rows


def write_summary_csv(result_root: Path, rows: list[dict[str, str]]) -> Path:
    path = result_root / "summary.csv"
    with path.open("w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=SUMMARY_FIELDS)
        writer.writeheader()
        writer.writerows(rows)
    return path


def unique_threads(summary_rows: list[dict[str, str]], benchmark: str) -> list[int]:
    return sorted({int(row["threads"]) for row in summary_rows if row["benchmark"] == benchmark})


def plot_ops(summary_rows: list[dict[str, str]], *, benchmark: str, output_path: Path) -> None:
    try:
        import matplotlib
    except ModuleNotFoundError as exc:
        raise RuntimeError("matplotlib is required to generate plots.") from exc

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.ticker import ScalarFormatter

    rows = [
        row
        for row in summary_rows
        if row["benchmark"] == benchmark and row["mean_ops_per_second"].strip()
    ]
    if not rows:
        raise RuntimeError(f"No summary rows available for benchmark {benchmark}.")

    fig, ax = plt.subplots(figsize=(9.5, 5.5))
    thread_values = unique_threads(summary_rows, benchmark)
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

    ax.set_title(f"Throughput vs Threads: {benchmark_label(benchmark)}")
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


def write_plots(result_root: Path, summary_rows: list[dict[str, str]]) -> list[Path]:
    plot_paths: list[Path] = []
    benchmarks = sorted({row["benchmark"] for row in summary_rows}, key=benchmark_sort_key)
    for benchmark in benchmarks:
        output_path = result_root / f"ops_vs_threads_{safe_name(benchmark)}.png"
        plot_ops(summary_rows, benchmark=benchmark, output_path=output_path)
        plot_paths.append(output_path)
    if not plot_paths:
        raise RuntimeError("No summary rows were available for plotting.")
    return plot_paths


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

        benchmarks = validate_benchmark_names(parse_csv_strings(args.benchmarks))
        locks = experiment_three.validate_locks(
            tuple(dict.fromkeys(experiment_three.normalize_lock(lock) for lock in parse_csv_strings(args.locks)))
        )
        threads = parse_csv_ints(args.threads)
        leveldb_dir = resolve_path(args.leveldb_dir)
        db_bench = resolve_path(args.db_bench) if args.db_bench is not None else default_db_bench_for_leveldb(leveldb_dir)
        fill_benchmark = validate_benchmark_names((args.fill_benchmark,))[0]
        init_existing_benchmarks = parse_init_existing_benchmarks(args.init_existing_benchmarks)

        if args.plot_only is not None:
            result_root = resolve_path(args.plot_only)
            if not result_root.is_dir():
                print(f"Plot-only result root does not exist: {result_root}", file=sys.stderr)
                return 2
            raw_rows = load_raw_rows(result_root)
            raw_path = result_root / "raw.csv"
            summary_rows = summarize_rows(raw_rows)
            summary_path = write_summary_csv(result_root, summary_rows)
            plot_paths = write_plots(result_root, summary_rows)
            print_outputs(result_root, raw_path, summary_path, plot_paths)
            return 0

        if args.jemalloc_library is None and uses_flexguard_pthread_interpose(locks):
            jemalloc_library = None
        else:
            jemalloc_library = resolve_jemalloc_library(args.jemalloc_library)
        result_root = resolve_path(args.output_root) if args.output_root is not None else default_result_root()
        experiment_three.ensure_output_root(result_root, args.force)
        logger = experiment_three.CommandLogger(result_root)

        ensure_lock_helpers(locks, build_missing=args.build_missing, logger=logger)
        ensure_db_bench(db_bench, leveldb_dir=leveldb_dir, build_missing=args.build_missing, logger=logger)

        write_settings(
            result_root,
            benchmarks=benchmarks,
            locks=locks,
            threads=threads,
            repeats=args.repeats,
            num=args.num,
            total_ops=args.total_ops,
            db_bench=db_bench,
            leveldb_dir=leveldb_dir,
            fill_benchmark=fill_benchmark,
            init_existing_benchmarks=init_existing_benchmarks,
            build_missing=args.build_missing,
            jemalloc_library=jemalloc_library,
        )
        raw_rows = run_benchmarks(
            result_root,
            benchmarks=benchmarks,
            locks=locks,
            threads=threads,
            repeats=args.repeats,
            num=args.num,
            total_ops=args.total_ops,
            db_bench=db_bench,
            fill_benchmark=fill_benchmark,
            init_existing_benchmarks=init_existing_benchmarks,
            logger=logger,
            jemalloc_library=jemalloc_library,
        )
        raw_path = write_raw_csv(result_root, raw_rows)
        summary_rows = summarize_rows(raw_rows)
        summary_path = write_summary_csv(result_root, summary_rows)
        plot_paths = write_plots(result_root, summary_rows)
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
