#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import os
import platform
import re
import shlex
import sys
from dataclasses import dataclass
from pathlib import Path
from statistics import mean
from typing import Iterable

import experiment_defaults
import experiment_failures
import run_experiment_three_common as parsec_common


REPO_ROOT = parsec_common.REPO_ROOT
PARSEC_DIR = parsec_common.FLEXGUARD_DIR / "ext" / "parsec-benchmark"
PARSECMGMT = PARSEC_DIR / "bin" / "parsecmgmt"
FLEXGUARD_INPUT_SIZE = "flexguard"
PARSEC_FLEXGUARD_FALLBACK_INPUT_SIZE = "native"
DEFAULT_DEDUP_COMPRESSION = "gzip"
FLEXGUARD_DEDUP_INPUT_ARCHIVE = PARSEC_DIR / "pkgs" / "kernels" / "dedup" / "inputs" / "input_native.tar"
FLEXGUARD_DEDUP_RUN_DIR = PARSEC_DIR / "pkgs" / "kernels" / "dedup" / "run"
FLEXGUARD_DEDUP_INPUT = PARSEC_DIR / "pkgs" / "kernels" / "dedup" / "run" / "FC-6-x86_64-disc1.iso"
DEFAULT_STREAMCLUSTER_MIN_CENTERS = 10
DEFAULT_STREAMCLUSTER_MAX_CENTERS = 30
DEFAULT_STREAMCLUSTER_DIMENSIONS = 512
DEFAULT_STREAMCLUSTER_NUM_POINTS = 32768
DEFAULT_STREAMCLUSTER_CHUNKSIZE = 32768
DEFAULT_STREAMCLUSTER_CLUSTERSIZE = 2000
DEFAULT_STREAMCLUSTER_INPUT = "none"

DEFAULT_BENCHMARKS = (
    "blackscholes",
    "canneal",
    "dedup",
    "streamcluster",
    "swaptions",
)
DEFAULT_INPUT_SIZE = FLEXGUARD_INPUT_SIZE
DEFAULT_BUILD_CONFIG = "gcc"
DEFAULT_LOCK_PROFILE = experiment_defaults.DEFAULT_LOCK_PROFILE
DEFAULT_LOCKS = experiment_defaults.DEFAULT_LOCKS
FULL_LOCKS = experiment_defaults.FULL_LOCKS
MINIMAL_LOCKS = experiment_defaults.MINIMAL_LOCKS
DEFAULT_THREADS = experiment_defaults.DEFAULT_THREADS
DEFAULT_REPEATS = experiment_defaults.DEFAULT_REPEATS
DEFAULT_COMMAND_TIMEOUT_SECONDS = 900
SINGLE_OVERSUBSCRIBED_LOCKS = experiment_defaults.SINGLE_OVERSUBSCRIBED_LOCKS
PER_LOCK_MAX_THREADS = experiment_defaults.per_lock_max_threads_for_settings(
    SINGLE_OVERSUBSCRIBED_LOCKS,
    DEFAULT_THREADS,
)

RAW_FIELDS = (
    "benchmark",
    "package",
    "input_size",
    "build_config",
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
    "package",
    "input_size",
    "build_config",
    "lock",
    "threads",
    "mean_setup_time_ms",
    "mean_run_time_ms",
    "mean_wall_seconds",
    "runs",
)

SETUP_PATTERN = re.compile(r"Setup time:\s*(\d+)")
BENCHMARK_PATTERN = re.compile(r"Benchmark time:\s*(\d+)")
RESULT_LOG_PATTERN = re.compile(
    rf"^(?P<benchmark>[A-Za-z0-9_.-]+)_(?P<lock>{'|'.join(re.escape(lock) for lock in sorted(experiment_defaults.LOCK_ORDER, key=len, reverse=True))})_"
    r"(?P<threads>\d+)_r(?P<repeat>\d+)\.log$"
)


@dataclass(frozen=True)
class ParsecBenchmark:
    key: str
    package: str
    group: str
    binary: str
    label: str

    @property
    def package_path(self) -> Path:
        return PARSEC_DIR / "pkgs" / self.group / self.package

    def binary_path(self, build_config: str) -> Path:
        return self.package_path / "inst" / parsec_platform(build_config) / "bin" / self.binary


BENCHMARKS = {
    spec.key: spec
    for spec in (
        ParsecBenchmark("blackscholes", "blackscholes", "apps", "blackscholes", "PARSEC blackscholes"),
        ParsecBenchmark("canneal", "canneal", "kernels", "canneal", "PARSEC canneal"),
        ParsecBenchmark("dedup", "dedup", "kernels", "dedup", "PARSEC dedup"),
        ParsecBenchmark("fluidanimate", "fluidanimate", "apps", "fluidanimate", "PARSEC fluidanimate"),
        ParsecBenchmark("streamcluster", "streamcluster", "kernels", "streamcluster", "PARSEC streamcluster"),
        ParsecBenchmark("swaptions", "swaptions", "apps", "swaptions", "PARSEC swaptions"),
    )
}


@dataclass(frozen=True)
class ParsedHookOutput:
    setup_cycles: int | None
    benchmark_cycles: int | None


@dataclass(frozen=True)
class RunCommand:
    cmd: list[str]
    env: dict[str, str | None] | None
    cwd: Path
    cleanup_paths: tuple[Path, ...] = ()


def positive_int(value: str) -> int:
    return parsec_common.positive_int(value)


def parse_csv_strings(value: str) -> tuple[str, ...]:
    return parsec_common.parse_csv_strings(value)


def parse_csv_ints(value: str) -> tuple[int, ...]:
    return parsec_common.parse_csv_ints(value)


def default_result_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment6_{timestamp}"


def normalize_machine(machine_name: str) -> str:
    normalized = machine_name.lower()
    if normalized in {"x86_64", "amd64"}:
        return "amd64"
    if normalized in {"i386", "i486", "i586", "i686"}:
        return "i386"
    return normalized


def normalize_system(system_name: str) -> str:
    normalized = system_name.lower()
    if normalized.startswith("linux"):
        return "linux"
    if normalized.startswith("darwin"):
        return "darwin"
    return normalized


def parsec_platform(build_config: str) -> str:
    base = os.environ.get("PARSECPLAT")
    if base:
        return f"{base}.{build_config}"
    return f"{normalize_machine(platform.machine())}-{normalize_system(platform.system())}.{build_config}"


def validate_benchmarks(benchmarks: tuple[str, ...]) -> tuple[str, ...]:
    normalized = tuple(dict.fromkeys(benchmark.strip().lower() for benchmark in benchmarks))
    unsupported = [benchmark for benchmark in normalized if benchmark not in BENCHMARKS]
    if unsupported:
        supported = ", ".join(sorted(BENCHMARKS))
        raise ValueError(f"Unsupported benchmark keys: {', '.join(unsupported)}. Supported: {supported}.")
    return normalized


def benchmark_label(benchmark: str) -> str:
    return BENCHMARKS[benchmark].label


def benchmark_sort_key(benchmark: str) -> tuple[int, str]:
    if benchmark in DEFAULT_BENCHMARKS:
        return (DEFAULT_BENCHMARKS.index(benchmark), benchmark)
    return (len(DEFAULT_BENCHMARKS), benchmark)


def lock_label(lock: str) -> str:
    return experiment_defaults.lock_label(lock)


def lock_sort_key(lock: str) -> tuple[int, str]:
    return experiment_defaults.lock_sort_key(lock)


def runnable_threads_for_lock(lock: str, threads: tuple[int, ...]) -> tuple[int, ...]:
    return experiment_defaults.runnable_threads_for_lock(lock, threads)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run or plot a broader PARSEC lock sweep through parsecmgmt.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"""\
Default benchmark settings:
  benchmarks={','.join(DEFAULT_BENCHMARKS)}
  input-size={DEFAULT_INPUT_SIZE} (FlexGuard suite config for dedup/streamcluster, native for other PARSEC packages)
  build-config={DEFAULT_BUILD_CONFIG}
  lock-profile={DEFAULT_LOCK_PROFILE}
  minimal locks={','.join(MINIMAL_LOCKS)}
  full locks={','.join(FULL_LOCKS)}
  machine-profile={experiment_defaults.ACTIVE_MACHINE_CONFIG.name} (override with {experiment_defaults.PROFILE_ENV})
  threads={','.join(str(thread) for thread in DEFAULT_THREADS)}
  repeats={DEFAULT_REPEATS}
  per_lock_max_threads={','.join(f"{lock}:{max_threads}" for lock, max_threads in PER_LOCK_MAX_THREADS.items())}

Examples:
  python3 experiments/run_experiment_six.py --build-missing
  python3 experiments/run_experiment_six.py --benchmarks blackscholes,canneal,swaptions --locks stock --threads 1,4 --repeats 1 --build-missing --skip-plots
  python3 experiments/run_experiment_six.py --input-size simlarge --build-missing
  python3 experiments/run_experiment_six.py --plot-only experiments/results/experiment6_manual
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
    parser.add_argument("--build-missing", action="store_true", help="Build missing PARSEC binaries or lock helpers where possible.")
    parser.add_argument("--skip-plots", action="store_true", help="Write CSVs but skip PNG generation.")
    parser.add_argument("--benchmarks", default=",".join(DEFAULT_BENCHMARKS), metavar="CSV")
    parser.add_argument(
        "--input-size",
        default=DEFAULT_INPUT_SIZE,
        choices=(FLEXGUARD_INPUT_SIZE, "test", "simdev", "simsmall", "simmedium", "simlarge", "native"),
        help=(
            "Input configuration. Default flexguard matches FlexGuard suite's direct dedup/streamcluster "
            "settings and uses native PARSEC input for packages without a FlexGuard suite config."
        ),
    )
    parser.add_argument(
        "--build-config",
        default=DEFAULT_BUILD_CONFIG,
        help=(
            "PARSEC build configuration passed to parsecmgmt -c. "
            "Use gcc-hooks when hook cycle output is needed and the packages are built for it."
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
            "Use stock to run without interpose. "
            "Aliases: mcs-tas == mcstas, mcs_tse/mcs-tse == mcs_extension, "
            "accordin == mcs_tas_accordin_admission_only, accordin_sampled, "
            "accordin_no_admission, accordin_taskset."
        ),
    )
    parser.add_argument("--threads", default=",".join(str(thread) for thread in DEFAULT_THREADS), metavar="CSV")
    parser.add_argument("--repeats", type=positive_int, default=DEFAULT_REPEATS)
    parser.add_argument(
        "--command-timeout-seconds",
        type=parsec_common.non_negative_int,
        default=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        help=(
            "Outer timeout for each logged benchmark command. 0 disables it. "
            f"Default: {DEFAULT_COMMAND_TIMEOUT_SECONDS}."
        ),
    )
    parser.add_argument("--tsc-khz", type=positive_int, default=None, help="Override TSC frequency in kHz. Default: auto-detect.")
    return parser.parse_args()


def ensure_parsecmgmt() -> None:
    if not PARSECMGMT.is_file() or not os.access(PARSECMGMT, os.X_OK):
        raise RuntimeError(f"parsecmgmt is missing or not executable: {PARSECMGMT}")


def ensure_parsec_binaries(
    benchmarks: tuple[str, ...],
    *,
    build_config: str,
    build_missing: bool,
    logger: parsec_common.CommandLogger,
) -> None:
    ensure_parsecmgmt()
    for benchmark in benchmarks:
        spec = BENCHMARKS[benchmark]
        binary = spec.binary_path(build_config)
        if binary.is_file() and os.access(binary, os.X_OK):
            continue
        if not build_missing:
            raise RuntimeError(
                f"{benchmark_label(benchmark)} binary is missing: {binary}. "
                "Rerun with --build-missing or build it with parsecmgmt."
            )
        logger.run(
            [
                str(PARSECMGMT),
                "-a",
                "build",
                "-p",
                spec.package,
                "-c",
                build_config,
            ],
            log_name=f"build_{benchmark}_{build_config}.log",
            cwd=PARSEC_DIR,
            timeout_seconds=0,
        )
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise RuntimeError(f"{benchmark_label(benchmark)} binary is still unavailable after build: {binary}")


def ensure_flexguard_inputs(
    benchmarks: tuple[str, ...],
    *,
    input_size: str,
    build_missing: bool,
    logger: parsec_common.CommandLogger,
) -> None:
    if input_size != FLEXGUARD_INPUT_SIZE or "dedup" not in benchmarks:
        return
    if FLEXGUARD_DEDUP_INPUT.is_file():
        return
    if not build_missing:
        raise RuntimeError(
            f"FlexGuard dedup input is missing: {FLEXGUARD_DEDUP_INPUT}. "
            "Rerun with --build-missing to fetch native PARSEC inputs."
        )
    if FLEXGUARD_DEDUP_INPUT_ARCHIVE.is_file():
        FLEXGUARD_DEDUP_RUN_DIR.mkdir(parents=True, exist_ok=True)
        logger.run(
            ["tar", "-xf", str(FLEXGUARD_DEDUP_INPUT_ARCHIVE), "-C", str(FLEXGUARD_DEDUP_RUN_DIR)],
            log_name="extract_dedup_native_input.log",
            cwd=PARSEC_DIR,
            timeout_seconds=0,
        )
        if FLEXGUARD_DEDUP_INPUT.is_file():
            return
    logger.run(
        ["./get-inputs", "-n"],
        log_name="fetch_parsec_native_inputs.log",
        cwd=PARSEC_DIR,
        timeout_seconds=0,
    )
    if not FLEXGUARD_DEDUP_INPUT.is_file():
        raise RuntimeError(f"FlexGuard dedup input is still missing after get-inputs: {FLEXGUARD_DEDUP_INPUT}")


def parse_hook_output(output: str) -> ParsedHookOutput:
    setup_cycles: int | None = None
    benchmark_cycles: int | None = None
    for line in output.splitlines():
        if setup_cycles is None:
            if match := SETUP_PATTERN.search(line):
                setup_cycles = int(match.group(1))
                continue
        if benchmark_cycles is None:
            if match := BENCHMARK_PATTERN.search(line):
                benchmark_cycles = int(match.group(1))
    return ParsedHookOutput(setup_cycles=setup_cycles, benchmark_cycles=benchmark_cycles)


def cycles_to_ms(cycles: int | None, calibration: parsec_common.TscCalibration) -> float | None:
    if cycles is None or calibration.tsc_khz is None:
        return None
    return cycles / calibration.tsc_khz


def format_float(value: float) -> str:
    return parsec_common.format_float(value)


def parse_log_wall_seconds(output: str) -> float | None:
    for line in reversed(output.splitlines()):
        if line.startswith("wall_seconds:"):
            return float(line.split(":", 1)[1].strip())
    return None


def submit_command_for_lock(lock: str) -> str:
    if lock == "stock":
        return "time"
    if experiment_defaults.is_accordin_lock(lock):
        prefix, env = parsec_common.accordin_command_prefix(lock)
        command = ["env", *parsec_common.env_command_tokens(env), *prefix]
        if lock in parsec_common.ROOT_REQUIRED_PRELOAD_LOCKS:
            command = ["sudo", "-n", *command]
        return shlex.join(command)
    if lock == "mcs_extension":
        preload = parsec_common.combine_ld_preload(parsec_common.MCS_EXTENSION_PRELOAD_LIBRARY)
        return shlex.join(["env", f"LD_PRELOAD={preload}"])

    wrapper = parsec_common.interpose_script_path(lock)
    if parsec_common.interpose_needs_sudo(lock):
        return shlex.join(["sudo", "-n", str(wrapper)])
    return shlex.quote(str(wrapper))


def command_with_lock(
    lock: str,
    payload: list[str],
) -> tuple[list[str], dict[str, str | None] | None]:
    cmd: list[str] = []
    if lock != "stock":
        if experiment_defaults.is_accordin_lock(lock):
            cmd, env = parsec_common.accordin_command_prefix(lock)
        elif lock == "mcs_extension":
            env = {"LD_PRELOAD": parsec_common.combine_ld_preload(parsec_common.MCS_EXTENSION_PRELOAD_LIBRARY)}
        else:
            cmd, env = parsec_common.interpose_command(lock)
    else:
        env = None
    cmd.extend(payload)
    return parsec_common.benchmark_command(lock, cmd, env)


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
    return temp_dir / f"{benchmark}_{lock}_{threads:03d}_r{repeat}_{os.getpid()}{suffix}"


def remove_if_exists(path: Path) -> None:
    try:
        path.unlink()
    except FileNotFoundError:
        return


def build_flexguard_dedup_command(
    *,
    lock: str,
    threads: int,
    output_path: Path,
    build_config: str,
) -> RunCommand:
    cmd, env = command_with_lock(
        lock,
        [
            str(BENCHMARKS["dedup"].binary_path(build_config)),
            "-c",
            "-p",
            f"-w{DEFAULT_DEDUP_COMPRESSION}",
            f"-t{threads}",
            f"-i{FLEXGUARD_DEDUP_INPUT}",
            f"-o{output_path}",
        ],
    )
    return RunCommand(cmd=cmd, env=env, cwd=REPO_ROOT, cleanup_paths=(output_path,))


def build_flexguard_streamcluster_command(
    *,
    lock: str,
    threads: int,
    output_path: Path,
    build_config: str,
) -> RunCommand:
    cmd, env = command_with_lock(
        lock,
        [
            str(BENCHMARKS["streamcluster"].binary_path(build_config)),
            str(DEFAULT_STREAMCLUSTER_MIN_CENTERS),
            str(DEFAULT_STREAMCLUSTER_MAX_CENTERS),
            str(DEFAULT_STREAMCLUSTER_DIMENSIONS),
            str(DEFAULT_STREAMCLUSTER_NUM_POINTS),
            str(DEFAULT_STREAMCLUSTER_CHUNKSIZE),
            str(DEFAULT_STREAMCLUSTER_CLUSTERSIZE),
            DEFAULT_STREAMCLUSTER_INPUT,
            str(output_path),
            str(threads),
        ],
    )
    return RunCommand(cmd=cmd, env=env, cwd=REPO_ROOT, cleanup_paths=(output_path,))


def parsec_input_size_for_benchmark(benchmark: str, input_size: str) -> str:
    if input_size != FLEXGUARD_INPUT_SIZE:
        return input_size
    if benchmark in {"dedup", "streamcluster"}:
        raise ValueError(f"{benchmark} uses a direct FlexGuard suite command in flexguard mode.")
    return PARSEC_FLEXGUARD_FALLBACK_INPUT_SIZE


def build_parsecmgmt_run_command(
    *,
    benchmark: str,
    lock: str,
    threads: int,
    input_size: str,
    build_config: str,
) -> RunCommand:
    spec = BENCHMARKS[benchmark]
    return RunCommand(
        cmd=[
            str(PARSECMGMT),
            "-a",
            "run",
            "-p",
            spec.package,
            "-c",
            build_config,
            "-i",
            input_size,
            "-n",
            str(threads),
            "-s",
            submit_command_for_lock(lock),
        ],
        env=None,
        cwd=PARSEC_DIR,
    )


def build_run_command(
    result_root: Path,
    *,
    benchmark: str,
    lock: str,
    threads: int,
    repeat: int,
    input_size: str,
    build_config: str,
) -> RunCommand:
    output_path = temp_output_path(
        result_root,
        benchmark=benchmark,
        lock=lock,
        threads=threads,
        repeat=repeat,
    )
    if input_size == FLEXGUARD_INPUT_SIZE and benchmark == "dedup":
        return build_flexguard_dedup_command(
            lock=lock,
            threads=threads,
            output_path=output_path,
            build_config=build_config,
        )
    if input_size == FLEXGUARD_INPUT_SIZE and benchmark == "streamcluster":
        return build_flexguard_streamcluster_command(
            lock=lock,
            threads=threads,
            output_path=output_path,
            build_config=build_config,
        )
    return build_parsecmgmt_run_command(
        benchmark=benchmark,
        lock=lock,
        threads=threads,
        input_size=parsec_input_size_for_benchmark(benchmark, input_size),
        build_config=build_config,
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


def row_from_output(
    *,
    result_root: Path,
    benchmark: str,
    lock: str,
    threads: int,
    repeat: int,
    input_size: str,
    build_config: str,
    output: str,
    wall_seconds: float,
    log_path: Path,
    calibration: parsec_common.TscCalibration,
) -> dict[str, str]:
    parsed = parse_hook_output(output)
    setup_time_ms = cycles_to_ms(parsed.setup_cycles, calibration)
    run_time_ms = cycles_to_ms(parsed.benchmark_cycles, calibration)
    if run_time_ms is None:
        run_time_ms = wall_seconds * 1000.0
    spec = BENCHMARKS[benchmark]
    return {
        "benchmark": benchmark,
        "package": spec.package,
        "input_size": input_size,
        "build_config": build_config,
        "lock": lock,
        "threads": str(threads),
        "repeat": str(repeat),
        "setup_cycles": "" if parsed.setup_cycles is None else str(parsed.setup_cycles),
        "benchmark_cycles": "" if parsed.benchmark_cycles is None else str(parsed.benchmark_cycles),
        "setup_time_ms": "" if setup_time_ms is None else format_float(setup_time_ms),
        "run_time_ms": format_float(run_time_ms),
        "wall_seconds": format_float(wall_seconds),
        "command_log": str(log_path.relative_to(result_root)),
    }


def row_from_completed_record(
    result_root: Path,
    record: dict[str, object],
    *,
    input_size: str,
    build_config: str,
    calibration: parsec_common.TscCalibration,
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
    if benchmark not in BENCHMARKS:
        return None
    output = log_path.read_text(encoding="utf-8", errors="replace")
    wall_value = record.get("wall_seconds")
    wall_seconds = float(wall_value) if isinstance(wall_value, (int, float)) else parse_log_wall_seconds(output)
    if wall_seconds is None:
        return None
    row = row_from_output(
        result_root=result_root,
        benchmark=benchmark,
        lock=lock,
        threads=threads,
        repeat=repeat,
        input_size=input_size,
        build_config=build_config,
        output=output,
        wall_seconds=wall_seconds,
        log_path=log_path,
        calibration=calibration,
    )
    return (benchmark, lock, threads, repeat), row


def completed_rows_from_records(
    result_root: Path,
    records: list[dict[str, object]],
    *,
    input_size: str,
    build_config: str,
    calibration: parsec_common.TscCalibration,
    ordered_targets: list[tuple[str, str, int, int]],
) -> list[dict[str, str]]:
    target_set = set(ordered_targets)
    rows_by_key: dict[tuple[str, str, int, int], dict[str, str]] = {}
    for record in records:
        completed = row_from_completed_record(
            result_root,
            record,
            input_size=input_size,
            build_config=build_config,
            calibration=calibration,
        )
        if completed is None:
            continue
        key, row = completed
        if key in target_set:
            rows_by_key[key] = row
    return [rows_by_key[key] for key in ordered_targets if key in rows_by_key]


def run_benchmarks(
    result_root: Path,
    *,
    benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    input_size: str,
    build_config: str,
    calibration: parsec_common.TscCalibration,
    args: argparse.Namespace,
    logger: parsec_common.CommandLogger,
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
                        print(f"Skipping completed {benchmark}/{lock}/{thread}/r{repeat}", flush=True)
                        continue
                    run_command = build_run_command(
                        result_root,
                        benchmark=benchmark,
                        lock=lock,
                        threads=thread,
                        repeat=repeat,
                        input_size=input_size,
                        build_config=build_config,
                    )
                    try:
                        log_name = f"{benchmark}_{lock}_{thread:03d}_r{repeat}.log"
                        try:
                            result = logger.run(
                                run_command.cmd,
                                log_name=log_name,
                                cwd=run_command.cwd,
                                env=run_command.env,
                            )
                            rows.append(
                                row_from_output(
                                    result_root=result_root,
                                    benchmark=benchmark,
                                    lock=lock,
                                    threads=thread,
                                    repeat=repeat,
                                    input_size=input_size,
                                    build_config=build_config,
                                    output=result.output,
                                    wall_seconds=result.wall_seconds,
                                    log_path=result.log_path,
                                    calibration=calibration,
                                )
                            )
                        except parsec_common.CommandError as exc:
                            experiment_failures.append_command_failure(
                                failures,
                                result_root=result_root,
                                experiment="experiment6",
                                workload="parsec",
                                benchmark=benchmark,
                                lock=lock,
                                threads=thread,
                                repeat=repeat,
                                exc=exc,
                            )
                            experiment_failures.write_failures_csv(result_root, failures)
                            continue
                    finally:
                        for cleanup_path in run_command.cleanup_paths:
                            remove_if_exists(cleanup_path)
    return rows


def write_settings(
    result_root: Path,
    *,
    benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    calibration: parsec_common.TscCalibration,
    args: argparse.Namespace,
) -> None:
    settings = {
        "benchmarks": [
            {
                "key": benchmark,
                "label": benchmark_label(benchmark),
                "package": BENCHMARKS[benchmark].package,
                "group": BENCHMARKS[benchmark].group,
            }
            for benchmark in benchmarks
        ],
        "locks": [{"key": lock, "label": lock_label(lock)} for lock in locks],
        "lock_profile": args.lock_profile,
        "lock_profile_source": "manual" if args.locks is not None else "profile",
        "threads": list(threads),
        "runnable_threads_by_lock": {lock: list(runnable_threads_for_lock(lock, threads)) for lock in locks},
        "single_oversubscribed_locks": list(SINGLE_OVERSUBSCRIBED_LOCKS),
        "per_lock_max_threads": experiment_defaults.per_lock_max_threads_for_settings(locks, threads),
        "machine_profile": experiment_defaults.ACTIVE_MACHINE_CONFIG.name,
        "machine_profile_env": experiment_defaults.PROFILE_ENV,
        "machine_core_count": experiment_defaults.MACHINE_CORE_COUNT,
        "input_size": args.input_size,
        "flexguard_input_size": args.input_size == FLEXGUARD_INPUT_SIZE,
        "parsec_flexguard_fallback_input_size": PARSEC_FLEXGUARD_FALLBACK_INPUT_SIZE,
        "flexguard_dedup": {
            "input": str(FLEXGUARD_DEDUP_INPUT),
            "compression": DEFAULT_DEDUP_COMPRESSION,
        },
        "flexguard_streamcluster": {
            "min_centers": DEFAULT_STREAMCLUSTER_MIN_CENTERS,
            "max_centers": DEFAULT_STREAMCLUSTER_MAX_CENTERS,
            "dimensions": DEFAULT_STREAMCLUSTER_DIMENSIONS,
            "num_points": DEFAULT_STREAMCLUSTER_NUM_POINTS,
            "chunksize": DEFAULT_STREAMCLUSTER_CHUNKSIZE,
            "clustersize": DEFAULT_STREAMCLUSTER_CLUSTERSIZE,
            "input": DEFAULT_STREAMCLUSTER_INPUT,
        },
        "build_config": args.build_config,
        "parsec_platform": parsec_platform(args.build_config),
        "parsec_dir": str(PARSEC_DIR),
        "repeats": args.repeats,
        "command_timeout_seconds": args.command_timeout_seconds,
        "build_missing": args.build_missing,
        "source": calibration.source,
        "tsc_khz": calibration.tsc_khz,
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
    groups: dict[tuple[str, str, str, str, int], list[dict[str, str]]] = {}
    for row in rows:
        key = (
            row["benchmark"],
            row["input_size"],
            row["build_config"],
            row["lock"],
            int(row["threads"]),
        )
        groups.setdefault(key, []).append(row)

    summary_rows: list[dict[str, str]] = []
    for benchmark, input_size, build_config, lock, threads in sorted(
        groups,
        key=lambda item: (benchmark_sort_key(item[0]), item[1], item[2], lock_sort_key(item[3]), item[4]),
    ):
        group_rows = groups[(benchmark, input_size, build_config, lock, threads)]
        summary_rows.append(
            {
                "benchmark": benchmark,
                "package": BENCHMARKS[benchmark].package,
                "input_size": input_size,
                "build_config": build_config,
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


def plot_throughput(summary_rows: list[dict[str, str]], *, benchmark: str, output_path: Path) -> None:
    try:
        import matplotlib
    except ModuleNotFoundError as exc:
        raise RuntimeError("matplotlib is required to generate plots. Use --skip-plots to skip PNG generation.") from exc

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.ticker import ScalarFormatter

    rows = [
        row
        for row in summary_rows
        if row["benchmark"] == benchmark and row["mean_run_time_ms"].strip()
    ]
    if not rows:
        return

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
    parsec_common.add_thread_axis_formatting(ax, thread_values)
    ax.xaxis.set_major_formatter(ScalarFormatter())
    ax.grid(True, axis="y", alpha=0.28)
    ax.grid(True, axis="x", which="major", alpha=0.16)
    ax.legend(frameon=False)
    fig.tight_layout()
    fig.savefig(output_path, dpi=180)
    plt.close(fig)


def write_plots(result_root: Path, summary_rows: list[dict[str, str]]) -> list[Path]:
    plot_paths: list[Path] = []
    for benchmark in sorted({row["benchmark"] for row in summary_rows}, key=benchmark_sort_key):
        output_path = result_root / f"throughput_vs_threads_{benchmark}.png"
        plot_throughput(summary_rows, benchmark=benchmark, output_path=output_path)
        if output_path.is_file():
            plot_paths.append(output_path)
    return plot_paths


def maybe_write_plots(result_root: Path, summary_rows: list[dict[str, str]], *, skip_plots: bool) -> list[Path]:
    if skip_plots:
        return []
    try:
        return write_plots(result_root, summary_rows)
    except RuntimeError as exc:
        if "matplotlib is required" not in str(exc):
            raise
        print(f"Warning: {exc}", file=sys.stderr)
        return []


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

        benchmarks = validate_benchmarks(parse_csv_strings(args.benchmarks))
        locks = experiment_defaults.resolve_locks(
            profile=args.lock_profile,
            locks=None if args.locks is None else parse_csv_strings(args.locks),
        )
        threads = parse_csv_ints(args.threads)

        if args.plot_only is not None:
            result_root = parsec_common.resolve_path(args.plot_only)
            if not result_root.is_dir():
                print(f"Plot-only result root does not exist: {result_root}", file=sys.stderr)
                return 2
            raw_rows = load_raw_rows(result_root)
            raw_path = result_root / "raw.csv"
            summary_rows = summarize_rows(raw_rows)
            summary_path = write_summary_csv(result_root, summary_rows)
            plot_paths = maybe_write_plots(result_root, summary_rows, skip_plots=args.skip_plots)
            print_outputs(result_root, raw_path, summary_path, plot_paths)
            return 0

        result_root = parsec_common.resolve_path(args.output_root) if args.output_root is not None else default_result_root()
        parsec_common.ensure_output_root(result_root, args.force, args.resume)
        logger = parsec_common.CommandLogger(
            result_root,
            resume=args.resume,
            command_timeout_seconds=args.command_timeout_seconds,
        )
        parsec_common.ensure_interpose_helpers(locks, build_missing=args.build_missing, logger=logger)
        if any(experiment_defaults.is_accordin_lock(lock) for lock in locks):
            parsec_common.ensure_accordin_preload(build_missing=args.build_missing, logger=logger)
        if "mcs_extension" in locks:
            parsec_common.ensure_mcs_extension_preload(build_missing=args.build_missing, logger=logger)
        ensure_parsec_binaries(
            benchmarks,
            build_config=args.build_config,
            build_missing=args.build_missing,
            logger=logger,
        )
        ensure_flexguard_inputs(
            benchmarks,
            input_size=args.input_size,
            build_missing=args.build_missing,
            logger=logger,
        )

        calibration = parsec_common.detect_tsc_calibration(args.tsc_khz)
        write_settings(
            result_root,
            benchmarks=benchmarks,
            locks=locks,
            threads=threads,
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
            completed_rows_from_records(
                result_root,
                logger.records,
                input_size=args.input_size,
                build_config=args.build_config,
                calibration=calibration,
                ordered_targets=ordered_targets,
            )
            if args.resume
            else []
        )
        failures: list[dict[str, str]] = []
        raw_rows = run_benchmarks(
            result_root,
            benchmarks=benchmarks,
            locks=locks,
            threads=threads,
            input_size=args.input_size,
            build_config=args.build_config,
            calibration=calibration,
            args=args,
            logger=logger,
            failures=failures,
            existing_rows=existing_rows,
        )
        raw_path = write_raw_csv(result_root, raw_rows)
        summary_rows = summarize_rows(raw_rows)
        summary_path = write_summary_csv(result_root, summary_rows)
        plot_paths = maybe_write_plots(result_root, summary_rows, skip_plots=args.skip_plots)
        print_outputs(result_root, raw_path, summary_path, plot_paths)
        failures_path = experiment_failures.write_failures_csv(result_root, failures)
        experiment_failures.print_failure_summary(failures, failures_path)
        return 1 if failures else 0
    except parsec_common.CommandError as exc:
        print(str(exc), file=sys.stderr)
        print(f"Command log: {exc.log_path}", file=sys.stderr)
        return exc.returncode
    except (OSError, RuntimeError, ValueError) as exc:
        print(str(exc), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
