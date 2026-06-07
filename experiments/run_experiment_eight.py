#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import math
import os
import shlex
import statistics
import subprocess
import time
from dataclasses import dataclass
from decimal import Decimal, InvalidOperation
from fractions import Fraction
from pathlib import Path
from typing import Iterable, Sequence

import experiment_defaults
import run_experiment_six as experiment_six
import run_experiment_three as experiment_three


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CPU_FRACTIONS = "1/8,1/4,1/2,1"
DEFAULT_CRITICAL_NS = experiment_defaults.MUTEXBENCH_DEFAULT_CRITICAL_NS
DEFAULT_OUTSIDE_NS = experiment_defaults.MUTEXBENCH_DEFAULT_OUTSIDE_NS
DEFAULT_DURATION_MS = experiment_defaults.MUTEXBENCH_DEFAULT_DURATION_MS
DEFAULT_WARMUP_DURATION_MS = experiment_defaults.MUTEXBENCH_DEFAULT_WARMUP_DURATION_MS
DEFAULT_REPEATS = experiment_defaults.DEFAULT_REPEATS
DEFAULT_COMMAND_TIMEOUT_SECONDS = experiment_six.DEFAULT_COMMAND_TIMEOUT_SECONDS
EXCLUDED_DEFAULT_LOCKS = {experiment_defaults.ACCORDIN_TASKSET_LOCK}
LOCK_PROFILES = {
    "minimal": tuple(lock for lock in experiment_defaults.MINIMAL_LOCKS if lock not in EXCLUDED_DEFAULT_LOCKS),
    "full": tuple(lock for lock in experiment_defaults.FULL_LOCKS if lock not in EXCLUDED_DEFAULT_LOCKS),
}
DEFAULT_LOCK_PROFILE = "full"
FALLBACK_PLOT_COLORS = (
    "C0",
    "C1",
    "C2",
    "C3",
    "C4",
    "C5",
    "C6",
    "C7",
    "C8",
    "C9",
)

RAW_FIELDS = (
    "lock",
    "lock_label",
    "threads",
    "cpu_fraction",
    "cpu_fraction_value",
    "taskset_cpu_count",
    "taskset_cpus",
    "numa_nodes",
    "critical_ns",
    "outside_ns",
    "repeat",
    "throughput_ops_per_sec",
    "throughput_per_taskset_cpu_ops_per_sec",
    "elapsed_seconds",
    "bench_wall_seconds",
    "total_operations",
    "avg_lock_hold_ns",
    "avg_wait_ns_estimated",
    "avg_lock_handoff_ns_estimated",
    "lock_hold_samples",
    "command_log",
)
SUMMARY_FIELDS = tuple(field for field in RAW_FIELDS if field not in {"repeat", "command_log"})
SUMMARY_NUMERIC_FIELDS = tuple(
    field
    for field in SUMMARY_FIELDS
    if field
    not in {
        "lock",
        "lock_label",
        "cpu_fraction",
        "taskset_cpus",
        "numa_nodes",
    }
)


@dataclass(frozen=True)
class CpuNode:
    node: int
    cpus: tuple[int, ...]


@dataclass(frozen=True)
class CpuTopology:
    source: str
    nodes: tuple[CpuNode, ...]

    def ordered_cpus(self) -> tuple[int, ...]:
        return tuple(cpu for node in self.nodes for cpu in node.cpus)

    def cpu_to_node(self) -> dict[int, int]:
        return {cpu: node.node for node in self.nodes for cpu in node.cpus}

    def cpu_count(self) -> int:
        return len(self.ordered_cpus())


@dataclass(frozen=True)
class CpuFractionSpec:
    cpu_fraction_label: str
    cpu_fraction_value: float
    cpu_count: int
    cpus: tuple[int, ...]
    cpu_list: str
    numa_nodes: str


def shlex_join(cmd: Sequence[str]) -> str:
    return shlex.join(str(part) for part in cmd)


def default_output_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment8_overload_throughput_{timestamp}"


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be > 0")
    return parsed


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("value must be >= 0")
    return parsed


def format_fraction(value: Fraction) -> str:
    if value.denominator == 1:
        return str(value.numerator)
    return f"{value.numerator}/{value.denominator}"


def parse_cpu_fractions(text: str) -> tuple[Fraction, ...]:
    values: list[Fraction] = []
    for raw in text.split(","):
        item = raw.strip()
        if not item:
            continue
        try:
            value = Fraction(item)
        except ValueError as exc:
            raise argparse.ArgumentTypeError(f"--cpu-fractions contains an invalid value: {item}") from exc
        if value <= 0:
            raise argparse.ArgumentTypeError(f"--cpu-fractions values must be > 0: {item}")
        if value > 1:
            raise argparse.ArgumentTypeError(f"--cpu-fractions values must be <= 1: {item}")
        values.append(value)
    if not values:
        raise argparse.ArgumentTypeError("--cpu-fractions must contain at least one value")
    return tuple(dict.fromkeys(values))


def parse_csv_strings(text: str) -> tuple[str, ...]:
    return tuple(item.strip() for item in text.split(",") if item.strip())


def lock_profile_names() -> tuple[str, ...]:
    return tuple(LOCK_PROFILES)


def lock_profile_locks(profile: str) -> tuple[str, ...]:
    try:
        return LOCK_PROFILES[profile]
    except KeyError as exc:
        supported = ", ".join(lock_profile_names())
        raise ValueError(f"Unsupported lock profile: {profile}. Supported profiles: {supported}") from exc


def parse_locks(text: str | None, profile: str) -> tuple[str, ...]:
    raw_locks = lock_profile_locks(profile) if text is None else parse_csv_strings(text)
    locks: list[str] = []
    for raw in raw_locks:
        lock = experiment_six.normalize_lock(raw)
        if lock not in experiment_six.SUPPORTED_LOCKS:
            supported = sorted(experiment_six.SUPPORTED_LOCKS | set(experiment_six.LOCAL_LOCK_ALIASES))
            raise argparse.ArgumentTypeError(
                f"Unsupported experiment8 lock {raw!r}. Supported locks: {', '.join(supported)}"
            )
        if lock not in locks:
            locks.append(lock)
    if not locks:
        raise argparse.ArgumentTypeError("At least one lock must be selected")
    return tuple(locks)


def topology_from_node_cpus(source: str, node_cpus: dict[int, Iterable[int]]) -> CpuTopology:
    nodes: list[CpuNode] = []
    for node, cpus in sorted(node_cpus.items()):
        cpu_list = tuple(sorted(set(cpus)))
        if cpu_list:
            nodes.append(CpuNode(node=node, cpus=cpu_list))
    if not nodes:
        raise RuntimeError("CPU topology did not contain any usable CPUs")
    return CpuTopology(source=source, nodes=tuple(nodes))


def detect_logical_cpu_topology() -> CpuTopology:
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
            if node < 0:
                node = 0
            node_cpus.setdefault(node, []).append(cpu)
    if node_cpus:
        return topology_from_node_cpus("lscpu -p=CPU,NODE,ONLINE logical CPUs", node_cpus)

    cpu_count = os.cpu_count() or 1
    return topology_from_node_cpus("fallback:os.cpu_count logical CPUs", {0: range(cpu_count)})


def format_cpu_list(cpus: Iterable[int]) -> str:
    return ",".join(str(cpu) for cpu in cpus)


def numa_nodes_for_cpus(topology: CpuTopology, cpus: Iterable[int]) -> str:
    cpu_to_node = topology.cpu_to_node()
    return ";".join(str(node) for node in sorted({cpu_to_node[cpu] for cpu in cpus if cpu in cpu_to_node}))


def cpu_fraction_specs(
    fractions: Iterable[Fraction],
    topology: CpuTopology,
    *,
    threads: int,
) -> tuple[CpuFractionSpec, ...]:
    ordered = topology.ordered_cpus()
    if not ordered:
        raise RuntimeError("CPU topology did not contain any usable CPUs")
    specs: list[CpuFractionSpec] = []
    for fraction in fractions:
        cpu_count = max(1, math.ceil(threads * fraction))
        cpu_count = min(cpu_count, len(ordered))
        cpus = ordered[:cpu_count]
        specs.append(
            CpuFractionSpec(
                cpu_fraction_label=format_fraction(fraction),
                cpu_fraction_value=float(fraction),
                cpu_count=cpu_count,
                cpus=cpus,
                cpu_list=format_cpu_list(cpus),
                numa_nodes=numa_nodes_for_cpus(topology, cpus),
            )
        )
    return tuple(specs)


def lock_label(lock: str) -> str:
    return experiment_six.lock_label(lock)


def mutexbench_command(
    lock: str,
    threads: int,
    cpu_spec: CpuFractionSpec,
    args: argparse.Namespace,
) -> tuple[list[str], dict[str, str | None], bool]:
    if experiment_six.is_mcs_accordin_lock(lock):
        lock_kind = experiment_six.MCS_ACCORDIN_DIRECT_LOCK_KIND
        env = experiment_six.mcs_accordin_env()
        needs_sudo = True
        cmd_prefix: list[str] = []
        timeslice_extension = "off"
    elif experiment_six.is_flexguard_interpose_lock(lock) or experiment_six.is_otherlocks_interpose_lock(lock):
        lock_kind = "mutex"
        env = {}
        needs_sudo = (
            experiment_six.flexguard_interpose_needs_sudo(lock)
            if experiment_six.is_flexguard_interpose_lock(lock)
            else False
        )
        script = (
            experiment_six.flexguard_interpose_script(lock)
            if experiment_six.is_flexguard_interpose_lock(lock)
            else experiment_six.otherlocks_interpose_script(lock)
        )
        cmd_prefix = [str(script)]
        timeslice_extension = (
            experiment_six.flexguard_timeslice_extension(lock)
            if experiment_six.is_flexguard_interpose_lock(lock)
            else "off"
        )
    else:
        lock_kind = experiment_six.BUILTIN_LOCK_KINDS.get(lock, experiment_six.ACCORDIN_DIRECT_LOCK_KIND)
        env = experiment_six.accordin_env(lock) if experiment_six.is_accordin_direct_lock(lock) else {}
        needs_sudo = experiment_six.is_accordin_direct_lock(lock)
        cmd_prefix = []
        timeslice_extension = "off"

    cmd = [
        *cmd_prefix,
        str(experiment_six.MUTEX_BENCH),
        "--threads",
        str(threads),
        "--duration-ms",
        str(args.duration_ms),
        "--warmup-duration-ms",
        str(args.warmup_duration_ms),
        "--critical-ns",
        str(args.critical_ns),
        "--outside-ns",
        str(args.outside_ns),
        "--lock-kind",
        lock_kind,
        "--timeslice-extension",
        timeslice_extension,
    ]
    return ["taskset", "-c", cpu_spec.cpu_list, *cmd], env, needs_sudo


def read_existing_keys(raw_path: Path) -> set[tuple[str, str, int, int, int, int]]:
    if not raw_path.is_file():
        return set()
    keys: set[tuple[str, str, int, int, int, int]] = set()
    with raw_path.open(newline="", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        for row in reader:
            keys.add(
                (
                    row["lock"],
                    row["cpu_fraction"],
                    int(row["critical_ns"]),
                    int(row["outside_ns"]),
                    int(row["threads"]),
                    int(row["repeat"]),
                )
            )
    return keys


def run_one(
    lock: str,
    threads: int,
    cpu_spec: CpuFractionSpec,
    repeat: int,
    args: argparse.Namespace,
    logs_dir: Path,
) -> dict[str, str]:
    cmd, env, needs_sudo = mutexbench_command(lock, threads, cpu_spec, args)
    run_cmd = experiment_six.env_command(cmd, env, needs_sudo=needs_sudo, sudo_mode=args.sudo_mode)
    log_path = (
        logs_dir
        / f"{lock}_f{cpu_spec.cpu_fraction_label.replace('/', '_')}_k{cpu_spec.cpu_count}_t{threads}_r{repeat}.log"
    )

    start_ns = time.time_ns()
    try:
        completed = subprocess.run(
            run_cmd,
            check=False,
            cwd=REPO_ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=args.command_timeout_seconds,
        )
        output = completed.stdout
        status = completed.returncode
    except subprocess.TimeoutExpired as exc:
        output = exc.stdout or ""
        status = 124
    end_ns = time.time_ns()
    bench_wall_seconds = (end_ns - start_ns) / 1_000_000_000.0

    log_path.write_text("$ " + shlex_join(run_cmd) + "\n\n" + output, encoding="utf-8")
    if status != 0:
        raise RuntimeError(
            f"benchmark failed for lock={lock} fraction={cpu_spec.cpu_fraction_label} "
            f"threads={threads} repeat={repeat}; see {log_path}"
        )

    metrics = experiment_six.parse_metrics(output)
    if not metrics.get("throughput_ops_per_sec"):
        raise RuntimeError(f"benchmark output missing throughput_ops_per_sec; see {log_path}")

    throughput = float(metrics["throughput_ops_per_sec"])
    row = {
        "lock": lock,
        "lock_label": lock_label(lock),
        "threads": str(threads),
        "cpu_fraction": cpu_spec.cpu_fraction_label,
        "cpu_fraction_value": f"{cpu_spec.cpu_fraction_value:.12f}",
        "taskset_cpu_count": str(cpu_spec.cpu_count),
        "taskset_cpus": cpu_spec.cpu_list,
        "numa_nodes": cpu_spec.numa_nodes,
        "critical_ns": str(args.critical_ns),
        "outside_ns": str(args.outside_ns),
        "repeat": str(repeat),
        "throughput_ops_per_sec": metrics["throughput_ops_per_sec"],
        "throughput_per_taskset_cpu_ops_per_sec": f"{throughput / cpu_spec.cpu_count:.6f}",
        "bench_wall_seconds": f"{bench_wall_seconds:.6f}",
        "command_log": str(log_path),
    }
    for field in RAW_FIELDS:
        if field not in row:
            row[field] = metrics.get(field, "")
    return row


def write_settings(root: Path, args: argparse.Namespace, topology: CpuTopology, specs: Sequence[CpuFractionSpec]) -> None:
    settings = {
        "experiment": "experiment8_overload_throughput",
        "description": "Single-lock mutexbench throughput with threads fixed to logical CPU count and external taskset CPU fractions.",
        "output_root": str(root),
        "machine_profile": experiment_defaults.ACTIVE_MACHINE_CONFIG.name,
        "machine_config_physical_cores": experiment_defaults.MACHINE_CORE_COUNT,
        "detected_logical_cpus": topology.cpu_count(),
        "threads": args.threads,
        "lock_profile": args.lock_profile,
        "lock_profile_source": args.lock_profile_source,
        "locks": list(args.lock_keys),
        "cpu_fractions": [format_fraction(fraction) for fraction in args.cpu_fractions],
        "cpu_fraction_specs": [
            {
                "cpu_fraction": spec.cpu_fraction_label,
                "cpu_fraction_value": spec.cpu_fraction_value,
                "taskset_cpu_count": spec.cpu_count,
                "taskset_cpus": spec.cpu_list,
                "numa_nodes": spec.numa_nodes,
            }
            for spec in specs
        ],
        "topology": {
            "source": topology.source,
            "nodes": [{"node": node.node, "cpus": list(node.cpus)} for node in topology.nodes],
        },
        "critical_ns": args.critical_ns,
        "outside_ns": args.outside_ns,
        "duration_ms": args.duration_ms,
        "warmup_duration_ms": args.warmup_duration_ms,
        "repeats": args.repeats,
        "sudo_mode": args.sudo_mode,
        "command_timeout_seconds": args.command_timeout_seconds,
    }
    (root / "settings.json").write_text(json.dumps(settings, indent=2) + "\n", encoding="utf-8")


def write_summary(raw_path: Path, summary_path: Path) -> None:
    with raw_path.open(newline="", encoding="utf-8") as f:
        rows = list(csv.DictReader(f))
    groups: dict[tuple[str, str, str, str, str], list[dict[str, str]]] = {}
    for row in rows:
        groups.setdefault(
            (row["lock"], row["cpu_fraction"], row["threads"], row["critical_ns"], row["outside_ns"]),
            [],
        ).append(row)

    with summary_path.open("w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=SUMMARY_FIELDS, lineterminator="\n")
        writer.writeheader()
        for key in sorted(
            groups,
            key=lambda item: (
                experiment_defaults.lock_sort_key(item[0]),
                float(groups[item][0]["cpu_fraction_value"]),
                int(item[2]),
                int(item[3]),
                int(item[4]),
            ),
        ):
            group_rows = groups[key]
            first = group_rows[0]
            out: dict[str, str] = {
                "lock": first["lock"],
                "lock_label": first["lock_label"],
                "threads": first["threads"],
                "cpu_fraction": first["cpu_fraction"],
                "taskset_cpus": first["taskset_cpus"],
                "numa_nodes": first["numa_nodes"],
                "critical_ns": first["critical_ns"],
                "outside_ns": first["outside_ns"],
            }
            for field in SUMMARY_NUMERIC_FIELDS:
                values = [float(row[field]) for row in group_rows if row.get(field) not in ("", None)]
                out[field] = f"{statistics.mean(values):.6f}" if values else ""
            writer.writerow(out)


def safe_name(value: str) -> str:
    out = "".join(ch.lower() if ch.isalnum() else "_" for ch in value.strip())
    return "_".join(part for part in out.split("_") if part) or "unknown"


def plot_color_map(lock_keys: Iterable[str]) -> dict[str, str]:
    color_keys = tuple(dict.fromkeys(experiment_defaults.normalize_lock(lock) for lock in lock_keys))
    return {
        lock: FALLBACK_PLOT_COLORS[index % len(FALLBACK_PLOT_COLORS)]
        for index, lock in enumerate(color_keys)
    }


def lock_plot_style(lock: str, color_by_key: dict[str, str]) -> dict[str, str]:
    style_key = experiment_defaults.normalize_lock(lock)
    return {
        "color": experiment_three.plot_color(style_key, color_by_key[style_key]),
        "linestyle": experiment_three.plot_linestyle(style_key),
        "marker": experiment_three.plot_marker(style_key),
    }


BAR_HATCHES = ("", "//", "\\\\", "xx", "..", "++", "--", "oo", "**", "OO")


def load_summary_rows(summary_path: Path) -> list[dict[str, str]]:
    if not summary_path.is_file():
        raise RuntimeError(f"{summary_path} does not exist")
    with summary_path.open(newline="", encoding="utf-8") as f:
        return list(csv.DictReader(f))


def summary_decimal(row: dict[str, str], field: str) -> Decimal:
    value = row.get(field, "")
    try:
        return Decimal(value.strip())
    except (AttributeError, InvalidOperation) as exc:
        raise ValueError(f"summary row has invalid {field}: {value!r}") from exc


def format_plot_number(value: Decimal) -> str:
    if value == 0:
        return "0"
    if value == value.to_integral_value():
        return str(value.quantize(Decimal(1)))
    text = format(value.normalize(), "f")
    return text.rstrip("0").rstrip(".") or "0"


def summary_workload_key(row: dict[str, str]) -> tuple[Decimal, Decimal]:
    return summary_decimal(row, "critical_ns"), summary_decimal(row, "outside_ns")


def generate_plots(result_root: Path) -> tuple[Path, ...]:
    try:
        import matplotlib
    except ImportError as exc:
        raise RuntimeError("matplotlib is required to generate experiment8 plots") from exc

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    rows = load_summary_rows(result_root / "summary.csv")
    rows_by_workload: dict[tuple[Decimal, Decimal], list[dict[str, str]]] = {}
    for row in rows:
        rows_by_workload.setdefault(summary_workload_key(row), []).append(row)
    color_by_key = plot_color_map(row["lock"] for row in rows)
    plot_paths: list[Path] = []
    for critical_value, outside_value in sorted(rows_by_workload):
        critical_ns = format_plot_number(critical_value)
        outside_ns = format_plot_number(outside_value)
        combo_rows = rows_by_workload[(critical_value, outside_value)]
        locks = tuple(dict.fromkeys(row["lock"] for row in combo_rows))
        fig, ax = plt.subplots(figsize=(9.2, 5.3))
        category_keys = sorted(
            {
                (
                    summary_decimal(row, "cpu_fraction_value"),
                    summary_decimal(row, "taskset_cpu_count"),
                    row["cpu_fraction"],
                )
                for row in combo_rows
            },
            key=lambda item: (item[0], item[1], item[2]),
        )
        category_index = {key: index for index, key in enumerate(category_keys)}
        group_width = 0.82
        bar_width = group_width / max(len(locks), 1)
        for lock_index, lock in enumerate(locks):
            lock_rows = [row for row in combo_rows if row["lock"] == lock]
            if not lock_rows:
                continue
            xs = [
                category_index[
                    (
                        summary_decimal(row, "cpu_fraction_value"),
                        summary_decimal(row, "taskset_cpu_count"),
                        row["cpu_fraction"],
                    )
                ]
                + (lock_index - (len(locks) - 1) / 2) * bar_width
                for row in lock_rows
            ]
            ys = [float(row["throughput_ops_per_sec"]) / 1_000_000.0 for row in lock_rows]
            label = lock_rows[0].get("lock_label") or lock
            style = lock_plot_style(lock, color_by_key)
            ax.bar(
                xs,
                ys,
                width=bar_width * 0.92,
                color=style["color"],
                edgecolor="#333333",
                linewidth=0.45,
                hatch=BAR_HATCHES[lock_index % len(BAR_HATCHES)],
                label=label,
            )
        xtick_labels = [format_plot_number(cpu_count) for _, cpu_count, _ in category_keys]
        ax.set_title(f"Overload Throughput, critical={critical_ns} ns, outside={outside_ns} ns")
        ax.set_xlabel("Taskset logical CPUs")
        ax.set_ylabel("Throughput (M ops/s)")
        ax.set_xticks(range(len(category_keys)), xtick_labels)
        ax.set_xlim(-0.5, max(len(category_keys) - 0.5, 0.5))
        ax.set_axisbelow(True)
        ax.grid(True, axis="y", alpha=0.28)
        if locks:
            ax.legend(loc="best", fontsize=8, frameon=False)
        fig.tight_layout()
        output_path = result_root / f"throughput_vs_taskset_cpus_c{safe_name(critical_ns)}_o{safe_name(outside_ns)}.png"
        fig.savefig(output_path, dpi=180)
        plt.close(fig)
        plot_paths.append(output_path)
    return tuple(plot_paths)


def regenerate_summary_and_plots(result_root: Path) -> tuple[Path, tuple[Path, ...]]:
    raw_path = result_root / "raw.csv"
    summary_path = result_root / "summary.csv"
    if not raw_path.is_file():
        raise RuntimeError(f"{raw_path} does not exist")
    write_summary(raw_path, summary_path)
    return summary_path, generate_plots(result_root)


def prepare_output(root: Path, *, force: bool, resume: bool) -> tuple[Path, Path, Path]:
    raw_path = root / "raw.csv"
    summary_path = root / "summary.csv"
    logs_dir = root / "logs"
    if raw_path.exists() and not (force or resume):
        raise RuntimeError(f"{raw_path} already exists; use --force to replace or --resume to append missing rows")
    if force:
        for path in (raw_path, summary_path, root / "settings.json"):
            path.unlink(missing_ok=True)
    root.mkdir(parents=True, exist_ok=True)
    logs_dir.mkdir(parents=True, exist_ok=True)
    return raw_path, summary_path, logs_dir


def dry_run(args: argparse.Namespace, topology: CpuTopology, specs: Sequence[CpuFractionSpec]) -> None:
    print("raw_fields: " + ",".join(RAW_FIELDS))
    print("summary_fields: " + ",".join(SUMMARY_FIELDS))
    print(f"topology: source={topology.source} logical_cpus={topology.cpu_count()}")
    experiment_six.ensure_builds(args.lock_keys, dry_run=True)
    for lock in args.lock_keys:
        for spec in specs:
            cmd, env, needs_sudo = mutexbench_command(lock, args.threads, spec, args)
            run_cmd = experiment_six.env_command(cmd, env, needs_sudo=needs_sudo, sudo_mode=args.sudo_mode)
            print(
                f"lock={lock} threads={args.threads} fraction={spec.cpu_fraction_label} "
                f"taskset_cpus={spec.cpu_list} repeat=1 {shlex_join(run_cmd)}"
            )


def run_experiment(args: argparse.Namespace, topology: CpuTopology, specs: Sequence[CpuFractionSpec]) -> Path:
    root = args.output_root
    raw_path, summary_path, logs_dir = prepare_output(root, force=args.force, resume=args.resume)
    write_settings(root, args, topology, specs)
    experiment_six.ensure_builds(args.lock_keys, dry_run=False)
    existing = read_existing_keys(raw_path) if args.resume else set()
    raw_exists = raw_path.exists() and args.resume

    with raw_path.open("a" if raw_exists else "w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=RAW_FIELDS, lineterminator="\n")
        if not raw_exists:
            writer.writeheader()
        for lock in args.lock_keys:
            for spec in specs:
                for repeat in range(1, args.repeats + 1):
                    key = (lock, spec.cpu_fraction_label, args.critical_ns, args.outside_ns, args.threads, repeat)
                    if key in existing:
                        print(
                            f"Skipping complete run: lock={lock} fraction={spec.cpu_fraction_label} "
                            f"threads={args.threads} repeat={repeat}"
                        )
                        continue
                    print(
                        f"Running lock={lock} fraction={spec.cpu_fraction_label} "
                        f"taskset_cpus={spec.cpu_count} threads={args.threads} repeat={repeat}"
                    )
                    row = run_one(lock, args.threads, spec, repeat, args, logs_dir)
                    writer.writerow({field: row.get(field, "") for field in RAW_FIELDS})
                    f.flush()

    write_summary(raw_path, summary_path)
    return root


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Experiment 8: overload throughput with fixed logical-thread count and taskset CPU fractions.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"""\
Default benchmark settings:
  lock-profile={DEFAULT_LOCK_PROFILE}
  threads=auto-detected logical CPU count
  cpu-fractions={DEFAULT_CPU_FRACTIONS}
  critical-ns={DEFAULT_CRITICAL_NS}, outside-ns={DEFAULT_OUTSIDE_NS}
  warmup={DEFAULT_WARMUP_DURATION_MS // 1000}s, duration={DEFAULT_DURATION_MS // 1000}s
  repeats={DEFAULT_REPEATS}

Examples:
  python3 experiments/run_experiment_eight.py
  python3 experiments/run_experiment_eight.py --locks mutex,mcs,flexguard,accordin --repeats 1
  python3 experiments/run_experiment_eight.py --cpu-fractions 1/4,1/2,1 --duration-ms 2000
""",
    )
    parser.add_argument("--output-root", type=Path, default=None)
    parser.add_argument("--plot-only", type=Path, default=None, metavar="RESULT_ROOT", help="Regenerate summary.csv and PNGs from raw.csv.")
    parser.add_argument("--skip-plots", action="store_true", help="Do not generate PNG plots after running benchmarks.")
    parser.add_argument("--force", action="store_true", help="Replace existing raw/summary/settings files")
    parser.add_argument("--resume", action="store_true", help="Skip complete raw rows already present")
    parser.add_argument("--dry-run", action="store_true", help="Print commands and CSV schema without running")
    parser.add_argument("--lock-profile", choices=lock_profile_names(), default=DEFAULT_LOCK_PROFILE)
    parser.add_argument("--locks", help="Comma-separated lock list. Default comes from --lock-profile.")
    parser.add_argument("--threads", type=positive_int, default=None, help="Worker threads. Default is the detected logical CPU count.")
    parser.add_argument("--cpu-fractions", default=DEFAULT_CPU_FRACTIONS, help=f"Comma-separated taskset CPU fractions. Default: {DEFAULT_CPU_FRACTIONS}.")
    parser.add_argument("--critical-ns", type=positive_int, default=DEFAULT_CRITICAL_NS)
    parser.add_argument("--outside-ns", type=non_negative_int, default=DEFAULT_OUTSIDE_NS)
    parser.add_argument("--duration-ms", type=positive_int, default=DEFAULT_DURATION_MS)
    parser.add_argument("--warmup-duration-ms", type=non_negative_int, default=DEFAULT_WARMUP_DURATION_MS)
    parser.add_argument("--repeats", type=positive_int, default=DEFAULT_REPEATS)
    parser.add_argument("--sudo-mode", choices=("all", "auto", "none"), default="auto")
    parser.add_argument("--command-timeout-seconds", type=positive_int, default=DEFAULT_COMMAND_TIMEOUT_SECONDS)
    args = parser.parse_args()

    try:
        args.cpu_fractions = parse_cpu_fractions(args.cpu_fractions)
        args.lock_keys = parse_locks(args.locks, args.lock_profile)
    except argparse.ArgumentTypeError as exc:
        parser.error(str(exc))
    args.lock_profile_source = "manual" if args.locks is not None else "profile"
    args.output_root = args.output_root or default_output_root()
    return args


def main() -> int:
    args = parse_args()
    if args.plot_only is not None:
        summary_path, plot_paths = regenerate_summary_and_plots(args.plot_only)
        print(f"Summary results: {summary_path}")
        for path in plot_paths:
            print(f"Plot: {path}")
        return 0

    topology = detect_logical_cpu_topology()
    if args.threads is None:
        args.threads = topology.cpu_count()
    specs = cpu_fraction_specs(args.cpu_fractions, topology, threads=args.threads)
    if args.dry_run:
        dry_run(args, topology, specs)
        return 0

    root = run_experiment(args, topology, specs)
    print(f"Raw results: {root / 'raw.csv'}")
    print(f"Summary results: {root / 'summary.csv'}")
    if not args.skip_plots:
        for path in generate_plots(root):
            print(f"Plot: {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
