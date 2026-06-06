#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from statistics import mean
from typing import Callable, Iterable, Sequence

import experiment_failures
import experiment_defaults


REPO_ROOT = Path(__file__).resolve().parents[1]
FLEXGUARD_DIR = REPO_ROOT / "bench" / "flexguard"
FLEXGUARD_BUILD_DIR = FLEXGUARD_DIR / "build"
MAKE_ALL_SCRIPT = FLEXGUARD_DIR / "scripts" / "make_all.sh"
OTHERLOCKS_DIR = REPO_ROOT / "bench" / "otherlocks"
OTHERLOCKS_BUILD_DIR = OTHERLOCKS_DIR / "build"
BUILD_DEDUP_SCRIPT = FLEXGUARD_DIR / "scripts" / "build_dedup.sh"
BUILD_STREAMCLUSTER_SCRIPT = FLEXGUARD_DIR / "scripts" / "build_streamcluster.sh"
DEDUP_BINARY = (
    FLEXGUARD_DIR
    / "ext"
    / "parsec-benchmark"
    / "pkgs"
    / "kernels"
    / "dedup"
    / "inst"
    / "amd64-linux.gcc"
    / "bin"
    / "dedup"
)
DEDUP_INPUT = (
    FLEXGUARD_DIR
    / "ext"
    / "parsec-benchmark"
    / "pkgs"
    / "kernels"
    / "dedup"
    / "run"
    / "FC-6-x86_64-disc1.iso"
)
STREAMCLUSTER_BINARY = (
    FLEXGUARD_DIR
    / "ext"
    / "parsec-benchmark"
    / "pkgs"
    / "kernels"
    / "streamcluster"
    / "inst"
    / "amd64-linux.gcc"
    / "bin"
    / "streamcluster"
)

DEFAULT_BENCHMARKS = ("dedup", "streamcluster")
DEFAULT_LOCK_PROFILE = experiment_defaults.DEFAULT_LOCK_PROFILE
DEFAULT_LOCKS = experiment_defaults.DEFAULT_LOCKS
FULL_LOCKS = experiment_defaults.FULL_LOCKS
MINIMAL_LOCKS = experiment_defaults.MINIMAL_LOCKS
DEFAULT_THREADS = experiment_defaults.DEFAULT_THREADS
DEFAULT_REPEATS = experiment_defaults.DEFAULT_REPEATS
DEFAULT_DEDUP_COMPRESSION = "gzip"
DEFAULT_STREAMCLUSTER_MIN_CENTERS = 10
DEFAULT_STREAMCLUSTER_MAX_CENTERS = 30
DEFAULT_STREAMCLUSTER_DIMENSIONS = 512
DEFAULT_STREAMCLUSTER_NUM_POINTS = 32768
DEFAULT_STREAMCLUSTER_CHUNKSIZE = 32768
DEFAULT_STREAMCLUSTER_CLUSTERSIZE = 2000
DEFAULT_STREAMCLUSTER_INPUT = "none"
DEFAULT_COMMAND_TIMEOUT_SECONDS = 14400
COMMAND_TIMEOUT_KILL_AFTER_SECONDS = 60
MACHINE_CORE_COUNT = experiment_defaults.MACHINE_CORE_COUNT
ACTIVE_MACHINE_CONFIG = experiment_defaults.ACTIVE_MACHINE_CONFIG
PROFILE_ENV = experiment_defaults.PROFILE_ENV
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
    "setup_cycles",
    "benchmark_cycles",
    "setup_time_ms",
    "run_time_ms",
    "wall_seconds",
    "command_log",
)
SUMMARY_FIELDS = (
    "benchmark",
    "lock",
    "threads",
    "mean_setup_time_ms",
    "mean_run_time_ms",
    "mean_wall_seconds",
    "runs",
)

SETUP_PATTERN = re.compile(r"Setup time:\s*(\d+)")
BENCHMARK_PATTERN = re.compile(r"Benchmark time:\s*(\d+)")
BASE_DIR_PATTERN = re.compile(r"^BASE_DIR=(?P<quote>[\"']?)(?P<value>.*?)(?P=quote)$", re.MULTILINE)

LOCK_LABELS = experiment_defaults.LOCK_LABELS
BENCHMARK_LABELS = {
    "dedup": "PARSEC dedup",
    "streamcluster": "PARSEC streamcluster",
}
ACCORDIN_PRELOAD_LIBRARY = REPO_ROOT / "target" / "release" / "libmcs_tas_accordin.so"
MCS_ACCORDIN_PRELOAD_LIBRARY = REPO_ROOT / "target" / "release" / "libmcs_accordin.so"
MCS_EXTENSION_PRELOAD_LIBRARY = REPO_ROOT / "target" / "release" / "libmcs_tse.so"
BPF_INTERPOSE_LOCK_PREFIXES = ("flexguard",)
ROOT_REQUIRED_PRELOAD_LOCKS = {
    lock
    for lock in experiment_defaults.ACCORDIN_VARIANT_LOCKS
} | {experiment_defaults.MCS_ACCORDIN_LOCK}
LOCK_ALIASES = experiment_defaults.LOCK_ALIASES
LOCK_ORDER = experiment_defaults.LOCK_ORDER
RESULT_LOG_PATTERN = re.compile(
    rf"^(?P<benchmark>.+)_(?P<lock>{'|'.join(re.escape(lock) for lock in sorted(LOCK_ORDER, key=len, reverse=True))})_"
    r"(?P<threads>\d+)_r(?P<repeat>\d+)\.log$"
)


@dataclass(frozen=True)
class CommandResult:
    log_path: Path
    output: str
    wall_seconds: float


@dataclass(frozen=True)
class ParsedBenchmarkOutput:
    setup_cycles: int
    benchmark_cycles: int


@dataclass(frozen=True)
class TscCalibration:
    tsc_khz: int | None
    source: str


class CommandError(RuntimeError):
    def __init__(self, message: str, returncode: int, log_path: Path) -> None:
        super().__init__(message)
        self.returncode = returncode
        self.log_path = log_path


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


class CommandLogger:
    def __init__(
        self,
        result_root: Path,
        *,
        resume: bool = False,
        command_timeout_seconds: int = DEFAULT_COMMAND_TIMEOUT_SECONDS,
        echo_output: bool = True,
        output_filter: Callable[[str], str | None] | None = None,
    ) -> None:
        self.result_root = result_root
        self.log_dir = result_root / "logs"
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.records = self.load_records() if resume else []
        self.command_timeout_seconds = command_timeout_seconds
        self.echo_output = echo_output
        self.output_filter = output_filter

    def load_records(self) -> list[dict[str, object]]:
        path = self.result_root / "commands.json"
        if not path.is_file():
            return []
        with path.open("r", encoding="utf-8") as f:
            records = json.load(f)
        if not isinstance(records, list):
            raise RuntimeError(f"commands.json is not a command-record list: {path}")
        return records

    def run(
        self,
        cmd: list[str],
        *,
        log_name: str,
        cwd: Path = REPO_ROOT,
        env: dict[str, str | None] | None = None,
        timeout_seconds: int | None = None,
    ) -> CommandResult:
        effective_timeout = self.command_timeout_seconds if timeout_seconds is None else timeout_seconds
        run_cmd = wrap_command_timeout(cmd, effective_timeout)
        log_path = self.log_dir / log_name
        started_at = dt.datetime.now(dt.timezone.utc)
        started_perf = time.perf_counter()
        record: dict[str, object] = {
            "command": run_cmd,
            "command_text": shlex.join(run_cmd),
            "cwd": str(cwd),
            "log_path": str(log_path),
            "started_at": started_at.isoformat(),
        }
        if effective_timeout > 0:
            record["inner_command"] = cmd
            record["command_timeout_seconds"] = effective_timeout
        run_env = os.environ.copy()
        if env:
            for key, value in env.items():
                if value is None:
                    run_env.pop(key, None)
                else:
                    run_env[key] = value

        output_chunks: list[str] = []
        with log_path.open("w", encoding="utf-8") as log_file:
            log_file.write(f"cwd: {cwd}\n")
            log_file.write(f"command: {shlex.join(run_cmd)}\n")
            if effective_timeout > 0:
                log_file.write(f"inner_command: {shlex.join(cmd)}\n")
                log_file.write(f"command_timeout_seconds: {effective_timeout}\n")
            log_file.write(f"started_at: {started_at.isoformat()}\n\n")
            log_file.flush()

            process = subprocess.Popen(
                run_cmd,
                cwd=str(cwd),
                env=run_env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,
            )
            assert process.stdout is not None
            for line in process.stdout:
                if self.output_filter is not None:
                    line = self.output_filter(line)
                    if line is None:
                        continue
                output_chunks.append(line)
                log_file.write(line)
                log_file.flush()
                if self.echo_output:
                    print(line, end="", flush=True)
            returncode = process.wait()
            wall_seconds = time.perf_counter() - started_perf

            finished_at = dt.datetime.now(dt.timezone.utc)
            log_file.write(f"\nfinished_at: {finished_at.isoformat()}\n")
            log_file.write(f"returncode: {returncode}\n")
            log_file.write(f"wall_seconds: {wall_seconds:.6f}\n")

        record["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
        record["returncode"] = returncode
        record["wall_seconds"] = wall_seconds
        self.records.append(record)
        self.write_manifest()

        if returncode != 0:
            raise CommandError(
                f"Command failed with exit code {returncode}: {shlex.join(run_cmd)}",
                returncode,
                log_path,
            )
        return CommandResult(
            log_path=log_path,
            output="".join(output_chunks),
            wall_seconds=wall_seconds,
        )

    def write_manifest(self) -> None:
        with (self.result_root / "commands.json").open("w", encoding="utf-8") as f:
            json.dump(self.records, f, indent=2)
            f.write("\n")


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


def parse_args(
    argv: Sequence[str] | None = None,
    *,
    default_benchmarks: tuple[str, ...] = DEFAULT_BENCHMARKS,
    fixed_benchmarks: tuple[str, ...] | None = None,
    result_prefix: str = "experiment3",
    description: str | None = None,
) -> argparse.Namespace:
    selected_benchmarks = fixed_benchmarks or default_benchmarks
    include_dedup_options = "dedup" in selected_benchmarks
    include_streamcluster_options = "streamcluster" in selected_benchmarks
    script_path = Path(sys.argv[0]).as_posix()
    benchmark_label_text = "benchmark" if len(selected_benchmarks) == 1 else "benchmarks"

    settings_lines = [
        "Default benchmark settings:",
        f"  {benchmark_label_text}={','.join(selected_benchmarks)}",
        f"  lock-profile={DEFAULT_LOCK_PROFILE}",
        f"  minimal locks={','.join(MINIMAL_LOCKS)}",
        f"  full locks={','.join(FULL_LOCKS)}",
        f"  machine-profile={ACTIVE_MACHINE_CONFIG.name} (override with {PROFILE_ENV})",
        f"  threads={','.join(str(thread) for thread in DEFAULT_THREADS)}",
        f"  repeats={DEFAULT_REPEATS}",
    ]
    if include_dedup_options:
        settings_lines.append(f"  dedup-compression={DEFAULT_DEDUP_COMPRESSION}")
    if include_streamcluster_options:
        settings_lines.extend(
            [
                f"  streamcluster min/max={DEFAULT_STREAMCLUSTER_MIN_CENTERS}/{DEFAULT_STREAMCLUSTER_MAX_CENTERS}",
                f"  streamcluster dimensions={DEFAULT_STREAMCLUSTER_DIMENSIONS}, num-points={DEFAULT_STREAMCLUSTER_NUM_POINTS}",
                f"  streamcluster chunksize={DEFAULT_STREAMCLUSTER_CHUNKSIZE}, clustersize={DEFAULT_STREAMCLUSTER_CLUSTERSIZE}",
            ]
        )
    settings_lines.append(
        "  per_lock_max_threads="
        f"{','.join(f'{lock}:{max_threads}' for lock, max_threads in PER_LOCK_MAX_THREADS.items())}"
    )

    examples = [
        "Examples:",
        f"  python3 {script_path}",
    ]
    if fixed_benchmarks == ("dedup",):
        examples.append(f"  python3 {script_path} --locks mutex --threads 1 --repeats 1")
    elif fixed_benchmarks == ("streamcluster",):
        examples.append(
            f"  python3 {script_path} --locks mutex --threads 1 --repeats 1 "
            "--streamcluster-dimensions 16 --streamcluster-num-points 64 "
            "--streamcluster-chunksize 64 --streamcluster-clustersize 16"
        )
    else:
        examples.append(
            f"  python3 {script_path} --benchmarks streamcluster --locks mutex --threads 1 "
            "--repeats 1 --streamcluster-dimensions 16 --streamcluster-num-points 64 "
            "--streamcluster-chunksize 64 --streamcluster-clustersize 16"
        )
    examples.extend(
        [
            f"  {PROFILE_ENV}=original python3 {script_path}",
            f"  python3 {script_path} --plot-only experiments/results/{result_prefix}_manual",
        ]
    )

    parser = argparse.ArgumentParser(
        description=description or "Run or plot the PARSEC dedup and streamcluster experiment sweep.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="\n".join(settings_lines + [""] + examples),
    )
    parser.set_defaults(
        benchmarks=",".join(selected_benchmarks),
        dedup_input=DEDUP_INPUT,
        dedup_compression=DEFAULT_DEDUP_COMPRESSION,
        streamcluster_min_centers=DEFAULT_STREAMCLUSTER_MIN_CENTERS,
        streamcluster_max_centers=DEFAULT_STREAMCLUSTER_MAX_CENTERS,
        streamcluster_dimensions=DEFAULT_STREAMCLUSTER_DIMENSIONS,
        streamcluster_num_points=DEFAULT_STREAMCLUSTER_NUM_POINTS,
        streamcluster_chunksize=DEFAULT_STREAMCLUSTER_CHUNKSIZE,
        streamcluster_clustersize=DEFAULT_STREAMCLUSTER_CLUSTERSIZE,
        streamcluster_input=DEFAULT_STREAMCLUSTER_INPUT,
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        default=None,
        help=f"Directory for a new run. Default: experiments/results/{result_prefix}_<timestamp>.",
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
        "--resume",
        action="store_true",
        help=(
            "Continue an existing --output-root by parsing successful logs in commands.json, "
            "skipping completed points, and appending new command records."
        ),
    )
    parser.add_argument(
        "--build-missing",
        action="store_true",
        help="Build missing interpose helpers or PARSEC binaries before running.",
    )
    parser.add_argument(
        "--command-timeout-seconds",
        type=non_negative_int,
        default=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        help=(
            "Outer timeout for each logged command. 0 disables it. "
            f"Default: {DEFAULT_COMMAND_TIMEOUT_SECONDS}."
        ),
    )
    if fixed_benchmarks is None:
        parser.add_argument(
            "--benchmarks",
            default=",".join(default_benchmarks),
            metavar="CSV",
            help=(
                "Comma-separated benchmark keys. "
                f"Default: {','.join(default_benchmarks)}."
            ),
        )
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
            "Comma-separated lock keys. Overrides --lock-profile. "
            "Use mutex to run without interpose. "
            "Aliases: mcs-tas == mcstas, mcs_tse/mcs-tse == mcs_extension, "
            "accordin == mcs_tas_accordin_admission_only."
        ),
    )
    parser.add_argument(
        "--threads",
        default=",".join(str(thread) for thread in DEFAULT_THREADS),
        metavar="CSV",
        help=(
            "Comma-separated thread counts. "
            f"Default: {','.join(str(thread) for thread in DEFAULT_THREADS)}."
        ),
    )
    parser.add_argument(
        "--repeats",
        type=positive_int,
        default=DEFAULT_REPEATS,
        help=f"Number of repeats per benchmark/lock/thread point. Default: {DEFAULT_REPEATS}.",
    )
    parser.add_argument(
        "--tsc-khz",
        type=positive_int,
        default=None,
        help="Override TSC frequency in kHz. Default: auto-detect.",
    )
    if include_dedup_options:
        parser.add_argument(
            "--dedup-input",
            type=Path,
            default=DEDUP_INPUT,
            help=f"Dedup input file. Default: {DEDUP_INPUT}.",
        )
        parser.add_argument(
            "--dedup-compression",
            default=DEFAULT_DEDUP_COMPRESSION,
            help=f"Dedup compression type for -w. Default: {DEFAULT_DEDUP_COMPRESSION}.",
        )
    if include_streamcluster_options:
        parser.add_argument(
            "--streamcluster-min-centers",
            type=positive_int,
            default=DEFAULT_STREAMCLUSTER_MIN_CENTERS,
            help=f"Streamcluster minimum centers. Default: {DEFAULT_STREAMCLUSTER_MIN_CENTERS}.",
        )
        parser.add_argument(
            "--streamcluster-max-centers",
            type=positive_int,
            default=DEFAULT_STREAMCLUSTER_MAX_CENTERS,
            help=f"Streamcluster maximum centers. Default: {DEFAULT_STREAMCLUSTER_MAX_CENTERS}.",
        )
        parser.add_argument(
            "--streamcluster-dimensions",
            type=positive_int,
            default=DEFAULT_STREAMCLUSTER_DIMENSIONS,
            help=f"Streamcluster dimensions. Default: {DEFAULT_STREAMCLUSTER_DIMENSIONS}.",
        )
        parser.add_argument(
            "--streamcluster-num-points",
            type=positive_int,
            default=DEFAULT_STREAMCLUSTER_NUM_POINTS,
            help=f"Streamcluster number of points. Default: {DEFAULT_STREAMCLUSTER_NUM_POINTS}.",
        )
        parser.add_argument(
            "--streamcluster-chunksize",
            type=positive_int,
            default=DEFAULT_STREAMCLUSTER_CHUNKSIZE,
            help=f"Streamcluster chunksize. Default: {DEFAULT_STREAMCLUSTER_CHUNKSIZE}.",
        )
        parser.add_argument(
            "--streamcluster-clustersize",
            type=positive_int,
            default=DEFAULT_STREAMCLUSTER_CLUSTERSIZE,
            help=f"Streamcluster clustersize. Default: {DEFAULT_STREAMCLUSTER_CLUSTERSIZE}.",
        )
        parser.add_argument(
            "--streamcluster-input",
            default=DEFAULT_STREAMCLUSTER_INPUT,
            help=f"Streamcluster input argument. Default: {DEFAULT_STREAMCLUSTER_INPUT}.",
        )
    return parser.parse_args(argv)


def parse_csv_strings(value: str) -> tuple[str, ...]:
    items = tuple(item.strip() for item in value.split(",") if item.strip())
    if not items:
        raise ValueError("CSV value must contain at least one item")
    return items


def normalize_lock(lock: str) -> str:
    return experiment_defaults.normalize_lock(lock)


def validate_locks(locks: tuple[str, ...]) -> tuple[str, ...]:
    return experiment_defaults.validate_locks(locks)


def combine_ld_preload(preload_library: Path) -> str:
    existing = os.environ.get("LD_PRELOAD", "").strip()
    return f"{preload_library}:{existing}" if existing else str(preload_library)


def accordin_preload_env(
    preload_library: Path,
    *,
    lock: str = experiment_defaults.ACCORDIN_BASE_LOCK,
) -> dict[str, str | None]:
    env: dict[str, str | None] = {
        "LD_PRELOAD": combine_ld_preload(preload_library),
        "ACCORDIN_DISABLE_ADMISSION": None,
        "MCS_TAS_ACCORDIN_DISABLE_BPF": None,
        "MCS_TAS_ACCORDIN_STATS_ONLY": None,
    }
    if experiment_defaults.accordin_disables_admission(lock):
        env["ACCORDIN_DISABLE_ADMISSION"] = "1"
        env["MCS_TAS_ACCORDIN_STATS_ONLY"] = "1"
    return env


def taskset_wrapper_env(env: dict[str, str | None]) -> dict[str, str | None]:
    return {key: None for key in env}


def accordin_command_prefix_from_env(
    lock: str,
    env: dict[str, str | None],
) -> tuple[list[str], dict[str, str | None]]:
    if experiment_defaults.accordin_uses_taskset(lock):
        return (
            [
                "taskset",
                "-c",
                experiment_defaults.DEFAULT_MCS_ACCORDIN_TASKSET_CPUS,
                "env",
                *env_command_tokens(env),
            ],
            taskset_wrapper_env(env),
        )
    return [], env


def accordin_command_prefix(lock: str) -> tuple[list[str], dict[str, str | None]]:
    return accordin_command_prefix_from_env(
        lock,
        accordin_preload_env(ACCORDIN_PRELOAD_LIBRARY, lock=lock),
    )


def mcs_accordin_preload_env(
    preload_library: Path = MCS_ACCORDIN_PRELOAD_LIBRARY,
) -> dict[str, str | None]:
    return {
        "LD_PRELOAD": combine_ld_preload(preload_library),
        "ACCORDIN_DISABLE_ADMISSION": None,
        "MCS_ACCORDIN_DISABLE_BPF": None,
        "MCS_ACCORDIN_STATS_ONLY": None,
        "MCS_TAS_ACCORDIN_DISABLE_BPF": None,
        "MCS_TAS_ACCORDIN_STATS_ONLY": None,
    }


def mcs_accordin_command_prefix() -> tuple[list[str], dict[str, str | None]]:
    return [], mcs_accordin_preload_env()


def parse_csv_ints(value: str) -> tuple[int, ...]:
    items = tuple(int(item.strip()) for item in value.split(",") if item.strip())
    if not items:
        raise ValueError("CSV value must contain at least one integer")
    if any(item <= 0 for item in items):
        raise ValueError("Thread counts must be positive")
    return items


def validate_benchmarks(benchmarks: tuple[str, ...]) -> tuple[str, ...]:
    unsupported = [benchmark for benchmark in benchmarks if benchmark not in BENCHMARK_LABELS]
    if unsupported:
        raise ValueError(f"Unsupported benchmark keys: {', '.join(unsupported)}")
    return benchmarks


def resolve_path(path: Path) -> Path:
    return path.expanduser().resolve()


def resolve_optional_input(value: str) -> str:
    if value == "none":
        return value
    return str(resolve_path(Path(value)))


def default_result_root(result_prefix: str = "experiment3") -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"{result_prefix}_{timestamp}"


def ensure_output_root(path: Path, force: bool, resume: bool = False) -> None:
    if path.exists() and not path.is_dir():
        raise RuntimeError(f"Output root exists but is not a directory: {path}")
    if path.exists() and any(path.iterdir()) and not force and not resume:
        raise RuntimeError(f"Output root already exists and is not empty: {path}. Use --force to write there.")
    path.mkdir(parents=True, exist_ok=True)


def lock_label(lock: str) -> str:
    return experiment_defaults.lock_label(lock)


def benchmark_label(benchmark: str) -> str:
    return BENCHMARK_LABELS.get(benchmark, benchmark)


def lock_sort_key(lock: str) -> tuple[int, str]:
    return experiment_defaults.lock_sort_key(lock)


def runnable_threads_for_lock(lock: str, threads: tuple[int, ...]) -> tuple[int, ...]:
    return experiment_defaults.runnable_threads_for_lock(lock, threads)


def per_lock_max_threads_for_settings(
    locks: tuple[str, ...],
    threads: tuple[int, ...],
) -> dict[str, int]:
    return experiment_defaults.per_lock_max_threads_for_settings(locks, threads)


def benchmark_binary_path(benchmark: str) -> Path:
    if benchmark == "dedup":
        return DEDUP_BINARY
    if benchmark == "streamcluster":
        return STREAMCLUSTER_BINARY
    raise ValueError(f"Unsupported benchmark: {benchmark}")


def benchmark_build_script(benchmark: str) -> Path:
    if benchmark == "dedup":
        return BUILD_DEDUP_SCRIPT
    if benchmark == "streamcluster":
        return BUILD_STREAMCLUSTER_SCRIPT
    raise ValueError(f"Unsupported benchmark: {benchmark}")


def interpose_script_path(lock: str) -> Path:
    if experiment_defaults.is_otherlocks_interpose_lock(lock):
        return OTHERLOCKS_BUILD_DIR / f"interpose_{lock}.sh"
    return FLEXGUARD_BUILD_DIR / f"interpose_{lock}.sh"


def interpose_library_path(lock: str) -> Path:
    if experiment_defaults.is_otherlocks_interpose_lock(lock):
        return OTHERLOCKS_BUILD_DIR / f"interpose_{lock}.so"
    return FLEXGUARD_BUILD_DIR / f"interpose_{lock}.so"


def interpose_expected_base_dir(lock: str) -> Path:
    if experiment_defaults.is_otherlocks_interpose_lock(lock):
        return OTHERLOCKS_DIR.resolve()
    return FLEXGUARD_DIR.resolve()


def is_native_mutex_lock(lock: str) -> bool:
    return lock in {"mutex", "stock"}


def interpose_needs_sudo(lock: str) -> bool:
    return lock.startswith(BPF_INTERPOSE_LOCK_PREFIXES)


def env_command_tokens(env: dict[str, str | None]) -> list[str]:
    tokens: list[str] = []
    for key, value in sorted(env.items()):
        if value is None:
            tokens.extend(["-u", key])
    for key, value in sorted(env.items()):
        if value is not None:
            tokens.append(f"{key}={value}")
    return tokens


def with_sudo_env(cmd: list[str], env: dict[str, str | None] | None) -> tuple[list[str], None]:
    sudo_cmd = ["sudo", "-n", "env"]
    if env is not None:
        sudo_cmd.extend(env_command_tokens(env))
    sudo_cmd.extend(cmd)
    return sudo_cmd, None


def interpose_command(lock: str, env: dict[str, str | None] | None = None) -> tuple[list[str], dict[str, str | None] | None]:
    cmd = [str(interpose_script_path(lock))]
    if interpose_needs_sudo(lock):
        return with_sudo_env(cmd, env)
    return cmd, env


def benchmark_command(lock: str, cmd: list[str], env: dict[str, str | None] | None) -> tuple[list[str], dict[str, str | None] | None]:
    if env is not None and lock in ROOT_REQUIRED_PRELOAD_LOCKS:
        return with_sudo_env(cmd, env)
    return cmd, env


def interpose_script_base_dir(script: Path) -> Path | None:
    try:
        content = script.read_text(encoding="utf-8")
    except OSError:
        return None
    match = BASE_DIR_PATTERN.search(content)
    if match is None:
        return None
    value = match.group("value").strip()
    if not value:
        return None
    return Path(value).expanduser().resolve()


def interpose_helper_error(lock: str) -> str | None:
    script = interpose_script_path(lock)
    library = interpose_library_path(lock)

    if not script.is_file():
        return f"{lock} wrapper is missing: {script}"
    if not os.access(script, os.X_OK):
        return f"{lock} wrapper is not executable: {script}"

    script_base_dir = interpose_script_base_dir(script)
    expected_base_dir = interpose_expected_base_dir(lock)
    if script_base_dir != expected_base_dir:
        if script_base_dir is None:
            return f"{lock} wrapper does not declare BASE_DIR: {script}"
        return f"{lock} wrapper BASE_DIR points to {script_base_dir}, expected {expected_base_dir}"

    if not library.is_file():
        return f"{lock} preload library is missing: {library}"
    return None


def invalid_interpose_helpers(locks: Iterable[str]) -> list[str]:
    invalid: list[str] = []
    for lock in locks:
        if (
            is_native_mutex_lock(lock)
            or lock == "mcs_extension"
            or experiment_defaults.is_accordin_lock(lock)
            or experiment_defaults.is_mcs_accordin_lock(lock)
        ):
            continue
        error = interpose_helper_error(lock)
        if error is not None:
            invalid.append(error)
    return invalid


def invalid_interpose_helper_locks(locks: Iterable[str]) -> tuple[str, ...]:
    invalid: list[str] = []
    for lock in locks:
        if (
            is_native_mutex_lock(lock)
            or lock == "mcs_extension"
            or experiment_defaults.is_accordin_lock(lock)
            or experiment_defaults.is_mcs_accordin_lock(lock)
        ):
            continue
        if interpose_helper_error(lock) is not None:
            invalid.append(lock)
    return tuple(invalid)


def ensure_accordin_preload(
    *,
    build_missing: bool,
    logger: CommandLogger,
) -> None:
    if ACCORDIN_PRELOAD_LIBRARY.is_file():
        return
    if not build_missing:
        raise RuntimeError(
            f"LD_PRELOAD helper is missing: {ACCORDIN_PRELOAD_LIBRARY}. "
            "Run cargo build -p mcs_tas_accordin --release or rerun with --build-missing."
        )

    logger.run(
        ["cargo", "build", "-p", "mcs_tas_accordin", "--release"],
        log_name="build_mcs_tas_accordin.log",
        cwd=REPO_ROOT,
        timeout_seconds=0,
    )
    if not ACCORDIN_PRELOAD_LIBRARY.is_file():
        raise RuntimeError(f"LD_PRELOAD helper was not built: {ACCORDIN_PRELOAD_LIBRARY}")


def ensure_mcs_accordin_preload(
    *,
    build_missing: bool,
    logger: CommandLogger,
) -> None:
    if MCS_ACCORDIN_PRELOAD_LIBRARY.is_file():
        return
    if not build_missing:
        raise RuntimeError(
            f"LD_PRELOAD helper is missing: {MCS_ACCORDIN_PRELOAD_LIBRARY}. "
            "Run cargo build -p mcs_accordin --release or rerun with --build-missing."
        )

    logger.run(
        ["cargo", "build", "-p", "mcs_accordin", "--release"],
        log_name="build_mcs_accordin.log",
        cwd=REPO_ROOT,
        timeout_seconds=0,
    )
    if not MCS_ACCORDIN_PRELOAD_LIBRARY.is_file():
        raise RuntimeError(f"LD_PRELOAD helper was not built: {MCS_ACCORDIN_PRELOAD_LIBRARY}")


def ensure_mcs_extension_preload(
    *,
    build_missing: bool,
    logger: CommandLogger,
) -> None:
    if MCS_EXTENSION_PRELOAD_LIBRARY.is_file():
        return
    if not build_missing:
        raise RuntimeError(
            f"LD_PRELOAD helper is missing: {MCS_EXTENSION_PRELOAD_LIBRARY}. "
            "Run cargo build -p mcs_tse --release or rerun with --build-missing."
        )

    logger.run(
        ["cargo", "build", "-p", "mcs_tse", "--release"],
        log_name="build_mcs_tse.log",
        cwd=REPO_ROOT,
        timeout_seconds=0,
    )
    if not MCS_EXTENSION_PRELOAD_LIBRARY.is_file():
        raise RuntimeError(f"LD_PRELOAD helper was not built: {MCS_EXTENSION_PRELOAD_LIBRARY}")


def ensure_interpose_helpers(
    locks: tuple[str, ...],
    *,
    build_missing: bool,
    logger: CommandLogger,
) -> None:
    invalid_locks = invalid_interpose_helper_locks(locks)
    invalid = invalid_interpose_helpers(locks)
    if not invalid:
        return

    if not build_missing:
        raise RuntimeError(
            "Interpose helpers are missing or stale: "
            f"{'; '.join(invalid)}. Rerun with --build-missing."
        )

    flexguard_locks = tuple(
        lock for lock in invalid_locks if not experiment_defaults.is_otherlocks_interpose_lock(lock)
    )
    otherlocks_locks = tuple(
        lock for lock in invalid_locks if experiment_defaults.is_otherlocks_interpose_lock(lock)
    )

    if flexguard_locks:
        if not MAKE_ALL_SCRIPT.is_file():
            raise RuntimeError(f"Build helper was not found: {MAKE_ALL_SCRIPT}")
        logger.run(
            ["bash", str(MAKE_ALL_SCRIPT)],
            log_name="build_flexguard_interpose_helpers.log",
            cwd=FLEXGUARD_DIR,
            timeout_seconds=0,
        )

    for lock in otherlocks_locks:
        logger.run(
            ["make", "-C", str(OTHERLOCKS_DIR), f"build/interpose_{lock}.sh"],
            log_name=f"build_otherlocks_{lock}.log",
            cwd=REPO_ROOT,
            timeout_seconds=0,
        )

    invalid_after_build = invalid_interpose_helpers(locks)
    if invalid_after_build:
        raise RuntimeError(
            "Interpose helpers are still missing or stale after build: "
            f"{'; '.join(invalid_after_build)}"
        )


def ensure_parsec_binaries(
    benchmarks: tuple[str, ...],
    *,
    build_missing: bool,
    logger: CommandLogger,
) -> None:
    for benchmark in benchmarks:
        binary = benchmark_binary_path(benchmark)
        if binary.is_file() and os.access(binary, os.X_OK):
            continue
        if not build_missing:
            raise RuntimeError(
                f"{benchmark_label(benchmark)} binary is missing: {binary}. "
                f"Run {benchmark_build_script(benchmark)} or rerun with --build-missing."
            )
        build_script = benchmark_build_script(benchmark)
        if not build_script.is_file():
            raise RuntimeError(f"Build script was not found: {build_script}")
        logger.run(
            ["bash", str(build_script)],
            log_name=f"build_{benchmark}.log",
            cwd=FLEXGUARD_DIR,
            timeout_seconds=0,
        )
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise RuntimeError(f"{benchmark_label(benchmark)} binary is still unavailable after build: {binary}")


def ensure_dedup_input(input_path: Path) -> None:
    if not input_path.is_file():
        raise RuntimeError(
            "Dedup input file was not found: "
            f"{input_path}. Provide --dedup-input with an existing file."
        )


def parse_positive_int_text(value: str) -> int | None:
    stripped = value.strip()
    if not stripped.isdigit():
        return None
    parsed = int(stripped)
    if parsed <= 0:
        return None
    return parsed


def detect_tsc_from_sysfs() -> TscCalibration | None:
    candidate_paths = [
        Path("/sys/devices/system/cpu/cpu0/tsc_freq_khz"),
        Path("/sys/devices/system/cpu/tsc_freq_khz"),
    ]
    candidate_paths.extend(sorted(Path("/sys/devices/system/cpu").glob("cpu[0-9]*/tsc_freq_khz")))

    seen: set[Path] = set()
    for path in candidate_paths:
        if path in seen:
            continue
        seen.add(path)
        if not path.is_file():
            continue
        try:
            value = parse_positive_int_text(path.read_text(encoding="utf-8"))
        except OSError:
            continue
        if value is not None:
            return TscCalibration(tsc_khz=value, source=f"sysfs:{path}")
    return None


def detect_tsc_from_bpftrace() -> TscCalibration | None:
    if shutil.which("bpftrace") is None:
        return None

    commands = (
        (
            "bpftrace_sudo",
            "sudo -n bpftrace -e 'BEGIN { printf(\"%u\", *kaddr(\"tsc_khz\")); exit(); }' | sed -n 2p",
        ),
        (
            "bpftrace",
            "bpftrace -e 'BEGIN { printf(\"%u\", *kaddr(\"tsc_khz\")); exit(); }' | sed -n 2p",
        ),
    )
    for source_name, command in commands:
        try:
            completed = subprocess.run(
                ["bash", "-lc", command],
                cwd=str(REPO_ROOT),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=8,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            continue
        value = parse_positive_int_text(completed.stdout or "")
        if value is not None:
            return TscCalibration(tsc_khz=value, source=source_name)
    return None


def detect_tsc_from_cpuinfo() -> TscCalibration | None:
    cpuinfo_path = Path("/proc/cpuinfo")
    if not cpuinfo_path.is_file():
        return None
    try:
        content = cpuinfo_path.read_text(encoding="utf-8")
    except OSError:
        return None

    mhz_values: list[float] = []
    for line in content.splitlines():
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        if key.strip() != "cpu MHz":
            continue
        try:
            mhz = float(value.strip())
        except ValueError:
            continue
        if mhz > 0.0:
            mhz_values.append(mhz)

    if not mhz_values:
        return None
    return TscCalibration(
        tsc_khz=int(round(mean(mhz_values) * 1000.0)),
        source="proc_cpuinfo_cpu_mhz_mean_estimate",
    )


def detect_tsc_calibration(tsc_khz_override: int | None) -> TscCalibration:
    if tsc_khz_override is not None:
        return TscCalibration(tsc_khz=tsc_khz_override, source="cli:--tsc-khz")

    for detector in (detect_tsc_from_sysfs, detect_tsc_from_bpftrace, detect_tsc_from_cpuinfo):
        calibration = detector()
        if calibration is not None:
            return calibration
    return TscCalibration(tsc_khz=None, source="wall_seconds_fallback")


def write_settings(
    result_root: Path,
    *,
    benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    dedup_input: Path,
    streamcluster_input: str,
    calibration: TscCalibration,
    args: argparse.Namespace,
) -> None:
    settings = {
        "benchmarks": [
            {"key": benchmark, "label": benchmark_label(benchmark)}
            for benchmark in benchmarks
        ],
        "locks": [{"key": lock, "label": lock_label(lock)} for lock in locks],
        "lock_profile": args.lock_profile,
        "lock_profile_source": "manual" if args.locks is not None else "profile",
        "threads": list(threads),
        "runnable_threads_by_lock": {lock: list(runnable_threads_for_lock(lock, threads)) for lock in locks},
        "single_oversubscribed_locks": list(SINGLE_OVERSUBSCRIBED_LOCKS),
        "per_lock_max_threads": per_lock_max_threads_for_settings(locks, threads),
        "machine_profile": ACTIVE_MACHINE_CONFIG.name,
        "machine_profile_env": PROFILE_ENV,
        "repeats": args.repeats,
        "build_missing": args.build_missing,
        "command_timeout_seconds": args.command_timeout_seconds,
        "source": calibration.source,
        "tsc_khz": calibration.tsc_khz,
        "flexguard_dir": str(FLEXGUARD_DIR),
        "machine_core_count": MACHINE_CORE_COUNT,
    }
    if "dedup" in benchmarks:
        settings["dedup"] = {
            "binary": str(DEDUP_BINARY),
            "input": str(dedup_input),
            "compression": args.dedup_compression,
        }
    if "streamcluster" in benchmarks:
        settings["streamcluster"] = {
            "binary": str(STREAMCLUSTER_BINARY),
            "min_centers": args.streamcluster_min_centers,
            "max_centers": args.streamcluster_max_centers,
            "dimensions": args.streamcluster_dimensions,
            "num_points": args.streamcluster_num_points,
            "chunksize": args.streamcluster_chunksize,
            "clustersize": args.streamcluster_clustersize,
            "input": streamcluster_input,
        }
    with (result_root / "settings.json").open("w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")


def parse_benchmark_output(output: str) -> ParsedBenchmarkOutput:
    setup_cycles: int | None = None
    benchmark_cycles: int | None = None

    for line in output.splitlines():
        if setup_cycles is None:
            setup_match = SETUP_PATTERN.search(line)
            if setup_match is not None:
                setup_cycles = int(setup_match.group(1))
                continue
        if benchmark_cycles is None:
            benchmark_match = BENCHMARK_PATTERN.search(line)
            if benchmark_match is not None:
                benchmark_cycles = int(benchmark_match.group(1))

    if setup_cycles is None:
        raise RuntimeError("Benchmark output did not contain a setup cycle count.")
    if benchmark_cycles is None:
        raise RuntimeError("Benchmark output did not contain a run cycle count.")
    return ParsedBenchmarkOutput(
        setup_cycles=setup_cycles,
        benchmark_cycles=benchmark_cycles,
    )


def format_float(value: float) -> str:
    return f"{value:.6f}"


def cycles_to_ms(cycles: int, calibration: TscCalibration) -> float | None:
    if calibration.tsc_khz is None:
        return None
    return cycles / calibration.tsc_khz


def build_dedup_command(
    *,
    lock: str,
    threads: int,
    input_path: Path,
    compression: str,
    output_path: Path,
) -> tuple[list[str], dict[str, str | None] | None]:
    cmd: list[str] = []
    if not is_native_mutex_lock(lock):
        if experiment_defaults.is_accordin_lock(lock):
            cmd, env = accordin_command_prefix(lock)
        elif lock == "mcs_extension":
            env = {"LD_PRELOAD": combine_ld_preload(MCS_EXTENSION_PRELOAD_LIBRARY)}
        else:
            cmd, env = interpose_command(lock)
    else:
        env = None
    cmd.extend(
        [
            str(DEDUP_BINARY),
            "-c",
            "-p",
            f"-w{compression}",
            f"-t{threads}",
            f"-i{input_path}",
            f"-o{output_path}",
        ]
    )
    return benchmark_command(lock, cmd, env)


def build_streamcluster_command(
    *,
    lock: str,
    threads: int,
    input_value: str,
    output_path: Path,
    args: argparse.Namespace,
) -> tuple[list[str], dict[str, str | None] | None]:
    cmd: list[str] = []
    if not is_native_mutex_lock(lock):
        if experiment_defaults.is_accordin_lock(lock):
            cmd, env = accordin_command_prefix(lock)
        elif lock == "mcs_extension":
            env = {"LD_PRELOAD": combine_ld_preload(MCS_EXTENSION_PRELOAD_LIBRARY)}
        else:
            cmd, env = interpose_command(lock)
    else:
        env = None
    cmd.extend(
        [
            str(STREAMCLUSTER_BINARY),
            str(args.streamcluster_min_centers),
            str(args.streamcluster_max_centers),
            str(args.streamcluster_dimensions),
            str(args.streamcluster_num_points),
            str(args.streamcluster_chunksize),
            str(args.streamcluster_clustersize),
            input_value,
            str(output_path),
            str(threads),
        ]
    )
    return benchmark_command(lock, cmd, env)


def parse_result_log_name(log_path: Path) -> tuple[str, str, int, int] | None:
    match = RESULT_LOG_PATTERN.match(log_path.name)
    if match is None:
        return None
    return (
        match.group("benchmark"),
        match.group("lock"),
        int(match.group("threads")),
        int(match.group("repeat")),
    )


def target_keys(
    *,
    benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    repeats: int,
) -> list[tuple[str, str, int, int]]:
    return [
        (benchmark, lock, thread, repeat)
        for benchmark in benchmarks
        for lock in locks
        for thread in runnable_threads_for_lock(lock, threads)
        for repeat in range(1, repeats + 1)
    ]


def row_from_completed_record(
    result_root: Path,
    record: dict[str, object],
    calibration: TscCalibration,
) -> tuple[tuple[str, str, int, int], dict[str, str]] | None:
    if record.get("returncode") != 0:
        return None
    log_path_text = record.get("log_path")
    if not isinstance(log_path_text, str):
        return None
    log_path = Path(log_path_text)
    identity = parse_result_log_name(log_path)
    if identity is None or not log_path.is_file():
        return None

    benchmark, lock, threads, repeat = identity
    output = log_path.read_text(encoding="utf-8", errors="replace")
    parsed = parse_benchmark_output(output)
    setup_time_ms = cycles_to_ms(parsed.setup_cycles, calibration)
    run_time_ms = cycles_to_ms(parsed.benchmark_cycles, calibration)

    wall_value = record.get("wall_seconds")
    wall_seconds = float(wall_value) if isinstance(wall_value, (int, float)) else parse_log_wall_seconds(output)
    if run_time_ms is None:
        run_time_ms = wall_seconds * 1000.0

    key = (benchmark, lock, threads, repeat)
    return key, {
        "benchmark": benchmark,
        "lock": lock,
        "threads": str(threads),
        "repeat": str(repeat),
        "setup_cycles": str(parsed.setup_cycles),
        "benchmark_cycles": str(parsed.benchmark_cycles),
        "setup_time_ms": "" if setup_time_ms is None else format_float(setup_time_ms),
        "run_time_ms": format_float(run_time_ms),
        "wall_seconds": format_float(wall_seconds),
        "command_log": str(log_path.relative_to(result_root)),
    }


def parse_log_wall_seconds(output: str) -> float:
    for line in reversed(output.splitlines()):
        if line.startswith("wall_seconds:"):
            return float(line.split(":", 1)[1].strip())
    raise RuntimeError("Completed log did not contain wall_seconds.")


def completed_rows_from_records(
    result_root: Path,
    records: list[dict[str, object]],
    calibration: TscCalibration,
    ordered_targets: list[tuple[str, str, int, int]],
) -> list[dict[str, str]]:
    target_set = set(ordered_targets)
    rows_by_key: dict[tuple[str, str, int, int], dict[str, str]] = {}
    for record in records:
        completed = row_from_completed_record(result_root, record, calibration)
        if completed is None:
            continue
        key, row = completed
        if key in target_set:
            rows_by_key[key] = row
    return [rows_by_key[key] for key in ordered_targets if key in rows_by_key]


def temp_output_path(
    result_root: Path,
    *,
    benchmark: str,
    lock: str,
    threads: int,
    repeat: int,
) -> Path:
    suffix = ".dat.ddp" if benchmark == "dedup" else ".txt"
    temp_dir = result_root / "tmp"
    temp_dir.mkdir(parents=True, exist_ok=True)
    name = f"{benchmark}_{lock}_{threads:03d}_r{repeat}_{uuid.uuid4().hex}{suffix}"
    return temp_dir / name


def remove_if_exists(path: Path) -> None:
    try:
        path.unlink()
    except FileNotFoundError:
        return


def run_benchmarks(
    result_root: Path,
    *,
    experiment_name: str,
    benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    dedup_input: Path,
    streamcluster_input: str,
    calibration: TscCalibration,
    args: argparse.Namespace,
    logger: CommandLogger,
    failures: list[dict[str, str]],
    existing_rows: list[dict[str, str]] | None = None,
) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = list(existing_rows or [])
    completed_keys = {
        (row["benchmark"], row["lock"], int(row["threads"]), int(row["repeat"]))
        for row in rows
    }

    for benchmark in benchmarks:
        for lock in locks:
            for thread in runnable_threads_for_lock(lock, threads):
                for repeat in range(1, args.repeats + 1):
                    key = (benchmark, lock, thread, repeat)
                    if key in completed_keys:
                        print(
                            f"Skipping completed {benchmark}/{lock}/{thread}/r{repeat}",
                            flush=True,
                        )
                        continue
                    output_path = temp_output_path(
                        result_root,
                        benchmark=benchmark,
                        lock=lock,
                        threads=thread,
                        repeat=repeat,
                    )
                    remove_if_exists(output_path)
                    try:
                        env: dict[str, str | None] | None
                        if benchmark == "dedup":
                            cmd, env = build_dedup_command(
                                lock=lock,
                                threads=thread,
                                input_path=dedup_input,
                                compression=args.dedup_compression,
                                output_path=output_path,
                            )
                        elif benchmark == "streamcluster":
                            cmd, env = build_streamcluster_command(
                                lock=lock,
                                threads=thread,
                                input_value=streamcluster_input,
                                output_path=output_path,
                                args=args,
                            )
                        else:
                            raise RuntimeError(f"Unsupported benchmark: {benchmark}")

                        log_name = f"{benchmark}_{lock}_{thread:03d}_r{repeat}.log"
                        try:
                            result = logger.run(cmd, log_name=log_name, env=env)
                            parsed = parse_benchmark_output(result.output)
                            setup_time_ms = cycles_to_ms(parsed.setup_cycles, calibration)
                            run_time_ms = cycles_to_ms(parsed.benchmark_cycles, calibration)
                            if run_time_ms is None:
                                run_time_ms = result.wall_seconds * 1000.0
                            rows.append(
                                {
                                    "benchmark": benchmark,
                                    "lock": lock,
                                    "threads": str(thread),
                                    "repeat": str(repeat),
                                    "setup_cycles": str(parsed.setup_cycles),
                                    "benchmark_cycles": str(parsed.benchmark_cycles),
                                    "setup_time_ms": "" if setup_time_ms is None else format_float(setup_time_ms),
                                    "run_time_ms": format_float(run_time_ms),
                                    "wall_seconds": format_float(result.wall_seconds),
                                    "command_log": str(result.log_path.relative_to(result_root)),
                                }
                            )
                        except CommandError as exc:
                            experiment_failures.append_command_failure(
                                failures,
                                result_root=result_root,
                                experiment=experiment_name,
                                benchmark=benchmark,
                                lock=lock,
                                threads=thread,
                                repeat=repeat,
                                exc=exc,
                            )
                            experiment_failures.write_failures_csv(result_root, failures)
                            continue
                    finally:
                        remove_if_exists(output_path)

    return rows


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


def mean_field(rows: list[dict[str, str]], field: str) -> str:
    values = [float(row[field]) for row in rows if row[field].strip()]
    if not values:
        return ""
    return format_float(mean(values))


def summarize_rows(rows: list[dict[str, str]]) -> list[dict[str, str]]:
    groups: dict[tuple[str, str, int], list[dict[str, str]]] = {}
    for row in rows:
        key = (row["benchmark"], row["lock"], int(row["threads"]))
        groups.setdefault(key, []).append(row)

    summary_rows: list[dict[str, str]] = []
    for benchmark, lock, threads in sorted(
        groups.keys(),
        key=lambda item: (
            DEFAULT_BENCHMARKS.index(item[0]) if item[0] in DEFAULT_BENCHMARKS else len(DEFAULT_BENCHMARKS),
            lock_sort_key(item[1]),
            item[2],
        ),
    ):
        group_rows = groups[(benchmark, lock, threads)]
        summary_rows.append(
            {
                "benchmark": benchmark,
                "lock": lock,
                "threads": str(threads),
                "mean_setup_time_ms": mean_field(group_rows, "setup_time_ms"),
                "mean_run_time_ms": mean_field(group_rows, "run_time_ms"),
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


def add_thread_axis_formatting(ax, threads: list[int]) -> None:
    if not threads:
        return

    if len(threads) > 1:
        x_min = threads[0] / 1.08
        x_max = max(threads[-1] * 1.25, MACHINE_CORE_COUNT * 1.15)
        ax.set_xscale("log", base=2)
        ax.set_xlim(x_min, x_max)
    else:
        ax.set_xlim(max(0.0, threads[0] - 0.5), threads[0] + 0.5)
        x_max = threads[0] + 0.5
    ax.set_xticks(threads)

    if len(threads) > 1 and x_max > MACHINE_CORE_COUNT:
        if threads[-1] > MACHINE_CORE_COUNT:
            ax.axvspan(MACHINE_CORE_COUNT, x_max, color="0.92", alpha=0.55, linewidth=0, zorder=0)
        ax.axvline(MACHINE_CORE_COUNT, color="0.22", linewidth=1.0, linestyle="--", alpha=0.75, zorder=1)


def plot_throughput(summary_rows: list[dict[str, str]], *, benchmark: str, output_path: Path) -> None:
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
        if row["benchmark"] == benchmark and row["mean_run_time_ms"].strip()
    ]
    if not rows:
        raise RuntimeError(f"No summary rows available for benchmark {benchmark}.")

    fig, ax = plt.subplots(figsize=(9.5, 5.5))
    thread_values = unique_threads(summary_rows, benchmark)
    lock_keys = sorted({row["lock"] for row in rows}, key=lock_sort_key)

    for lock in lock_keys:
        points = [
            (int(row["threads"]), 1000.0 / float(row["mean_run_time_ms"]))
            for row in rows
            if row["lock"] == lock and float(row["mean_run_time_ms"]) > 0.0
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
    ax.set_ylabel("Mean throughput (runs/s, higher is better)")
    add_thread_axis_formatting(ax, thread_values)
    ax.xaxis.set_major_formatter(ScalarFormatter())
    ax.grid(True, axis="y", alpha=0.28)
    ax.grid(True, axis="x", which="major", alpha=0.16)
    ax.legend(frameon=False)
    fig.tight_layout()
    fig.savefig(output_path, dpi=180)
    plt.close(fig)


def write_plots(result_root: Path, summary_rows: list[dict[str, str]]) -> list[Path]:
    plot_paths: list[Path] = []
    for benchmark in DEFAULT_BENCHMARKS:
        if not any(row["benchmark"] == benchmark for row in summary_rows):
            continue
        output_path = result_root / f"throughput_vs_threads_{benchmark}.png"
        plot_throughput(summary_rows, benchmark=benchmark, output_path=output_path)
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


def main(
    argv: Sequence[str] | None = None,
    *,
    default_benchmarks: tuple[str, ...] = DEFAULT_BENCHMARKS,
    fixed_benchmarks: tuple[str, ...] | None = None,
    result_prefix: str = "experiment3",
    description: str | None = None,
) -> int:
    args = parse_args(
        argv,
        default_benchmarks=default_benchmarks,
        fixed_benchmarks=fixed_benchmarks,
        result_prefix=result_prefix,
        description=description,
    )

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
        if args.streamcluster_min_centers > args.streamcluster_max_centers:
            print("--streamcluster-min-centers cannot be greater than --streamcluster-max-centers.", file=sys.stderr)
            return 2

        benchmarks = validate_benchmarks(fixed_benchmarks or parse_csv_strings(args.benchmarks))
        locks = experiment_defaults.resolve_locks(
            profile=args.lock_profile,
            locks=None if args.locks is None else parse_csv_strings(args.locks),
        )
        threads = parse_csv_ints(args.threads)
        dedup_input = resolve_path(args.dedup_input)
        streamcluster_input = resolve_optional_input(args.streamcluster_input)

        if "streamcluster" in benchmarks and streamcluster_input != "none" and not Path(streamcluster_input).is_file():
            print(f"Streamcluster input file does not exist: {streamcluster_input}", file=sys.stderr)
            return 2

        if args.plot_only is not None:
            result_root = resolve_path(args.plot_only)
            if not result_root.is_dir():
                print(f"Plot-only result root does not exist: {result_root}", file=sys.stderr)
                return 2
            raw_rows = load_raw_rows(result_root)
            if fixed_benchmarks is not None:
                fixed_benchmark_set = set(fixed_benchmarks)
                raw_rows = [row for row in raw_rows if row["benchmark"] in fixed_benchmark_set]
            raw_path = result_root / "raw.csv"
            summary_rows = summarize_rows(raw_rows)
            summary_path = write_summary_csv(result_root, summary_rows)
            plot_paths = write_plots(result_root, summary_rows)
            print_outputs(result_root, raw_path, summary_path, plot_paths)
            return 0

        result_root = resolve_path(args.output_root) if args.output_root is not None else default_result_root(result_prefix)
        ensure_output_root(result_root, args.force, args.resume)
        logger = CommandLogger(
            result_root,
            resume=args.resume,
            command_timeout_seconds=args.command_timeout_seconds,
        )
        ensure_interpose_helpers(locks, build_missing=args.build_missing, logger=logger)
        if any(experiment_defaults.is_accordin_lock(lock) for lock in locks):
            ensure_accordin_preload(build_missing=args.build_missing, logger=logger)
        if "mcs_extension" in locks:
            ensure_mcs_extension_preload(build_missing=args.build_missing, logger=logger)
        ensure_parsec_binaries(benchmarks, build_missing=args.build_missing, logger=logger)
        if "dedup" in benchmarks:
            ensure_dedup_input(dedup_input)

        calibration = detect_tsc_calibration(args.tsc_khz)
        write_settings(
            result_root,
            benchmarks=benchmarks,
            locks=locks,
            threads=threads,
            dedup_input=dedup_input,
            streamcluster_input=streamcluster_input,
            calibration=calibration,
            args=args,
        )
        ordered_targets = target_keys(
            benchmarks=benchmarks,
            locks=locks,
            threads=threads,
            repeats=args.repeats,
        )
        existing_rows = (
            completed_rows_from_records(result_root, logger.records, calibration, ordered_targets)
            if args.resume
            else []
        )
        failures: list[dict[str, str]] = []
        raw_rows = run_benchmarks(
            result_root,
            experiment_name=result_prefix,
            benchmarks=benchmarks,
            locks=locks,
            threads=threads,
            dedup_input=dedup_input,
            streamcluster_input=streamcluster_input,
            calibration=calibration,
            args=args,
            logger=logger,
            failures=failures,
            existing_rows=existing_rows,
        )
        raw_path = write_raw_csv(result_root, raw_rows)
        summary_rows = summarize_rows(raw_rows)
        summary_path = write_summary_csv(result_root, summary_rows)
        plot_paths = write_plots(result_root, summary_rows) if summary_rows else []
        print_outputs(result_root, raw_path, summary_path, plot_paths)
        failures_path = experiment_failures.write_failures_csv(result_root, failures)
        experiment_failures.print_failure_summary(failures, failures_path)
        return 1 if failures else 0
    except CommandError as exc:
        print(str(exc), file=sys.stderr)
        print(f"Command log: {exc.log_path}", file=sys.stderr)
        return exc.returncode
    except Exception as exc:
        print(str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
