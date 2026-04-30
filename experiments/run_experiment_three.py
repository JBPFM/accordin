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
from typing import Iterable


REPO_ROOT = Path(__file__).resolve().parents[1]
FLEXGUARD_DIR = REPO_ROOT / "bench" / "flexguard"
FLEXGUARD_BUILD_DIR = FLEXGUARD_DIR / "build"
MAKE_ALL_SCRIPT = FLEXGUARD_DIR / "scripts" / "make_all.sh"
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
DEFAULT_LOCKS = (
    "mcs",
    "mcstp",
    "mcs-tas",
    "mcs_extension",
    "flexguard",
    "accordin",
)
DEFAULT_THREADS = (8, 16, 32, 64)
DEFAULT_REPEATS = 3
DEFAULT_DEDUP_COMPRESSION = "gzip"
DEFAULT_STREAMCLUSTER_MIN_CENTERS = 10
DEFAULT_STREAMCLUSTER_MAX_CENTERS = 30
DEFAULT_STREAMCLUSTER_DIMENSIONS = 512
DEFAULT_STREAMCLUSTER_NUM_POINTS = 32768
DEFAULT_STREAMCLUSTER_CHUNKSIZE = 32768
DEFAULT_STREAMCLUSTER_CLUSTERSIZE = 2000
DEFAULT_STREAMCLUSTER_INPUT = "none"
MACHINE_CORE_COUNT = 96

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

LOCK_LABELS = {
    "stock": "Stock",
    "mcs": "MCS",
    "mcstp": "MCS-TP",
    "mcstas": "MCS-TAS",
    "mcs_extension": "MCS + TSE",
    "flexguard": "FlexGuard",
    "malthusian": "Malthusian",
    "reciprocating": "Reciprocating",
    "accordin": "Accordin",
    "mcs_accordin": "MCS-TAS Simple",
}
BENCHMARK_LABELS = {
    "dedup": "PARSEC dedup",
    "streamcluster": "PARSEC streamcluster",
}
ACCORDIN_PRELOAD_LIBRARY = REPO_ROOT / "target" / "release" / "libmcs_accordin.so"
MCS_EXTENSION_PRELOAD_LIBRARY = REPO_ROOT / "target" / "release" / "libmcs_tse.so"
MCS_ACCORDIN_PRELOAD_LIBRARY = REPO_ROOT / "target" / "release" / "libmcs_accordin.so"
BPF_INTERPOSE_LOCK_PREFIXES = ("flexguard",)
ROOT_REQUIRED_PRELOAD_LOCKS = {"accordin", "mcs_accordin"}
LOCK_ALIASES = {
    "accordin": "accordin",
    "mcs-tas": "mcstas",
    "mcs_extension": "mcs_extension",
    "mcs_accordin": "mcs_accordin",
    "stock": "stock",
}
LOCK_ORDER = (
    "stock",
    "mcs",
    "mcstp",
    "mcstas",
    "mcs_extension",
    "flexguard",
    "accordin",
    "reciprocating",
    "mcs_accordin",
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


class CommandLogger:
    def __init__(self, result_root: Path) -> None:
        self.result_root = result_root
        self.log_dir = result_root / "logs"
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.records: list[dict[str, object]] = []

    def run(
        self,
        cmd: list[str],
        *,
        log_name: str,
        cwd: Path = REPO_ROOT,
        env: dict[str, str] | None = None,
    ) -> CommandResult:
        log_path = self.log_dir / log_name
        started_at = dt.datetime.now(dt.timezone.utc)
        started_perf = time.perf_counter()
        record: dict[str, object] = {
            "command": cmd,
            "command_text": shlex.join(cmd),
            "cwd": str(cwd),
            "log_path": str(log_path),
            "started_at": started_at.isoformat(),
        }
        run_env = os.environ.copy()
        if env:
            run_env.update(env)

        output_chunks: list[str] = []
        with log_path.open("w", encoding="utf-8") as log_file:
            log_file.write(f"cwd: {cwd}\n")
            log_file.write(f"command: {shlex.join(cmd)}\n")
            log_file.write(f"started_at: {started_at.isoformat()}\n\n")
            log_file.flush()

            process = subprocess.Popen(
                cmd,
                cwd=str(cwd),
                env=run_env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,
            )
            assert process.stdout is not None
            for line in process.stdout:
                output_chunks.append(line)
                log_file.write(line)
                log_file.flush()
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
                f"Command failed with exit code {returncode}: {shlex.join(cmd)}",
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


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run or plot the PARSEC dedup and streamcluster experiment sweep.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"""\
Default benchmark settings:
  benchmarks={','.join(DEFAULT_BENCHMARKS)}
  locks={','.join(DEFAULT_LOCKS)}
  threads={','.join(str(thread) for thread in DEFAULT_THREADS)}
  repeats={DEFAULT_REPEATS}, dedup-compression={DEFAULT_DEDUP_COMPRESSION}
  streamcluster min/max={DEFAULT_STREAMCLUSTER_MIN_CENTERS}/{DEFAULT_STREAMCLUSTER_MAX_CENTERS}
  streamcluster dimensions={DEFAULT_STREAMCLUSTER_DIMENSIONS}, num-points={DEFAULT_STREAMCLUSTER_NUM_POINTS}
  streamcluster chunksize={DEFAULT_STREAMCLUSTER_CHUNKSIZE}, clustersize={DEFAULT_STREAMCLUSTER_CLUSTERSIZE}

Examples:
  python3 experiments/run_experiment_three.py
  python3 experiments/run_experiment_three.py --benchmarks streamcluster --locks stock --threads 1 --repeats 1 --streamcluster-dimensions 16 --streamcluster-num-points 64 --streamcluster-chunksize 64 --streamcluster-clustersize 16
  python3 experiments/run_experiment_three.py --plot-only experiments/results/experiment3_manual
""",
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        default=None,
        help="Directory for a new run. Default: experiments/results/experiment3_<timestamp>.",
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
        help="Build missing interpose helpers or PARSEC binaries before running.",
    )
    parser.add_argument(
        "--benchmarks",
        default=",".join(DEFAULT_BENCHMARKS),
        metavar="CSV",
        help=(
            "Comma-separated benchmark keys. "
            f"Default: {','.join(DEFAULT_BENCHMARKS)}."
        ),
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
    return parser.parse_args()


def parse_csv_strings(value: str) -> tuple[str, ...]:
    items = tuple(item.strip() for item in value.split(",") if item.strip())
    if not items:
        raise ValueError("CSV value must contain at least one item")
    return items


def normalize_lock(lock: str) -> str:
    normalized = lock.strip().lower()
    return LOCK_ALIASES.get(normalized, normalized)


def validate_locks(locks: tuple[str, ...]) -> tuple[str, ...]:
    supported = set(DEFAULT_LOCKS) | set(LOCK_ALIASES.values())
    supported.update(LOCK_LABELS.keys())
    unsupported = [lock for lock in locks if lock not in supported]
    if unsupported:
        raise ValueError(
            f"Unsupported lock keys: {', '.join(unsupported)}. "
            f"Supported keys: {', '.join(sorted(supported))}"
        )
    return locks


def combine_ld_preload(preload_library: Path) -> str:
    existing = os.environ.get("LD_PRELOAD", "").strip()
    return f"{preload_library}:{existing}" if existing else str(preload_library)


def accordin_preload_env(preload_library: Path) -> dict[str, str]:
    env = {"LD_PRELOAD": combine_ld_preload(preload_library)}
    if "ACCORDIN_CPU_MASK_K" in os.environ:
        env["ACCORDIN_CPU_MASK_K"] = os.environ["ACCORDIN_CPU_MASK_K"]
    if "K" in os.environ:
        env["K"] = os.environ["K"]
    return env


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


def default_result_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment3_{timestamp}"


def ensure_output_root(path: Path, force: bool) -> None:
    if path.exists() and not path.is_dir():
        raise RuntimeError(f"Output root exists but is not a directory: {path}")
    if path.exists() and any(path.iterdir()) and not force:
        raise RuntimeError(f"Output root already exists and is not empty: {path}. Use --force to write there.")
    path.mkdir(parents=True, exist_ok=True)


def lock_label(lock: str) -> str:
    return LOCK_LABELS.get(lock, lock)


def benchmark_label(benchmark: str) -> str:
    return BENCHMARK_LABELS.get(benchmark, benchmark)


def lock_sort_key(lock: str) -> tuple[int, str]:
    if lock in LOCK_ORDER:
        return (LOCK_ORDER.index(lock), lock)
    return (len(LOCK_ORDER), lock)


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
    return FLEXGUARD_BUILD_DIR / f"interpose_{lock}.sh"


def interpose_library_path(lock: str) -> Path:
    return FLEXGUARD_BUILD_DIR / f"interpose_{lock}.so"


def interpose_needs_sudo(lock: str) -> bool:
    return lock.startswith(BPF_INTERPOSE_LOCK_PREFIXES)


def with_sudo_env(cmd: list[str], env: dict[str, str] | None) -> tuple[list[str], None]:
    sudo_cmd = ["sudo", "-n", "env"]
    if env is not None:
        sudo_cmd.extend(f"{key}={value}" for key, value in sorted(env.items()))
    sudo_cmd.extend(cmd)
    return sudo_cmd, None


def interpose_command(lock: str, env: dict[str, str] | None = None) -> tuple[list[str], dict[str, str] | None]:
    cmd = [str(interpose_script_path(lock))]
    if interpose_needs_sudo(lock):
        return with_sudo_env(cmd, env)
    return cmd, env


def benchmark_command(lock: str, cmd: list[str], env: dict[str, str] | None) -> tuple[list[str], dict[str, str] | None]:
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
    expected_base_dir = FLEXGUARD_DIR.resolve()
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
        if lock in {"stock", "accordin", "mcs_extension", "mcs_accordin"}:
            continue
        error = interpose_helper_error(lock)
        if error is not None:
            invalid.append(error)
    return invalid


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
            "Run cargo build -p mcs_accordin --release or rerun with --build-missing."
        )

    logger.run(
        ["cargo", "build", "-p", "mcs_accordin", "--release"],
        log_name="build_mcs_accordin.log",
        cwd=REPO_ROOT,
    )
    if not ACCORDIN_PRELOAD_LIBRARY.is_file():
        raise RuntimeError(f"LD_PRELOAD helper was not built: {ACCORDIN_PRELOAD_LIBRARY}")


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
    )
    if not MCS_EXTENSION_PRELOAD_LIBRARY.is_file():
        raise RuntimeError(f"LD_PRELOAD helper was not built: {MCS_EXTENSION_PRELOAD_LIBRARY}")


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
    )
    if not MCS_ACCORDIN_PRELOAD_LIBRARY.is_file():
        raise RuntimeError(f"LD_PRELOAD helper was not built: {MCS_ACCORDIN_PRELOAD_LIBRARY}")


def ensure_interpose_helpers(
    locks: tuple[str, ...],
    *,
    build_missing: bool,
    logger: CommandLogger,
) -> None:
    invalid = invalid_interpose_helpers(locks)
    if not invalid:
        return

    if not build_missing:
        raise RuntimeError(
            "Interpose helpers are missing or stale: "
            f"{'; '.join(invalid)}. Run bench/flexguard/scripts/make_all.sh or rerun with --build-missing."
        )

    if not MAKE_ALL_SCRIPT.is_file():
        raise RuntimeError(f"Build helper was not found: {MAKE_ALL_SCRIPT}")

    logger.run(
        ["bash", str(MAKE_ALL_SCRIPT)],
        log_name="build_interpose_helpers.log",
        cwd=FLEXGUARD_DIR,
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
        "threads": list(threads),
        "repeats": args.repeats,
        "build_missing": args.build_missing,
        "source": calibration.source,
        "tsc_khz": calibration.tsc_khz,
        "dedup": {
            "binary": str(DEDUP_BINARY),
            "input": str(dedup_input),
            "compression": args.dedup_compression,
        },
        "streamcluster": {
            "binary": str(STREAMCLUSTER_BINARY),
            "min_centers": args.streamcluster_min_centers,
            "max_centers": args.streamcluster_max_centers,
            "dimensions": args.streamcluster_dimensions,
            "num_points": args.streamcluster_num_points,
            "chunksize": args.streamcluster_chunksize,
            "clustersize": args.streamcluster_clustersize,
            "input": streamcluster_input,
        },
        "flexguard_dir": str(FLEXGUARD_DIR),
        "machine_core_count": MACHINE_CORE_COUNT,
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
) -> tuple[list[str], dict[str, str] | None]:
    cmd: list[str] = []
    if lock != "stock":
        if lock == "accordin":
            env = accordin_preload_env(ACCORDIN_PRELOAD_LIBRARY)
        elif lock == "mcs_extension":
            env = {"LD_PRELOAD": combine_ld_preload(MCS_EXTENSION_PRELOAD_LIBRARY)}
        elif lock == "mcs_accordin":
            env = accordin_preload_env(MCS_ACCORDIN_PRELOAD_LIBRARY)
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
) -> tuple[list[str], dict[str, str] | None]:
    cmd: list[str] = []
    if lock != "stock":
        if lock == "accordin":
            env = accordin_preload_env(ACCORDIN_PRELOAD_LIBRARY)
        elif lock == "mcs_extension":
            env = {"LD_PRELOAD": combine_ld_preload(MCS_EXTENSION_PRELOAD_LIBRARY)}
        elif lock == "mcs_accordin":
            env = accordin_preload_env(MCS_ACCORDIN_PRELOAD_LIBRARY)
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
    benchmarks: tuple[str, ...],
    locks: tuple[str, ...],
    threads: tuple[int, ...],
    dedup_input: Path,
    streamcluster_input: str,
    calibration: TscCalibration,
    args: argparse.Namespace,
    logger: CommandLogger,
) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = []

    for benchmark in benchmarks:
        for lock in locks:
            for thread in threads:
                for repeat in range(1, args.repeats + 1):
                    output_path = temp_output_path(
                        result_root,
                        benchmark=benchmark,
                        lock=lock,
                        threads=thread,
                        repeat=repeat,
                    )
                    remove_if_exists(output_path)
                    try:
                        env: dict[str, str] | None
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
        ax.annotate(
            f"{MACHINE_CORE_COUNT} cores",
            xy=(MACHINE_CORE_COUNT, 0.96),
            xycoords=ax.get_xaxis_transform(),
            xytext=(6, 0),
            textcoords="offset points",
            ha="left",
            va="top",
            fontsize=8,
            color="0.2",
        )


def plot_runtime(summary_rows: list[dict[str, str]], *, benchmark: str, output_path: Path) -> None:
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
            (int(row["threads"]), float(row["mean_run_time_ms"]))
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

    ax.set_title(f"Run Time vs Threads: {benchmark_label(benchmark)}")
    ax.set_xlabel("Threads")
    ax.set_ylabel("Mean run time (ms)")
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
        output_path = result_root / f"runtime_vs_threads_{benchmark}.png"
        plot_runtime(summary_rows, benchmark=benchmark, output_path=output_path)
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
        if args.streamcluster_min_centers > args.streamcluster_max_centers:
            print("--streamcluster-min-centers cannot be greater than --streamcluster-max-centers.", file=sys.stderr)
            return 2

        benchmarks = validate_benchmarks(parse_csv_strings(args.benchmarks))
        locks = validate_locks(tuple(dict.fromkeys(normalize_lock(lock) for lock in parse_csv_strings(args.locks))))
        threads = parse_csv_ints(args.threads)
        dedup_input = resolve_path(args.dedup_input)
        streamcluster_input = resolve_optional_input(args.streamcluster_input)

        if streamcluster_input != "none" and not Path(streamcluster_input).is_file():
            print(f"Streamcluster input file does not exist: {streamcluster_input}", file=sys.stderr)
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
            plot_paths = write_plots(result_root, summary_rows)
            print_outputs(result_root, raw_path, summary_path, plot_paths)
            return 0

        result_root = resolve_path(args.output_root) if args.output_root is not None else default_result_root()
        ensure_output_root(result_root, args.force)
        logger = CommandLogger(result_root)
        ensure_interpose_helpers(locks, build_missing=args.build_missing, logger=logger)
        if "accordin" in locks:
            ensure_accordin_preload(build_missing=args.build_missing, logger=logger)
        if "mcs_extension" in locks:
            ensure_mcs_extension_preload(build_missing=args.build_missing, logger=logger)
        if "mcs_accordin" in locks:
            ensure_mcs_accordin_preload(build_missing=args.build_missing, logger=logger)
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
        raw_rows = run_benchmarks(
            result_root,
            benchmarks=benchmarks,
            locks=locks,
            threads=threads,
            dedup_input=dedup_input,
            streamcluster_input=streamcluster_input,
            calibration=calibration,
            args=args,
            logger=logger,
        )
        raw_path = write_raw_csv(result_root, raw_rows)
        summary_rows = summarize_rows(raw_rows)
        summary_path = write_summary_csv(result_root, summary_rows)
        plot_paths = write_plots(result_root, summary_rows)
        print_outputs(result_root, raw_path, summary_path, plot_paths)
        return 0
    except CommandError as exc:
        print(str(exc), file=sys.stderr)
        print(f"Command log: {exc.log_path}", file=sys.stderr)
        return exc.returncode
    except Exception as exc:
        print(str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
