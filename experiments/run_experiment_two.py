#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import os
import shlex
import subprocess
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from statistics import mean

import experiment_defaults
import experiment_failures

REPO_ROOT = Path(__file__).resolve().parents[1]
MUTEXBENCH_DIR = REPO_ROOT / "bench" / "mutexbench"
SWEEP_SCRIPT = MUTEXBENCH_DIR / "scripts" / "sweep_mutex_throughput.sh"
MCS_TSE_RELEASE_LIB = REPO_ROOT / "target" / "release" / "libmcs_tse.so"
MCS_TSE_DEBUG_LIB = REPO_ROOT / "target" / "debug" / "libmcs_tse.so"

DEFAULT_LOCKS = experiment_defaults.EXPERIMENT_TWO_DEFAULT_LOCKS
DEFAULT_CRITICAL_NS = experiment_defaults.MUTEXBENCH_DEFAULT_CRITICAL_NS
DEFAULT_OUTSIDE_NS = experiment_defaults.MUTEXBENCH_DEFAULT_OUTSIDE_NS
DEFAULT_DURATION_MS = experiment_defaults.MUTEXBENCH_DEFAULT_DURATION_MS
DEFAULT_WARMUP_DURATION_MS = experiment_defaults.MUTEXBENCH_DEFAULT_WARMUP_DURATION_MS
DEFAULT_REPEATS = experiment_defaults.MUTEXBENCH_DEFAULT_REPEATS
DEFAULT_COMMAND_TIMEOUT_SECONDS = 30
COMMAND_TIMEOUT_KILL_AFTER_SECONDS = 60
EXPERIMENT_TWO_STYLE_THREADS = experiment_defaults.EXPERIMENT_TWO_STYLE_THREADS

RAW_FIELDS = (
    "lock",
    "lock_label",
    "threads",
    "cpu_count",
    "cpu_list",
    "numa_nodes",
    "critical_ns",
    "outside_ns",
    "repeat",
    "throughput_ops_per_sec",
    "elapsed_seconds",
    "total_operations",
    "avg_lock_hold_ns",
    "avg_wait_ns_estimated",
    "avg_lock_handoff_ns_estimated",
    "lock_hold_samples",
    "avg_cpu_pct",
    "source_raw",
    "command_log",
)

SUMMARY_FIELDS = (
    "lock",
    "lock_label",
    "threads",
    "cpu_count",
    "cpu_list",
    "numa_nodes",
    "critical_ns",
    "outside_ns",
    "runs",
    "mean_throughput_ops_per_sec",
    "mean_elapsed_seconds",
    "mean_total_operations",
    "mean_avg_lock_hold_ns",
    "mean_avg_wait_ns_estimated",
    "mean_avg_lock_handoff_ns_estimated",
    "mean_lock_hold_samples",
    "mean_avg_cpu_pct",
)

FALLBACK_TOPOLOGIES: dict[str, dict[int, tuple[int, ...]]] = {
    "current-20c40t": {
        0: tuple(range(0, 40, 2)),
        1: tuple(range(1, 40, 2)),
    },
    "current-48c96t": {
        0: tuple(range(0, 96, 2)),
        1: tuple(range(1, 96, 2)),
    },
}

TOPOLOGY_ALIASES = {
    "auto": "auto",
    "current": "auto",
    "local": "auto",
    "small": "current-20c40t",
    "20c40t": "current-20c40t",
    "current-20c40t": "current-20c40t",
    "large": "current-48c96t",
    "original": "current-48c96t",
    "48c96t": "current-48c96t",
    "96cpu": "current-48c96t",
    "current-48c96t": "current-48c96t",
}


@dataclass(frozen=True)
class NodeSpec:
    node: int
    cpus: tuple[int, ...]


@dataclass(frozen=True)
class CpuTopology:
    name: str
    nodes: tuple[NodeSpec, ...]
    source: str

    def ordered_cpus(self) -> tuple[int, ...]:
        return tuple(cpu for node in self.nodes for cpu in node.cpus)

    def cpu_to_node(self) -> dict[int, int]:
        return {cpu: node.node for node in self.nodes for cpu in node.cpus}

    def first_node_count(self) -> int:
        return len(self.nodes[0].cpus) if self.nodes else 0

    def max_threads(self) -> int:
        return len(self.ordered_cpus())


@dataclass(frozen=True)
class LockSpec:
    key: str
    label: str
    mode: str
    lock_kind: str
    timeslice_extension: str = "off"
    preload_library: Path | None = None


@dataclass(frozen=True)
class CommandResult:
    log_path: Path


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
    ) -> None:
        self.result_root = result_root
        self.log_dir = result_root / "logs"
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.manifest_path = result_root / "commands.json"
        self.records = self.load_records() if resume else []
        self.command_timeout_seconds = command_timeout_seconds

    def load_records(self) -> list[dict[str, object]]:
        if not self.manifest_path.is_file():
            return []
        with self.manifest_path.open("r", encoding="utf-8") as f:
            records = json.load(f)
        if not isinstance(records, list):
            raise RuntimeError(f"commands.json is not a command-record list: {self.manifest_path}")
        return records

    def run(
        self,
        cmd: list[str],
        *,
        log_name: str,
        cwd: Path = REPO_ROOT,
        env: dict[str, str] | None = None,
        dry_run: bool = False,
        timeout_seconds: int | None = None,
    ) -> CommandResult:
        effective_timeout = self.command_timeout_seconds if timeout_seconds is None else timeout_seconds
        run_cmd = wrap_command_timeout(cmd, effective_timeout)
        log_path = self.log_dir / log_name
        record: dict[str, object] = {
            "command": run_cmd,
            "command_text": shlex.join(run_cmd),
            "cwd": str(cwd),
            "log_path": str(log_path),
            "dry_run": dry_run,
            "started_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        }
        if effective_timeout > 0:
            record["inner_command"] = cmd
            record["command_timeout_seconds"] = effective_timeout
        if env:
            record["env_overrides"] = dict(sorted(env.items()))

        if dry_run:
            print(shlex.join(run_cmd))
            record["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
            record["returncode"] = 0
            self.records.append(record)
            self.write_manifest()
            return CommandResult(log_path=log_path)

        run_env = os.environ.copy()
        if env:
            run_env.update(env)

        with log_path.open("w", encoding="utf-8") as log_file:
            log_file.write(f"cwd: {cwd}\n")
            log_file.write(f"command: {shlex.join(run_cmd)}\n")
            if effective_timeout > 0:
                log_file.write(f"inner_command: {shlex.join(cmd)}\n")
                log_file.write(f"command_timeout_seconds: {effective_timeout}\n")
            log_file.write(f"started_at: {record['started_at']}\n\n")
            if env:
                log_file.write("env_overrides:\n")
                for key, value in sorted(env.items()):
                    log_file.write(f"  {key}={value}\n")
                log_file.write("\n")
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

            finished_at = dt.datetime.now(dt.timezone.utc).isoformat()
            log_file.write(f"\nfinished_at: {finished_at}\n")
            log_file.write(f"returncode: {returncode}\n")

        record["finished_at"] = finished_at
        record["returncode"] = returncode
        self.records.append(record)
        self.write_manifest()
        if returncode != 0:
            raise CommandError(
                f"Command failed with exit code {returncode}: {shlex.join(run_cmd)}",
                returncode,
                log_path,
            )
        return CommandResult(log_path=log_path)

    def write_manifest(self) -> None:
        with self.manifest_path.open("w", encoding="utf-8") as f:
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


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run experiment two: mutexbench with NUMA-first fixed CPU affinity.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"""\
Default benchmark settings:
  locks={','.join(DEFAULT_LOCKS)}
  critical-ns={DEFAULT_CRITICAL_NS}, outside-ns={DEFAULT_OUTSIDE_NS}
  duration-ms={DEFAULT_DURATION_MS}, warmup-duration-ms={DEFAULT_WARMUP_DURATION_MS}, repeats={DEFAULT_REPEATS}
  CPU policy: use all online CPUs from the first NUMA node first, then later NUMA nodes.

Examples:
  python3 experiments/run_experiment_two.py
  python3 experiments/run_experiment_two.py --threads 2,4,8,16,24,32,40,48,56,64,72,80,88,96
  python3 experiments/run_experiment_two.py --topology-profile current-20c40t
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
        "--resume",
        action="store_true",
        help="Continue an existing --output-root by skipping lock/thread points already present in raw.csv.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Write settings and commands.json, print benchmark commands, but do not execute them.",
    )
    parser.add_argument(
        "--build-missing",
        action="store_true",
        help="Build missing mcs_tse preload library before running.",
    )
    parser.add_argument(
        "--command-timeout-seconds",
        type=non_negative_int,
        default=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        help=(
            "Outer timeout for each benchmark command. 0 disables it. "
            f"Default: {DEFAULT_COMMAND_TIMEOUT_SECONDS}."
        ),
    )
    parser.add_argument(
        "--locks",
        default=",".join(DEFAULT_LOCKS),
        metavar="CSV",
        help=(
            "Comma-separated locks. Default: mcs_tse,mcs_tas. "
            "Supported: mcs_tse, mcs_tas/mcs-tas/mcstas, mcs_extension."
        ),
    )
    parser.add_argument(
        "--threads",
        default=None,
        metavar="CSV",
        help=(
            "Comma-separated thread counts. Default: experiment-one style powers of two, "
            "plus the first-NUMA-node CPU count and the machine CPU count."
        ),
    )
    parser.add_argument(
        "--topology-profile",
        default="auto",
        choices=sorted(TOPOLOGY_ALIASES),
        help=(
            "CPU topology source. Default auto reads lscpu. "
            "Forced profiles are fallback CPU layouts for the two supported machines."
        ),
    )
    parser.add_argument(
        "--critical-ns",
        type=positive_int,
        default=DEFAULT_CRITICAL_NS,
        help=f"Mutexbench critical-section burn time in ns. Default: {DEFAULT_CRITICAL_NS}.",
    )
    parser.add_argument(
        "--outside-ns",
        type=positive_int,
        default=DEFAULT_OUTSIDE_NS,
        help=f"Mutexbench outside-section burn time in ns. Default: {DEFAULT_OUTSIDE_NS}.",
    )
    parser.add_argument(
        "--duration-ms",
        type=positive_int,
        default=DEFAULT_DURATION_MS,
        help=f"Measurement duration per repeat. Default: {DEFAULT_DURATION_MS}.",
    )
    parser.add_argument(
        "--warmup-duration-ms",
        type=non_negative_int,
        default=DEFAULT_WARMUP_DURATION_MS,
        help=f"Warmup duration per repeat. Default: {DEFAULT_WARMUP_DURATION_MS}.",
    )
    parser.add_argument(
        "--repeats",
        type=positive_int,
        default=DEFAULT_REPEATS,
        help=f"Repeats per lock/thread point. Default: {DEFAULT_REPEATS}.",
    )
    parser.add_argument(
        "--timing-sample-stride",
        type=positive_int,
        default=8,
        help="Forwarded mutexbench timing sample stride. Default: 8.",
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
    return tuple(dict.fromkeys(items))


def normalize_topology_profile(value: str) -> str:
    try:
        return TOPOLOGY_ALIASES[value]
    except KeyError as exc:
        raise ValueError(f"Unsupported topology profile: {value}") from exc


def topology_from_node_cpus(name: str, node_cpus: dict[int, tuple[int, ...]], source: str) -> CpuTopology:
    nodes = tuple(
        NodeSpec(node=node, cpus=tuple(sorted(cpus)))
        for node, cpus in sorted(node_cpus.items())
        if cpus
    )
    if not nodes:
        raise RuntimeError("CPU topology did not contain any online CPUs.")
    return CpuTopology(name=name, nodes=nodes, source=source)


def detect_lscpu_topology() -> CpuTopology | None:
    try:
        completed = subprocess.run(
            ["lscpu", "-p=CPU,NODE,ONLINE"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except OSError:
        return None
    if completed.returncode != 0:
        return None

    node_cpus: dict[int, list[int]] = defaultdict(list)
    for line in completed.stdout.splitlines():
        if not line or line.startswith("#"):
            continue
        parts = line.split(",")
        if len(parts) != 3:
            continue
        cpu_text, node_text, online = parts
        if online.strip().upper() not in {"Y", "YES", "1"}:
            continue
        try:
            cpu = int(cpu_text)
            node = int(node_text)
        except ValueError:
            continue
        node_cpus[node].append(cpu)

    if not node_cpus:
        return None
    return topology_from_node_cpus(
        name="auto",
        node_cpus={node: tuple(cpus) for node, cpus in node_cpus.items()},
        source="lscpu -p=CPU,NODE,ONLINE",
    )


def fallback_auto_topology() -> CpuTopology:
    logical_cpus = os.cpu_count() or 0
    profile = "current-20c40t" if logical_cpus and logical_cpus <= 40 else "current-48c96t"
    return topology_from_node_cpus(
        name=profile,
        node_cpus=FALLBACK_TOPOLOGIES[profile],
        source="fallback-profile",
    )


def select_topology(profile_arg: str) -> CpuTopology:
    profile = normalize_topology_profile(profile_arg)
    if profile == "auto":
        detected = detect_lscpu_topology()
        if detected is not None:
            return detected
        return fallback_auto_topology()
    return topology_from_node_cpus(
        name=profile,
        node_cpus=FALLBACK_TOPOLOGIES[profile],
        source=f"forced-profile:{profile}",
    )


def default_thread_counts(topology: CpuTopology) -> tuple[int, ...]:
    max_threads = topology.max_threads()
    first_node_count = topology.first_node_count()
    candidates = [thread for thread in EXPERIMENT_TWO_STYLE_THREADS if thread <= max_threads]
    candidates.extend([first_node_count, max_threads])
    return tuple(sorted({thread for thread in candidates if 1 <= thread <= max_threads}))


def cpu_list_for_threads(topology: CpuTopology, threads: int) -> tuple[int, ...]:
    ordered = topology.ordered_cpus()
    if threads > len(ordered):
        raise ValueError(
            f"threads={threads} exceeds available CPU count {len(ordered)} "
            f"for topology {topology.name}."
        )
    return ordered[:threads]


def format_cpu_list(cpus: tuple[int, ...]) -> str:
    return ",".join(str(cpu) for cpu in cpus)


def numa_nodes_for_cpu_list(topology: CpuTopology, cpus: tuple[int, ...]) -> str:
    cpu_to_node = topology.cpu_to_node()
    nodes = sorted({cpu_to_node[cpu] for cpu in cpus if cpu in cpu_to_node})
    return ";".join(str(node) for node in nodes)


def default_result_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment2_{timestamp}"


def ensure_output_root(path: Path, *, force: bool, resume: bool, dry_run: bool) -> None:
    if path.exists() and not path.is_dir():
        raise RuntimeError(f"Output root exists but is not a directory: {path}")
    if path.exists() and any(path.iterdir()) and not force and not resume:
        raise RuntimeError(f"Output root already exists and is not empty: {path}. Use --force or --resume.")
    path.mkdir(parents=True, exist_ok=True)
    if dry_run:
        (path / "per_run").mkdir(parents=True, exist_ok=True)


def resolve_path(path: Path) -> Path:
    return path.expanduser().resolve()


def relative_to_root(path: Path, root: Path) -> str:
    try:
        return str(path.relative_to(root))
    except ValueError:
        return str(path)


def resolve_mcs_tse_library() -> Path:
    experiment_defaults.require_available_core_backend("mcs_tse")
    env_path = os.environ.get("MCS_TSE_LIB")
    if env_path:
        candidate = Path(env_path).expanduser()
        return candidate if candidate.is_absolute() else (REPO_ROOT / candidate).resolve()
    if MCS_TSE_RELEASE_LIB.is_file():
        return MCS_TSE_RELEASE_LIB
    if MCS_TSE_DEBUG_LIB.is_file():
        return MCS_TSE_DEBUG_LIB
    return MCS_TSE_RELEASE_LIB


def ensure_mcs_tse_library(logger: CommandLogger, *, build_missing: bool, dry_run: bool) -> Path:
    library = resolve_mcs_tse_library()
    if library.is_file() or dry_run:
        return library
    if not build_missing:
        raise RuntimeError(
            f"mcs_tse preload library is missing: {library}. "
            "Run cargo build -p mcs_tse --release or rerun with --build-missing."
        )
    logger.run(
        ["cargo", "build", "-p", "mcs_tse", "--release"],
        log_name="build_mcs_tse.log",
        cwd=REPO_ROOT,
        timeout_seconds=0,
    )
    library = resolve_mcs_tse_library()
    if not library.is_file():
        raise RuntimeError(f"mcs_tse preload library was not built: {library}")
    return library


def normalize_lock_key(key: str) -> str:
    return experiment_defaults.normalize_experiment_two_lock(key)


def resolve_lock_specs(
    lock_keys: tuple[str, ...],
    *,
    logger: CommandLogger,
    build_missing: bool,
    dry_run: bool,
) -> tuple[LockSpec, ...]:
    specs: list[LockSpec] = []
    seen: set[str] = set()
    for raw_key in lock_keys:
        key = normalize_lock_key(raw_key)
        if key in seen:
            continue
        seen.add(key)

        if key == "mcs_tse":
            specs.append(
                LockSpec(
                    key="mcs_tse",
                    label=experiment_defaults.lock_label("mcs_tse"),
                    mode="ld_preload",
                    lock_kind="mutex",
                    preload_library=ensure_mcs_tse_library(
                        logger,
                        build_missing=build_missing,
                        dry_run=dry_run,
                    ),
                )
            )
        elif key == "mcs_tas":
            specs.append(
                LockSpec(
                    key="mcs_tas",
                    label=experiment_defaults.lock_label("mcs_tas"),
                    mode="native",
                    lock_kind="mcs-tas",
                )
            )
        elif key == "mcs_extension":
            specs.append(
                LockSpec(
                    key="mcs_extension",
                    label=f"{experiment_defaults.lock_label('mcs_extension')} (native)",
                    mode="native_timeslice_extension",
                    lock_kind="mcs",
                    timeslice_extension="require",
                )
            )
    return tuple(specs)


def ensure_inputs() -> None:
    if not SWEEP_SCRIPT.is_file() or not os.access(SWEEP_SCRIPT, os.X_OK):
        raise RuntimeError(f"Mutexbench sweep script is not executable: {SWEEP_SCRIPT}")


def build_sweep_command(
    spec: LockSpec,
    *,
    threads: int,
    cpu_list: str,
    args: argparse.Namespace,
    raw_path: Path,
    summary_path: Path,
    output_root: Path,
) -> list[str]:
    cmd = [
        "taskset",
        "-c",
        cpu_list,
        str(SWEEP_SCRIPT),
        "--threads",
        str(threads),
        "--critical-ns",
        str(args.critical_ns),
        "--outside-ns",
        str(args.outside_ns),
        "--duration-ms",
        str(args.duration_ms),
        "--warmup-duration-ms",
        str(args.warmup_duration_ms),
        "--timing-sample-stride",
        str(args.timing_sample_stride),
        "--repeats",
        str(args.repeats),
        "--timeslice-extension",
        spec.timeslice_extension,
        "--lock-kind",
        spec.lock_kind,
        "--output-root",
        str(output_root),
        "--output-raw",
        str(raw_path),
        "--output-summary",
        str(summary_path),
    ]
    if spec.preload_library is not None:
        cmd.extend(["--bench-ld-preload", str(spec.preload_library)])
    return cmd


def write_settings(
    result_root: Path,
    *,
    topology: CpuTopology,
    locks: tuple[LockSpec, ...],
    threads: tuple[int, ...],
    args: argparse.Namespace,
) -> None:
    cpu_lists = {
        str(thread): format_cpu_list(cpu_list_for_threads(topology, thread))
        for thread in threads
    }
    settings = {
        "locks": [
            {
                "key": lock.key,
                "label": lock.label,
                "mode": lock.mode,
                "lock_kind": lock.lock_kind,
                "timeslice_extension": lock.timeslice_extension,
                "preload_library": str(lock.preload_library) if lock.preload_library is not None else None,
            }
            for lock in locks
        ],
        "threads": list(threads),
        "cpu_lists_by_thread": cpu_lists,
        "cpu_policy": "numa-first: first NUMA node CPUs sorted ascending, then later NUMA nodes sorted ascending",
        "topology": {
            "name": topology.name,
            "source": topology.source,
            "max_threads": topology.max_threads(),
            "first_node_cpu_count": topology.first_node_count(),
            "nodes": [
                {"node": node.node, "cpus": list(node.cpus)}
                for node in topology.nodes
            ],
        },
        "critical_ns": args.critical_ns,
        "outside_ns": args.outside_ns,
        "duration_ms": args.duration_ms,
        "warmup_duration_ms": args.warmup_duration_ms,
        "repeats": args.repeats,
        "command_timeout_seconds": args.command_timeout_seconds,
        "timing_sample_stride": args.timing_sample_stride,
        "build_missing": args.build_missing,
        "dry_run": args.dry_run,
    }
    with (result_root / "settings.json").open("w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")


def load_raw_rows(path: Path) -> list[dict[str, str]]:
    if not path.is_file():
        return []
    with path.open("r", encoding="utf-8", newline="") as f:
        return list(csv.DictReader(f))


def write_raw_csv(path: Path, rows: list[dict[str, str]]) -> None:
    with path.open("w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=RAW_FIELDS)
        writer.writeheader()
        for row in rows:
            writer.writerow({field: row.get(field, "") for field in RAW_FIELDS})


def as_float(row: dict[str, str], field: str) -> float:
    value = row.get(field, "")
    return float(value) if value not in {"", None} else 0.0


def summarize_rows(rows: list[dict[str, str]], lock_order: tuple[str, ...]) -> list[dict[str, str]]:
    groups: dict[tuple[str, str, str, str], list[dict[str, str]]] = defaultdict(list)
    for row in rows:
        groups[(row["lock"], row["threads"], row["critical_ns"], row["outside_ns"])].append(row)

    order_index = {lock: index for index, lock in enumerate(lock_order)}
    summary: list[dict[str, str]] = []
    for key, group_rows in sorted(
        groups.items(),
        key=lambda item: (
            order_index.get(item[0][0], len(order_index)),
            int(item[0][1]),
            int(item[0][2]),
            int(item[0][3]),
        ),
    ):
        _lock, _threads, _critical_ns, _outside_ns = key
        first = group_rows[0]
        summary.append(
            {
                "lock": first["lock"],
                "lock_label": first["lock_label"],
                "threads": first["threads"],
                "cpu_count": first["cpu_count"],
                "cpu_list": first["cpu_list"],
                "numa_nodes": first["numa_nodes"],
                "critical_ns": first["critical_ns"],
                "outside_ns": first["outside_ns"],
                "runs": str(len(group_rows)),
                "mean_throughput_ops_per_sec": f"{mean(as_float(row, 'throughput_ops_per_sec') for row in group_rows):.6f}",
                "mean_elapsed_seconds": f"{mean(as_float(row, 'elapsed_seconds') for row in group_rows):.6f}",
                "mean_total_operations": f"{mean(as_float(row, 'total_operations') for row in group_rows):.6f}",
                "mean_avg_lock_hold_ns": f"{mean(as_float(row, 'avg_lock_hold_ns') for row in group_rows):.6f}",
                "mean_avg_wait_ns_estimated": f"{mean(as_float(row, 'avg_wait_ns_estimated') for row in group_rows):.6f}",
                "mean_avg_lock_handoff_ns_estimated": f"{mean(as_float(row, 'avg_lock_handoff_ns_estimated') for row in group_rows):.6f}",
                "mean_lock_hold_samples": f"{mean(as_float(row, 'lock_hold_samples') for row in group_rows):.6f}",
                "mean_avg_cpu_pct": f"{mean(as_float(row, 'avg_cpu_pct') for row in group_rows):.6f}",
            }
        )
    return summary


def write_summary_csv(path: Path, rows: list[dict[str, str]]) -> None:
    with path.open("w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=SUMMARY_FIELDS)
        writer.writeheader()
        for row in rows:
            writer.writerow({field: row.get(field, "") for field in SUMMARY_FIELDS})


def append_sweep_rows(
    rows: list[dict[str, str]],
    *,
    spec: LockSpec,
    topology: CpuTopology,
    cpu_list: tuple[int, ...],
    raw_path: Path,
    result_root: Path,
    log_path: Path,
) -> None:
    with raw_path.open("r", encoding="utf-8", newline="") as f:
        reader = csv.DictReader(f)
        for source_row in reader:
            rows.append(
                {
                    "lock": spec.key,
                    "lock_label": spec.label,
                    "threads": source_row["threads"],
                    "cpu_count": str(len(cpu_list)),
                    "cpu_list": format_cpu_list(cpu_list),
                    "numa_nodes": numa_nodes_for_cpu_list(topology, cpu_list),
                    "critical_ns": source_row["critical_iters"],
                    "outside_ns": source_row["outside_iters"],
                    "repeat": source_row["repeat"],
                    "throughput_ops_per_sec": source_row["throughput_ops_per_sec"],
                    "elapsed_seconds": source_row["elapsed_seconds"],
                    "total_operations": source_row["total_operations"],
                    "avg_lock_hold_ns": source_row["avg_lock_hold_ns"],
                    "avg_wait_ns_estimated": source_row["avg_wait_ns_estimated"],
                    "avg_lock_handoff_ns_estimated": source_row["avg_lock_handoff_ns_estimated"],
                    "lock_hold_samples": source_row["lock_hold_samples"],
                    "avg_cpu_pct": source_row["avg_cpu_pct"],
                    "source_raw": relative_to_root(raw_path, result_root),
                    "command_log": relative_to_root(log_path, result_root),
                }
            )


def completed_points(rows: list[dict[str, str]], repeats: int) -> set[tuple[str, int, int, int]]:
    counts: dict[tuple[str, int, int, int], int] = defaultdict(int)
    for row in rows:
        key = (
            row["lock"],
            int(row["threads"]),
            int(row["critical_ns"]),
            int(row["outside_ns"]),
        )
        counts[key] += 1
    return {key for key, count in counts.items() if count >= repeats}


def write_plots(
    result_root: Path,
    summary_rows: list[dict[str, str]],
    *,
    topology: CpuTopology,
) -> None:
    if not summary_rows:
        return
    try:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError as exc:
        print(f"Skipping plots because matplotlib is unavailable: {exc}")
        return

    metrics = (
        ("mean_throughput_ops_per_sec", "Throughput (ops/s)", "throughput_vs_threads.png"),
        ("mean_avg_lock_hold_ns", "Avg lock hold (ns)", "lock_hold_ns_vs_threads.png"),
        ("mean_avg_lock_handoff_ns_estimated", "Avg handoff (ns)", "handoff_ns_vs_threads.png"),
        ("mean_avg_cpu_pct", "Avg CPU (%)", "cpu_pct_vs_threads.png"),
    )
    labels = sorted({row["lock_label"] for row in summary_rows})
    first_node_count = topology.first_node_count()

    for field, ylabel, filename in metrics:
        fig, ax = plt.subplots(figsize=(8, 4.8))
        for label in labels:
            rows = [row for row in summary_rows if row["lock_label"] == label]
            rows.sort(key=lambda row: int(row["threads"]))
            ax.plot(
                [int(row["threads"]) for row in rows],
                [float(row[field]) for row in rows],
                marker="o",
                linewidth=2,
                label=label,
            )
        if first_node_count > 0:
            ax.axvline(
                first_node_count,
                color="#666666",
                linestyle="--",
                linewidth=1,
                label=f"first NUMA node CPUs ({first_node_count})",
            )
        ax.set_xlabel("Threads / bound CPUs")
        ax.set_ylabel(ylabel)
        ax.set_xscale("log", base=2)
        thread_values = sorted({int(row["threads"]) for row in summary_rows})
        ax.set_xticks(thread_values)
        ax.set_xticklabels([str(value) for value in thread_values])
        ax.grid(True, which="both", axis="y", linestyle=":", linewidth=0.8)
        ax.legend()
        fig.tight_layout()
        fig.savefig(result_root / filename, dpi=180)
        plt.close(fig)


def run_benchmarks(
    result_root: Path,
    *,
    topology: CpuTopology,
    locks: tuple[LockSpec, ...],
    threads: tuple[int, ...],
    args: argparse.Namespace,
    logger: CommandLogger,
    rows: list[dict[str, str]],
    failures: list[dict[str, str]],
) -> None:
    done = completed_points(rows, args.repeats) if args.resume else set()
    for spec in locks:
        for thread in threads:
            point = (spec.key, thread, args.critical_ns, args.outside_ns)
            if point in done:
                print(f"Skipping completed point: lock={spec.key} threads={thread}")
                continue

            cpus = cpu_list_for_threads(topology, thread)
            cpu_list = format_cpu_list(cpus)
            run_root = result_root / "per_run" / spec.key / f"t{thread:03d}"
            run_root.mkdir(parents=True, exist_ok=True)
            raw_path = run_root / "raw.csv"
            summary_path = run_root / "summary.csv"
            cmd = build_sweep_command(
                spec,
                threads=thread,
                cpu_list=cpu_list,
                args=args,
                raw_path=raw_path,
                summary_path=summary_path,
                output_root=run_root,
            )
            print(
                f"Running lock={spec.key} threads={thread} cpus={cpu_list} "
                f"numa_nodes={numa_nodes_for_cpu_list(topology, cpus)}"
            )
            try:
                result = logger.run(
                    cmd,
                    log_name=f"sweep_{spec.key}_t{thread:03d}.log",
                    cwd=REPO_ROOT,
                    dry_run=args.dry_run,
                )
            except CommandError as exc:
                experiment_failures.append_command_failure(
                    failures,
                    result_root=result_root,
                    experiment="experiment2",
                    workload="mutexbench",
                    benchmark="sweep",
                    lock=spec.key,
                    threads=thread,
                    repeat="all",
                    stage="run",
                    exc=exc,
                )
                experiment_failures.write_failures_csv(result_root, failures)
                continue
            if args.dry_run:
                continue
            append_sweep_rows(
                rows,
                spec=spec,
                topology=topology,
                cpu_list=cpus,
                raw_path=raw_path,
                result_root=result_root,
                log_path=result.log_path,
            )
            write_raw_csv(result_root / "raw.csv", rows)
            write_summary_csv(
                result_root / "summary.csv",
                summarize_rows(rows, tuple(lock.key for lock in locks)),
            )


def main() -> int:
    args = parse_args()

    if args.plot_only is not None:
        result_root = resolve_path(args.plot_only)
        raw_path = result_root / "raw.csv"
        if not raw_path.is_file():
            raise RuntimeError(f"raw.csv was not found: {raw_path}")
        rows = load_raw_rows(raw_path)
        lock_order = tuple(dict.fromkeys(row["lock"] for row in rows))
        summary_rows = summarize_rows(rows, lock_order)
        write_summary_csv(result_root / "summary.csv", summary_rows)

        settings_path = result_root / "settings.json"
        if settings_path.is_file():
            with settings_path.open("r", encoding="utf-8") as f:
                settings = json.load(f)
            topology = topology_from_node_cpus(
                name=settings.get("topology", {}).get("name", "settings"),
                node_cpus={
                    int(node["node"]): tuple(int(cpu) for cpu in node["cpus"])
                    for node in settings.get("topology", {}).get("nodes", [])
                },
                source="settings.json",
            )
        else:
            topology = select_topology(args.topology_profile)
        write_plots(result_root, summary_rows, topology=topology)
        print(f"Summary: {result_root / 'summary.csv'}")
        return 0

    result_root = resolve_path(args.output_root) if args.output_root is not None else default_result_root()
    ensure_output_root(result_root, force=args.force, resume=args.resume, dry_run=args.dry_run)
    ensure_inputs()

    topology = select_topology(args.topology_profile)
    threads = parse_csv_ints(args.threads) if args.threads is not None else default_thread_counts(topology)
    invalid_threads = [thread for thread in threads if thread > topology.max_threads()]
    if invalid_threads:
        raise RuntimeError(
            f"Thread counts exceed topology CPU count {topology.max_threads()}: "
            f"{','.join(str(thread) for thread in invalid_threads)}"
        )

    logger = CommandLogger(
        result_root,
        resume=args.resume,
        command_timeout_seconds=args.command_timeout_seconds,
    )
    lock_keys = parse_csv_strings(args.locks)
    locks = resolve_lock_specs(
        lock_keys,
        logger=logger,
        build_missing=args.build_missing,
        dry_run=args.dry_run,
    )

    write_settings(result_root, topology=topology, locks=locks, threads=threads, args=args)
    rows = load_raw_rows(result_root / "raw.csv") if args.resume else []
    failures: list[dict[str, str]] = []
    run_benchmarks(
        result_root,
        topology=topology,
        locks=locks,
        threads=threads,
        args=args,
        logger=logger,
        rows=rows,
        failures=failures,
    )
    if not args.dry_run:
        summary_rows = summarize_rows(rows, tuple(lock.key for lock in locks))
        write_raw_csv(result_root / "raw.csv", rows)
        write_summary_csv(result_root / "summary.csv", summary_rows)
        write_plots(result_root, summary_rows, topology=topology)
        print(f"Raw results: {result_root / 'raw.csv'}")
        print(f"Summary: {result_root / 'summary.csv'}")
    else:
        print(f"Dry-run output root: {result_root}")
    failures_path = experiment_failures.write_failures_csv(result_root, failures)
    experiment_failures.print_failure_summary(failures, failures_path)
    return 1 if failures else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except CommandError as exc:
        print(str(exc), file=sys.stderr)
        print(f"Command log: {exc.log_path}", file=sys.stderr)
        raise SystemExit(exc.returncode)
    except Exception as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(1)
