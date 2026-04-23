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


THREADS = (1, 2, 4, 8, 16, 32, 64, 96, 128, 192, 256)
CRITICAL_NS = 300
OUTSIDE_NS = 3000
DURATION_MS = 5000
WARMUP_DURATION_MS = 1000
REPEATS = 4
DEFAULT_MCS_TAS_SIMPLE_TASKSET_CPUS = "0,2,4,8,10,12,14,16,18,20,22"
MCS_TAS_SIMPLE_RELEASE_LIB = REPO_ROOT / "target" / "release" / "libmcs_tas_simple.so"


@dataclass(frozen=True)
class LockSpec:
    label: str
    key: str
    optional: bool = False


LOCKS = (
    LockSpec("MCS", "mcs"),
    LockSpec("MCS-TP", "mcstp"),
    LockSpec("mcs-tas", "mcs-tas"),
    LockSpec("mcs_tas_simple taskset 11c", "mcs_tas_simple", optional=True),
    LockSpec("mcs + extension", "mcs_extension"),
    LockSpec("FlexGuard", "flexguard"),
)

BASE_LOCK_KEYS = ("mcs", "mcstp", "mcs-tas", "flexguard")
FLEXGUARD_INTERPOSE_KEYS = ("mcstp", "flexguard")

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
    ) -> None:
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

    def write_manifest(self) -> None:
        manifest_path = self.result_root / "commands.json"
        with manifest_path.open("w", encoding="utf-8") as f:
            json.dump(self.records, f, indent=2)
            f.write("\n")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run or plot the mutexbench experiment-one sweep.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"""\
Default benchmark settings:
  critical-ns={CRITICAL_NS}, outside-ns={OUTSIDE_NS}, duration=5s, warmup=1s, repeats={REPEATS}
  threads={','.join(str(v) for v in THREADS)}
  default full run includes mcs_tas_simple under taskset CPUs={DEFAULT_MCS_TAS_SIMPLE_TASKSET_CPUS}

Examples:
  python3 experiments/run_experiment_one.py
  python3 experiments/run_experiment_one.py --output-root experiments/results/experiment1_manual
  python3 experiments/run_experiment_one.py --mcs-tas-simple-taskset-cpus 0,2,4,8,10,12,14,16,18,20,22
  python3 experiments/run_experiment_one.py --plot-only experiments/results/experiment1_manual
""",
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        default=None,
        help="Directory for a new run. Default: experiments/results/experiment1_<timestamp>.",
    )
    parser.add_argument(
        "--plot-only",
        type=Path,
        default=None,
        metavar="RESULT_ROOT",
        help="Skip benchmark execution and regenerate combined CSV and PNGs from RESULT_ROOT.",
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
        "--mcs-tas-simple-taskset-cpus",
        default=DEFAULT_MCS_TAS_SIMPLE_TASKSET_CPUS,
        metavar="CPU_LIST",
        help=(
            "CPU list passed to taskset for the mcs_tas_simple series. "
            f"Default: {DEFAULT_MCS_TAS_SIMPLE_TASKSET_CPUS}."
        ),
    )
    parser.add_argument(
        "--skip-mcs-tas-simple-taskset",
        action="store_true",
        help="Skip only the taskset mcs_tas_simple series. Default full run includes it.",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Allow benchmark output into an existing non-empty output root.",
    )
    return parser.parse_args()


def resolve_path(path: Path) -> Path:
    return path.expanduser().resolve()


def default_result_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment1_{timestamp}"


def write_settings(
    result_root: Path,
    mcs_extension_mode: str,
    sudo_mode: str,
    mcs_tas_simple_taskset_enabled: bool,
    mcs_tas_simple_taskset_cpus: str,
) -> None:
    settings = {
        "threads": list(THREADS),
        "critical_ns": CRITICAL_NS,
        "outside_ns": OUTSIDE_NS,
        "duration_ms": DURATION_MS,
        "warmup_duration_ms": WARMUP_DURATION_MS,
        "repeats": REPEATS,
        "mcs_extension_mode": mcs_extension_mode,
        "sudo_mode": sudo_mode,
        "mcs_tas_simple_taskset_enabled": mcs_tas_simple_taskset_enabled,
        "mcs_tas_simple_taskset_cpus": mcs_tas_simple_taskset_cpus,
        "locks": [{"label": lock.label, "key": lock.key} for lock in LOCKS],
        "flexguard_dir": str(FLEXGUARD_DIR),
    }
    with (result_root / "settings.json").open("w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")


def ensure_executable(path: Path, description: str) -> None:
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"{description} is not executable: {path}")


def ensure_mutex_bench(logger: CommandLogger) -> None:
    binary = MUTEXBENCH_DIR / "mutex_bench"
    if not binary.is_file() or not os.access(binary, os.X_OK):
        logger.run(["make", "-C", str(MUTEXBENCH_DIR), "mutex_bench"], log_name="build_mutex_bench.log")
    ensure_executable(binary, "mutexbench binary")


def flexguard_interpose_path(key: str) -> Path:
    return FLEXGUARD_DIR / "build" / f"interpose_{key}.sh"


def ensure_flexguard_interpose(key: str, logger: CommandLogger) -> None:
    script = flexguard_interpose_path(key)
    if script.is_file() and os.access(script, os.X_OK):
        return

    target = f"build/interpose_{key}.sh"
    try:
        logger.run(["make", "-C", str(FLEXGUARD_DIR), target], log_name=f"build_flexguard_{key}.log")
    except CommandError as exc:
        raise RuntimeError(
            f"Required bench/flexguard script cannot be built: {script}. "
            f"The attempted Makefile target was {target}; see {exc.log_path}."
        ) from exc

    if not script.is_file() or not os.access(script, os.X_OK):
        raise RuntimeError(
            f"Required bench/flexguard script was not produced as an executable file: {script}"
        )


def ensure_inputs(logger: CommandLogger) -> None:
    ensure_executable(SWEEP_MULTI, "multi-lock sweep script")
    ensure_executable(SWEEP_SINGLE, "single-lock sweep script")
    ensure_mutex_bench(logger)
    for key in FLEXGUARD_INTERPOSE_KEYS:
        ensure_flexguard_interpose(key, logger)


def common_sweep_args() -> list[str]:
    return [
        "--threads",
        ",".join(str(v) for v in THREADS),
        "--critical-ns",
        str(CRITICAL_NS),
        "--outside-ns",
        str(OUTSIDE_NS),
        "--duration-ms",
        str(DURATION_MS),
        "--warmup-duration-ms",
        str(WARMUP_DURATION_MS),
        "--repeats",
        str(REPEATS),
    ]


def ensure_mcs_tas_simple(logger: CommandLogger) -> None:
    if not MCS_TAS_SIMPLE_RELEASE_LIB.is_file():
        logger.run(
            ["cargo", "build", "-p", "mcs_tas_simple", "--release"],
            log_name="build_mcs_tas_simple.log",
        )

    if not MCS_TAS_SIMPLE_RELEASE_LIB.is_file():
        raise RuntimeError(f"mcs_tas_simple library was not produced: {MCS_TAS_SIMPLE_RELEASE_LIB}")


def run_benchmarks(result_root: Path, args: argparse.Namespace, logger: CommandLogger) -> None:
    env = {"FLEXGUARD_DIR": str(FLEXGUARD_DIR)}
    base_cmd = [
        str(SWEEP_MULTI),
        "--locks",
        ",".join(BASE_LOCK_KEYS),
        "--output-root",
        str(result_root),
        "--sudo-mode",
        args.sudo_mode,
        "--timeslice-extension",
        "off",
        "--",
        *common_sweep_args(),
    ]
    logger.run(base_cmd, log_name="sweep_base_locks.log", env=env)

    extension_dir = result_root / "mcs_extension"
    extension_dir.mkdir(parents=True, exist_ok=True)
    extension_cmd = [
        str(SWEEP_SINGLE),
        *common_sweep_args(),
        "--lock-kind",
        "mcs",
        "--timeslice-extension",
        args.mcs_extension_mode,
        "--output-raw",
        str(extension_dir / "raw.csv"),
        "--output-summary",
        str(extension_dir / "summary.csv"),
    ]
    logger.run(extension_cmd, log_name="sweep_mcs_extension.log")

    if args.skip_mcs_tas_simple_taskset:
        return

    ensure_mcs_tas_simple(logger)
    taskset_cmd = [
        "taskset",
        "-c",
        args.mcs_tas_simple_taskset_cpus,
        str(SWEEP_MULTI),
        "--locks",
        "mcs_tas_simple",
        "--output-root",
        str(result_root),
        "--sudo-mode",
        "auto",
        "--timeslice-extension",
        "off",
        "--",
        *common_sweep_args(),
    ]
    logger.run(taskset_cmd, log_name="sweep_mcs_tas_simple_taskset.log")


def load_combined_rows(result_root: Path) -> list[dict[str, str]]:
    combined_rows: list[dict[str, str]] = []
    required = set(LATENCY_PLOT_REQUIRED_FIELDS) | {CPU_FIELD}

    for lock in LOCKS:
        lock_dir = result_root / lock.key
        if (
            lock.optional
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


def plot_metric(
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

    plot_rows = [row for row in rows if is_experiment_row(row)]
    if not plot_rows:
        raise RuntimeError(
            f"No rows matched critical_ns={CRITICAL_NS} and outside_ns={OUTSIDE_NS} for plotting."
        )

    fig, ax = plt.subplots(figsize=(9.5, 5.5))
    for lock in LOCKS:
        points = [
            (int(row["threads"]), float(row[metric]))
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

    ax.set_title(title)
    ax.set_xlabel("Threads")
    ax.set_ylabel(ylabel)
    ax.set_xscale("log", base=2)
    ax.set_xticks(list(THREADS))
    ax.xaxis.set_major_formatter(ScalarFormatter())
    ax.grid(True, axis="y", alpha=0.28)
    ax.grid(True, axis="x", which="major", alpha=0.16)
    ax.legend(frameon=False, ncol=2)
    fig.tight_layout()
    fig.savefig(output_path, dpi=180)
    plt.close(fig)


def write_plots(result_root: Path, rows: list[dict[str, str]]) -> list[Path]:
    outputs = [
        result_root / "hold_time_vs_threads.png",
        result_root / "handoff_time_vs_threads.png",
        result_root / "hold_plus_handoff_vs_threads.png",
    ]
    plot_metric(
        rows,
        metric="avg_lock_hold_ns",
        ylabel="Average lock hold time (ns)",
        title="Lock Hold Time vs Threads",
        output_path=outputs[0],
    )
    plot_metric(
        rows,
        metric=HANDOFF_FIELD,
        ylabel="Estimated lock handoff time (ns)",
        title="Lock Handoff Time vs Threads",
        output_path=outputs[1],
    )
    plot_metric(
        rows,
        metric="avg_hold_plus_handoff_ns",
        ylabel="Average lock hold plus handoff time (ns)",
        title="Lock Hold Plus Handoff Time vs Threads",
        output_path=outputs[2],
    )
    return outputs


def ensure_output_root(path: Path, force: bool) -> None:
    if path.exists() and not path.is_dir():
        raise RuntimeError(f"Output root exists but is not a directory: {path}")
    if path.exists() and any(path.iterdir()) and not force:
        raise RuntimeError(f"Output root already exists and is not empty: {path}. Use --force to write there.")
    path.mkdir(parents=True, exist_ok=True)


def print_outputs(result_root: Path, combined_path: Path, plot_paths: Iterable[Path]) -> None:
    print(f"Result root: {result_root}")
    print(f"Combined CSV: {combined_path}")
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
            rows = load_combined_rows(result_root)
            combined_path = write_combined_csv(result_root, rows)
            plot_paths = write_plots(result_root, rows)
            print_outputs(result_root, combined_path, plot_paths)
            return 0

        result_root = resolve_path(args.output_root) if args.output_root is not None else default_result_root()
        ensure_output_root(result_root, args.force)
        write_settings(
            result_root,
            args.mcs_extension_mode,
            args.sudo_mode,
            not args.skip_mcs_tas_simple_taskset,
            args.mcs_tas_simple_taskset_cpus,
        )
        logger = CommandLogger(result_root)
        ensure_inputs(logger)
        run_benchmarks(result_root, args, logger)
        rows = load_combined_rows(result_root)
        combined_path = write_combined_csv(result_root, rows)
        plot_paths = write_plots(result_root, rows)
        print_outputs(result_root, combined_path, plot_paths)
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
