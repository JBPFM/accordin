#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import json
import os
import shutil
import statistics
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence

import experiment_defaults
import run_experiment_one as experiment_one
import run_experiment_three_common as parsec_common
from run_experiment_three_common import *  # noqa: F401,F403


REPO_ROOT = parsec_common.REPO_ROOT
MUTEXBENCH_DIR = REPO_ROOT / "bench" / "mutexbench"
SWEEP_MULTI = MUTEXBENCH_DIR / "scripts" / "sweep_mutex_throughput_multi_lock.sh"
SWEEP_SINGLE = MUTEXBENCH_DIR / "scripts" / "sweep_mutex_throughput.sh"
DEFAULT_BASELINE_ROOT = MUTEXBENCH_DIR / "results_baseline"
DEFAULT_WARMUP_DURATION_MS = 1000
DEFAULT_COMMAND_TIMEOUT_SECONDS = 21600
DEFAULT_OUTPUT_ROOT = REPO_ROOT / "experiments" / "results" / "experiment3_mutexbench_results_baseline"
ACCORDIN_DIRECT_PACKAGE = "mcs_tas_accordin_direct"
ACCORDIN_DIRECT_LOCK_KIND = "mcs_tas_accordin_direct"
ACCORDIN_DIRECT_RELEASE_LIB = REPO_ROOT / "target" / "release" / "libmcs_tas_accordin_direct.so"
ACCORDIN_DIRECT_LIB_ENV = "MCS_TAS_ACCORDIN_DIRECT_LIB"
ACCORDIN_DIRECT_DISABLE_BPF_ENV = "MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF"
ACCORDIN_DIRECT_STATS_ONLY_ENV = "MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY"
ACCORDIN_DIRECT_ENV_PREFIX = "MCS_TAS_ACCORDIN_DIRECT_"

REQUIRED_BASELINE_LOCKS = experiment_defaults.EXPERIMENT_ONE_FULL_LOCKS
ACCORDIN_LOCKS = experiment_defaults.ACCORDIN_VARIANT_LOCKS
MULTI_LOCK_SWEEP_LOCKS = (
    "mcs",
    "mcstp",
    "mcs-tas",
    "flexguard",
    "malthusian",
    "reciprocating",
)
SUPPORTED_LOCKS = set(REQUIRED_BASELINE_LOCKS)
LOCK_PRESENCE_ALIASES = {
    "mcs-tas": ("mcs-tas", "mcs_tas", "mcstas"),
    "mcs_extension": ("mcs_extension", "mcs_tse"),
    experiment_defaults.ACCORDIN_BASE_LOCK: (
        experiment_defaults.ACCORDIN_BASE_LOCK,
        "mcs_tas_accordin",
        "mcs_accordin",
        "accordin",
    ),
}


@dataclass(frozen=True)
class BaselineMatrix:
    threads: tuple[int, ...]
    critical_ns: tuple[int, ...]
    outside_ns: tuple[int, ...]
    repeats: int
    duration_ms: int
    warmup_duration_ms: int = DEFAULT_WARMUP_DURATION_MS

    @property
    def expected_rows_per_lock(self) -> int:
        return len(self.threads) * len(self.critical_ns) * len(self.outside_ns) * self.repeats


def matrix_with_threads(matrix: BaselineMatrix, threads: Iterable[int]) -> BaselineMatrix:
    return BaselineMatrix(
        threads=tuple(threads),
        critical_ns=matrix.critical_ns,
        outside_ns=matrix.outside_ns,
        repeats=matrix.repeats,
        duration_ms=matrix.duration_ms,
        warmup_duration_ms=matrix.warmup_duration_ms,
    )


def runnable_threads_for_lock(lock: str, matrix: BaselineMatrix) -> tuple[int, ...]:
    return matrix.threads


def expected_rows_for_lock(lock: str, matrix: BaselineMatrix) -> int:
    threads = runnable_threads_for_lock(lock, matrix)
    return len(threads) * len(matrix.critical_ns) * len(matrix.outside_ns) * matrix.repeats


def csv_join(values: Iterable[int | str]) -> str:
    return ",".join(str(value) for value in values)


def lock_aliases(lock: str) -> tuple[str, ...]:
    return LOCK_PRESENCE_ALIASES.get(lock, (lock,))


def lock_has_raw(root: Path, lock: str) -> bool:
    return any((root / alias / "raw.csv").is_file() for alias in lock_aliases(lock))


def present_lock_names(root: Path) -> tuple[str, ...]:
    if not root.is_dir():
        return ()
    return tuple(sorted(child.name for child in root.iterdir() if (child / "raw.csv").is_file()))


def missing_experiment_locks(root: Path) -> tuple[str, ...]:
    return tuple(lock for lock in REQUIRED_BASELINE_LOCKS if not lock_has_raw(root, lock))


def infer_baseline_matrix(root: Path, *, warmup_duration_ms: int = DEFAULT_WARMUP_DURATION_MS) -> BaselineMatrix:
    threads: set[int] = set()
    critical_ns: set[int] = set()
    outside_ns: set[int] = set()
    repeats: set[int] = set()
    durations_ms: list[int] = []

    for raw_path in sorted(root.glob("*/raw.csv")):
        with raw_path.open(newline="", encoding="utf-8") as f:
            reader = csv.DictReader(f)
            for row in reader:
                threads.add(int(row["threads"]))
                critical_ns.add(int(row["critical_iters"]))
                outside_ns.add(int(row["outside_iters"]))
                repeats.add(int(row["repeat"]))
                durations_ms.append(int(round(float(row["elapsed_seconds"]) * 1000)))

    if not threads or not critical_ns or not outside_ns or not repeats or not durations_ms:
        raise RuntimeError(f"Cannot infer baseline matrix from existing raw.csv files under {root}")

    duration_ms = Counter(durations_ms).most_common(1)[0][0]
    return BaselineMatrix(
        threads=tuple(sorted(threads)),
        critical_ns=tuple(sorted(critical_ns)),
        outside_ns=tuple(sorted(outside_ns)),
        repeats=max(repeats),
        duration_ms=duration_ms,
        warmup_duration_ms=warmup_duration_ms,
    )


def raw_row_count(path: Path) -> int:
    if not path.is_file():
        return 0
    with path.open(newline="", encoding="utf-8") as f:
        return sum(1 for _ in csv.DictReader(f))


def lock_is_complete(root: Path, lock: str, matrix: BaselineMatrix) -> bool:
    expected_rows = expected_rows_for_lock(lock, matrix)
    return any(raw_row_count(root / alias / "raw.csv") == expected_rows for alias in lock_aliases(lock))


def incomplete_or_missing_locks(root: Path, locks: tuple[str, ...], matrix: BaselineMatrix) -> tuple[str, ...]:
    return tuple(lock for lock in locks if not lock_is_complete(root, lock, matrix))


def parse_csv_strings(value: str) -> tuple[str, ...]:
    values = tuple(item.strip() for item in value.split(",") if item.strip())
    if not values:
        raise argparse.ArgumentTypeError("CSV value must contain at least one lock")
    return values


def normalize_lock(lock: str) -> str:
    normalized = experiment_defaults.normalize_lock(lock)
    if normalized == "mcstas":
        return "mcs-tas"
    return normalized


def resolve_requested_locks(
    baseline_root: Path,
    output_root: Path,
    lock_arg: str,
    matrix: BaselineMatrix,
) -> tuple[str, ...]:
    if lock_arg == "missing":
        return incomplete_or_missing_locks(output_root, missing_experiment_locks(baseline_root), matrix)

    locks = tuple(dict.fromkeys(normalize_lock(lock) for lock in parse_csv_strings(lock_arg)))
    unsupported = [lock for lock in locks if lock not in SUPPORTED_LOCKS]
    if unsupported:
        supported = ",".join(REQUIRED_BASELINE_LOCKS)
        raise ValueError(f"Unsupported lock keys: {','.join(unsupported)}. Supported: {supported}")
    return incomplete_or_missing_locks(output_root, locks, matrix)


def ensure_executable(path: Path, description: str) -> None:
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"{description} is not executable: {path}")


def ensure_mutex_bench(logger: CommandLogger) -> None:  # type: ignore[name-defined]
    logger.run(["make", "-C", str(MUTEXBENCH_DIR), "mutex_bench"], log_name="build_mutex_bench.log", timeout_seconds=0)
    ensure_executable(MUTEXBENCH_DIR / "mutex_bench", "mutexbench binary")


def ensure_flexguard_helpers(locks: Iterable[str], logger: CommandLogger) -> None:  # type: ignore[name-defined]
    for lock in locks:
        if lock in {"mcstp", "malthusian", "flexguard"}:
            experiment_one.ensure_flexguard_interpose(lock, logger)


def ensure_accordin_direct_library(logger: CommandLogger) -> None:  # type: ignore[name-defined]
    if not ACCORDIN_DIRECT_RELEASE_LIB.is_file():
        logger.run(
            ["cargo", "build", "-p", ACCORDIN_DIRECT_PACKAGE, "--release"],
            log_name=f"build_{ACCORDIN_DIRECT_PACKAGE}.log",
            timeout_seconds=0,
        )
    if not ACCORDIN_DIRECT_RELEASE_LIB.is_file():
        raise RuntimeError(f"{ACCORDIN_DIRECT_PACKAGE} library was not produced: {ACCORDIN_DIRECT_RELEASE_LIB}")


def common_sweep_args(matrix: BaselineMatrix, threads: tuple[int, ...] | None = None) -> list[str]:
    sweep_threads = threads if threads is not None else matrix.threads
    return [
        "--threads",
        csv_join(sweep_threads),
        "--critical-ns",
        csv_join(matrix.critical_ns),
        "--outside-ns",
        csv_join(matrix.outside_ns),
        "--duration-ms",
        str(matrix.duration_ms),
        "--warmup-duration-ms",
        str(matrix.warmup_duration_ms),
        "--repeats",
        str(matrix.repeats),
    ]


def accordin_direct_env(lock: str) -> dict[str, str | None]:
    env: dict[str, str | None] = {
        "ACCORDIN_CPU_MASK_K": None,
        "ACCORDIN_DISABLE_ADMISSION": None,
        "K": None,
        "MCS_TAS_ACCORDIN_DISABLE_BPF": None,
        ACCORDIN_DIRECT_DISABLE_BPF_ENV: None,
        ACCORDIN_DIRECT_STATS_ONLY_ENV: None,
    }
    for key, value in os.environ.items():
        if key.startswith(ACCORDIN_DIRECT_ENV_PREFIX):
            env[key] = value
    env[ACCORDIN_DIRECT_LIB_ENV] = str(ACCORDIN_DIRECT_RELEASE_LIB)
    if experiment_defaults.accordin_uses_sampling(lock):
        env["K"] = str(experiment_defaults.DEFAULT_ACCORDIN_CONCURRENCY)
    if experiment_defaults.accordin_disables_admission(lock):
        env["ACCORDIN_DISABLE_ADMISSION"] = "1"
        env[ACCORDIN_DIRECT_STATS_ONLY_ENV] = "1"
    return env


def accordin_sweep_command(root: Path, lock: str, matrix: BaselineMatrix, taskset_cpus: str) -> tuple[list[str], dict[str, str | None]]:
    lock_dir = root / lock
    cmd = [
        str(SWEEP_SINGLE),
        *common_sweep_args(matrix, runnable_threads_for_lock(lock, matrix)),
        "--lock-kind",
        ACCORDIN_DIRECT_LOCK_KIND,
        "--timeslice-extension",
        "off",
        "--output-raw",
        str(lock_dir / "raw.csv"),
        "--output-summary",
        str(lock_dir / "summary.csv"),
    ]
    if experiment_defaults.accordin_uses_taskset(lock):
        cmd = ["taskset", "-c", taskset_cpus, *cmd]
    return cmd, accordin_direct_env(lock)


def mcs_extension_command(root: Path, matrix: BaselineMatrix, mode: str) -> list[str]:
    lock_dir = root / "mcs_extension"
    return [
        str(SWEEP_SINGLE),
        *common_sweep_args(matrix, runnable_threads_for_lock("mcs_extension", matrix)),
        "--lock-kind",
        "mcs",
        "--timeslice-extension",
        mode,
        "--output-raw",
        str(lock_dir / "raw.csv"),
        "--output-summary",
        str(lock_dir / "summary.csv"),
    ]


def multi_lock_command(
    root: Path,
    locks: tuple[str, ...],
    matrix: BaselineMatrix,
    sudo_mode: str,
    threads: tuple[int, ...],
) -> list[str]:
    return [
        str(SWEEP_MULTI),
        "--locks",
        csv_join(locks),
        "--output-root",
        str(root),
        "--sudo-mode",
        sudo_mode,
        "--timeslice-extension",
        "off",
        "--",
        *common_sweep_args(matrix, threads),
    ]


def command_text(cmd: list[str], env: dict[str, str | None] | None = None) -> str:
    if not env:
        return " ".join(cmd)
    env_tokens = [f"{key}={value}" for key, value in sorted(env.items()) if value is not None]
    return " ".join(["env", *env_tokens, *cmd])


def run_command(
    logger: CommandLogger,  # type: ignore[name-defined]
    cmd: list[str],
    *,
    log_name: str,
    dry_run: bool,
    env: dict[str, str | None] | None = None,
    sudo_env: bool = False,
) -> None:
    if sudo_env:
        cmd, env = with_sudo_env(cmd, env)  # type: ignore[name-defined]
    if dry_run:
        print(command_text(cmd, env))
        return
    logger.run(cmd, log_name=log_name, env=env)


def prepare_target_dirs(root: Path, locks: tuple[str, ...], *, force: bool) -> None:
    for lock in locks:
        path = root / lock
        if path.exists() and force:
            shutil.rmtree(path)
        elif (path / "raw.csv").exists():
            raise RuntimeError(f"Target lock already has raw.csv; use --force to replace it: {path}")


def write_settings(
    root: Path,
    baseline_root: Path,
    locks: tuple[str, ...],
    matrix: BaselineMatrix,
    args: argparse.Namespace,
) -> None:
    settings = {
        "experiment": "experiment3_mutexbench_baseline_supplement",
        "output_root": str(root),
        "baseline_root": str(baseline_root),
        "locks": list(locks),
        "baseline_required_locks": list(REQUIRED_BASELINE_LOCKS),
        "baseline_present_locks": list(present_lock_names(baseline_root)),
        "output_present_locks_before_run": list(present_lock_names(root)),
        "threads": list(matrix.threads),
        "runnable_threads_by_lock": {
            lock: list(runnable_threads_for_lock(lock, matrix))
            for lock in locks
        },
        "critical_ns": list(matrix.critical_ns),
        "outside_ns": list(matrix.outside_ns),
        "duration_ms": matrix.duration_ms,
        "warmup_duration_ms": matrix.warmup_duration_ms,
        "repeats": matrix.repeats,
        "mcs_extension_mode": args.mcs_extension_mode,
        "sudo_mode": args.sudo_mode,
        "mcs_accordin_taskset_cpus": args.mcs_accordin_taskset_cpus,
    }
    with (root / "experiment3_baseline_supplement_settings.json").open("w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")


def run_plots(root: Path, logger: CommandLogger, *, dry_run: bool) -> None:  # type: ignore[name-defined]
    plot_cmd = [
        "python3",
        str(MUTEXBENCH_DIR / "scripts" / "batch_plot_all_out.py"),
        "--data",
        str(root),
        "--jobs",
        "1",
    ]
    run_command(logger, plot_cmd, log_name="plot_results_baseline.log", dry_run=dry_run)


def lock_thread_groups(locks: tuple[str, ...], matrix: BaselineMatrix) -> list[tuple[tuple[str, ...], tuple[int, ...]]]:
    groups: list[tuple[list[str], tuple[int, ...]]] = []
    for lock in locks:
        threads = runnable_threads_for_lock(lock, matrix)
        for group_locks, group_threads in groups:
            if group_threads == threads:
                group_locks.append(lock)
                break
        else:
            groups.append(([lock], threads))
    return [(tuple(group_locks), threads) for group_locks, threads in groups]


def run_baseline_supplement(
    root: Path,
    baseline_root: Path,
    locks: tuple[str, ...],
    matrix: BaselineMatrix,
    args: argparse.Namespace,
) -> None:
    logger = CommandLogger(root, command_timeout_seconds=args.command_timeout_seconds)  # type: ignore[name-defined]
    ensure_executable(SWEEP_MULTI, "multi-lock sweep script")
    ensure_executable(SWEEP_SINGLE, "single-lock sweep script")
    if not args.dry_run:
        ensure_mutex_bench(logger)
        prepare_target_dirs(root, locks, force=args.force)
        write_settings(root, baseline_root, locks, matrix, args)

    multi_locks = tuple(lock for lock in locks if lock in MULTI_LOCK_SWEEP_LOCKS)
    for group_locks, group_threads in lock_thread_groups(multi_locks, matrix):
        if not args.dry_run:
            ensure_flexguard_helpers(group_locks, logger)
        run_command(
            logger,
            multi_lock_command(root, group_locks, matrix, args.sudo_mode, group_threads),
            log_name=f"sweep_{'_'.join(group_locks)}.log",
            dry_run=args.dry_run,
            env={"FLEXGUARD_DIR": str(experiment_one.FLEXGUARD_DIR)},
        )

    if "mcs_extension" in locks:
        run_command(
            logger,
            mcs_extension_command(root, matrix, args.mcs_extension_mode),
            log_name="sweep_mcs_extension.log",
            dry_run=args.dry_run,
        )

    accordin_locks = tuple(lock for lock in locks if lock in ACCORDIN_LOCKS)
    if accordin_locks and not args.dry_run:
        ensure_accordin_direct_library(logger)
    for lock in accordin_locks:
        cmd, env = accordin_sweep_command(root, lock, matrix, args.mcs_accordin_taskset_cpus)
        run_command(logger, cmd, log_name=f"sweep_{lock}.log", dry_run=args.dry_run, env=env, sudo_env=True)

    if not args.skip_plots:
        run_plots(root, logger, dry_run=args.dry_run)


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


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Supplement bench/mutexbench/results_baseline with experiment-one locks missing from the baseline matrix.",
    )
    parser.add_argument(
        "--baseline-root",
        type=Path,
        default=DEFAULT_BASELINE_ROOT,
        help=f"Existing mutexbench baseline root used to infer missing locks and non-thread matrix. Default: {DEFAULT_BASELINE_ROOT}.",
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        default=DEFAULT_OUTPUT_ROOT,
        help=f"Experiment result root for supplemental outputs. Default: {DEFAULT_OUTPUT_ROOT}.",
    )
    parser.add_argument(
        "--locks",
        default="missing",
        help=(
            "Comma-separated locks to run, or 'missing' to infer missing/incomplete experiment locks. "
            f"Default: missing. Supported: {','.join(REQUIRED_BASELINE_LOCKS)}."
        ),
    )
    parser.add_argument("--duration-ms", type=positive_int, default=None, help="Override inferred hot duration.")
    parser.add_argument(
        "--threads",
        type=parsec_common.parse_csv_ints,
        default=experiment_defaults.DEFAULT_THREADS,
        help=(
            "Thread counts for supplemental runs. "
            f"Default: experiments DEFAULT_THREADS={csv_join(experiment_defaults.DEFAULT_THREADS)}."
        ),
    )
    parser.add_argument(
        "--warmup-duration-ms",
        type=non_negative_int,
        default=DEFAULT_WARMUP_DURATION_MS,
        help=f"Warmup duration for supplemental runs. Default: {DEFAULT_WARMUP_DURATION_MS}.",
    )
    parser.add_argument(
        "--mcs-extension-mode",
        choices=("require", "auto", "off"),
        default="require",
        help="timeslice-extension mode for mcs_extension. Default: require.",
    )
    parser.add_argument(
        "--sudo-mode",
        choices=("auto", "all", "none"),
        default="auto",
        help="Sudo policy forwarded to multi-lock sweep. Default: auto.",
    )
    parser.add_argument(
        "--mcs-accordin-taskset-cpus",
        default=experiment_defaults.DEFAULT_MCS_ACCORDIN_TASKSET_CPUS,
        help=f"CPU list for {experiment_defaults.ACCORDIN_TASKSET_LOCK}.",
    )
    parser.add_argument(
        "--command-timeout-seconds",
        type=non_negative_int,
        default=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        help=f"Outer timeout per sweep command; 0 disables it. Default: {DEFAULT_COMMAND_TIMEOUT_SECONDS}.",
    )
    parser.add_argument("--force", action="store_true", help="Replace existing target lock directories.")
    parser.add_argument("--dry-run", action="store_true", help="Print commands without executing benchmark sweeps.")
    parser.add_argument("--skip-plots", action="store_true", help="Do not regenerate results_baseline plots after sweeps.")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        baseline_root = parsec_common.resolve_path(args.baseline_root)
        root = parsec_common.resolve_path(args.output_root)
        matrix = infer_baseline_matrix(baseline_root, warmup_duration_ms=args.warmup_duration_ms)
        matrix = matrix_with_threads(matrix, args.threads)
        if args.duration_ms is not None:
            matrix = BaselineMatrix(
                threads=matrix.threads,
                critical_ns=matrix.critical_ns,
                outside_ns=matrix.outside_ns,
                repeats=matrix.repeats,
                duration_ms=args.duration_ms,
                warmup_duration_ms=matrix.warmup_duration_ms,
            )
        locks = resolve_requested_locks(baseline_root, root, args.locks, matrix)
        if not locks:
            print(f"No missing or incomplete experiment locks under {root}.")
            return 0
        print(f"Baseline root: {baseline_root}")
        print(f"Result root: {root}")
        print(f"Supplement locks: {csv_join(locks)}")
        print(
            "Matrix: "
            f"threads={csv_join(matrix.threads)} "
            f"critical={csv_join(matrix.critical_ns)} "
            f"outside={csv_join(matrix.outside_ns)} "
            f"duration_ms={matrix.duration_ms} "
            f"warmup_ms={matrix.warmup_duration_ms} "
            f"repeats={matrix.repeats}"
        )
        run_baseline_supplement(root, baseline_root, locks, matrix, args)
        incomplete = () if args.dry_run else incomplete_or_missing_locks(root, locks, matrix)
        if incomplete:
            print(f"Incomplete locks after run: {csv_join(incomplete)}", file=sys.stderr)
            return 1
        return 0
    except CommandError as exc:  # type: ignore[name-defined]
        print(str(exc), file=sys.stderr)
        print(f"Command log: {exc.log_path}", file=sys.stderr)
        return exc.returncode
    except Exception as exc:
        print(str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
