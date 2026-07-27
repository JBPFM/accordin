#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from statistics import mean, median
from typing import Iterable

import experiment_defaults
import experiment_failures
import run_experiment_three as experiment_three


REPO_ROOT = experiment_three.REPO_ROOT
FLEXGUARD_DIR = experiment_three.FLEXGUARD_DIR
LEVELDB_VERSION = "1.23"
DEFAULT_LEVELDB_DIR = REPO_ROOT / "third_party" / f"leveldb-{LEVELDB_VERSION}"
DEFAULT_DB_BENCH = DEFAULT_LEVELDB_DIR / "build" / "db_bench"

DEFAULT_BENCHMARKS = ("readrandom", "fillrandom")
DEFAULT_LOCK_PROFILE = experiment_defaults.DEFAULT_LOCK_PROFILE
DEFAULT_THREADS = experiment_defaults.DEFAULT_THREADS
DEFAULT_REPEATS = 5
SUMMARY_DISCARD_INITIAL_REPEATS = 1
SUMMARY_OUTLIER_ABS_DEVIATION_PCT = 20.0
SUMMARY_OUTLIER_MIN_ROWS = 3
PLOT_MIN_THREADS = 4
PLOT_EXCLUDED_LOCKS = {"mcs_extension"}
PLOT_TITLE_FONTSIZE = 18
PLOT_LABEL_FONTSIZE = 16
PLOT_TICK_FONTSIZE = 13
PLOT_LEGEND_FONTSIZE = 13
DEFAULT_NUM = 500_000
DEFAULT_TOTAL_OPS = 1_572_864
DEFAULT_FILL_BENCHMARK = "fillseq"
INIT_FILL_THREADS = 1
DEFAULT_INIT_EXISTING_BENCHMARKS = ("readrandom", "readseq", "overwrite")
DEFAULT_COMMAND_TIMEOUT_SECONDS = 3600
ACCORDIN_HOOK_SCOPE_ENV = "ACCORDIN_HOOK_SCOPE"
ACCORDIN_LEVELDB_HOOK_SCOPE: str | None = None
# LevelDB locks are interposed through the pthread mutex hook. Locks without a pthread
# interpose helper cannot be loaded that way and are excluded from the lock profiles.
LEVELDB_UNSUPPORTED_LOCKS = frozenset(
    {
        experiment_defaults.PTHREAD_SPINLOCK_LOCK,
        experiment_defaults.MCS_ACCORDIN_LOCK,
    }
)
MINIMAL_LOCKS = experiment_defaults.ACCORDIN_VARIANT_LOCKS
FULL_LOCKS = tuple(
    dict.fromkeys(
        (
            "mutex",
            *(
                lock
                for lock in experiment_defaults.FULL_LOCKS
                if lock != "mutex" and lock not in LEVELDB_UNSUPPORTED_LOCKS
            ),
        )
    )
)
LOCK_PROFILES = {
    "minimal": MINIMAL_LOCKS,
    "full": FULL_LOCKS,
}
DEFAULT_LOCKS = LOCK_PROFILES[DEFAULT_LOCK_PROFILE]
LEVELDB_REBUILD_INPUTS = (
    Path("db/db_impl.cc"),
    Path("db/db_impl.h"),
    Path("port/port_stdcxx.h"),
)
SINGLE_OVERSUBSCRIBED_LOCKS = experiment_defaults.SINGLE_OVERSUBSCRIBED_LOCKS
PER_LOCK_MAX_THREADS = experiment_defaults.per_lock_max_threads_for_settings(
    SINGLE_OVERSUBSCRIBED_LOCKS,
    DEFAULT_THREADS,
)

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
    "benchmark_elapsed_ns",
    "leveldb_lock_hold_ns_total",
    "leveldb_lock_hold_samples",
    "avg_lock_hold_ns",
    "avg_lock_handoff_ns_estimated",
    "wall_seconds",
    "init_command_log",
    "command_log",
)
SUMMARY_FIELDS = (
    "benchmark",
    "lock",
    "threads",
    "mean_latency_micros_per_op",
    "mean_benchmark_seconds",
    "mean_ops_per_second",
    "mean_benchmark_elapsed_ns",
    "mean_leveldb_lock_hold_ns_total",
    "mean_leveldb_lock_hold_samples",
    "mean_avg_lock_hold_ns",
    "mean_avg_lock_handoff_ns_estimated",
    "mean_init_wall_seconds",
    "mean_wall_seconds",
    "runs",
)
LATENCY_PATTERN = re.compile(
    r"^(?P<name>\w+)\s+:\s+(?P<micros>\d+(?:\.\d+)?)\s+micros/op;",
    re.MULTILINE,
)
LEVELDB_PROGRESS_PATTERN = re.compile(r"^\.\.\. (?:finished \d+ ops|crc=0x[0-9a-fA-F]+)\s*$")
LOCK_TIMING_PATTERN = re.compile(
    r"^(?P<name>"
    r"leveldb_lock_benchmark_elapsed_ns|"
    r"leveldb_lock_hold_ns_total|"
    r"leveldb_lock_hold_samples|"
    r"leveldb_lock_avg_hold_ns"
    r"):\s+(?P<value>\d+(?:\.\d+)?)\s*$",
    re.MULTILINE,
)
BENCHMARK_NAME_PATTERN = re.compile(r"^[A-Za-z0-9_]+$")
RunKey = tuple[str, str, int, int]


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
  lock-profile={DEFAULT_LOCK_PROFILE}
  minimal locks={','.join(MINIMAL_LOCKS)}
  full locks={','.join(FULL_LOCKS)}
  machine-profile={experiment_defaults.ACTIVE_MACHINE_CONFIG.name} (override with {experiment_defaults.PROFILE_ENV})
  threads={','.join(str(thread) for thread in DEFAULT_THREADS)}
  repeats={DEFAULT_REPEATS}, discard_initial_repeats={SUMMARY_DISCARD_INITIAL_REPEATS}, num={DEFAULT_NUM}, total_ops={DEFAULT_TOTAL_OPS}
  leveldb={DEFAULT_LEVELDB_DIR} (tag {LEVELDB_VERSION})
  db_bench={DEFAULT_DB_BENCH}
  fill_benchmark={DEFAULT_FILL_BENCHMARK}, init_fill_threads={INIT_FILL_THREADS}
  init_existing_benchmarks={','.join(DEFAULT_INIT_EXISTING_BENCHMARKS)}
  per_lock_max_threads={','.join(f"{lock}:{max_threads}" for lock, max_threads in PER_LOCK_MAX_THREADS.items())}

Examples:
  python3 experiments/run_experiment_four.py
  python3 experiments/run_experiment_four.py --locks mutex --threads 1 --repeats 1 --benchmarks fillseq --num 1000
  {experiment_defaults.PROFILE_ENV}=original python3 experiments/run_experiment_four.py
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
    parser.add_argument("--skip-plots", action="store_true", help="Write CSVs but skip PNG generation.")
    parser.add_argument(
        "--force",
        action="store_true",
        help="Allow benchmark output into an existing non-empty output root.",
    )
    parser.add_argument(
        "--keep-db",
        action="store_true",
        help="Preserve each temporary LevelDB DB directory instead of removing it after the run.",
    )
    parser.add_argument(
        "--rerun-failures",
        type=Path,
        default=None,
        metavar="FAILED_RUNS_CSV",
        help="Run only benchmark/lock/thread/repeat points listed in an experiment failed_runs.csv.",
    )
    parser.add_argument(
        "--build-missing",
        action="store_true",
        help="Build missing interpose helpers or db_bench before running.",
    )
    parser.add_argument(
        "--command-timeout-seconds",
        type=experiment_three.non_negative_int,
        default=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        help=(
            "Outer timeout for each logged benchmark command. 0 disables it. "
            f"Default: {DEFAULT_COMMAND_TIMEOUT_SECONDS}."
        ),
    )
    parser.add_argument(
        "--benchmarks",
        default=",".join(DEFAULT_BENCHMARKS),
        metavar="CSV",
        help=f"Comma-separated LevelDB db_bench benchmark names. Default: {','.join(DEFAULT_BENCHMARKS)}.",
    )
    parser.add_argument(
        "--lock-profile",
        choices=tuple(LOCK_PROFILES),
        default=DEFAULT_LOCK_PROFILE,
        help=(
            "Named lock set used when --locks is omitted. "
            f"Default: {DEFAULT_LOCK_PROFILE}. "
            f"minimal={','.join(MINIMAL_LOCKS)}; "
            f"full={','.join(FULL_LOCKS)}."
        ),
    )
    parser.add_argument(
        "--locks",
        default=None,
        metavar="CSV",
        help=(
            "Comma-separated lock keys. Overrides --lock-profile. "
            "Use mutex to run without interpose. "
            "Every other lock runs through the pthread mutex hook. "
            "Aliases: mcs-tas == mcstas, mcs_tse/mcs-tse == mcs_extension; "
            "only the admission-only Accordin variant is supported."
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
    return parser.parse_args()


def parse_csv_strings(value: str) -> tuple[str, ...]:
    return experiment_three.parse_csv_strings(value)


def parse_csv_ints(value: str) -> tuple[int, ...]:
    return experiment_three.parse_csv_ints(value)


def normalize_leveldb_lock(lock: str) -> str:
    return experiment_defaults.normalize_lock(lock.strip().lower())


def validate_leveldb_locks(locks: tuple[str, ...]) -> tuple[str, ...]:
    supported = (
        set(FULL_LOCKS)
        | set(MINIMAL_LOCKS)
        | set(experiment_defaults.LOCK_ALIASES.values())
        | set(experiment_defaults.LOCK_LABELS)
    ) - set(LEVELDB_UNSUPPORTED_LOCKS)
    unsupported = [lock for lock in locks if lock not in supported]
    if unsupported:
        raise ValueError(
            f"Unsupported lock keys: {', '.join(unsupported)}. "
            f"Supported keys: {', '.join(sorted(supported))}"
        )
    return locks


def resolve_leveldb_locks(
    *,
    profile: str,
    locks: Iterable[str] | None,
) -> tuple[str, ...]:
    try:
        raw_locks = LOCK_PROFILES[profile] if locks is None else tuple(locks)
    except KeyError as exc:
        supported_profiles = ", ".join(LOCK_PROFILES)
        raise ValueError(
            f"Unsupported lock profile: {profile}. Supported profiles: {supported_profiles}."
        ) from exc
    normalized = tuple(dict.fromkeys(normalize_leveldb_lock(lock) for lock in raw_locks))
    return validate_leveldb_locks(normalized)


def validate_benchmark_names(benchmarks: tuple[str, ...]) -> tuple[str, ...]:
    invalid = [benchmark for benchmark in benchmarks if BENCHMARK_NAME_PATTERN.fullmatch(benchmark) is None]
    if invalid:
        raise ValueError(f"Unsupported benchmark names: {', '.join(invalid)}")
    return benchmarks


def parse_init_existing_benchmarks(value: str) -> tuple[str, ...]:
    if value.strip().lower() in {"", "none"}:
        return ()
    return validate_benchmark_names(parse_csv_strings(value))


def load_failure_selection(path: Path) -> set[RunKey]:
    path = resolve_path(path)
    if not path.is_file():
        raise RuntimeError(f"Failure CSV was not found: {path}")
    selected: set[RunKey] = set()
    with path.open("r", encoding="utf-8", newline="") as f:
        reader = csv.DictReader(f)
        if reader.fieldnames is None:
            raise RuntimeError(f"Failure CSV is missing a header: {path}")
        required = {"benchmark", "lock", "threads", "repeat"}
        missing = sorted(required - set(reader.fieldnames))
        if missing:
            raise RuntimeError(f"Failure CSV is missing required columns: {', '.join(missing)}")
        for row in reader:
            benchmark = row["benchmark"].strip()
            lock = row["lock"].strip()
            if not benchmark or not lock:
                continue
            selected.add((benchmark, lock, int(row["threads"]), int(row["repeat"])))
    if not selected:
        raise RuntimeError(f"Failure CSV did not contain any runnable experiment-four failures: {path}")
    return selected


def resolve_path(path: Path) -> Path:
    return experiment_three.resolve_path(path)


def existing_ld_preload_entries() -> tuple[str, ...]:
    existing = os.environ.get("LD_PRELOAD", "").strip()
    if not existing:
        return ()
    return tuple(entry for entry in existing.split(":") if entry)


def merge_ld_preload_entries(*libraries: Path | None) -> str | None:
    entries = [str(library) for library in libraries if library is not None]
    entries.extend(existing_ld_preload_entries())
    return ":".join(entries) if entries else None


def preload_env(*libraries: Path | None) -> dict[str, str] | None:
    ld_preload = merge_ld_preload_entries(*libraries)
    if ld_preload is None:
        return None
    return {"LD_PRELOAD": ld_preload}


def accordin_preload_env(
    preload_library: Path,
    *,
    lock: str = experiment_defaults.ACCORDIN_BASE_LOCK,
) -> dict[str, str | None]:
    env = experiment_three.accordin_preload_env(preload_library, lock=lock)
    env[ACCORDIN_HOOK_SCOPE_ENV] = ACCORDIN_LEVELDB_HOOK_SCOPE
    env.update(experiment_defaults.accordin_width_env())
    return env


def default_result_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment4_{timestamp}"


def lock_label(lock: str) -> str:
    return experiment_defaults.lock_label(lock)


def lock_plot_color_key(lock: str) -> str:
    return lock_plot_style_key(lock)


def lock_plot_style_key(lock: str) -> str:
    return normalize_leveldb_lock(lock)


def lock_plot_style(lock: str, color_by_key: dict[str, str]) -> dict[str, str]:
    style_lock = lock_plot_style_key(lock)
    return {
        "color": experiment_three.plot_color(style_lock, color_by_key[style_lock]),
        "linestyle": experiment_three.plot_linestyle(style_lock),
        "marker": experiment_three.plot_marker(style_lock),
    }


def lock_sort_key(lock: str) -> tuple[int, str]:
    return experiment_defaults.lock_sort_key(lock)


def runnable_threads_for_lock(lock: str, threads: tuple[int, ...]) -> tuple[int, ...]:
    return experiment_defaults.runnable_threads_for_lock(lock, threads)


def per_lock_max_threads_for_settings(
    locks: tuple[str, ...],
    threads: tuple[int, ...],
) -> dict[str, int]:
    return experiment_defaults.per_lock_max_threads_for_settings(locks, threads)


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


def db_bench_needs_rebuild(db_bench: Path, leveldb_dir: Path) -> bool:
    try:
        db_bench_mtime = db_bench.stat().st_mtime
    except OSError:
        return True

    for relative_source in LEVELDB_REBUILD_INPUTS:
        source = leveldb_dir / relative_source
        try:
            if source.stat().st_mtime > db_bench_mtime:
                return True
        except OSError:
            continue
    return False


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
        timeout_seconds=0,
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
    if build_missing and leveldb_dir == DEFAULT_LEVELDB_DIR:
        ensure_leveldb_source(leveldb_dir, build_missing=build_missing, logger=logger)

    if db_bench.is_file() and os.access(db_bench, os.X_OK):
        if not (build_missing and db_bench_needs_rebuild(db_bench, leveldb_dir)):
            return
    if db_bench.is_file() and os.access(db_bench, os.X_OK) and not build_missing:
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
        timeout_seconds=0,
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
        timeout_seconds=0,
    )
    if not db_bench.is_file() or not os.access(db_bench, os.X_OK):
        raise RuntimeError(f"LevelDB db_bench is still unavailable after build: {db_bench}")


def ensure_lock_helpers(
    locks: tuple[str, ...],
    *,
    build_missing: bool,
    logger: experiment_three.CommandLogger,
) -> None:
    interpose_locks = tuple(
        lock
        for lock in locks
        if lock != "mutex" and not experiment_defaults.is_accordin_lock(lock)
    )
    experiment_three.ensure_interpose_helpers(interpose_locks, build_missing=build_missing, logger=logger)
    if any(experiment_defaults.is_accordin_lock(lock) for lock in locks):
        experiment_three.ensure_accordin_preload(build_missing=build_missing, logger=logger)
    if "mcs_extension" in locks:
        experiment_three.ensure_mcs_extension_preload(build_missing=build_missing, logger=logger)


def lock_command_prefix(lock: str) -> tuple[list[str], dict[str, str | None] | None]:
    lock = normalize_leveldb_lock(lock)
    if experiment_three.is_native_mutex_lock(lock):
        return [], None
    if experiment_defaults.is_accordin_lock(lock):
        return experiment_three.accordin_command_prefix_from_env(
            lock,
            accordin_preload_env(experiment_three.ACCORDIN_PRELOAD_LIBRARY, lock=lock),
        )
    if lock == "mcs_extension":
        return [], preload_env(experiment_three.MCS_EXTENSION_PRELOAD_LIBRARY)
    return experiment_three.interpose_command(lock)


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
    reads: int | None = None,
) -> tuple[list[str], dict[str, str | None] | None]:
    lock = normalize_leveldb_lock(lock)
    cmd, env = lock_command_prefix(lock)
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
        threads=INIT_FILL_THREADS,
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


def parse_lock_timing_metrics(output: str) -> dict[str, str]:
    metrics: dict[str, str] = {}
    for match in LOCK_TIMING_PATTERN.finditer(output):
        metrics[match.group("name")] = match.group("value")
    return metrics


def lock_timing_fields_from_output(output: str, *, effective_total_ops: int) -> dict[str, str]:
    metrics = parse_lock_timing_metrics(output)
    fields = {
        "benchmark_elapsed_ns": "",
        "leveldb_lock_hold_ns_total": "",
        "leveldb_lock_hold_samples": "",
        "avg_lock_hold_ns": "",
        "avg_lock_handoff_ns_estimated": "",
    }
    if not metrics:
        return fields

    elapsed = metrics.get("leveldb_lock_benchmark_elapsed_ns")
    hold_total = metrics.get("leveldb_lock_hold_ns_total")
    hold_samples = metrics.get("leveldb_lock_hold_samples")
    avg_hold = metrics.get("leveldb_lock_avg_hold_ns")
    if elapsed is None or hold_total is None or hold_samples is None:
        missing = [
            name
            for name, value in (
                ("leveldb_lock_benchmark_elapsed_ns", elapsed),
                ("leveldb_lock_hold_ns_total", hold_total),
                ("leveldb_lock_hold_samples", hold_samples),
            )
            if value is None
        ]
        raise RuntimeError(f"db_bench lock timing output is missing: {', '.join(missing)}")

    elapsed_ns = int(float(elapsed))
    hold_total_ns = int(float(hold_total))
    if hold_total_ns > elapsed_ns:
        raise RuntimeError(
            "LevelDB lock hold total exceeds benchmark elapsed time: "
            f"hold_ns_total={hold_total_ns} elapsed_ns={elapsed_ns}"
        )

    fields["benchmark_elapsed_ns"] = str(elapsed_ns)
    fields["leveldb_lock_hold_ns_total"] = str(hold_total_ns)
    fields["leveldb_lock_hold_samples"] = str(int(float(hold_samples)))
    fields["avg_lock_hold_ns"] = format_float(
        float(avg_hold) if avg_hold is not None else (
            hold_total_ns / float(fields["leveldb_lock_hold_samples"])
            if int(fields["leveldb_lock_hold_samples"]) > 0
            else 0.0
        )
    )
    if effective_total_ops > 0:
        fields["avg_lock_handoff_ns_estimated"] = format_float(
            (elapsed_ns - hold_total_ns) / float(effective_total_ops)
        )
    return fields


def drop_leveldb_progress_line(line: str) -> str | None:
    if LEVELDB_PROGRESS_PATTERN.fullmatch(line.strip()):
        return None
    return line


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
        if path.name.startswith("experiment4_leveldb_"):
            result = subprocess.run(
                ["sudo", "-n", "rm", "-rf", "--", str(path)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
            if result.returncode == 0:
                return
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
    failures: list[dict[str, str]],
    selected_runs: set[RunKey] | None,
    keep_db: bool,
) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = []
    init_benchmarks = set(init_existing_benchmarks)

    for benchmark in benchmarks:
        use_existing_db = benchmark in init_benchmarks
        uses_reads = benchmark.startswith("read")
        for lock in locks:
            effective_lock = normalize_leveldb_lock(lock)
            for thread in runnable_threads_for_lock(effective_lock, threads):
                if uses_reads:
                    db_bench_num = num
                    reads_per_thread = ceil_div(total_ops, thread)
                    effective_total_ops = reads_per_thread * thread
                else:
                    db_bench_num = ceil_div(total_ops, thread)
                    reads_per_thread = None
                    effective_total_ops = db_bench_num * thread
                for repeat in range(1, repeats + 1):
                    run_key = (benchmark, effective_lock, thread, repeat)
                    if selected_runs is not None and run_key not in selected_runs:
                        continue
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
                                    f"init_{safe_name(benchmark)}_{safe_name(effective_lock)}_"
                                    f"{thread:03d}_r{repeat}.log"
                                ),
                                cwd=REPO_ROOT,
                            )

                        cmd, env = build_measured_command(
                            lock=effective_lock,
                            db_bench=db_bench,
                            db_path=db_path,
                            benchmark=benchmark,
                            threads=thread,
                            num=db_bench_num,
                            reads=reads_per_thread,
                            use_existing_db=use_existing_db,
                        )
                        result = logger.run(
                            cmd,
                            log_name=(
                                f"{safe_name(benchmark)}_{safe_name(effective_lock)}_"
                                f"{thread:03d}_r{repeat}.log"
                            ),
                            cwd=REPO_ROOT,
                            env=env,
                        )
                        latency = parse_latency(result.output, benchmark)
                        lock_timing_fields = lock_timing_fields_from_output(
                            result.output,
                            effective_total_ops=effective_total_ops,
                        )
                        rows.append(
                            {
                                "benchmark": benchmark,
                                "lock": effective_lock,
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
                                **lock_timing_fields,
                                "wall_seconds": format_float(result.wall_seconds),
                                "init_command_log": relative_log(
                                    result_root,
                                    None if init_result is None else init_result.log_path,
                                ),
                                "command_log": relative_log(result_root, result.log_path),
                            }
                        )
                    except experiment_three.CommandError as exc:
                        stage = "init" if use_existing_db and init_result is None else "run"
                        experiment_failures.append_command_failure(
                            failures,
                            result_root=result_root,
                            experiment="experiment4",
                            workload="leveldb",
                            benchmark=benchmark,
                            lock=effective_lock,
                            threads=thread,
                            repeat=repeat,
                            stage=stage,
                            exc=exc,
                        )
                        experiment_failures.write_failures_csv(result_root, failures)
                        continue
                    finally:
                        if keep_db:
                            print(f"Preserved temporary DB path: {db_path}", file=sys.stderr)
                        else:
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
    lock_profile: str,
    lock_profile_source: str,
    command_timeout_seconds: int,
) -> None:
    effective_locks = tuple(dict.fromkeys(normalize_leveldb_lock(lock) for lock in locks))
    uses_mutex_hook_accordin = any(
        experiment_defaults.is_accordin_lock(lock)
        for lock in effective_locks
    )
    settings = {
        "benchmarks": [{"key": benchmark, "label": benchmark_label(benchmark)} for benchmark in benchmarks],
        "locks": [{"key": lock, "label": lock_label(lock)} for lock in locks],
        "effective_locks": list(effective_locks),
        "lock_profile": lock_profile,
        "lock_profile_source": lock_profile_source,
        "threads": list(threads),
        "machine_profile": experiment_defaults.ACTIVE_MACHINE_CONFIG.name,
        "machine_profile_env": experiment_defaults.PROFILE_ENV,
        "single_oversubscribed_locks": list(SINGLE_OVERSUBSCRIBED_LOCKS),
        "per_lock_max_threads": per_lock_max_threads_for_settings(locks, threads),
        "runnable_threads_by_lock": {lock: list(runnable_threads_for_lock(lock, threads)) for lock in locks},
        "repeats": repeats,
        "summary_discard_initial_repeats": SUMMARY_DISCARD_INITIAL_REPEATS,
        "summary_outlier_abs_deviation_pct": SUMMARY_OUTLIER_ABS_DEVIATION_PCT,
        "summary_outlier_baseline": "median",
        "summary_ops_source": "leveldb_internal_micros_per_op",
        "lock_timing_metrics": {
            "source": "leveldb_db_mutex_hold_time",
            "handoff_estimate": "(benchmark_elapsed_ns - leveldb_lock_hold_ns_total) / effective_total_ops",
            "aggregation": "thread_local_hold_time_flushed_after_measurement",
        },
        "num": num,
        "total_ops": total_ops,
        "command_timeout_seconds": command_timeout_seconds,
        "build_missing": build_missing,
        "db_bench": str(db_bench),
        "leveldb_dir": str(leveldb_dir),
        "leveldb_version": LEVELDB_VERSION if leveldb_dir == DEFAULT_LEVELDB_DIR else "custom",
        "fill_benchmark": fill_benchmark,
        "init_fill_threads": INIT_FILL_THREADS,
        "init_existing_benchmarks": list(init_existing_benchmarks),
        "accordin_hook_scope_env": ACCORDIN_HOOK_SCOPE_ENV,
        "accordin_hook_scope": ACCORDIN_LEVELDB_HOOK_SCOPE if uses_mutex_hook_accordin else None,
        "accordin_width_env": experiment_defaults.accordin_width_env(),
        "leveldb_accordin_mutex_hook_variants": list(experiment_defaults.ACCORDIN_VARIANT_LOCKS),
        "mcs_tas_accordin_mutex_hook_preload": str(experiment_three.ACCORDIN_PRELOAD_LIBRARY),
        "flexguard_dir": str(FLEXGUARD_DIR),
        "machine_core_count": experiment_defaults.MACHINE_CORE_COUNT,
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
            "benchmark_elapsed_ns",
            "leveldb_lock_hold_ns_total",
            "leveldb_lock_hold_samples",
            "avg_lock_hold_ns",
            "avg_lock_handoff_ns_estimated",
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
    benchmark_seconds = row_benchmark_seconds(row)
    per_thread_ops = row_per_thread_ops(row)
    if benchmark_seconds is not None and benchmark_seconds > 0 and per_thread_ops is not None:
        return int(row["threads"]) * per_thread_ops / benchmark_seconds

    effective_total_ops = row["effective_total_ops"].strip()
    wall_seconds = row["wall_seconds"].strip()
    if effective_total_ops and wall_seconds:
        wall = float(wall_seconds)
        if wall > 0:
            return float(effective_total_ops) / wall

    return None


def row_per_thread_ops(row: dict[str, str]) -> int | None:
    reads_per_thread = row["reads_per_thread"].strip()
    if reads_per_thread:
        return int(reads_per_thread)

    db_bench_num = row["db_bench_num"].strip()
    if db_bench_num:
        return int(db_bench_num)

    effective_total_ops = row["effective_total_ops"].strip()
    threads = row["threads"].strip()
    if effective_total_ops and threads:
        thread_count = int(threads)
        if thread_count > 0:
            return ceil_div(int(effective_total_ops), thread_count)
    return None


def row_benchmark_seconds(row: dict[str, str]) -> float | None:
    latency = row["latency_micros_per_op"].strip()
    per_thread_ops = row_per_thread_ops(row)
    if not latency or per_thread_ops is None:
        return None
    latency_micros = float(latency)
    if latency_micros <= 0:
        return None
    return per_thread_ops * latency_micros / 1_000_000.0


def mean_ops_per_second(rows: list[dict[str, str]]) -> str:
    values = [ops for row in rows if (ops := row_ops_per_second(row)) is not None]
    if not values:
        return ""
    return format_float(mean(values))


def mean_benchmark_seconds(rows: list[dict[str, str]]) -> str:
    values = [
        seconds
        for row in rows
        if (seconds := row_benchmark_seconds(row)) is not None
    ]
    if not values:
        return ""
    return format_float(mean(values))


def discard_initial_repeat_rows(rows: list[dict[str, str]]) -> list[dict[str, str]]:
    if len(rows) <= SUMMARY_DISCARD_INITIAL_REPEATS:
        return rows
    repeat_rows = sorted(rows, key=lambda row: int(row["repeat"]))
    return repeat_rows[SUMMARY_DISCARD_INITIAL_REPEATS:]


def filter_summary_outlier_rows(rows: list[dict[str, str]]) -> list[dict[str, str]]:
    ops_values = [
        (row, ops)
        for row in rows
        if (ops := row_ops_per_second(row)) is not None
    ]
    if len(ops_values) < SUMMARY_OUTLIER_MIN_ROWS:
        return rows
    baseline = median(ops for _, ops in ops_values)
    if baseline <= 0:
        return rows

    max_deviation = SUMMARY_OUTLIER_ABS_DEVIATION_PCT / 100.0
    outlier_ids = {
        id(row)
        for row, ops in ops_values
        if abs((ops / baseline) - 1.0) + 1e-12 >= max_deviation
    }
    if not outlier_ids:
        return rows
    filtered_rows = [row for row in rows if id(row) not in outlier_ids]
    return filtered_rows or rows


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
        group_rows = filter_summary_outlier_rows(
            discard_initial_repeat_rows(groups[(benchmark, lock, thread_count)])
        )
        if not group_rows:
            continue
        summary_rows.append(
            {
                "benchmark": benchmark,
                "lock": lock,
                "threads": str(thread_count),
                "mean_latency_micros_per_op": mean_field(group_rows, "latency_micros_per_op"),
                "mean_benchmark_seconds": mean_benchmark_seconds(group_rows),
                "mean_ops_per_second": mean_ops_per_second(group_rows),
                "mean_benchmark_elapsed_ns": mean_field(group_rows, "benchmark_elapsed_ns"),
                "mean_leveldb_lock_hold_ns_total": mean_field(
                    group_rows,
                    "leveldb_lock_hold_ns_total",
                ),
                "mean_leveldb_lock_hold_samples": mean_field(
                    group_rows,
                    "leveldb_lock_hold_samples",
                ),
                "mean_avg_lock_hold_ns": mean_field(group_rows, "avg_lock_hold_ns"),
                "mean_avg_lock_handoff_ns_estimated": mean_field(
                    group_rows,
                    "avg_lock_handoff_ns_estimated",
                ),
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
    return sorted(
        {
            int(row["threads"])
            for row in summary_rows
            if row["benchmark"] == benchmark and plot_row_included(row)
        }
    )


def plot_row_included(row: dict[str, str]) -> bool:
    if int(row["threads"]) < PLOT_MIN_THREADS:
        return False
    return normalize_leveldb_lock(row["lock"]) not in PLOT_EXCLUDED_LOCKS


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
        if (
            row["benchmark"] == benchmark
            and row["mean_ops_per_second"].strip()
            and plot_row_included(row)
        )
    ]
    if not rows:
        raise RuntimeError(f"No summary rows available for benchmark {benchmark}.")

    fig, ax = plt.subplots(figsize=(10.5, 6.2))
    thread_values = unique_threads(summary_rows, benchmark)
    lock_keys = sorted({row["lock"] for row in rows}, key=lock_sort_key)
    colors = plt.rcParams["axes.prop_cycle"].by_key().get("color", ["C0"]) or ["C0"]
    color_keys = list(dict.fromkeys(lock_plot_color_key(lock) for lock in lock_keys))
    color_by_key = {
        key: colors[index % len(colors)]
        for index, key in enumerate(color_keys)
    }

    for lock in lock_keys:
        points = [
            (int(row["threads"]), float(row["mean_ops_per_second"]))
            for row in rows
            if row["lock"] == lock
        ]
        if not points:
            continue
        points.sort()
        style = lock_plot_style(lock, color_by_key)
        ax.plot(
            [thread for thread, _ in points],
            [value for _, value in points],
            marker=style["marker"],
            color=style["color"],
            linestyle=style["linestyle"],
            linewidth=1.8,
            markersize=4,
            markerfacecolor="white",
            markeredgewidth=1.4,
            label=lock_label(lock),
        )

    ax.set_title(
        f"Throughput vs Threads: {benchmark_label(benchmark)}",
        fontsize=PLOT_TITLE_FONTSIZE,
    )
    ax.set_xlabel("Threads", fontsize=PLOT_LABEL_FONTSIZE)
    ax.set_ylabel("Mean throughput (ops/s, higher is better)", fontsize=PLOT_LABEL_FONTSIZE)
    experiment_three.add_thread_axis_formatting(ax, thread_values)
    ax.xaxis.set_major_formatter(ScalarFormatter())
    ax.grid(True, axis="y", alpha=0.28)
    ax.grid(True, axis="x", which="major", alpha=0.16)
    ax.tick_params(axis="both", labelsize=PLOT_TICK_FONTSIZE)
    ax.legend(frameon=False, ncol=2, fontsize=PLOT_LEGEND_FONTSIZE)
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
        if args.skip_plots and args.plot_only is not None:
            print("--skip-plots cannot be used together with --plot-only.", file=sys.stderr)
            return 2

        benchmarks = validate_benchmark_names(parse_csv_strings(args.benchmarks))
        locks = resolve_leveldb_locks(
            profile=args.lock_profile,
            locks=None if args.locks is None else parse_csv_strings(args.locks),
        )
        threads = parse_csv_ints(args.threads)
        leveldb_dir = resolve_path(args.leveldb_dir)
        db_bench = resolve_path(args.db_bench) if args.db_bench is not None else default_db_bench_for_leveldb(leveldb_dir)
        fill_benchmark = validate_benchmark_names((args.fill_benchmark,))[0]
        init_existing_benchmarks = parse_init_existing_benchmarks(args.init_existing_benchmarks)
        selected_runs = load_failure_selection(args.rerun_failures) if args.rerun_failures is not None else None

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

        result_root = resolve_path(args.output_root) if args.output_root is not None else default_result_root()
        experiment_three.ensure_output_root(result_root, args.force)
        logger = experiment_three.CommandLogger(
            result_root,
            command_timeout_seconds=args.command_timeout_seconds,
            echo_output=False,
            output_filter=drop_leveldb_progress_line,
        )

        ensure_lock_helpers(
            locks,
            build_missing=args.build_missing,
            logger=logger,
        )
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
            lock_profile=args.lock_profile,
            lock_profile_source="manual" if args.locks is not None else "profile",
            command_timeout_seconds=args.command_timeout_seconds,
        )
        failures: list[dict[str, str]] = []
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
            failures=failures,
            selected_runs=selected_runs,
            keep_db=args.keep_db,
        )
        raw_path = write_raw_csv(result_root, raw_rows)
        summary_rows = summarize_rows(raw_rows)
        summary_path = write_summary_csv(result_root, summary_rows)
        plot_paths = [] if args.skip_plots else write_plots(result_root, summary_rows) if summary_rows else []
        print_outputs(result_root, raw_path, summary_path, plot_paths)
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
