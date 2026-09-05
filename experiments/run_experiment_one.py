#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import os
import shlex
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


REPO_ROOT = Path(__file__).resolve().parents[1]
MUTEXBENCH_DIR = REPO_ROOT / "bench" / "mutexbench"
FLEXGUARD_DIR = REPO_ROOT / "bench" / "flexguard"
SWEEP_MULTI = MUTEXBENCH_DIR / "scripts" / "sweep_mutex_throughput_multi_lock.sh"
SWEEP_SINGLE = MUTEXBENCH_DIR / "scripts" / "sweep_mutex_throughput.sh"
SCHEMA_DIR = MUTEXBENCH_DIR / "scripts"

sys.path.insert(0, str(SCHEMA_DIR))
from bench_csv_schema import (  # noqa: E402
    CPU_FIELD,
    HANDOFF_FIELD,
    LATENCY_PLOT_REQUIRED_FIELDS,
    THROUGHPUT_FIELD,
    WAIT_FIELD,
    load_plot_rows,
)

import experiment_defaults  # noqa: E402
import experiment_failures  # noqa: E402
import run_experiment_three_common as experiment_three  # noqa: E402
from machine_config import (  # noqa: E402
    DEFAULT_MCS_ACCORDIN_TASKSET_CPUS,
)

MACHINE_CORE_COUNT = experiment_defaults.MACHINE_CORE_COUNT
ACTIVE_MACHINE_CONFIG = experiment_defaults.ACTIVE_MACHINE_CONFIG
PROFILE_ENV = experiment_defaults.PROFILE_ENV
THREADS = experiment_defaults.DEFAULT_THREADS
DEFAULT_LOCK_PROFILE = experiment_defaults.DEFAULT_LOCK_PROFILE
EXPERIMENT_ONE_MINIMAL_LOCKS = experiment_defaults.EXPERIMENT_ONE_MINIMAL_LOCKS
EXPERIMENT_ONE_FULL_LOCKS = experiment_defaults.EXPERIMENT_ONE_FULL_LOCKS
PLOT_THREADS = tuple(thread for thread in THREADS if thread >= 4)
HOLD_TIME_MAX_THREADS = 96
CRITICAL_NS = experiment_defaults.MUTEXBENCH_DEFAULT_CRITICAL_NS
OUTSIDE_NS = experiment_defaults.MUTEXBENCH_DEFAULT_OUTSIDE_NS
DURATION_MS = experiment_defaults.MUTEXBENCH_DEFAULT_DURATION_MS
WARMUP_DURATION_MS = experiment_defaults.MUTEXBENCH_DEFAULT_WARMUP_DURATION_MS
REPEATS = experiment_defaults.MUTEXBENCH_DEFAULT_REPEATS
DEFAULT_COMMAND_TIMEOUT_SECONDS = 21600
COMMAND_TIMEOUT_KILL_AFTER_SECONDS = 60
ACCORDIN_TASKSET_LOCK = experiment_defaults.ACCORDIN_TASKSET_LOCK
ACCORDIN_DIRECT_PACKAGE = "mcs_tas_accordin_direct"
ACCORDIN_DIRECT_LOCK_KIND = "mcs_tas_accordin_direct"
ACCORDIN_DIRECT_RELEASE_LIB = REPO_ROOT / "target" / "release" / "libmcs_tas_accordin_direct.so"
ACCORDIN_DIRECT_LIB_ENV = "MCS_TAS_ACCORDIN_DIRECT_LIB"
ACCORDIN_DIRECT_DISABLE_BPF_ENV = "MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF"
ACCORDIN_DIRECT_STATS_ONLY_ENV = "MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY"
ACCORDIN_DIRECT_ENV_PREFIX = "MCS_TAS_ACCORDIN_DIRECT_"
ACCORDIN_PRELOAD_LOCKS = experiment_defaults.EXPERIMENT_ONE_ACCORDIN_LOCKS
FOCUS_LOCK_KEYS = experiment_defaults.EXPERIMENT_ONE_FOCUS_LOCKS
PIECEWISE_Y_THRESHOLD_NS = 1000.0
PIECEWISE_Y_LINEAR_SCALE = 3.0
BROKEN_Y_NORMAL = (1e2, 1e4)
BROKEN_Y_UPPER_MIN = 1e5
BROKEN_LOWER_AXIS_PADDING = 1.2
THREAD_AXIS_MIN = PLOT_THREADS[0] / 1.08
THREAD_AXIS_MAX = PLOT_THREADS[-1] * 1.25
PLOT_TITLE_FONTSIZE = 16
PLOT_LABEL_FONTSIZE = 14
PLOT_TICK_FONTSIZE = 12
PLOT_LEGEND_FONTSIZE = 11
PLOT_ANNOTATION_FONTSIZE = 11


@dataclass(frozen=True)
class LockSpec:
    label: str
    key: str
    optional: bool = False
    result_dirs: tuple[str, ...] = ()

    def result_dir_names(self) -> tuple[str, ...]:
        if self.result_dirs:
            return self.result_dirs
        return (self.key,)


@dataclass(frozen=True)
class FlexguardInterposeBuildSpec:
    make_target: str | None = None
    make_vars: tuple[str, ...] = ()
    clean_first: bool = False


LOCKS = tuple(
    LockSpec(config.label, config.key, config.optional, config.result_dirs)
    for config in experiment_defaults.EXPERIMENT_ONE_LOCKS
)

BASE_LOCK_KEYS = experiment_defaults.EXPERIMENT_ONE_BASE_LOCKS
SINGLE_OVERSUBSCRIBED_LOCK_KEYS = experiment_defaults.SINGLE_OVERSUBSCRIBED_LOCKS
FLEXGUARD_INTERPOSE_KEYS = ("mcstp", "malthusian", "flexguard")
FLEXGUARD_INTERPOSE_BUILD_SPECS = {
    "flexguard": FlexguardInterposeBuildSpec(make_target="build/interpose_flexguard.sh"),
    "mcstp": FlexguardInterposeBuildSpec(
        make_vars=("LOCK_VERSION=MCSTP", "ADD_PADDING=1", "USE_REAL_PTHREAD=1"),
        clean_first=True,
    ),
    "malthusian": FlexguardInterposeBuildSpec(
        make_vars=("LOCK_VERSION=MALTHUSIAN", "ADD_PADDING=1", "USE_REAL_PTHREAD=1"),
        clean_first=True,
    ),
}
LOCKS_BY_KEY = {lock.key: lock for lock in LOCKS}
SUPPLEMENT_DEFAULT_LOCK_KEYS = experiment_defaults.EXPERIMENT_ONE_SUPPLEMENT_DEFAULT_LOCKS

COMBINED_FIELDS = (
    "lock_label",
    "lock_key",
    "threads",
    "critical_ns",
    "outside_ns",
    "repeats",
    "avg_lock_hold_ns",
    HANDOFF_FIELD,
    "avg_hold_plus_handoff_ns",
    THROUGHPUT_FIELD,
    CPU_FIELD,
    WAIT_FIELD,
    "elapsed_seconds",
    "total_operations",
    "lock_hold_samples",
)


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
        command_timeout_seconds: int = DEFAULT_COMMAND_TIMEOUT_SECONDS,
    ) -> None:
        self.result_root = result_root
        self.log_dir = result_root / "logs"
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.manifest_path = result_root / "commands.json"
        self.records: list[dict[str, object]] = self.load_existing_records()
        self.command_timeout_seconds = command_timeout_seconds

    def load_existing_records(self) -> list[dict[str, object]]:
        if not self.manifest_path.is_file():
            return []
        with self.manifest_path.open("r", encoding="utf-8") as f:
            records = json.load(f)
        if not isinstance(records, list) or not all(isinstance(record, dict) for record in records):
            raise RuntimeError(f"Existing command manifest must be a JSON list of objects: {self.manifest_path}")
        return list(records)

    def resolve_log_path(self, log_name: str) -> Path:
        log_path = self.log_dir / log_name
        if not log_path.exists():
            return log_path

        stem = log_path.stem
        suffix = log_path.suffix
        index = 1
        while True:
            candidate = self.log_dir / f"{stem}_{index}{suffix}"
            if not candidate.exists():
                return candidate
            index += 1

    def run(
        self,
        cmd: list[str],
        *,
        log_name: str,
        cwd: Path = REPO_ROOT,
        env: dict[str, str] | None = None,
        timeout_seconds: int | None = None,
    ) -> None:
        effective_timeout = self.command_timeout_seconds if timeout_seconds is None else timeout_seconds
        run_cmd = wrap_command_timeout(cmd, effective_timeout)
        log_path = self.resolve_log_path(log_name)
        started_at = dt.datetime.now(dt.timezone.utc)
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
            run_env.update(env)

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
                log_file.write(line)
                log_file.flush()
                print(line, end="", flush=True)
            returncode = process.wait()

            finished_at = dt.datetime.now(dt.timezone.utc)
            log_file.write(f"\nfinished_at: {finished_at.isoformat()}\n")
            log_file.write(f"returncode: {returncode}\n")

        record["finished_at"] = finished_at.isoformat()
        record["returncode"] = returncode
        self.records.append(record)
        self.write_manifest()
        if returncode != 0:
            raise CommandError(
                f"Command failed with exit code {returncode}: {shlex.join(run_cmd)}",
                returncode,
                log_path,
            )

    def write_manifest(self) -> None:
        with self.manifest_path.open("w", encoding="utf-8") as f:
            json.dump(self.records, f, indent=2)
            f.write("\n")


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("value must be non-negative")
    return parsed


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be positive")
    return parsed


def parse_csv_ints(value: str) -> tuple[int, ...]:
    items = tuple(int(item.strip()) for item in value.split(",") if item.strip())
    if not items:
        raise argparse.ArgumentTypeError("CSV value must contain at least one integer")
    if any(item <= 0 for item in items):
        raise argparse.ArgumentTypeError("thread counts must be positive")
    return items


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run or plot the mutexbench experiment-one sweep.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"""\
Default benchmark settings:
  lock-profile={DEFAULT_LOCK_PROFILE}
  minimal locks={','.join(EXPERIMENT_ONE_MINIMAL_LOCKS)}
  full locks={','.join(EXPERIMENT_ONE_FULL_LOCKS)}
  critical-ns={CRITICAL_NS}, outside-ns={OUTSIDE_NS}, duration=5s, warmup=1s, repeats={REPEATS}
  machine-profile={ACTIVE_MACHINE_CONFIG.name} (override with {PROFILE_ENV})
  threads={','.join(str(v) for v in THREADS)}
  single-overload locks={','.join(SINGLE_OVERSUBSCRIBED_LOCK_KEYS)} use threads={','.join(str(v) for v in runnable_threads_for_lock("mcs"))}

Examples:
  python3 experiments/run_experiment_one.py
  python3 experiments/run_experiment_one.py --output-root experiments/results/experiment1_manual
  python3 experiments/run_experiment_one.py --threads 128 --repeats 1
  {PROFILE_ENV}=original python3 experiments/run_experiment_one.py
  python3 experiments/run_experiment_one.py --plot-only experiments/results/experiment1_manual
  python3 experiments/run_experiment_one.py --output-root experiments/results/experiment1_20260423_194548 --supplement-locks
""",
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        default=None,
        help="Directory for a new run or supplement target root. Default: experiments/results/experiment1_<timestamp>.",
    )
    parser.add_argument(
        "--plot-only",
        type=Path,
        default=None,
        metavar="RESULT_ROOT",
        help="Skip benchmark execution and regenerate combined CSV and PNGs from RESULT_ROOT.",
    )
    parser.add_argument(
        "--lock-profile",
        choices=experiment_defaults.experiment_one_lock_profile_names(),
        default=DEFAULT_LOCK_PROFILE,
        help=(
            "Named experiment-one lock set. "
            f"Default: {DEFAULT_LOCK_PROFILE}. "
            f"minimal={','.join(EXPERIMENT_ONE_MINIMAL_LOCKS)}; "
            f"full={','.join(EXPERIMENT_ONE_FULL_LOCKS)}."
        ),
    )
    parser.add_argument(
        "--supplement-locks",
        nargs="?",
        const=",".join(SUPPLEMENT_DEFAULT_LOCK_KEYS),
        default=None,
        metavar="LOCKS",
        help=(
            "Run only the selected base-sweep locks inside --output-root, then regenerate combined CSV and PNGs. "
            f"Default when the flag is provided without a value: {','.join(SUPPLEMENT_DEFAULT_LOCK_KEYS)}. "
            f"Supported locks: {','.join(BASE_LOCK_KEYS)}. "
            "Other experiment-one lock results must already exist under --output-root for plotting. "
            "Existing target lock results cause an error unless --force is used, and non-target lock directories are left untouched."
        ),
    )
    parser.add_argument(
        "--mcs-extension-mode",
        choices=("require", "auto", "off"),
        default="require",
        help="timeslice-extension mode for the native MCS extension curve. Default: require.",
    )
    parser.add_argument(
        "--sudo-mode",
        choices=("auto", "all", "none"),
        default="auto",
        help="Sudo policy forwarded to the multi-lock sweep. Default: auto.",
    )
    parser.add_argument(
        "--threads",
        type=parse_csv_ints,
        default=THREADS,
        metavar="CSV",
        help=f"Comma-separated thread counts. Default: {','.join(str(v) for v in THREADS)}.",
    )
    parser.add_argument(
        "--repeats",
        type=positive_int,
        default=REPEATS,
        help=f"Number of repeats per point. Default: {REPEATS}.",
    )
    parser.add_argument(
        "--mcs-accordin-taskset-cpus",
        default=DEFAULT_MCS_ACCORDIN_TASKSET_CPUS,
        metavar="CPU_LIST",
        help=(
            f"CPU list passed to taskset if the legacy {experiment_defaults.ACCORDIN_TASKSET_LOCK} series is re-enabled. "
            f"Default: {DEFAULT_MCS_ACCORDIN_TASKSET_CPUS}."
        ),
    )
    parser.add_argument(
        "--skip-mcs-accordin-taskset",
        action="store_true",
        help=(
            f"Skip only the legacy taskset accordin ({experiment_defaults.ACCORDIN_TASKSET_LOCK}) "
            "series if it is re-enabled."
        ),
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Allow benchmark output into an existing non-empty output root, and in supplement mode allow overwriting target lock results.",
    )
    parser.add_argument(
        "--command-timeout-seconds",
        type=non_negative_int,
        default=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        help=(
            "Outer timeout for each sweep command. 0 disables it. "
            f"Default: {DEFAULT_COMMAND_TIMEOUT_SECONDS}."
        ),
    )
    return parser.parse_args()


def resolve_path(path: Path) -> Path:
    return path.expanduser().resolve()


def default_result_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment1_{timestamp}"


def parse_supplement_lock_keys(value: str) -> tuple[str, ...]:
    requested_keys = [item.strip().lower() for item in value.split(",") if item.strip()]
    if not requested_keys:
        raise ValueError("--supplement-locks requires at least one lock key.")

    invalid_keys = [key for key in requested_keys if key not in BASE_LOCK_KEYS]
    if invalid_keys:
        supported = ",".join(BASE_LOCK_KEYS)
        invalid = ",".join(invalid_keys)
        raise ValueError(f"--supplement-locks only supports base-sweep locks ({supported}). Invalid: {invalid}")

    unique_keys: list[str] = []
    seen: set[str] = set()
    for key in requested_keys:
        if key in seen:
            continue
        seen.add(key)
        unique_keys.append(key)
    return tuple(unique_keys)


def selected_lock_keys_for_profile(profile: str) -> tuple[str, ...]:
    return experiment_defaults.experiment_one_lock_profile_locks(profile)


def lock_specs_for_keys(lock_keys: Iterable[str]) -> tuple[LockSpec, ...]:
    selected = set(lock_keys)
    unknown = sorted(selected - set(LOCKS_BY_KEY))
    if unknown:
        raise ValueError(f"Unsupported experiment-one lock keys: {','.join(unknown)}")
    return tuple(lock for lock in LOCKS if lock.key in selected)


def runnable_threads_for_lock(lock_key: str, threads: tuple[int, ...] = THREADS) -> tuple[int, ...]:
    return experiment_defaults.runnable_threads_for_lock(lock_key, threads)


def write_settings(
    result_root: Path,
    mcs_extension_mode: str,
    sudo_mode: str,
    mcs_accordin_taskset_enabled: bool,
    mcs_accordin_taskset_cpus: str,
    lock_profile: str,
    selected_lock_keys: tuple[str, ...],
    command_timeout_seconds: int,
    threads: tuple[int, ...],
    repeats: int,
) -> None:
    selected_locks = lock_specs_for_keys(selected_lock_keys)
    settings = {
        "threads": list(threads),
        "lock_profile": lock_profile,
        "lock_profile_source": "profile",
        "selected_locks": [{"label": lock.label, "key": lock.key} for lock in selected_locks],
        "machine_profile": ACTIVE_MACHINE_CONFIG.name,
        "machine_profile_env": PROFILE_ENV,
        "critical_ns": CRITICAL_NS,
        "outside_ns": OUTSIDE_NS,
        "duration_ms": DURATION_MS,
        "warmup_duration_ms": WARMUP_DURATION_MS,
        "repeats": repeats,
        "command_timeout_seconds": command_timeout_seconds,
        "single_oversubscribed_locks": list(SINGLE_OVERSUBSCRIBED_LOCK_KEYS),
        "runnable_threads_by_lock": {
            lock.key: list(runnable_threads_for_lock(lock.key, threads))
            for lock in selected_locks
        },
        "mcs_extension_mode": mcs_extension_mode,
        "sudo_mode": sudo_mode,
        "accordin_taskset_lock": ACCORDIN_TASKSET_LOCK,
        "mcs_accordin_taskset_enabled": mcs_accordin_taskset_enabled,
        "mcs_accordin_taskset_cpus": mcs_accordin_taskset_cpus,
        "accordin_direct_lock_kind": ACCORDIN_DIRECT_LOCK_KIND,
        "accordin_direct_library": str(ACCORDIN_DIRECT_RELEASE_LIB),
        "locks": [{"label": lock.label, "key": lock.key} for lock in selected_locks],
        "flexguard_dir": str(FLEXGUARD_DIR),
    }
    with (result_root / "settings.json").open("w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")


def write_settings_if_missing(
    result_root: Path,
    mcs_extension_mode: str,
    sudo_mode: str,
    mcs_accordin_taskset_enabled: bool,
    mcs_accordin_taskset_cpus: str,
    lock_profile: str,
    selected_lock_keys: tuple[str, ...],
    command_timeout_seconds: int,
    threads: tuple[int, ...],
    repeats: int,
) -> None:
    settings_path = result_root / "settings.json"
    if settings_path.exists():
        if not settings_path.is_file():
            raise RuntimeError(f"Settings path exists but is not a file: {settings_path}")
        return
    write_settings(
        result_root,
        mcs_extension_mode,
        sudo_mode,
        mcs_accordin_taskset_enabled,
        mcs_accordin_taskset_cpus,
        lock_profile,
        selected_lock_keys,
        command_timeout_seconds,
        threads,
        repeats,
    )


def ensure_executable(path: Path, description: str) -> None:
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"{description} is not executable: {path}")


def ensure_mutex_bench(logger: CommandLogger) -> None:
    binary = MUTEXBENCH_DIR / "mutex_bench"
    logger.run(
        ["make", "-C", str(MUTEXBENCH_DIR), "mutex_bench"],
        log_name="build_mutex_bench.log",
        timeout_seconds=0,
    )
    ensure_executable(binary, "mutexbench binary")


def flexguard_interpose_path(key: str) -> Path:
    return FLEXGUARD_DIR / "build" / f"interpose_{key}.sh"


def flexguard_interpose_library_path(key: str) -> Path:
    return FLEXGUARD_DIR / "build" / f"interpose_{key}.so"


def ensure_flexguard_interpose(key: str, logger: CommandLogger) -> None:
    script = flexguard_interpose_path(key)
    library = flexguard_interpose_library_path(key)
    if script.is_file() and os.access(script, os.X_OK) and library.is_file():
        return

    spec = FLEXGUARD_INTERPOSE_BUILD_SPECS.get(
        key,
        FlexguardInterposeBuildSpec(make_target=f"build/interpose_{key}.sh"),
    )
    if spec.make_target is not None:
        try:
            logger.run(
                ["make", "-C", str(FLEXGUARD_DIR), spec.make_target],
                log_name=f"build_flexguard_{key}.log",
                timeout_seconds=0,
            )
        except CommandError as exc:
            raise RuntimeError(
                f"Required bench/flexguard interpose helper cannot be built: {script}. "
                f"The attempted Makefile target was {spec.make_target}; see {exc.log_path}."
            ) from exc
    else:
        if spec.clean_first:
            try:
                logger.run(
                    ["make", "-C", str(FLEXGUARD_DIR), "clean"],
                    log_name=f"build_flexguard_{key}_clean.log",
                    timeout_seconds=0,
                )
            except CommandError as exc:
                raise RuntimeError(
                    f"Required bench/flexguard interpose helper could not be cleaned before rebuild: {script}. "
                    f"See {exc.log_path}."
                ) from exc

        build_cmd = ["make", "-C", str(FLEXGUARD_DIR), *spec.make_vars, "interpose.so", "interpose.sh"]
        try:
            logger.run(build_cmd, log_name=f"build_flexguard_{key}.log", timeout_seconds=0)
        except CommandError as exc:
            raise RuntimeError(
                f"Required bench/flexguard interpose helper cannot be built: {script}. "
                f"The attempted build command was {shlex.join(build_cmd)}; see {exc.log_path}."
            ) from exc

        source_script = FLEXGUARD_DIR / "interpose.sh"
        source_library = FLEXGUARD_DIR / "interpose.so"
        if not source_script.is_file() or not os.access(source_script, os.X_OK):
            raise RuntimeError(
                f"Required bench/flexguard helper script was not produced as an executable file: {source_script}"
            )
        if not source_library.is_file():
            raise RuntimeError(
                f"Required bench/flexguard helper library was not produced: {source_library}"
            )
        library.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source_script, script)
        shutil.copy2(source_library, library)

    if not script.is_file() or not os.access(script, os.X_OK):
        raise RuntimeError(
            f"Required bench/flexguard script was not produced as an executable file: {script}"
        )
    if not library.is_file():
        raise RuntimeError(f"Required bench/flexguard library was not produced: {library}")


def ensure_inputs(
    logger: CommandLogger,
    *,
    lock_keys: Iterable[str] = BASE_LOCK_KEYS,
    include_single_sweep: bool = True,
) -> None:
    ensure_executable(SWEEP_MULTI, "multi-lock sweep script")
    if include_single_sweep:
        ensure_executable(SWEEP_SINGLE, "single-lock sweep script")
    ensure_mutex_bench(logger)
    requested_lock_keys = set(lock_keys)
    for key in FLEXGUARD_INTERPOSE_KEYS:
        if key not in requested_lock_keys:
            continue
        ensure_flexguard_interpose(key, logger)


def common_sweep_args(args: argparse.Namespace, threads: tuple[int, ...]) -> list[str]:
    return [
        "--threads",
        ",".join(str(v) for v in threads),
        "--critical-ns",
        str(CRITICAL_NS),
        "--outside-ns",
        str(OUTSIDE_NS),
        "--duration-ms",
        str(DURATION_MS),
        "--warmup-duration-ms",
        str(WARMUP_DURATION_MS),
        "--repeats",
        str(args.repeats),
    ]


def lock_thread_groups(
    lock_keys: tuple[str, ...],
    requested_threads: tuple[int, ...],
) -> list[tuple[tuple[str, ...], tuple[int, ...]]]:
    groups: list[tuple[list[str], tuple[int, ...]]] = []
    for lock_key in lock_keys:
        threads = runnable_threads_for_lock(lock_key, requested_threads)
        for group_lock_keys, group_threads in groups:
            if group_threads == threads:
                group_lock_keys.append(lock_key)
                break
        else:
            groups.append(([lock_key], threads))
    return [(tuple(group_lock_keys), group_threads) for group_lock_keys, group_threads in groups]


def thread_group_log_suffix(threads: tuple[int, ...]) -> str:
    if threads == THREADS:
        return "full"
    if threads == (128,):
        return "128"
    return "single_oversubscribed"


def run_multi_lock_sweeps(
    result_root: Path,
    args: argparse.Namespace,
    lock_keys: tuple[str, ...],
    logger: CommandLogger,
    failures: list[dict[str, str]],
    *,
    log_prefix: str,
) -> None:
    env = {"FLEXGUARD_DIR": str(FLEXGUARD_DIR)}
    groups = lock_thread_groups(lock_keys, args.threads)
    for group_lock_keys, threads in groups:
        sweep_cmd = [
            str(SWEEP_MULTI),
            "--locks",
            ",".join(group_lock_keys),
            "--output-root",
            str(result_root),
            "--sudo-mode",
            args.sudo_mode,
            "--timeslice-extension",
            "off",
            "--",
            *common_sweep_args(args, threads),
        ]
        if len(groups) == 1:
            log_name = f"{log_prefix}.log"
        else:
            log_name = f"{log_prefix}_{thread_group_log_suffix(threads)}.log"
        try:
            logger.run(sweep_cmd, log_name=log_name, env=env)
        except CommandError as exc:
            experiment_failures.append_command_failure(
                failures,
                result_root=result_root,
                experiment="experiment1",
                workload="mutexbench",
                benchmark="sweep",
                lock=",".join(group_lock_keys),
                threads=",".join(str(thread) for thread in threads),
                repeat="all",
                exc=exc,
            )
            experiment_failures.write_failures_csv(result_root, failures)
            continue


def ensure_accordin_direct_library(logger: CommandLogger) -> None:
    if not ACCORDIN_DIRECT_RELEASE_LIB.is_file():
        logger.run(
            ["make", ACCORDIN_DIRECT_PACKAGE],
            log_name=f"build_{ACCORDIN_DIRECT_PACKAGE}.log",
            timeout_seconds=0,
        )

    if not ACCORDIN_DIRECT_RELEASE_LIB.is_file():
        raise RuntimeError(f"{ACCORDIN_DIRECT_PACKAGE} library was not produced: {ACCORDIN_DIRECT_RELEASE_LIB}")


def accordin_direct_sweep_env(lock: str) -> dict[str, str | None]:
    env: dict[str, str | None] = {
        "ACCORDIN_DISABLE_ADMISSION": None,
        "MCS_TAS_ACCORDIN_DISABLE_BPF": None,
        ACCORDIN_DIRECT_DISABLE_BPF_ENV: None,
        ACCORDIN_DIRECT_STATS_ONLY_ENV: None,
    }
    for key, value in os.environ.items():
        if key.startswith(ACCORDIN_DIRECT_ENV_PREFIX):
            env[key] = value
    env[ACCORDIN_DIRECT_LIB_ENV] = str(ACCORDIN_DIRECT_RELEASE_LIB)
    env[ACCORDIN_DIRECT_DISABLE_BPF_ENV] = None
    if experiment_defaults.accordin_disables_admission(lock):
        env["ACCORDIN_DISABLE_ADMISSION"] = "1"
        env[ACCORDIN_DIRECT_STATS_ONLY_ENV] = "1"
    return env


def accordin_sweep_command(
    *,
    lock: str,
    result_root: Path,
    args: argparse.Namespace,
) -> tuple[list[str], dict[str, str | None]]:
    lock_dir = result_root / lock
    lock_dir.mkdir(parents=True, exist_ok=True)
    cmd = [
        str(SWEEP_SINGLE),
        *common_sweep_args(args, runnable_threads_for_lock(lock, args.threads)),
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
        cmd = ["taskset", "-c", args.mcs_accordin_taskset_cpus, *cmd]
    return cmd, accordin_direct_sweep_env(lock)


def run_accordin_sweeps(
    result_root: Path,
    args: argparse.Namespace,
    lock_keys: tuple[str, ...],
    logger: CommandLogger,
    failures: list[dict[str, str]],
) -> None:
    if not lock_keys:
        return

    ensure_accordin_direct_library(logger)
    for lock in lock_keys:
        if lock == ACCORDIN_TASKSET_LOCK and args.skip_mcs_accordin_taskset:
            continue
        cmd, env = accordin_sweep_command(lock=lock, result_root=result_root, args=args)
        sudo_cmd, _ = experiment_three.with_sudo_env(cmd, env)
        try:
            logger.run(sudo_cmd, log_name=f"sweep_{lock}.log")
        except CommandError as exc:
            experiment_failures.append_command_failure(
                failures,
                result_root=result_root,
                experiment="experiment1",
                workload="mutexbench",
                benchmark="sweep",
                lock=lock,
                threads=",".join(str(thread) for thread in runnable_threads_for_lock(lock, args.threads)),
                repeat="all",
                exc=exc,
            )
            experiment_failures.write_failures_csv(result_root, failures)


def run_benchmarks(
    result_root: Path,
    args: argparse.Namespace,
    logger: CommandLogger,
    selected_lock_keys: tuple[str, ...],
    failures: list[dict[str, str]],
) -> None:
    selected = set(selected_lock_keys)
    base_lock_keys = tuple(lock_key for lock_key in BASE_LOCK_KEYS if lock_key in selected)
    accordin_lock_keys = tuple(lock_key for lock_key in ACCORDIN_PRELOAD_LOCKS if lock_key in selected)
    if base_lock_keys:
        run_multi_lock_sweeps(
            result_root,
            args,
            base_lock_keys,
            logger,
            failures,
            log_prefix="sweep_base_locks",
        )

    run_accordin_sweeps(result_root, args, accordin_lock_keys, logger, failures)

    if "mcs_extension" in selected:
        extension_dir = result_root / "mcs_extension"
        extension_dir.mkdir(parents=True, exist_ok=True)
        extension_cmd = [
            str(SWEEP_SINGLE),
            *common_sweep_args(args, runnable_threads_for_lock("mcs_extension", args.threads)),
            "--lock-kind",
            "mcs",
            "--timeslice-extension",
            args.mcs_extension_mode,
            "--output-raw",
            str(extension_dir / "raw.csv"),
            "--output-summary",
            str(extension_dir / "summary.csv"),
        ]
        try:
            logger.run(extension_cmd, log_name="sweep_mcs_extension.log")
        except CommandError as exc:
            experiment_failures.append_command_failure(
                failures,
                result_root=result_root,
                experiment="experiment1",
                workload="mutexbench",
                benchmark="sweep",
                lock="mcs_extension",
                threads=",".join(str(thread) for thread in runnable_threads_for_lock("mcs_extension", args.threads)),
                repeat="all",
                exc=exc,
            )
            experiment_failures.write_failures_csv(result_root, failures)


def run_supplement_benchmarks(
    result_root: Path,
    args: argparse.Namespace,
    lock_keys: tuple[str, ...],
    logger: CommandLogger,
    failures: list[dict[str, str]],
) -> None:
    run_multi_lock_sweeps(
        result_root,
        args,
        lock_keys,
        logger,
        failures,
        log_prefix=f"sweep_supplement_{'_'.join(lock_keys)}",
    )


def selected_lock_keys_from_settings(result_root: Path) -> tuple[str, ...] | None:
    settings_path = result_root / "settings.json"
    if not settings_path.is_file():
        return None
    try:
        with settings_path.open("r", encoding="utf-8") as f:
            settings = json.load(f)
    except (OSError, json.JSONDecodeError):
        return None

    raw_locks = settings.get("selected_locks") or settings.get("locks")
    if not isinstance(raw_locks, list):
        return None

    lock_keys: list[str] = []
    for item in raw_locks:
        if isinstance(item, dict):
            key = item.get("key")
        else:
            key = item
        if isinstance(key, str) and key in LOCKS_BY_KEY and key not in lock_keys:
            lock_keys.append(key)
    return tuple(lock_keys) if lock_keys else None


def load_combined_rows(
    result_root: Path,
    selected_lock_keys: tuple[str, ...] | None = None,
    *,
    allow_missing: bool = False,
) -> list[dict[str, str]]:
    combined_rows: list[dict[str, str]] = []
    required = set(LATENCY_PLOT_REQUIRED_FIELDS) | {CPU_FIELD}
    lock_specs = lock_specs_for_keys(
        selected_lock_keys or selected_lock_keys_from_settings(result_root) or tuple(lock.key for lock in LOCKS)
    )

    for lock in lock_specs:
        lock_dir = next(
            (
                result_root / dir_name
                for dir_name in lock.result_dir_names()
                if ((result_root / dir_name) / "summary.csv").is_file()
                or ((result_root / dir_name) / "raw.csv").is_file()
            ),
            result_root / lock.result_dir_names()[0],
        )
        if (
            (lock.optional or allow_missing)
            and not (lock_dir / "summary.csv").is_file()
            and not (lock_dir / "raw.csv").is_file()
        ):
            continue
        rows = load_plot_rows(lock_dir, required_fields=required)
        for row in rows:
            hold_plus_handoff_ns = str(float(row["avg_lock_hold_ns"]) + float(row[HANDOFF_FIELD]))
            combined_rows.append(
                {
                    "lock_label": lock.label,
                    "lock_key": lock.key,
                    "threads": row["threads"],
                    "critical_ns": row["critical_iters"],
                    "outside_ns": row["outside_iters"],
                    "repeats": row.get("repeats", ""),
                    "avg_lock_hold_ns": row["avg_lock_hold_ns"],
                    HANDOFF_FIELD: row[HANDOFF_FIELD],
                    "avg_hold_plus_handoff_ns": hold_plus_handoff_ns,
                    THROUGHPUT_FIELD: row[THROUGHPUT_FIELD],
                    CPU_FIELD: row[CPU_FIELD],
                    WAIT_FIELD: row.get(WAIT_FIELD, ""),
                    "elapsed_seconds": row.get("elapsed_seconds", ""),
                    "total_operations": row.get("total_operations", ""),
                    "lock_hold_samples": row.get("lock_hold_samples", ""),
                }
            )

    combined_rows.sort(key=lambda row: (lock_order(row["lock_key"]), int(row["threads"])))
    return combined_rows


def lock_order(key: str) -> int:
    for index, lock in enumerate(LOCKS):
        if lock.key == key:
            return index
    return len(LOCKS)


def write_combined_csv(result_root: Path, rows: list[dict[str, str]]) -> Path:
    path = result_root / "combined_summary.csv"
    with path.open("w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=COMBINED_FIELDS)
        writer.writeheader()
        writer.writerows(rows)
    return path


def is_experiment_row(row: dict[str, str]) -> bool:
    return int(row["critical_ns"]) == CRITICAL_NS and int(row["outside_ns"]) == OUTSIDE_NS


def is_plot_row(row: dict[str, str], *, max_threads: int | None = None) -> bool:
    thread = int(row["threads"])
    if max_threads is not None and thread > max_threads:
        return False
    return is_experiment_row(row) and thread in PLOT_THREADS


def has_plot_rows(rows: list[dict[str, str]], *, max_threads: int | None = None) -> bool:
    return any(is_plot_row(row, max_threads=max_threads) for row in rows)


def metric_values(rows: list[dict[str, str]], metric: str) -> list[float]:
    values: list[float] = []
    for row in rows:
        value = row.get(metric, "").strip()
        if value:
            values.append(float(value))
    return values


def compact_broken_lower_ylim(values: list[float]) -> tuple[float, float]:
    lower_values = [
        value
        for value in values
        if BROKEN_Y_NORMAL[0] <= value <= BROKEN_Y_NORMAL[1]
    ]
    if not lower_values:
        return BROKEN_Y_NORMAL

    lower_bound = max(
        BROKEN_Y_NORMAL[0],
        min(lower_values) / BROKEN_LOWER_AXIS_PADDING,
    )
    return (lower_bound, BROKEN_Y_NORMAL[1])


def linear_y_limit(rows: list[dict[str, str]], metric: str) -> float | None:
    values = metric_values(rows, metric)
    if not values or max(values) <= PIECEWISE_Y_THRESHOLD_NS:
        return None
    return PIECEWISE_Y_THRESHOLD_NS


def apply_piecewise_y_scale(ax, rows: list[dict[str, str]], metric: str) -> None:
    values = metric_values(rows, metric)
    if not values:
        return

    linear_limit = linear_y_limit(rows, metric)
    if linear_limit is None or max(values) <= linear_limit:
        return

    ax.set_yscale(
        "symlog",
        base=10,
        linthresh=linear_limit,
        linscale=PIECEWISE_Y_LINEAR_SCALE,
    )
    ax.axhline(linear_limit, color="0.55", linewidth=0.8, linestyle=":", alpha=0.65)


def draw_machine_core_line(
    ax,
    *,
    axis_max: float = THREAD_AXIS_MAX,
    shade_oversubscribed: bool = True,
) -> None:
    if shade_oversubscribed and MACHINE_CORE_COUNT < axis_max:
        ax.axvspan(
            MACHINE_CORE_COUNT,
            axis_max,
            color="0.92",
            alpha=0.55,
            linewidth=0,
            zorder=0,
        )
    if MACHINE_CORE_COUNT <= axis_max:
        ax.axvline(
            MACHINE_CORE_COUNT,
            color="0.22",
            linewidth=1.0,
            linestyle="--",
            alpha=0.75,
            zorder=1,
        )


def annotate_oversubscribed_region(
    ax,
    *,
    y_fraction: float = 0.94,
    axis_max: float = THREAD_AXIS_MAX,
) -> None:
    ax.annotate(
        "Oversubscribed",
        xy=((MACHINE_CORE_COUNT * axis_max) ** 0.5, y_fraction),
        xycoords=ax.get_xaxis_transform(),
        ha="center",
        va="top",
        fontsize=PLOT_ANNOTATION_FONTSIZE,
        color="0.35",
    )


def plot_metric(
    rows: list[dict[str, str]],
    *,
    metric: str,
    ylabel: str,
    title: str,
    output_path: Path,
    max_threads: int | None = None,
    value_scale: float = 1.0,
    piecewise_y_scale: bool = True,
) -> None:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.ticker import ScalarFormatter

    plot_threads = tuple(
        thread
        for thread in PLOT_THREADS
        if max_threads is None or thread <= max_threads
    )
    if not plot_threads:
        raise RuntimeError(f"No plot threads matched max_threads={max_threads}.")

    plot_rows = [row for row in rows if is_plot_row(row, max_threads=max_threads)]
    if not plot_rows:
        raise RuntimeError(
            f"No rows matched critical_ns={CRITICAL_NS} and outside_ns={OUTSIDE_NS} for plotting."
        )

    fig, ax = plt.subplots(figsize=(9.5, 5.5))
    for lock in LOCKS:
        points = [
            (int(row["threads"]), float(row[metric]) * value_scale)
            for row in plot_rows
            if row["lock_key"] == lock.key
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
            label=lock.label,
        )

    ax.set_title(title, fontsize=PLOT_TITLE_FONTSIZE)
    ax.set_xlabel("Threads", fontsize=PLOT_LABEL_FONTSIZE)
    ax.set_ylabel(ylabel, fontsize=PLOT_LABEL_FONTSIZE)
    if piecewise_y_scale:
        apply_piecewise_y_scale(ax, plot_rows, metric)
    ax.set_xscale("log", base=2)
    thread_axis_min = plot_threads[0] / 1.08
    thread_axis_max = plot_threads[-1] * 1.25
    ax.set_xlim(thread_axis_min, thread_axis_max)
    ax.set_xticks(list(plot_threads))
    ax.xaxis.set_major_formatter(ScalarFormatter())
    show_oversubscribed_region = plot_threads[-1] > MACHINE_CORE_COUNT
    draw_machine_core_line(
        ax,
        axis_max=thread_axis_max,
        shade_oversubscribed=show_oversubscribed_region,
    )
    if show_oversubscribed_region:
        annotate_oversubscribed_region(ax, axis_max=thread_axis_max)
    ax.grid(True, axis="y", alpha=0.28)
    ax.grid(True, axis="x", which="major", alpha=0.16)
    ax.tick_params(axis="both", labelsize=PLOT_TICK_FONTSIZE)
    ax.legend(frameon=False, ncol=2, fontsize=PLOT_LEGEND_FONTSIZE)
    fig.tight_layout()
    fig.savefig(output_path, dpi=180)
    plt.close(fig)


def plot_focused_comparison(
    rows: list[dict[str, str]],
    *,
    metric: str,
    ylabel: str,
    title: str,
    output_path: Path,
) -> None:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.ticker import ScalarFormatter

    plot_rows = [
        row
        for row in rows
        if is_plot_row(row) and row["lock_key"] in FOCUS_LOCK_KEYS
    ]
    if not plot_rows:
        return

    fig, ax = plt.subplots(figsize=(9.5, 5.5))
    for lock_key in FOCUS_LOCK_KEYS:
        lock = next(lock for lock in LOCKS if lock.key == lock_key)
        points = [
            (int(row["threads"]), float(row[metric]))
            for row in plot_rows
            if row["lock_key"] == lock_key
        ]
        if not points:
            continue
        points.sort()
        ax.plot(
            [thread for thread, _ in points],
            [value for _, value in points],
            marker="o",
            linewidth=2.2,
            markersize=4.5,
            label=lock.label,
        )

    ratio_rows: dict[int, dict[str, float]] = {}
    for row in plot_rows:
        ratio_rows.setdefault(int(row["threads"]), {})[row["lock_key"]] = float(row[metric])
    for thread, values in sorted(ratio_rows.items()):
        if not all(key in values for key in FOCUS_LOCK_KEYS):
            continue
        lower = values[FOCUS_LOCK_KEYS[0]]
        upper = values["flexguard"]
        if lower <= 0.0 or upper <= lower:
            continue
        ratio = upper / lower
        y = lower + (upper - lower) * 0.58
        ax.annotate(
            f"{ratio:.1f}x",
            xy=(thread, y),
            xytext=(0, 6),
            textcoords="offset points",
            ha="center",
            va="bottom",
            fontsize=PLOT_ANNOTATION_FONTSIZE,
            color="0.25",
        )

    values = metric_values(plot_rows, metric)
    if values:
        lower = min(values)
        upper = max(values)
        pad = max((upper - lower) * 0.16, upper * 0.05)
        ax.set_ylim(max(0.0, lower - pad), upper + pad)
    ax.set_title(title, fontsize=PLOT_TITLE_FONTSIZE)
    ax.set_xlabel("Threads", fontsize=PLOT_LABEL_FONTSIZE)
    ax.set_ylabel(ylabel, fontsize=PLOT_LABEL_FONTSIZE)
    ax.set_xscale("log", base=2)
    ax.set_xlim(THREAD_AXIS_MIN, THREAD_AXIS_MAX)
    ax.set_xticks(list(PLOT_THREADS))
    ax.xaxis.set_major_formatter(ScalarFormatter())
    draw_machine_core_line(ax)
    annotate_oversubscribed_region(ax)
    ax.grid(True, axis="y", alpha=0.28)
    ax.grid(True, axis="x", which="major", alpha=0.16)
    ax.tick_params(axis="both", labelsize=PLOT_TICK_FONTSIZE)
    ax.legend(frameon=False, fontsize=PLOT_LEGEND_FONTSIZE)
    fig.tight_layout()
    fig.savefig(output_path, dpi=180)
    plt.close(fig)


def plot_broken_axis_metric(
    rows: list[dict[str, str]],
    *,
    metric: str,
    metric_label: str | None = None,
    secondary_metric: str | None = None,
    secondary_metric_label: str | None = None,
    compact_lower_axis: bool = False,
    ylabel: str,
    title: str,
    output_path: Path,
) -> None:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.lines import Line2D
    from matplotlib.ticker import LogFormatterMathtext, ScalarFormatter

    plot_rows = [row for row in rows if is_plot_row(row)]
    if not plot_rows:
        raise RuntimeError(
            f"No rows matched critical_ns={CRITICAL_NS} and outside_ns={OUTSIDE_NS} for plotting."
        )

    metrics = [metric]
    if secondary_metric is not None:
        metrics.append(secondary_metric)
    values = [value for metric_name in metrics for value in metric_values(plot_rows, metric_name)]
    if not values:
        raise RuntimeError(f"No values found for metric {metric}.")

    upper_values = [value for value in values if value >= BROKEN_Y_UPPER_MIN]
    upper_max = max(upper_values) if upper_values else BROKEN_Y_UPPER_MIN
    upper_ylim = (BROKEN_Y_UPPER_MIN, max(1e6, upper_max * 1.12))
    lower_ylim = compact_broken_lower_ylim(values) if compact_lower_axis else BROKEN_Y_NORMAL
    colors = plt.rcParams["axes.prop_cycle"].by_key().get("color", ["C0"])
    lock_colors = {lock.key: colors[index % len(colors)] for index, lock in enumerate(LOCKS)}

    fig, (upper_ax, lower_ax) = plt.subplots(
        2,
        1,
        sharex=True,
        figsize=(9.5, 6.8),
        gridspec_kw={"height_ratios": [0.9, 3.0], "hspace": 0.06},
    )

    for ax in (upper_ax, lower_ax):
        for lock in LOCKS:
            color = lock_colors[lock.key]
            metric_specs = [
                (metric, "-", "o", color, color, 1.8, 4.0),
            ]
            if secondary_metric is not None:
                metric_specs.append(
                    (secondary_metric, "--", "o", color, "white", 1.25, 3.5)
                )
            for (
                metric_name,
                linestyle,
                marker,
                color,
                markerfacecolor,
                linewidth,
                markersize,
            ) in metric_specs:
                points = [
                    (int(row["threads"]), float(row[metric_name]))
                    for row in plot_rows
                    if row["lock_key"] == lock.key
                ]
                if not points:
                    continue
                points.sort()
                ax.plot(
                    [thread for thread, _ in points],
                    [value for _, value in points],
                    color=color,
                    linestyle=linestyle,
                    marker=marker,
                    markerfacecolor=markerfacecolor,
                    markeredgecolor=color,
                    linewidth=linewidth,
                    markersize=markersize,
                )
        ax.set_yscale("log")
        ax.grid(True, axis="y", which="major", alpha=0.28)
        ax.grid(True, axis="x", which="major", alpha=0.16)
        ax.yaxis.set_major_formatter(LogFormatterMathtext(base=10))
        ax.tick_params(axis="both", labelsize=PLOT_TICK_FONTSIZE)
        draw_machine_core_line(ax)

    upper_ax.set_ylim(*upper_ylim)
    lower_ax.set_ylim(*lower_ylim)
    upper_ax.spines["bottom"].set_visible(False)
    lower_ax.spines["top"].set_visible(False)
    upper_ax.tick_params(labelbottom=False, bottom=False, labelsize=PLOT_TICK_FONTSIZE)
    lower_ax.tick_params(top=False, labelsize=PLOT_TICK_FONTSIZE)

    break_mark = 0.012
    break_kwargs = dict(transform=upper_ax.transAxes, color="0.25", clip_on=False, linewidth=1.0)
    upper_ax.plot((-break_mark, +break_mark), (-break_mark, +break_mark), **break_kwargs)
    upper_ax.plot((1 - break_mark, 1 + break_mark), (-break_mark, +break_mark), **break_kwargs)
    break_kwargs.update(transform=lower_ax.transAxes)
    lower_ax.plot((-break_mark, +break_mark), (1 - break_mark, 1 + break_mark), **break_kwargs)
    lower_ax.plot((1 - break_mark, 1 + break_mark), (1 - break_mark, 1 + break_mark), **break_kwargs)

    upper_ax.set_title(title, fontsize=PLOT_TITLE_FONTSIZE)
    fig.supylabel(ylabel, fontsize=PLOT_LABEL_FONTSIZE)
    lower_ax.set_xlabel("Threads", fontsize=PLOT_LABEL_FONTSIZE)
    lower_ax.set_xscale("log", base=2)
    lower_ax.set_xlim(THREAD_AXIS_MIN, THREAD_AXIS_MAX)
    lower_ax.set_xticks(list(PLOT_THREADS))
    lower_ax.xaxis.set_major_formatter(ScalarFormatter())
    annotate_oversubscribed_region(upper_ax, y_fraction=0.88)
    lock_handles = [
        Line2D(
            [0],
            [0],
            color=lock_colors[lock.key],
            marker="o",
            linewidth=1.8,
            markersize=4,
            label=lock.label,
        )
        for lock in LOCKS
        if any(row["lock_key"] == lock.key for row in plot_rows)
    ]
    lock_legend = upper_ax.legend(
        handles=lock_handles,
        frameon=False,
        ncol=2,
        loc="upper left",
        fontsize=PLOT_LEGEND_FONTSIZE,
    )
    upper_ax.add_artist(lock_legend)
    if secondary_metric is not None:
        style_handles = [
            Line2D(
                [0],
                [0],
                color="0.2",
                linestyle="-",
                marker="o",
                linewidth=1.8,
                markersize=4,
                label=metric_label or metric,
            ),
            Line2D(
                [0],
                [0],
                color="0.2",
                linestyle="--",
                marker="o",
                markerfacecolor="white",
                markeredgecolor="0.2",
                linewidth=1.25,
                markersize=3.5,
                label=secondary_metric_label or secondary_metric,
            ),
        ]
        upper_ax.legend(
            handles=style_handles,
            frameon=False,
            loc="upper right",
            fontsize=PLOT_LEGEND_FONTSIZE,
        )
    fig.subplots_adjust(left=0.10, right=0.98, top=0.90, bottom=0.11, hspace=0.06)
    fig.savefig(output_path, dpi=180)
    plt.close(fig)


def write_plots(result_root: Path, rows: list[dict[str, str]]) -> list[Path]:
    outputs: list[Path] = []
    if has_plot_rows(rows, max_threads=HOLD_TIME_MAX_THREADS):
        hold_time_path = result_root / "hold_time_vs_threads.png"
        plot_metric(
            rows,
            metric="avg_lock_hold_ns",
            ylabel="Average lock hold time (ns)",
            title="Lock Hold Time vs Threads",
            output_path=hold_time_path,
            max_threads=HOLD_TIME_MAX_THREADS,
        )
        outputs.append(hold_time_path)
    handoff_path = result_root / "handoff_time_vs_threads.png"
    plot_broken_axis_metric(
        rows,
        metric=HANDOFF_FIELD,
        ylabel="Estimated lock handoff time (ns)",
        title="Lock Handoff Time vs Threads",
        output_path=handoff_path,
    )
    outputs.append(handoff_path)
    ops_path = result_root / "ops_vs_threads.png"
    plot_metric(
        rows,
        metric=THROUGHPUT_FIELD,
        ylabel="Throughput (Mops/s)",
        title="Throughput vs Threads",
        output_path=ops_path,
        value_scale=1e-6,
        piecewise_y_scale=False,
    )
    outputs.append(ops_path)
    focus_path = result_root / "handoff_time_flexguard_vs_accordin.png"
    plot_focused_comparison(
        rows,
        metric=HANDOFF_FIELD,
        ylabel="Estimated lock handoff time (ns)",
        title="Handoff Time: Accordin vs FlexGuard",
        output_path=focus_path,
    )
    if focus_path.is_file():
        outputs.append(focus_path)
    return outputs


def ensure_output_root(path: Path, force: bool) -> None:
    if path.exists() and not path.is_dir():
        raise RuntimeError(f"Output root exists but is not a directory: {path}")
    if path.exists() and any(path.iterdir()) and not force:
        raise RuntimeError(f"Output root already exists and is not empty: {path}. Use --force to write there.")
    path.mkdir(parents=True, exist_ok=True)


def ensure_supplement_output_root(path: Path) -> None:
    if path.exists() and not path.is_dir():
        raise RuntimeError(f"Supplement output root exists but is not a directory: {path}")
    path.mkdir(parents=True, exist_ok=True)


def lock_has_existing_results(result_root: Path, lock_key: str) -> bool:
    lock = LOCKS_BY_KEY[lock_key]
    for dir_name in lock.result_dir_names():
        lock_dir = result_root / dir_name
        if (lock_dir / "raw.csv").exists() or (lock_dir / "summary.csv").exists():
            return True
    return False


def ensure_supplement_targets(result_root: Path, lock_keys: Iterable[str], force: bool) -> None:
    if force:
        return

    existing_locks = [LOCKS_BY_KEY[lock_key].label for lock_key in lock_keys if lock_has_existing_results(result_root, lock_key)]
    if not existing_locks:
        return

    raise RuntimeError(
        "Supplement target locks already have results under the output root. "
        f"Refusing to overwrite without --force: {', '.join(existing_locks)}"
    )


def print_outputs(result_root: Path, combined_path: Path, plot_paths: Iterable[Path]) -> None:
    print(f"Result root: {result_root}")
    print(f"Combined CSV: {combined_path}")
    for plot_path in plot_paths:
        print(f"Plot: {plot_path}")


def main() -> int:
    args = parse_args()
    if args.output_root is not None and args.plot_only is not None:
        print("--output-root cannot be used together with --plot-only.", file=sys.stderr)
        return 2
    if args.supplement_locks is not None and args.plot_only is not None:
        print("--supplement-locks cannot be used together with --plot-only.", file=sys.stderr)
        return 2

    selected_lock_keys = selected_lock_keys_for_profile(args.lock_profile)
    supplement_lock_keys: tuple[str, ...] | None = None
    if args.supplement_locks is not None:
        if args.output_root is None:
            print("--supplement-locks requires --output-root.", file=sys.stderr)
            return 2
        try:
            supplement_lock_keys = parse_supplement_lock_keys(args.supplement_locks)
        except ValueError as exc:
            print(str(exc), file=sys.stderr)
            return 2

    try:
        if args.plot_only is not None:
            result_root = resolve_path(args.plot_only)
            if not result_root.is_dir():
                print(f"Plot-only result root does not exist: {result_root}", file=sys.stderr)
                return 2
            rows = load_combined_rows(result_root)
            combined_path = write_combined_csv(result_root, rows)
            plot_paths = write_plots(result_root, rows)
            print_outputs(result_root, combined_path, plot_paths)
            return 0

        result_root = resolve_path(args.output_root) if args.output_root is not None else default_result_root()
        if supplement_lock_keys is not None:
            ensure_supplement_output_root(result_root)
            ensure_supplement_targets(result_root, supplement_lock_keys, args.force)
            write_settings_if_missing(
                result_root,
                args.mcs_extension_mode,
                args.sudo_mode,
                ACCORDIN_TASKSET_LOCK in selected_lock_keys and not args.skip_mcs_accordin_taskset,
                args.mcs_accordin_taskset_cpus,
                args.lock_profile,
                selected_lock_keys,
                args.command_timeout_seconds,
                args.threads,
                args.repeats,
            )
            logger = CommandLogger(result_root, command_timeout_seconds=args.command_timeout_seconds)
            ensure_inputs(logger, lock_keys=supplement_lock_keys, include_single_sweep=False)
            failures: list[dict[str, str]] = []
            run_supplement_benchmarks(result_root, args, supplement_lock_keys, logger, failures)
        else:
            ensure_output_root(result_root, args.force)
            write_settings(
                result_root,
                args.mcs_extension_mode,
                args.sudo_mode,
                ACCORDIN_TASKSET_LOCK in selected_lock_keys and not args.skip_mcs_accordin_taskset,
                args.mcs_accordin_taskset_cpus,
                args.lock_profile,
                selected_lock_keys,
                args.command_timeout_seconds,
                args.threads,
                args.repeats,
            )
            logger = CommandLogger(result_root, command_timeout_seconds=args.command_timeout_seconds)
            ensure_inputs(
                logger,
                lock_keys=selected_lock_keys,
                include_single_sweep=(
                    "mcs_extension" in selected_lock_keys
                    or any(lock in selected_lock_keys for lock in ACCORDIN_PRELOAD_LOCKS)
                ),
            )
            failures = []
            run_benchmarks(result_root, args, logger, selected_lock_keys, failures)
        rows = load_combined_rows(
            result_root,
            selected_lock_keys if supplement_lock_keys is None else None,
            allow_missing=bool(failures),
        )
        combined_path = write_combined_csv(result_root, rows)
        plot_paths = write_plots(result_root, rows) if rows else []
        print_outputs(result_root, combined_path, plot_paths)
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
