#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import math
import os
import shutil
import statistics
import subprocess
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
DEFAULT_OUTPUT_ROOT_PARENT = REPO_ROOT / "experiments" / "results"
DEFAULT_OUTPUT_ROOT_STEM = "experiment3_mutexbench_results_baseline"
_PLOT_STYLE_MODULE = None
ACCORDIN_DIRECT_PACKAGE = "mcs_tas_accordin_direct"
ACCORDIN_DIRECT_LOCK_KIND = "mcs_tas_accordin_direct"
ACCORDIN_DIRECT_RELEASE_LIB = REPO_ROOT / "target" / "release" / "libmcs_tas_accordin_direct.so"
ACCORDIN_DIRECT_LIB_ENV = "MCS_TAS_ACCORDIN_DIRECT_LIB"
ACCORDIN_DIRECT_DISABLE_BPF_ENV = "MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF"
ACCORDIN_DIRECT_STATS_ONLY_ENV = "MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY"
ACCORDIN_DIRECT_ENV_PREFIX = "MCS_TAS_ACCORDIN_DIRECT_"
FIXED_MUTEXBENCH_COMBOS = (
    (100, 3000),
    (300, 3000),
    (1000, 3000),
    (3000, 3000),
)
ACCORDIN_TASKSET_RATIO_COMBOS = FIXED_MUTEXBENCH_COMBOS

REQUIRED_BASELINE_LOCKS = experiment_defaults.EXPERIMENT_ONE_FULL_LOCKS
ACCORDIN_LOCKS = experiment_defaults.ACCORDIN_VARIANT_LOCKS
DEFAULT_LOCK_PROFILE = experiment_defaults.DEFAULT_LOCK_PROFILE
MINIMAL_LOCKS = experiment_defaults.MINIMAL_LOCKS
FULL_LOCKS = experiment_defaults.FULL_LOCKS
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


def experiment_three_plot_styles():
    global _PLOT_STYLE_MODULE
    if _PLOT_STYLE_MODULE is None:
        scripts_dir = str(MUTEXBENCH_DIR / "scripts")
        if scripts_dir not in sys.path:
            sys.path.insert(0, scripts_dir)
        import plot_throughput_by_ratio

        _PLOT_STYLE_MODULE = plot_throughput_by_ratio
    return _PLOT_STYLE_MODULE


def accordin_plot_color() -> str:
    return experiment_three_plot_styles().ACCORDIN_COLOR


def plot_color(lock: str, fallback: str = "C0") -> str:
    return experiment_three_plot_styles().lock_color(lock, fallback)


def plot_linestyle(lock: str) -> str:
    return experiment_three_plot_styles().lock_linestyle(lock)


def plot_marker(lock: str, fallback: str = "o") -> str:
    return experiment_three_plot_styles().lock_marker(lock, fallback)


def is_accordin_plot_lock(lock: str) -> bool:
    return lock in experiment_three_plot_styles().ACCORDIN_LOCKS


def accordin_plot_linestyle(lock: str) -> str:
    return plot_linestyle(lock)


def accordin_plot_marker(lock: str) -> str:
    return plot_marker(lock)


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


@dataclass(frozen=True)
class TasksetNode:
    node: int
    cpus: tuple[int, ...]


@dataclass(frozen=True)
class TasksetTopology:
    source: str
    nodes: tuple[TasksetNode, ...]

    def ordered_cpus(self) -> tuple[int, ...]:
        return tuple(cpu for node in self.nodes for cpu in node.cpus)

    def cpu_to_node(self) -> dict[int, int]:
        return {cpu: node.node for node in self.nodes for cpu in node.cpus}

    def cpu_count(self) -> int:
        return len(self.ordered_cpus())


@dataclass(frozen=True)
class TasksetSweepSpec:
    critical_ns: int
    outside_ns: int
    target_cpus: int
    cpus: tuple[int, ...]
    cpu_list: str
    numa_nodes: str
    matrix: BaselineMatrix


def matrix_with_threads(matrix: BaselineMatrix, threads: Iterable[int]) -> BaselineMatrix:
    return BaselineMatrix(
        threads=tuple(threads),
        critical_ns=matrix.critical_ns,
        outside_ns=matrix.outside_ns,
        repeats=matrix.repeats,
        duration_ms=matrix.duration_ms,
        warmup_duration_ms=matrix.warmup_duration_ms,
    )


def fixed_baseline_matrix(
    threads: Iterable[int],
    *,
    duration_ms: int | None = None,
    warmup_duration_ms: int = DEFAULT_WARMUP_DURATION_MS,
) -> BaselineMatrix:
    critical_ns = tuple(
        dict.fromkeys(critical for critical, _outside in FIXED_MUTEXBENCH_COMBOS)
    )
    outside_ns = tuple(
        dict.fromkeys(outside for _critical, outside in FIXED_MUTEXBENCH_COMBOS)
    )
    return BaselineMatrix(
        threads=tuple(threads),
        critical_ns=critical_ns,
        outside_ns=outside_ns,
        repeats=experiment_defaults.MUTEXBENCH_DEFAULT_REPEATS,
        duration_ms=(
            duration_ms
            if duration_ms is not None
            else experiment_defaults.MUTEXBENCH_DEFAULT_DURATION_MS
        ),
        warmup_duration_ms=warmup_duration_ms,
    )


def topology_from_node_cpus(source: str, node_cpus: dict[int, Iterable[int]]) -> TasksetTopology:
    nodes_list: list[TasksetNode] = []
    for node, cpus in sorted(node_cpus.items()):
        node_cpu_list = tuple(sorted(set(cpus)))
        if node_cpu_list:
            nodes_list.append(TasksetNode(node=node, cpus=node_cpu_list))
    nodes = tuple(nodes_list)
    if not nodes:
        raise RuntimeError("CPU topology did not contain any online CPUs.")
    return TasksetTopology(source=source, nodes=nodes)


def detect_taskset_topology() -> TasksetTopology:
    try:
        completed = subprocess.run(
            ["lscpu", "-p=CPU,NODE,ONLINE"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except OSError:
        completed = None

    node_cpus: dict[int, list[int]] = {}
    if completed is not None and completed.returncode == 0:
        for line in completed.stdout.splitlines():
            if not line or line.startswith("#"):
                continue
            parts = line.split(",")
            if len(parts) != 3:
                continue
            cpu_text, node_text, online_text = parts
            if online_text.strip().upper() not in {"Y", "YES", "1"}:
                continue
            try:
                cpu = int(cpu_text)
                node = int(node_text)
            except ValueError:
                continue
            node_cpus.setdefault(node, []).append(cpu)
    if node_cpus:
        return topology_from_node_cpus("lscpu -p=CPU,NODE,ONLINE", node_cpus)

    cpu_count = os.cpu_count() or 1
    return topology_from_node_cpus("fallback:os.cpu_count", {0: range(cpu_count)})


def taskset_target_cpu_count(critical_ns: int, outside_ns: int, available_cpus: int) -> int:
    if critical_ns <= 0:
        raise ValueError("critical_ns must be positive")
    if outside_ns < 0:
        raise ValueError("outside_ns must be non-negative")
    if available_cpus <= 0:
        raise ValueError("available_cpus must be positive")
    target = 1.0 + outside_ns / critical_ns
    return min(available_cpus, max(1, math.floor(target + 0.5)))


def taskset_cpu_list(topology: TasksetTopology, target_cpus: int) -> tuple[int, ...]:
    ordered = topology.ordered_cpus()
    if not ordered:
        raise ValueError("taskset topology has no CPUs")
    count = min(len(ordered), max(1, target_cpus))
    return ordered[:count]


def format_cpu_list(cpus: Iterable[int]) -> str:
    return ",".join(str(cpu) for cpu in cpus)


def numa_nodes_for_cpus(topology: TasksetTopology, cpus: Iterable[int]) -> str:
    cpu_to_node = topology.cpu_to_node()
    nodes = sorted({cpu_to_node[cpu] for cpu in cpus if cpu in cpu_to_node})
    return ";".join(str(node) for node in nodes)


def taskset_ratio_combos() -> tuple[tuple[int, int], ...]:
    return ACCORDIN_TASKSET_RATIO_COMBOS


def taskset_sweep_specs(matrix: BaselineMatrix, topology: TasksetTopology) -> tuple[TasksetSweepSpec, ...]:
    specs: list[TasksetSweepSpec] = []
    available_cpus = topology.cpu_count()
    for critical_ns, outside_ns in taskset_ratio_combos():
        target_cpus = taskset_target_cpu_count(critical_ns, outside_ns, available_cpus)
        cpus = taskset_cpu_list(topology, target_cpus)
        pair_matrix = BaselineMatrix(
            threads=matrix.threads,
            critical_ns=(critical_ns,),
            outside_ns=(outside_ns,),
            repeats=matrix.repeats,
            duration_ms=matrix.duration_ms,
            warmup_duration_ms=matrix.warmup_duration_ms,
        )
        specs.append(
            TasksetSweepSpec(
                critical_ns=critical_ns,
                outside_ns=outside_ns,
                target_cpus=target_cpus,
                cpus=cpus,
                cpu_list=format_cpu_list(cpus),
                numa_nodes=numa_nodes_for_cpus(topology, cpus),
                matrix=pair_matrix,
            )
        )
    return tuple(specs)


def runnable_threads_for_lock(lock: str, matrix: BaselineMatrix) -> tuple[int, ...]:
    return matrix.threads


def expected_rows_for_lock(lock: str, matrix: BaselineMatrix) -> int:
    threads = runnable_threads_for_lock(lock, matrix)
    if lock == experiment_defaults.ACCORDIN_TASKSET_LOCK:
        return len(threads) * len(taskset_ratio_combos()) * matrix.repeats
    return len(threads) * len(matrix.critical_ns) * len(matrix.outside_ns) * matrix.repeats


def csv_join(values: Iterable[int | str]) -> str:
    return ",".join(str(value) for value in values)


def default_result_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    base = DEFAULT_OUTPUT_ROOT_PARENT / f"{DEFAULT_OUTPUT_ROOT_STEM}_{timestamp}"
    candidate = base
    suffix = 1
    while candidate.exists():
        candidate = base.with_name(f"{base.name}_{suffix}")
        suffix += 1
    return candidate


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
    lock_arg: str | None,
    matrix: BaselineMatrix,
    *,
    force: bool = False,
    lock_profile: str | None = None,
) -> tuple[str, ...]:
    if lock_arg is None:
        lock_arg = csv_join(experiment_defaults.lock_profile_locks(lock_profile)) if lock_profile else "missing"

    if lock_arg == "missing":
        return incomplete_or_missing_locks(output_root, missing_experiment_locks(baseline_root), matrix)

    locks = tuple(dict.fromkeys(normalize_lock(lock) for lock in parse_csv_strings(lock_arg)))
    unsupported = [lock for lock in locks if lock not in SUPPORTED_LOCKS]
    if unsupported:
        supported = ",".join(REQUIRED_BASELINE_LOCKS)
        raise ValueError(f"Unsupported lock keys: {','.join(unsupported)}. Supported: {supported}")
    if force:
        return locks
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


def accordin_sweep_command(
    root: Path,
    lock: str,
    matrix: BaselineMatrix,
    taskset_cpus: str | None,
    *,
    raw_path: Path | None = None,
    summary_path: Path | None = None,
) -> tuple[list[str], dict[str, str | None]]:
    lock_dir = root / lock
    raw_path = raw_path or lock_dir / "raw.csv"
    summary_path = summary_path or lock_dir / "summary.csv"
    cmd = [
        str(SWEEP_SINGLE),
        *common_sweep_args(matrix, runnable_threads_for_lock(lock, matrix)),
        "--lock-kind",
        ACCORDIN_DIRECT_LOCK_KIND,
        "--timeslice-extension",
        "off",
        "--output-raw",
        str(raw_path),
        "--output-summary",
        str(summary_path),
    ]
    if experiment_defaults.accordin_uses_taskset(lock):
        if not taskset_cpus:
            raise RuntimeError(f"taskset CPU list is required for {lock}")
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


def csv_sort_key(row: dict[str, str], fields: tuple[str, ...]) -> tuple[int, ...]:
    return tuple(int(row[field]) for field in fields)


def merge_csv_files(paths: Sequence[Path], output_path: Path, *, sort_fields: tuple[str, ...]) -> None:
    if not paths:
        raise RuntimeError(f"No CSV parts available to merge into {output_path}")

    fieldnames: list[str] | None = None
    rows: list[dict[str, str]] = []
    for path in paths:
        with path.open(newline="", encoding="utf-8") as f:
            reader = csv.DictReader(f)
            current_fieldnames = list(reader.fieldnames or [])
            if fieldnames is None:
                fieldnames = current_fieldnames
            elif current_fieldnames != fieldnames:
                raise RuntimeError(f"CSV schema mismatch while merging {path}")
            rows.extend(dict(row) for row in reader)

    if fieldnames is None:
        raise RuntimeError(f"CSV part had no header while merging into {output_path}")

    rows.sort(key=lambda row: csv_sort_key(row, sort_fields))
    output_path.parent.mkdir(parents=True, exist_ok=True)
    with output_path.open("w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=fieldnames, lineterminator="\n")
        writer.writeheader()
        for row in rows:
            writer.writerow({field: row.get(field, "") for field in fieldnames})


def merge_taskset_part_csvs(lock_dir: Path, raw_paths: Sequence[Path], summary_paths: Sequence[Path]) -> None:
    merge_csv_files(
        raw_paths,
        lock_dir / "raw.csv",
        sort_fields=("threads", "critical_iters", "outside_iters", "repeat"),
    )
    merge_csv_files(
        summary_paths,
        lock_dir / "summary.csv",
        sort_fields=("threads", "critical_iters", "outside_iters"),
    )


def csv_data_row_count(path: Path) -> int:
    if not path.is_file():
        return 0
    with path.open(newline="", encoding="utf-8") as f:
        reader = csv.reader(f)
        if next(reader, None) is None:
            return 0
        return sum(1 for _ in reader)


def taskset_part_is_complete(raw_path: Path, summary_path: Path, sweep: TasksetSweepSpec) -> bool:
    expected_raw_rows = len(sweep.matrix.threads) * sweep.matrix.repeats
    expected_summary_rows = len(sweep.matrix.threads)
    return (
        csv_data_row_count(raw_path) == expected_raw_rows
        and csv_data_row_count(summary_path) == expected_summary_rows
    )


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
    *,
    taskset_topology: TasksetTopology | None = None,
    taskset_sweeps: tuple[TasksetSweepSpec, ...] = (),
) -> None:
    settings = {
        "experiment": "experiment3_mutexbench_baseline_supplement",
        "output_root": str(root),
        "baseline_root": str(baseline_root),
        "baseline_matrix_source": "fixed",
        "fixed_mutexbench_combos": [
            {"critical_ns": critical_ns, "outside_ns": outside_ns}
            for critical_ns, outside_ns in FIXED_MUTEXBENCH_COMBOS
        ],
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
        "mcs_accordin_taskset_policy": (
            "fixed_cli_override"
            if args.mcs_accordin_taskset_cpus
            else "selected_out3000_ratio_combos_dynamic_round(outside/critical + 1)_numa_first"
        ),
        "mcs_accordin_taskset_cpus": args.mcs_accordin_taskset_cpus,
        "mcs_accordin_taskset_topology": (
            {
                "source": taskset_topology.source,
                "nodes": [
                    {"node": node.node, "cpus": list(node.cpus)}
                    for node in taskset_topology.nodes
                ],
            }
            if taskset_topology is not None
            else None
        ),
        "mcs_accordin_taskset_dynamic_sweeps": [
            {
                "critical_ns": sweep.critical_ns,
                "outside_ns": sweep.outside_ns,
                "critical_ratio": sweep.critical_ns / (sweep.critical_ns + sweep.outside_ns),
                "target_cpus": sweep.target_cpus,
                "cpu_list": sweep.cpu_list,
                "numa_nodes": sweep.numa_nodes,
            }
            for sweep in taskset_sweeps
        ],
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


def run_dynamic_taskset_accordin_sweeps(
    logger: CommandLogger,  # type: ignore[name-defined]
    root: Path,
    lock: str,
    sweeps: tuple[TasksetSweepSpec, ...],
    *,
    dry_run: bool,
) -> None:
    lock_dir = root / lock
    parts_dir = lock_dir / "taskset_parts"
    if not dry_run:
        parts_dir.mkdir(parents=True, exist_ok=True)

    raw_paths: list[Path] = []
    summary_paths: list[Path] = []
    for sweep in sweeps:
        raw_path = parts_dir / f"raw_c{sweep.critical_ns}_o{sweep.outside_ns}.csv"
        summary_path = parts_dir / f"summary_c{sweep.critical_ns}_o{sweep.outside_ns}.csv"
        if not dry_run and taskset_part_is_complete(raw_path, summary_path, sweep):
            print(f"Skipping complete taskset part: critical={sweep.critical_ns} outside={sweep.outside_ns}")
            raw_paths.append(raw_path)
            summary_paths.append(summary_path)
            continue
        if not dry_run:
            raw_path.unlink(missing_ok=True)
            summary_path.unlink(missing_ok=True)
        cmd, env = accordin_sweep_command(
            root,
            lock,
            sweep.matrix,
            sweep.cpu_list,
            raw_path=raw_path,
            summary_path=summary_path,
        )
        run_command(
            logger,
            cmd,
            log_name=f"sweep_{lock}_c{sweep.critical_ns}_o{sweep.outside_ns}.log",
            dry_run=dry_run,
            env=env,
            sudo_env=True,
        )
        raw_paths.append(raw_path)
        summary_paths.append(summary_path)

    if not dry_run:
        merge_taskset_part_csvs(lock_dir, raw_paths, summary_paths)


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
    uses_dynamic_taskset = (
        experiment_defaults.ACCORDIN_TASKSET_LOCK in locks
        and args.mcs_accordin_taskset_cpus is None
    )
    taskset_topology = detect_taskset_topology() if uses_dynamic_taskset else None
    taskset_sweeps = (
        taskset_sweep_specs(matrix, taskset_topology)
        if taskset_topology is not None
        else ()
    )
    if not args.dry_run:
        ensure_mutex_bench(logger)
        prepare_target_dirs(root, locks, force=args.force)
        write_settings(
            root,
            baseline_root,
            locks,
            matrix,
            args,
            taskset_topology=taskset_topology,
            taskset_sweeps=taskset_sweeps,
        )

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
        if experiment_defaults.accordin_uses_taskset(lock) and args.mcs_accordin_taskset_cpus is None:
            run_dynamic_taskset_accordin_sweeps(
                logger,
                root,
                lock,
                taskset_sweeps,
                dry_run=args.dry_run,
            )
            continue

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
        description="Supplement mutexbench experiment-three locks using the fixed mutexbench matrix.",
    )
    parser.add_argument(
        "--baseline-root",
        type=Path,
        default=DEFAULT_BASELINE_ROOT,
        help=(
            "Existing mutexbench baseline root used only to infer missing locks. "
            f"The critical/outside matrix is fixed. Default: {DEFAULT_BASELINE_ROOT}."
        ),
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        default=None,
        help=(
            "Experiment result root for supplemental outputs. "
            f"Default: {DEFAULT_OUTPUT_ROOT_PARENT}/{DEFAULT_OUTPUT_ROOT_STEM}_<timestamp>."
        ),
    )
    parser.add_argument(
        "--plot-only",
        type=Path,
        default=None,
        metavar="RESULT_ROOT",
        help="Skip benchmark execution and regenerate mutexbench baseline PNGs from RESULT_ROOT.",
    )
    parser.add_argument(
        "--lock-profile",
        choices=experiment_defaults.lock_profile_names(),
        default=None,
        help=(
            "Named lock set to run when --locks is omitted. "
            "Default without this flag is still 'missing'. "
            f"minimal={','.join(MINIMAL_LOCKS)}; full={','.join(FULL_LOCKS)}."
        ),
    )
    parser.add_argument(
        "--locks",
        default=None,
        help=(
            "Comma-separated locks to run, or 'missing' to infer missing/incomplete experiment locks. "
            f"Default: missing unless --lock-profile is set. Supported: {','.join(REQUIRED_BASELINE_LOCKS)}."
        ),
    )
    parser.add_argument("--duration-ms", type=positive_int, default=None, help="Override fixed hot duration.")
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
        default=None,
        help=(
            f"Override CPU list for {experiment_defaults.ACCORDIN_TASKSET_LOCK}. "
            "Default runs only out=3000 ratio combos "
            "(crit=100,300,1000,3000) and computes per-combo taskset size as "
            "round(outside/critical + 1), using first NUMA node CPUs first and spilling to later NUMA nodes."
        ),
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
    if args.output_root is not None and args.plot_only is not None:
        print("--output-root cannot be used together with --plot-only.", file=sys.stderr)
        return 2
    if args.skip_plots and args.plot_only is not None:
        print("--skip-plots cannot be used together with --plot-only.", file=sys.stderr)
        return 2

    try:
        if args.plot_only is not None:
            root = parsec_common.resolve_path(args.plot_only)
            if not root.is_dir():
                print(f"Plot-only result root does not exist: {root}", file=sys.stderr)
                return 2
            logger = CommandLogger(root, command_timeout_seconds=args.command_timeout_seconds)  # type: ignore[name-defined]
            run_plots(root, logger, dry_run=args.dry_run)
            return 0

        baseline_root = parsec_common.resolve_path(args.baseline_root)
        root = parsec_common.resolve_path(args.output_root) if args.output_root is not None else default_result_root()
        matrix = fixed_baseline_matrix(
            args.threads,
            duration_ms=args.duration_ms,
            warmup_duration_ms=args.warmup_duration_ms,
        )
        locks = resolve_requested_locks(
            baseline_root,
            root,
            args.locks,
            matrix,
            force=args.force,
            lock_profile=args.lock_profile,
        )
        if not locks:
            print(f"No missing or incomplete experiment locks under {root}.")
            return 0
        print(f"Baseline root: {baseline_root}")
        print(f"Result root: {root}")
        print(f"Supplement locks: {csv_join(locks)}")
        print("Matrix source: fixed")
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
