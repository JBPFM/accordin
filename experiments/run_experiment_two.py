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
from dataclasses import dataclass
from pathlib import Path
from statistics import mean
from typing import Iterable


REPO_ROOT = Path(__file__).resolve().parents[1]
FLEXGUARD_DIR = REPO_ROOT / "bench" / "flexguard"
FLEXGUARD_BUILD_DIR = FLEXGUARD_DIR / "build"
MAKE_ALL_SCRIPT = FLEXGUARD_DIR / "scripts" / "make_all.sh"
PTHREAD_HOST_BINARY = FLEXGUARD_BUILD_DIR / "buckets_pthread_host"
MCS_ACCORDIN_PRELOAD_LIBRARY = REPO_ROOT / "target" / "release" / "libmcs_accordin.so"
MCS_EXTENSION_PRELOAD_LIBRARY = REPO_ROOT / "target" / "release" / "libmcs_tse.so"
DEFAULT_ACCORDIN_K = "2"
DEFAULT_THREADS = (1, 2, 4, 8, 16, 32, 64, 96, 128, 192, 256)
DEFAULT_LOCKS = (
    "mcs",
    "mcstp",
    "mcs-tas",
    "mcs_extension",
    "flexguard",
    "mcs_accordin",
    "reciprocating",
    "malthusian",
)
WORKLOAD_ORDER = ("uniform", "zipf")
DEFAULT_DURATION_MS = 5000
DEFAULT_REPEATS = 3
DEFAULT_BUCKETS = 100
DEFAULT_MAX_VALUE = 100000
DEFAULT_OFFSET_CHANGES = 40
DEFAULT_NON_CRITICAL_CYCLES = 0
DEFAULT_ZIPF_ALPHA = 10.0
MACHINE_CORE_COUNT = 96
REQUIRED_BUCKET_OPTIONS = ("--distribution", "--zipf-alpha")
REQUIRED_PTHREAD_HOST_SYMBOLS = (
    "pthread_mutex_init",
    "pthread_mutex_lock",
    "pthread_mutex_unlock",
)
PTHREAD_HOST_STALE_INPUTS = (
    FLEXGUARD_DIR / "bmarks" / "buckets.c",
    FLEXGUARD_DIR / "include" / "utils.h",
    FLEXGUARD_DIR / "include" / "lock_if.h",
    FLEXGUARD_DIR / "src" / "hash_map.c",
)
THROUGHPUT_PATTERN = re.compile(r"^#Throughput:\s*([0-9]+(?:\.[0-9]+)?)\s+CS/s$")
BUCKET_PATTERN = re.compile(
    r"^#Bucket\s+(\d+):\s+(\d+)\s+/\s+(\d+)\s+successful reads,\s+(\d+)\s+writes$"
)
UNRECOGNIZED_OPTION_PATTERN = re.compile(r"unrecognized option '([^']+)'")
RAW_FIELDS = (
    "lock",
    "workload",
    "threads",
    "buckets",
    "max_value",
    "offset_changes",
    "non_critical_cycles",
    "duration_ms",
    "repeat",
    "throughput_cs_per_sec",
    "hot_bucket_operation_share",
    "hottest_bucket_id",
    "total_bucket_operations",
    "command_log",
)
SUMMARY_FIELDS = (
    "lock",
    "workload",
    "threads",
    "mean_throughput_cs_per_sec",
    "mean_hot_bucket_operation_share",
    "runs",
)
WORKLOAD_LABELS = {
    "uniform": "Uniform",
    "zipf": "Zipf (skewed hot locks)",
}
LOCK_ALIASES = {
    "mcstas": "mcs-tas",
    "accordin": "mcs_accordin",
}
LOCK_LABELS = {
    "flexguard": "FlexGuard",
    "mcstp": "MCS-TP",
    "mcs-tas": "MCS-TAS",
    "mcstas": "MCS-TAS",
    "mcs": "MCS",
    "mcs_accordin": "MCS-TAS Simple",
    "accordin": "MCS-TAS Simple",
    "mcs_extension": "MCS + TSE",
    "reciprocating": "Reciprocating",
    "malthusian": "Malthusian",
    "mutex": "Mutex",
}
DIRECT_BINARY_LOCK_KEYS = {
    "mcs-tas": "mcstas",
}
ROOT_REQUIRED_LOCKS = {"flexguard", "mcs_accordin"}


@dataclass(frozen=True)
class CommandResult:
    log_path: Path
    output: str


@dataclass(frozen=True)
class BinarySupportCheck:
    path: Path
    missing_options: tuple[str, ...]


@dataclass(frozen=True)
class PthreadHostSymbolCheck:
    missing_imports: tuple[str, ...]
    unexpected_definitions: tuple[str, ...]


@dataclass(frozen=True)
class LockExecutionSpec:
    key: str
    label: str
    mode: str
    direct_binary: Path | None = None
    pthread_host_binary: Path | None = None
    wrapper_script: Path | None = None
    wrapper_library: Path | None = None
    preload_library: Path | None = None


@dataclass(frozen=True)
class ArtifactIssue:
    lock_key: str
    artifact_kind: str
    path: Path
    reason: str


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
            record["env_overrides"] = dict(sorted(env.items()))

        output_chunks: list[str] = []
        with log_path.open("w", encoding="utf-8") as log_file:
            log_file.write(f"cwd: {cwd}\n")
            log_file.write(f"command: {shlex.join(cmd)}\n")
            log_file.write(f"started_at: {started_at.isoformat()}\n\n")
            if env:
                log_file.write("env_overrides:\n")
                for key, value in sorted(env.items()):
                    log_file.write(f"  {key}={value}\n")
                log_file.write("\n")
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

            finished_at = dt.datetime.now(dt.timezone.utc)
            log_file.write(f"\nfinished_at: {finished_at.isoformat()}\n")
            log_file.write(f"returncode: {returncode}\n")

        record["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
        record["returncode"] = returncode
        self.records.append(record)
        self.write_manifest()

        if returncode != 0:
            raise CommandError(
                f"Command failed with exit code {returncode}: {shlex.join(cmd)}",
                returncode,
                log_path,
            )
        return CommandResult(log_path=log_path, output="".join(output_chunks))

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


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0.0:
        raise argparse.ArgumentTypeError("value must be positive")
    return parsed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run or plot the FlexGuard multi-lock hash-table experiment "
            "with experiment1-compatible buckets defaults."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"""\
Default benchmark settings:
  locks={','.join(DEFAULT_LOCKS)}
  threads={','.join(str(thread) for thread in DEFAULT_THREADS)}
  duration-ms={DEFAULT_DURATION_MS}, repeats={DEFAULT_REPEATS}, buckets={DEFAULT_BUCKETS}
  max-value={DEFAULT_MAX_VALUE}, offset-changes={DEFAULT_OFFSET_CHANGES}
  non-critical-cycles={DEFAULT_NON_CRITICAL_CYCLES}, zipf-alpha={DEFAULT_ZIPF_ALPHA}

Lock mapping notes:
  The default lock order matches experiment1 concepts.
  mcs uses build/buckets_mcs directly.
  mcs-tas uses build/buckets_mcstas directly; mcstas remains accepted as an alias.
  reciprocating uses build/buckets_reciprocating directly.
  malthusian uses build/buckets_malthusian directly.
  mcstp and flexguard run the pthread host through build/interpose_<lock>.sh.
  mcs_extension runs the pthread host with LD_PRELOAD=target/release/libmcs_tse.so.
  mcs_accordin runs the pthread host with LD_PRELOAD=target/release/libmcs_accordin.so.
  accordin remains accepted as an alias for mcs_accordin.
  mutex remains available and uses the pthread host without LD_PRELOAD.

Examples:
  python3 experiments/run_experiment_two.py
  python3 experiments/run_experiment_two.py --output-root experiments/results/experiment2_manual
  python3 experiments/run_experiment_two.py --threads 1,2,4 --duration-ms 1000 --repeats 1
  python3 experiments/run_experiment_two.py --plot-only experiments/results/experiment2_manual
""",
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        default=None,
        help="Directory for a new run. Default: experiments/results/experiment2_<timestamp>.",
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
        help=(
            "Build missing or stale benchmark artifacts, including direct buckets binaries, "
            "the pthread host, wrapper scripts, and LD_PRELOAD helper libraries."
        ),
    )
    parser.add_argument(
        "--locks",
        default=",".join(DEFAULT_LOCKS),
        metavar="CSV",
        help=(
            "Comma-separated experiment2 lock keys. "
            f"Default: {','.join(DEFAULT_LOCKS)}. "
            "Supported mappings: mcs, mcs-tas/mcstas, reciprocating, malthusian, "
            "mcstp, mcs_extension, flexguard, mcs_accordin/accordin, mutex. "
            "Unknown keys fall back to build/buckets_<lock>."
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
        "--duration-ms",
        type=positive_int,
        default=DEFAULT_DURATION_MS,
        help=f"Benchmark duration in milliseconds. Default: {DEFAULT_DURATION_MS}.",
    )
    parser.add_argument(
        "--repeats",
        type=positive_int,
        default=DEFAULT_REPEATS,
        help=f"Number of repeats per lock/workload/thread point. Default: {DEFAULT_REPEATS}.",
    )
    parser.add_argument(
        "--buckets",
        type=positive_int,
        default=DEFAULT_BUCKETS,
        help=f"Number of buckets. Default: {DEFAULT_BUCKETS}.",
    )
    parser.add_argument(
        "--max-value",
        type=positive_int,
        default=DEFAULT_MAX_VALUE,
        help=f"Maximum value. Default: {DEFAULT_MAX_VALUE}.",
    )
    parser.add_argument(
        "--offset-changes",
        type=positive_int,
        default=DEFAULT_OFFSET_CHANGES,
        help=f"Number of offset changes. Default: {DEFAULT_OFFSET_CHANGES}.",
    )
    parser.add_argument(
        "--non-critical-cycles",
        type=non_negative_int,
        default=DEFAULT_NON_CRITICAL_CYCLES,
        help=(
            "Number of non-critical cycles between critical sections. "
            f"Default: {DEFAULT_NON_CRITICAL_CYCLES}."
        ),
    )
    parser.add_argument(
        "--zipf-alpha",
        type=positive_float,
        default=DEFAULT_ZIPF_ALPHA,
        help=f"Zipf alpha for the zipf workload. Default: {DEFAULT_ZIPF_ALPHA}.",
    )
    return parser.parse_args()


def parse_csv_strings(value: str) -> tuple[str, ...]:
    items = tuple(item.strip() for item in value.split(",") if item.strip())
    if not items:
        raise ValueError("CSV value must contain at least one item")
    return items


def parse_csv_ints(value: str) -> tuple[int, ...]:
    items = tuple(int(item.strip()) for item in value.split(",") if item.strip())
    if not items:
        raise ValueError("CSV value must contain at least one integer")
    if any(item <= 0 for item in items):
        raise ValueError("Thread counts must be positive")
    return items


def resolve_path(path: Path) -> Path:
    return path.expanduser().resolve()


def normalize_lock_key(lock: str) -> str:
    normalized = lock.strip().lower()
    return LOCK_ALIASES.get(normalized, normalized)


def default_result_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment2_{timestamp}"


def ensure_output_root(path: Path, force: bool) -> None:
    if path.exists() and not path.is_dir():
        raise RuntimeError(f"Output root exists but is not a directory: {path}")
    if path.exists() and any(path.iterdir()) and not force:
        raise RuntimeError(f"Output root already exists and is not empty: {path}. Use --force to write there.")
    path.mkdir(parents=True, exist_ok=True)


def lock_label(lock: str) -> str:
    normalized = normalize_lock_key(lock)
    return LOCK_LABELS.get(normalized, normalized)


def workload_label(workload: str) -> str:
    return WORKLOAD_LABELS.get(workload, workload)


def direct_binary_path(lock: str) -> Path:
    return FLEXGUARD_BUILD_DIR / f"buckets_{lock}"


def wrapper_script_path(lock: str) -> Path:
    return FLEXGUARD_BUILD_DIR / f"interpose_{lock}.sh"


def check_binary_support(path: Path) -> BinarySupportCheck:
    try:
        completed = subprocess.run(
            [str(path), "--help"],
            cwd=str(REPO_ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
    except OSError as exc:
        raise RuntimeError(f"Failed to inspect {path} with --help: {exc}") from exc

    help_output = completed.stdout or ""
    missing_options = tuple(option for option in REQUIRED_BUCKET_OPTIONS if option not in help_output)
    if completed.returncode != 0 and not missing_options:
        raise RuntimeError(f"Failed to inspect {path}: --help exited with code {completed.returncode}.")
    return BinarySupportCheck(path=path, missing_options=missing_options)


def normalize_nm_symbol(symbol: str) -> str:
    return symbol.split("@", 1)[0]


def check_pthread_host_symbols(path: Path) -> PthreadHostSymbolCheck:
    try:
        completed = subprocess.run(
            ["nm", "-g", str(path)],
            cwd=str(REPO_ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
    except OSError as exc:
        raise RuntimeError(f"Failed to inspect pthread host symbols for {path}: {exc}") from exc

    if completed.returncode != 0:
        raise RuntimeError(f"Failed to inspect pthread host symbols for {path}: nm exited with code {completed.returncode}.")

    imported_symbols: set[str] = set()
    defined_symbols: set[str] = set()
    for line in completed.stdout.splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        symbol_type = parts[-2]
        symbol_name = normalize_nm_symbol(parts[-1])
        if symbol_name not in REQUIRED_PTHREAD_HOST_SYMBOLS:
            continue
        if symbol_type in {"U", "u", "w", "v"}:
            imported_symbols.add(symbol_name)
            continue
        defined_symbols.add(symbol_name)

    missing_imports = tuple(
        symbol for symbol in REQUIRED_PTHREAD_HOST_SYMBOLS if symbol not in imported_symbols
    )
    unexpected_definitions = tuple(
        symbol for symbol in REQUIRED_PTHREAD_HOST_SYMBOLS if symbol in defined_symbols
    )
    return PthreadHostSymbolCheck(
        missing_imports=missing_imports,
        unexpected_definitions=unexpected_definitions,
    )


def stale_dependencies(target: Path, dependencies: Iterable[Path]) -> tuple[Path, ...]:
    if not target.is_file():
        return ()

    target_mtime = target.stat().st_mtime
    return tuple(
        dependency
        for dependency in dependencies
        if dependency.is_file() and dependency.stat().st_mtime > target_mtime
    )


def lock_sort_key(lock: str) -> tuple[int, str]:
    normalized = normalize_lock_key(lock)
    if normalized in DEFAULT_LOCKS:
        return (DEFAULT_LOCKS.index(normalized), normalized)
    return (len(DEFAULT_LOCKS), normalized)


def workload_sort_key(workload: str) -> tuple[int, str]:
    if workload in WORKLOAD_ORDER:
        return (WORKLOAD_ORDER.index(workload), workload)
    return (len(WORKLOAD_ORDER), workload)


def lock_execution_spec(lock: str) -> LockExecutionSpec:
    normalized = normalize_lock_key(lock)
    label = lock_label(normalized)

    if normalized == "mcs":
        return LockExecutionSpec(
            key=normalized,
            label=label,
            mode="direct_binary",
            direct_binary=direct_binary_path("mcs"),
        )
    if normalized == "mcs-tas":
        return LockExecutionSpec(
            key=normalized,
            label=label,
            mode="direct_binary",
            direct_binary=direct_binary_path(DIRECT_BINARY_LOCK_KEYS[normalized]),
        )
    if normalized == "mcstp":
        return LockExecutionSpec(
            key=normalized,
            label=label,
            mode="interpose_wrapper",
            pthread_host_binary=PTHREAD_HOST_BINARY,
            wrapper_script=wrapper_script_path("mcstp"),
            wrapper_library=FLEXGUARD_BUILD_DIR / "interpose_mcstp.so",
        )
    if normalized == "flexguard":
        return LockExecutionSpec(
            key=normalized,
            label=label,
            mode="interpose_wrapper",
            pthread_host_binary=PTHREAD_HOST_BINARY,
            wrapper_script=wrapper_script_path("flexguard"),
            wrapper_library=FLEXGUARD_BUILD_DIR / "interpose_flexguard.so",
        )
    if normalized == "mcs_accordin":
        return LockExecutionSpec(
            key=normalized,
            label=label,
            mode="ld_preload",
            pthread_host_binary=PTHREAD_HOST_BINARY,
            preload_library=MCS_ACCORDIN_PRELOAD_LIBRARY,
        )
    if normalized == "mcs_extension":
        return LockExecutionSpec(
            key=normalized,
            label=label,
            mode="ld_preload",
            pthread_host_binary=PTHREAD_HOST_BINARY,
            preload_library=MCS_EXTENSION_PRELOAD_LIBRARY,
        )
    if normalized == "mutex":
        return LockExecutionSpec(
            key=normalized,
            label=label,
            mode="pthread_host",
            pthread_host_binary=PTHREAD_HOST_BINARY,
        )
    return LockExecutionSpec(
        key=normalized,
        label=label,
        mode="direct_binary",
        direct_binary=direct_binary_path(normalized),
    )


def resolve_lock_specs(locks: Iterable[str]) -> tuple[LockExecutionSpec, ...]:
    specs = tuple(lock_execution_spec(lock) for lock in locks)
    if not specs:
        raise RuntimeError("At least one lock must be selected.")
    return specs


def executable_missing(path: Path) -> bool:
    return not path.is_file() or not os.access(path, os.X_OK)


def add_issue(
    issues: list[ArtifactIssue],
    seen: set[tuple[str, str, str]],
    *,
    lock_key: str,
    artifact_kind: str,
    path: Path,
    reason: str,
) -> None:
    identity = (artifact_kind, str(path), reason)
    if identity in seen:
        return
    seen.add(identity)
    issues.append(
        ArtifactIssue(
            lock_key=lock_key,
            artifact_kind=artifact_kind,
            path=path,
            reason=reason,
        )
    )


def collect_artifact_issues(lock_specs: Iterable[LockExecutionSpec]) -> list[ArtifactIssue]:
    issues: list[ArtifactIssue] = []
    seen: set[tuple[str, str, str]] = set()

    for spec in lock_specs:
        if spec.direct_binary is not None:
            if executable_missing(spec.direct_binary):
                add_issue(
                    issues,
                    seen,
                    lock_key=spec.key,
                    artifact_kind="direct binary",
                    path=spec.direct_binary,
                    reason="missing executable",
                )
            else:
                try:
                    support = check_binary_support(spec.direct_binary)
                except RuntimeError as exc:
                    add_issue(
                        issues,
                        seen,
                        lock_key=spec.key,
                        artifact_kind="direct binary",
                        path=spec.direct_binary,
                        reason=str(exc),
                    )
                else:
                    if support.missing_options:
                        add_issue(
                            issues,
                            seen,
                            lock_key=spec.key,
                            artifact_kind="direct binary",
                            path=spec.direct_binary,
                            reason=f"missing {', '.join(support.missing_options)} in --help",
                        )

        if spec.pthread_host_binary is not None:
            if executable_missing(spec.pthread_host_binary):
                add_issue(
                    issues,
                    seen,
                    lock_key=spec.key,
                    artifact_kind="pthread host",
                    path=spec.pthread_host_binary,
                    reason="missing executable",
                )
            else:
                try:
                    support = check_binary_support(spec.pthread_host_binary)
                except RuntimeError as exc:
                    add_issue(
                        issues,
                        seen,
                        lock_key=spec.key,
                        artifact_kind="pthread host",
                        path=spec.pthread_host_binary,
                        reason=str(exc),
                    )
                else:
                    if support.missing_options:
                        add_issue(
                            issues,
                            seen,
                            lock_key=spec.key,
                            artifact_kind="pthread host",
                            path=spec.pthread_host_binary,
                            reason=f"missing {', '.join(support.missing_options)} in --help",
                        )
                    else:
                        try:
                            symbol_check = check_pthread_host_symbols(spec.pthread_host_binary)
                        except RuntimeError as exc:
                            add_issue(
                                issues,
                                seen,
                                lock_key=spec.key,
                                artifact_kind="pthread host",
                                path=spec.pthread_host_binary,
                                reason=str(exc),
                            )
                        else:
                            if symbol_check.missing_imports:
                                add_issue(
                                    issues,
                                    seen,
                                    lock_key=spec.key,
                                    artifact_kind="pthread host",
                                    path=spec.pthread_host_binary,
                                    reason=(
                                        "missing imported pthread symbols: "
                                        f"{', '.join(symbol_check.missing_imports)}"
                                    ),
                                )
                            if symbol_check.unexpected_definitions:
                                add_issue(
                                    issues,
                                    seen,
                                    lock_key=spec.key,
                                    artifact_kind="pthread host",
                                    path=spec.pthread_host_binary,
                                    reason=(
                                        "unexpected defined pthread symbols: "
                                        f"{', '.join(symbol_check.unexpected_definitions)}"
                                    ),
                                )
                            stale_inputs = stale_dependencies(
                                spec.pthread_host_binary,
                                PTHREAD_HOST_STALE_INPUTS,
                            )
                            if stale_inputs:
                                add_issue(
                                    issues,
                                    seen,
                                    lock_key=spec.key,
                                    artifact_kind="pthread host",
                                    path=spec.pthread_host_binary,
                                    reason=(
                                        "older than dependencies: "
                                        + ", ".join(str(path) for path in stale_inputs)
                                    ),
                                )

        if spec.wrapper_script is not None and executable_missing(spec.wrapper_script):
            add_issue(
                issues,
                seen,
                lock_key=spec.key,
                artifact_kind="wrapper script",
                path=spec.wrapper_script,
                reason="missing executable",
            )

        if spec.wrapper_library is not None and not spec.wrapper_library.is_file():
            add_issue(
                issues,
                seen,
                lock_key=spec.key,
                artifact_kind="wrapper library",
                path=spec.wrapper_library,
                reason="missing file",
            )

        if spec.preload_library is not None and not spec.preload_library.is_file():
            add_issue(
                issues,
                seen,
                lock_key=spec.key,
                artifact_kind="preload library",
                path=spec.preload_library,
                reason="missing file",
            )

    return issues


def format_artifact_issues(issues: Iterable[ArtifactIssue]) -> str:
    return "\n".join(
        f"  - {issue.lock_key}: {issue.artifact_kind} {issue.path} ({issue.reason})"
        for issue in issues
    )


def ensure_make_all_script() -> None:
    if not MAKE_ALL_SCRIPT.is_file() or not os.access(MAKE_ALL_SCRIPT, os.X_OK):
        raise RuntimeError(f"Build helper is not executable: {MAKE_ALL_SCRIPT}")


def build_with_make_all(logger: CommandLogger) -> None:
    ensure_make_all_script()
    logger.run([str(MAKE_ALL_SCRIPT)], log_name="build_flexguard_make_all.log", cwd=FLEXGUARD_DIR)


def copy_built_artifact(source: Path, destination: Path) -> None:
    if not source.is_file():
        raise RuntimeError(f"Expected build artifact was not produced: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)


def build_pthread_host(logger: CommandLogger) -> None:
    logger.run(["make", "clean"], log_name="build_buckets_pthread_host_clean.log", cwd=FLEXGUARD_DIR)
    logger.run(
        ["make", "LOCK_VERSION=MUTEX", "ADD_PADDING=1", "buckets"],
        log_name="build_buckets_pthread_host_make.log",
        cwd=FLEXGUARD_DIR,
    )
    copy_built_artifact(FLEXGUARD_DIR / "buckets", PTHREAD_HOST_BINARY)
    logger.run(["make", "clean"], log_name="build_buckets_pthread_host_post_clean.log", cwd=FLEXGUARD_DIR)


def build_mcs_accordin_preload(logger: CommandLogger) -> None:
    logger.run(
        ["cargo", "build", "-p", "mcs_accordin", "--release"],
        log_name="build_mcs_accordin_release.log",
        cwd=REPO_ROOT,
    )


def build_mcs_extension_preload(logger: CommandLogger) -> None:
    logger.run(
        ["cargo", "build", "-p", "mcs_tse", "--release"],
        log_name="build_mcs_tse_release.log",
        cwd=REPO_ROOT,
    )


def issue_requires_make_all(issue: ArtifactIssue) -> bool:
    if issue.artifact_kind == "direct binary":
        return True
    return issue.lock_key in {"mcstp", "flexguard"} and issue.artifact_kind in {"wrapper script", "wrapper library"}


def ensure_benchmark_artifacts(
    lock_specs: tuple[LockExecutionSpec, ...],
    build_missing: bool,
    logger: CommandLogger,
) -> None:
    issues = collect_artifact_issues(lock_specs)
    if not issues:
        return

    if not build_missing:
        raise RuntimeError(
            "Required benchmark artifacts are missing or stale:\n"
            f"{format_artifact_issues(issues)}\n"
            "Rerun with --build-missing to rebuild supported artifacts where applicable."
        )

    if any(issue_requires_make_all(issue) for issue in issues):
        build_with_make_all(logger)
    if any(issue.artifact_kind == "pthread host" for issue in issues):
        build_pthread_host(logger)
    if any(issue.lock_key == "mcs_accordin" and issue.artifact_kind == "preload library" for issue in issues):
        build_mcs_accordin_preload(logger)
    if any(issue.lock_key == "mcs_extension" and issue.artifact_kind == "preload library" for issue in issues):
        build_mcs_extension_preload(logger)

    issues_after_build = collect_artifact_issues(lock_specs)
    if issues_after_build:
        raise RuntimeError(
            "Required benchmark artifacts are still invalid after --build-missing:\n"
            f"{format_artifact_issues(issues_after_build)}"
        )


def write_settings(
    result_root: Path,
    *,
    lock_specs: tuple[LockExecutionSpec, ...],
    threads: tuple[int, ...],
    args: argparse.Namespace,
) -> None:
    settings = {
        "locks": [
            {
                "key": spec.key,
                "label": spec.label,
                "mode": spec.mode,
                "artifacts": {
                    name: str(path)
                    for name, path in (
                        ("direct_binary", spec.direct_binary),
                        ("pthread_host_binary", spec.pthread_host_binary),
                        ("wrapper_script", spec.wrapper_script),
                        ("wrapper_library", spec.wrapper_library),
                        ("preload_library", spec.preload_library),
                    )
                    if path is not None
                },
            }
            for spec in lock_specs
        ],
        "threads": list(threads),
        "workloads": [{"key": key, "label": label} for key, label in WORKLOAD_LABELS.items()],
        "duration_ms": args.duration_ms,
        "repeats": args.repeats,
        "buckets": args.buckets,
        "max_value": args.max_value,
        "offset_changes": args.offset_changes,
        "non_critical_cycles": args.non_critical_cycles,
        "zipf_alpha": args.zipf_alpha,
        "build_missing": args.build_missing,
        "flexguard_dir": str(FLEXGUARD_DIR),
        "machine_core_count": MACHINE_CORE_COUNT,
    }
    with (result_root / "settings.json").open("w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")


def parse_run_output(output: str) -> tuple[float, float, int, int]:
    unsupported_option = UNRECOGNIZED_OPTION_PATTERN.search(output)
    if unsupported_option is not None:
        option = unsupported_option.group(1)
        if option in ("--distribution", "--zipf-alpha"):
            raise RuntimeError(
                "The selected benchmark executable does not support "
                f"{option}. Rebuild bench/flexguard/build/buckets_<lock> from the updated source."
            )
        raise RuntimeError(f"Benchmark rejected option {option}.")

    throughput: float | None = None
    total_bucket_operations = 0
    hottest_bucket_id = -1
    hottest_bucket_operations = -1

    for line in output.splitlines():
        stripped = line.strip()
        throughput_match = THROUGHPUT_PATTERN.match(stripped)
        if throughput_match is not None:
            throughput = float(throughput_match.group(1))
            continue

        bucket_match = BUCKET_PATTERN.match(stripped)
        if bucket_match is None:
            continue

        bucket_id = int(bucket_match.group(1))
        reads = int(bucket_match.group(3))
        writes = int(bucket_match.group(4))
        operations = reads + writes
        total_bucket_operations += operations
        if operations > hottest_bucket_operations:
            hottest_bucket_operations = operations
            hottest_bucket_id = bucket_id

    if throughput is None:
        raise RuntimeError("Benchmark output did not contain a #Throughput line.")
    if hottest_bucket_id < 0:
        raise RuntimeError("Benchmark output did not contain any #Bucket lines.")

    share = 0.0
    if total_bucket_operations > 0:
        share = hottest_bucket_operations / total_bucket_operations
    return throughput, share, hottest_bucket_id, total_bucket_operations


def buckets_args_for_run(args: argparse.Namespace, *, workload: str, threads: int) -> list[str]:
    return [
        "--duration",
        str(args.duration_ms),
        "--num-threads",
        str(threads),
        "--buckets",
        str(args.buckets),
        "--max-value",
        str(args.max_value),
        "--offset-changes",
        str(args.offset_changes),
        "--non-critical-cycles",
        str(args.non_critical_cycles),
        "--distribution",
        workload,
        "--zipf-alpha",
        str(args.zipf_alpha),
    ]


def combine_ld_preload(preload_library: Path) -> str:
    existing = os.environ.get("LD_PRELOAD", "").strip()
    if existing:
        return f"{preload_library}:{existing}"
    return str(preload_library)


def accordin_preload_env(preload_library: Path) -> dict[str, str]:
    env = {"LD_PRELOAD": combine_ld_preload(preload_library)}
    if "ACCORDIN_CPU_MASK_K" not in os.environ and "K" not in os.environ:
        env["K"] = DEFAULT_ACCORDIN_K
    return env


def wrap_root_command(cmd: list[str], env: dict[str, str] | None) -> tuple[list[str], dict[str, str] | None]:
    if os.geteuid() == 0:
        return cmd, env
    if shutil.which("sudo") is None:
        raise RuntimeError("sudo is required to run mcs_accordin because it loads a sched_ext eBPF scheduler.")

    env_args = [f"{key}={value}" for key, value in sorted((env or {}).items())]
    if env_args:
        return ["sudo", "--", "env", *env_args, *cmd], None
    return ["sudo", "--", *cmd], None


def benchmark_command(
    spec: LockExecutionSpec,
    *,
    benchmark_args: list[str],
) -> tuple[list[str], dict[str, str] | None]:
    cmd: list[str]
    env: dict[str, str] | None
    if spec.mode == "direct_binary":
        assert spec.direct_binary is not None
        cmd = [str(spec.direct_binary), *benchmark_args]
        env = None
    elif spec.mode == "interpose_wrapper":
        assert spec.wrapper_script is not None
        assert spec.pthread_host_binary is not None
        cmd = [str(spec.wrapper_script), str(spec.pthread_host_binary), *benchmark_args]
        env = None
    elif spec.mode == "ld_preload":
        assert spec.pthread_host_binary is not None
        assert spec.preload_library is not None
        cmd = [str(spec.pthread_host_binary), *benchmark_args]
        if spec.key == "mcs_accordin":
            env = accordin_preload_env(spec.preload_library)
        else:
            env = {"LD_PRELOAD": combine_ld_preload(spec.preload_library)}
    elif spec.mode == "pthread_host":
        assert spec.pthread_host_binary is not None
        cmd = [str(spec.pthread_host_binary), *benchmark_args]
        env = None
    else:
        raise RuntimeError(f"Unsupported lock mode {spec.mode} for {spec.key}")

    if spec.key in ROOT_REQUIRED_LOCKS:
        return wrap_root_command(cmd, env)
    return cmd, env


def run_benchmarks(
    result_root: Path,
    *,
    lock_specs: tuple[LockExecutionSpec, ...],
    threads: tuple[int, ...],
    args: argparse.Namespace,
    logger: CommandLogger,
) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = []
    workloads = WORKLOAD_ORDER

    for workload in workloads:
        for spec in lock_specs:
            for thread in threads:
                for repeat in range(1, args.repeats + 1):
                    run_args = buckets_args_for_run(args, workload=workload, threads=thread)
                    cmd, env = benchmark_command(spec, benchmark_args=run_args)
                    log_name = f"buckets_{spec.key}_{workload}_{thread:03d}_r{repeat}.log"
                    result = logger.run(cmd, log_name=log_name, env=env)
                    throughput, share, hottest_bucket_id, total_bucket_operations = parse_run_output(
                        result.output
                    )
                    rows.append(
                        {
                            "lock": spec.key,
                            "workload": workload,
                            "threads": str(thread),
                            "buckets": str(args.buckets),
                            "max_value": str(args.max_value),
                            "offset_changes": str(args.offset_changes),
                            "non_critical_cycles": str(args.non_critical_cycles),
                            "duration_ms": str(args.duration_ms),
                            "repeat": str(repeat),
                            "throughput_cs_per_sec": f"{throughput:.6f}",
                            "hot_bucket_operation_share": f"{share:.6f}",
                            "hottest_bucket_id": str(hottest_bucket_id),
                            "total_bucket_operations": str(total_bucket_operations),
                            "command_log": str(result.log_path.relative_to(result_root)),
                        }
                    )
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
        rows = [dict(row) for row in reader]
    for row in rows:
        row["lock"] = normalize_lock_key(row["lock"])
    return rows


def summarize_rows(rows: list[dict[str, str]]) -> list[dict[str, str]]:
    groups: dict[tuple[str, str, int], list[dict[str, str]]] = {}
    for row in rows:
        normalized_row = dict(row)
        normalized_row["lock"] = normalize_lock_key(row["lock"])
        key = (normalized_row["lock"], normalized_row["workload"], int(normalized_row["threads"]))
        groups.setdefault(key, []).append(normalized_row)

    summary_rows: list[dict[str, str]] = []
    for lock, workload, threads in sorted(
        groups.keys(),
        key=lambda item: (workload_sort_key(item[1]), lock_sort_key(item[0]), item[2]),
    ):
        group_rows = groups[(lock, workload, threads)]
        summary_rows.append(
            {
                "lock": lock,
                "workload": workload,
                "threads": str(threads),
                "mean_throughput_cs_per_sec": f"{mean(float(row['throughput_cs_per_sec']) for row in group_rows):.6f}",
                "mean_hot_bucket_operation_share": (
                    f"{mean(float(row['hot_bucket_operation_share']) for row in group_rows):.6f}"
                ),
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


def unique_threads(summary_rows: list[dict[str, str]], workload: str) -> list[int]:
    return sorted({int(row["threads"]) for row in summary_rows if row["workload"] == workload})


def add_thread_axis_formatting(ax, threads: list[int]) -> None:
    if not threads:
        return

    if len(threads) > 1:
        ax.set_xscale("log", base=2)
        ax.set_xlim(threads[0] / 1.08, threads[-1] * 1.25)
    else:
        ax.set_xlim(max(0.0, threads[0] - 0.5), threads[0] + 0.5)
    ax.set_xticks(threads)

    if threads[-1] >= MACHINE_CORE_COUNT:
        ax.axvspan(MACHINE_CORE_COUNT, threads[-1] * 1.25, color="0.92", alpha=0.55, linewidth=0, zorder=0)
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


def plot_workload_metric(
    summary_rows: list[dict[str, str]],
    *,
    workload: str,
    metric_field: str,
    ylabel: str,
    title_prefix: str,
    output_path: Path,
) -> None:
    try:
        import matplotlib
    except ModuleNotFoundError as exc:
        raise RuntimeError("matplotlib is required to generate plots.") from exc

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.ticker import ScalarFormatter

    rows = [row for row in summary_rows if row["workload"] == workload]
    if not rows:
        raise RuntimeError(f"No summary rows available for workload {workload}.")

    fig, ax = plt.subplots(figsize=(9.5, 5.5))
    thread_values = unique_threads(summary_rows, workload)
    lock_keys = sorted({normalize_lock_key(row["lock"]) for row in rows}, key=lock_sort_key)

    for lock in lock_keys:
        points = [
            (int(row["threads"]), float(row[metric_field]))
            for row in rows
            if normalize_lock_key(row["lock"]) == lock
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

    ax.set_title(f"{title_prefix}: {workload_label(workload)}")
    ax.set_xlabel("Threads")
    ax.set_ylabel(ylabel)
    add_thread_axis_formatting(ax, thread_values)
    ax.xaxis.set_major_formatter(ScalarFormatter())
    ax.grid(True, axis="y", alpha=0.28)
    ax.grid(True, axis="x", which="major", alpha=0.16)
    if metric_field == "mean_hot_bucket_operation_share":
        ax.set_ylim(0.0, 1.02)
    ax.legend(frameon=False)
    fig.tight_layout()
    fig.savefig(output_path, dpi=180)
    plt.close(fig)


def write_plots(result_root: Path, summary_rows: list[dict[str, str]]) -> list[Path]:
    workloads = sorted({row["workload"] for row in summary_rows}, key=workload_sort_key)
    if not workloads:
        raise RuntimeError("No summary rows were available for plotting.")

    plot_paths: list[Path] = []
    for workload in workloads:
        throughput_path = result_root / f"throughput_vs_threads_{workload}.png"
        hot_bucket_path = result_root / f"hot_bucket_share_vs_threads_{workload}.png"
        plot_workload_metric(
            summary_rows,
            workload=workload,
            metric_field="mean_throughput_cs_per_sec",
            ylabel="Throughput (CS/s)",
            title_prefix="Hashtable Throughput vs Threads",
            output_path=throughput_path,
        )
        plot_workload_metric(
            summary_rows,
            workload=workload,
            metric_field="mean_hot_bucket_operation_share",
            ylabel="Hot bucket operation share",
            title_prefix="Hot Bucket Operation Share vs Threads",
            output_path=hot_bucket_path,
        )
        plot_paths.extend((throughput_path, hot_bucket_path))
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
        if args.plot_only is not None:
            result_root = resolve_path(args.plot_only)
            if not result_root.is_dir():
                print(f"Plot-only result root does not exist: {result_root}", file=sys.stderr)
                return 2
            raw_rows = load_raw_rows(result_root)
            summary_rows = summarize_rows(raw_rows)
            raw_path = result_root / "raw.csv"
            summary_path = write_summary_csv(result_root, summary_rows)
            plot_paths = write_plots(result_root, summary_rows)
            print_outputs(result_root, raw_path, summary_path, plot_paths)
            return 0

        lock_specs = resolve_lock_specs(parse_csv_strings(args.locks))
        threads = parse_csv_ints(args.threads)
        result_root = resolve_path(args.output_root) if args.output_root is not None else default_result_root()
        ensure_output_root(result_root, args.force)
        logger = CommandLogger(result_root)
        ensure_benchmark_artifacts(lock_specs, args.build_missing, logger)
        write_settings(result_root, lock_specs=lock_specs, threads=threads, args=args)
        raw_rows = run_benchmarks(
            result_root,
            lock_specs=lock_specs,
            threads=threads,
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
