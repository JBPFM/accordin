#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import shlex
import statistics
import subprocess
import time
from pathlib import Path
from typing import Iterable, Sequence

import experiment_defaults
import run_experiment_three as experiment_three
import run_experiment_six as experiment_six


REPO_ROOT = Path(__file__).resolve().parents[1]
LEGACY_MCS_ACCORDIN_LOCK = "mcs_accordin"
MCS_ACCORDIN_DIRECT_PACKAGE = experiment_six.MCS_ACCORDIN_DIRECT_PACKAGE
MCS_ACCORDIN_DIRECT_LOCK_KIND = experiment_six.MCS_ACCORDIN_DIRECT_LOCK_KIND
MCS_ACCORDIN_DIRECT_RELEASE_LIB = experiment_six.MCS_ACCORDIN_DIRECT_RELEASE_LIB
MCS_ACCORDIN_DIRECT_LIB_ENV = experiment_six.MCS_ACCORDIN_DIRECT_LIB_ENV
LEGACY_MCS_ACCORDIN_PACKAGE = MCS_ACCORDIN_DIRECT_PACKAGE
LEGACY_MCS_ACCORDIN_LIBRARY = MCS_ACCORDIN_DIRECT_RELEASE_LIB
DEFAULT_CRITICAL_NS = (100, 300, 1000, 30000)
DEFAULT_OUTSIDE_NS = 3000
DEFAULT_THREADS = (48, 96, 192)
DEFAULT_WARMUP_DURATION_MS = 2000
DEFAULT_DURATION_MS = 8000
DEFAULT_REPEATS = experiment_defaults.DEFAULT_REPEATS
DEFAULT_COMMAND_TIMEOUT_SECONDS = 21600
DEFAULT_LOCK_PROFILE = experiment_defaults.DEFAULT_LOCK_PROFILE
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
    "critical_ns",
    "outside_ns",
    "repeat",
    "fairness_factor",
    "per_thread_operations",
    "per_thread_min_operations",
    "per_thread_max_operations",
    "per_thread_mean_operations",
    "throughput_ops_per_sec",
    "elapsed_seconds",
    "bench_wall_seconds",
    "total_operations",
    "avg_lock_hold_ns",
    "avg_wait_ns_estimated",
    "avg_lock_handoff_ns_estimated",
    "lock_hold_samples",
    "command_log",
)

SUMMARY_FIELDS = (
    "lock",
    "lock_label",
    "threads",
    "critical_ns",
    "outside_ns",
    "repeats",
    *(
        field
        for field in RAW_FIELDS
        if field
        not in {
            "lock",
            "lock_label",
            "threads",
            "critical_ns",
            "outside_ns",
            "repeat",
            "per_thread_operations",
            "command_log",
        }
    ),
)
SUMMARY_NUMERIC_FIELDS = tuple(
    field
    for field in SUMMARY_FIELDS
    if field
    not in {
        "lock",
        "lock_label",
        "threads",
        "critical_ns",
        "outside_ns",
        "repeats",
    }
)


def default_output_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment7_fairness_{timestamp}"


def shlex_join(cmd: Sequence[str]) -> str:
    return shlex.join(str(part) for part in cmd)


def parse_csv_positive_ints(text: str, name: str) -> tuple[int, ...]:
    values: list[int] = []
    for raw in text.split(","):
        item = raw.strip()
        if not item:
            continue
        try:
            value = int(item)
        except ValueError as exc:
            raise argparse.ArgumentTypeError(f"{name} contains a non-integer value: {item}") from exc
        if value <= 0:
            raise argparse.ArgumentTypeError(f"{name} values must be > 0: {item}")
        values.append(value)
    if not values:
        raise argparse.ArgumentTypeError(f"{name} must contain at least one value")
    return tuple(dict.fromkeys(values))


def parse_per_thread_operations(text: str, *, expected_threads: int) -> tuple[int, ...]:
    values: list[int] = []
    for raw in text.split(","):
        item = raw.strip()
        if not item:
            continue
        try:
            value = int(item)
        except ValueError as exc:
            raise ValueError(f"per_thread_operations contains a non-integer value: {item}") from exc
        if value < 0:
            raise ValueError(f"per_thread_operations values must be >= 0: {item}")
        values.append(value)
    if len(values) != expected_threads:
        raise ValueError(f"expected {expected_threads} per-thread counters, got {len(values)}")
    return tuple(values)


def fairness_factor(ops: Iterable[int]) -> float:
    sorted_ops = sorted(ops, reverse=True)
    if not sorted_ops:
        raise ValueError("fairness_factor requires at least one operation counter")
    total = sum(sorted_ops)
    if total == 0:
        return 0.0
    n = len(sorted_ops)
    return sum(sorted_ops[: n // 2]) / total


def parse_locks(text: str | None, profile: str) -> tuple[str, ...]:
    raw_locks = (
        experiment_defaults.lock_profile_locks(profile)
        if text is None
        else experiment_six.parse_csv_strings(text)
    )
    locks: list[str] = []
    for raw in raw_locks:
        if raw.strip().lower() == LEGACY_MCS_ACCORDIN_LOCK:
            lock = LEGACY_MCS_ACCORDIN_LOCK
        else:
            lock = experiment_six.normalize_lock(raw)
        if lock != LEGACY_MCS_ACCORDIN_LOCK and lock not in experiment_six.SUPPORTED_LOCKS:
            supported = sorted(
                experiment_six.SUPPORTED_LOCKS
                | set(experiment_six.LOCAL_LOCK_ALIASES)
                | {LEGACY_MCS_ACCORDIN_LOCK}
            )
            raise argparse.ArgumentTypeError(
                f"Unsupported experiment7 lock {raw!r}. Supported locks: {', '.join(supported)}"
            )
        if lock not in locks:
            locks.append(lock)
    if not locks:
        raise argparse.ArgumentTypeError("At least one lock must be selected")
    return tuple(locks)


def is_legacy_mcs_accordin_lock(lock: str) -> bool:
    return lock == LEGACY_MCS_ACCORDIN_LOCK


def lock_label(lock: str) -> str:
    if is_legacy_mcs_accordin_lock(lock):
        return LEGACY_MCS_ACCORDIN_LOCK
    return experiment_six.lock_label(lock)


def legacy_mcs_accordin_env() -> dict[str, str | None]:
    return experiment_six.mcs_accordin_env()


def ensure_builds(locks: Iterable[str], *, dry_run: bool) -> None:
    lock_list = tuple(locks)
    experiment_six.ensure_builds(
        (lock for lock in lock_list if not is_legacy_mcs_accordin_lock(lock)),
        dry_run=dry_run,
    )
    if any(is_legacy_mcs_accordin_lock(lock) for lock in lock_list):
        build_cmd = ["cargo", "build", "-p", LEGACY_MCS_ACCORDIN_PACKAGE, "--release"]
        if dry_run:
            print(shlex_join(build_cmd))
        else:
            subprocess.run(build_cmd, cwd=REPO_ROOT, check=True)
            if not LEGACY_MCS_ACCORDIN_LIBRARY.is_file():
                raise RuntimeError(
                    f"{LEGACY_MCS_ACCORDIN_PACKAGE} library was not produced: "
                    f"{LEGACY_MCS_ACCORDIN_LIBRARY}"
                )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Experiment 7: single-lock mutexbench per-thread fairness.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=f"""\
Default benchmark settings:
  lock-profile={DEFAULT_LOCK_PROFILE}
  critical-ns={','.join(str(value) for value in DEFAULT_CRITICAL_NS)}
  outside-ns={DEFAULT_OUTSIDE_NS}
  warmup={DEFAULT_WARMUP_DURATION_MS // 1000}s, duration={DEFAULT_DURATION_MS // 1000}s
  threads={','.join(str(value) for value in DEFAULT_THREADS)}
  repeats={DEFAULT_REPEATS}

Examples:
  python3 experiments/run_experiment_seven.py
  python3 experiments/run_experiment_seven.py --lock-profile full
  python3 experiments/run_experiment_seven.py --locks mutex,mcs,mcs_extension --repeats 1
""",
    )
    parser.add_argument("--output-root", type=Path, default=None)
    parser.add_argument("--plot-only", type=Path, default=None, metavar="RESULT_ROOT", help="Regenerate summary.csv and PNGs from raw.csv.")
    parser.add_argument("--skip-plots", action="store_true", help="Do not generate PNG plots after running benchmarks.")
    parser.add_argument("--force", action="store_true", help="Replace existing raw/summary/settings files")
    parser.add_argument("--resume", action="store_true", help="Skip complete raw rows already present")
    parser.add_argument("--dry-run", action="store_true", help="Print commands and CSV schema without running")
    parser.add_argument(
        "--lock-profile",
        choices=experiment_defaults.lock_profile_names(),
        default=DEFAULT_LOCK_PROFILE,
        help="Named lock set used when --locks is omitted.",
    )
    parser.add_argument("--locks", help="Comma-separated lock list. Default comes from --lock-profile.")
    parser.add_argument(
        "--threads",
        type=lambda text: parse_csv_positive_ints(text, "--threads"),
        default=DEFAULT_THREADS,
        help=f"Comma-separated thread counts. Default: {','.join(str(value) for value in DEFAULT_THREADS)}.",
    )
    parser.add_argument(
        "--critical-ns",
        type=lambda text: parse_csv_positive_ints(text, "--critical-ns"),
        default=DEFAULT_CRITICAL_NS,
        help=f"Comma-separated critical-section durations. Default: {','.join(str(value) for value in DEFAULT_CRITICAL_NS)}.",
    )
    parser.add_argument("--outside-ns", type=int, default=DEFAULT_OUTSIDE_NS)
    parser.add_argument("--duration-ms", type=int, default=DEFAULT_DURATION_MS)
    parser.add_argument("--warmup-duration-ms", type=int, default=DEFAULT_WARMUP_DURATION_MS)
    parser.add_argument("--repeats", type=int, default=DEFAULT_REPEATS)
    parser.add_argument("--sudo-mode", choices=("all", "auto", "none"), default="auto")
    parser.add_argument("--command-timeout-seconds", type=int, default=DEFAULT_COMMAND_TIMEOUT_SECONDS)
    parser.add_argument(
        "--mcs-accordin-taskset-cpus",
        default=experiment_defaults.DEFAULT_MCS_ACCORDIN_TASKSET_CPUS,
        help=(
            f"CPU list passed to taskset if the legacy {experiment_defaults.ACCORDIN_TASKSET_LOCK} series is re-enabled. "
            f"Default: {experiment_defaults.DEFAULT_MCS_ACCORDIN_TASKSET_CPUS}."
        ),
    )
    args = parser.parse_args()

    if args.outside_ns <= 0:
        parser.error("--outside-ns must be > 0")
    if args.duration_ms <= 0:
        parser.error("--duration-ms must be > 0")
    if args.warmup_duration_ms < 0:
        parser.error("--warmup-duration-ms must be >= 0")
    if args.repeats <= 0:
        parser.error("--repeats must be > 0")
    if args.command_timeout_seconds <= 0:
        parser.error("--command-timeout-seconds must be > 0")
    if args.force and args.resume:
        parser.error("--force and --resume cannot be used together")

    try:
        args.lock_keys = parse_locks(args.locks, args.lock_profile)
    except argparse.ArgumentTypeError as exc:
        parser.error(str(exc))
    args.lock_profile_source = "manual" if args.locks is not None else "profile"
    args.output_root = args.output_root or default_output_root()
    return args


def mutexbench_command(
    lock: str,
    threads: int,
    critical_ns: int,
    args: argparse.Namespace,
) -> tuple[list[str], dict[str, str | None], bool]:
    if is_legacy_mcs_accordin_lock(lock):
        lock_kind = MCS_ACCORDIN_DIRECT_LOCK_KIND
        env = legacy_mcs_accordin_env()
        needs_sudo = True
        cmd_prefix = []
    elif experiment_six.is_flexguard_interpose_lock(lock) or experiment_six.is_otherlocks_interpose_lock(lock):
        lock_kind = "mutex"
        env: dict[str, str | None] = {}
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
    else:
        lock_kind = experiment_six.BUILTIN_LOCK_KINDS.get(lock, experiment_six.ACCORDIN_DIRECT_LOCK_KIND)
        env = experiment_six.accordin_env(lock) if experiment_six.is_accordin_direct_lock(lock) else {}
        needs_sudo = experiment_six.is_accordin_direct_lock(lock)
        cmd_prefix = []

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
        str(critical_ns),
        "--outside-ns",
        str(args.outside_ns),
        "--lock-kind",
        lock_kind,
        "--timeslice-extension",
        "off",
    ]
    if experiment_defaults.accordin_uses_taskset(lock):
        cmd = ["taskset", "-c", args.mcs_accordin_taskset_cpus, *cmd]
    return cmd, env, needs_sudo


def read_existing_keys(raw_path: Path) -> set[tuple[str, int, int, int]]:
    if not raw_path.is_file():
        return set()
    keys: set[tuple[str, int, int, int]] = set()
    with raw_path.open(newline="", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        for row in reader:
            keys.add((row["lock"], int(row["threads"]), int(row["critical_ns"]), int(row["repeat"])))
    return keys


def run_one(
    lock: str,
    threads: int,
    critical_ns: int,
    repeat: int,
    args: argparse.Namespace,
    logs_dir: Path,
) -> dict[str, str]:
    cmd, env, needs_sudo = mutexbench_command(lock, threads, critical_ns, args)
    run_cmd = experiment_six.env_command(cmd, env, needs_sudo=needs_sudo, sudo_mode=args.sudo_mode)
    log_path = logs_dir / f"{lock}_t{threads}_c{critical_ns}_r{repeat}.log"

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
        raise RuntimeError(f"benchmark failed for lock={lock} threads={threads} critical_ns={critical_ns} repeat={repeat}; see {log_path}")

    metrics = experiment_six.parse_metrics(output)
    required_metrics = (
        "throughput_ops_per_sec",
        "elapsed_seconds",
        "total_operations",
        "avg_lock_hold_ns",
        "avg_wait_ns_estimated",
        "avg_lock_handoff_ns_estimated",
        "lock_hold_samples",
        "per_thread_operations",
    )
    missing = [field for field in required_metrics if not metrics.get(field)]
    if missing:
        raise RuntimeError(f"benchmark output missing metrics {missing}; see {log_path}")

    per_thread_ops = parse_per_thread_operations(metrics["per_thread_operations"], expected_threads=threads)
    total_operations = int(metrics["total_operations"])
    if sum(per_thread_ops) != total_operations:
        raise RuntimeError(
            f"per-thread operation sum {sum(per_thread_ops)} != total_operations {total_operations}; see {log_path}"
        )

    row = {
        "lock": lock,
        "lock_label": lock_label(lock),
        "threads": str(threads),
        "critical_ns": str(critical_ns),
        "outside_ns": str(args.outside_ns),
        "repeat": str(repeat),
        "fairness_factor": f"{fairness_factor(per_thread_ops):.12f}",
        "per_thread_operations": ",".join(str(value) for value in per_thread_ops),
        "per_thread_min_operations": str(min(per_thread_ops)),
        "per_thread_max_operations": str(max(per_thread_ops)),
        "per_thread_mean_operations": f"{statistics.mean(per_thread_ops):.6f}",
        "bench_wall_seconds": f"{bench_wall_seconds:.6f}",
        "command_log": str(log_path),
    }
    for field in RAW_FIELDS:
        if field not in row:
            row[field] = metrics.get(field, "")
    return row


def write_settings(root: Path, args: argparse.Namespace) -> None:
    settings_path = root / "settings.json"
    previous: dict[str, object] = {}
    if args.resume and settings_path.is_file():
        try:
            previous = json.loads(settings_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            previous = {}
    locks = list(dict.fromkeys([*previous.get("locks", []), *args.lock_keys]))
    settings = {
        "experiment": "experiment7_mutexbench_fairness",
        "description": "Single-lock mutexbench per-thread operation fairness. fairness_factor is top-half per-thread ops divided by total ops.",
        "output_root": str(root),
        "lock_profile": args.lock_profile,
        "lock_profile_source": args.lock_profile_source,
        "locks": locks,
        "threads": list(args.threads),
        "critical_ns": list(args.critical_ns),
        "outside_ns": args.outside_ns,
        "duration_ms": args.duration_ms,
        "warmup_duration_ms": args.warmup_duration_ms,
        "repeats": args.repeats,
        "sudo_mode": args.sudo_mode,
        "command_timeout_seconds": args.command_timeout_seconds,
        "mcs_accordin_taskset_cpus": args.mcs_accordin_taskset_cpus,
        "fairness_formula": "sum(sorted(ops, reverse=True)[:n//2]) / sum(ops)",
        "legacy_mcs_accordin_library": str(LEGACY_MCS_ACCORDIN_LIBRARY) if LEGACY_MCS_ACCORDIN_LOCK in locks else None,
    }
    settings_path.write_text(json.dumps(settings, indent=2) + "\n", encoding="utf-8")


def write_summary(raw_path: Path, summary_path: Path) -> None:
    with raw_path.open(newline="", encoding="utf-8") as f:
        rows = list(csv.DictReader(f))
    groups: dict[tuple[str, str, str], list[dict[str, str]]] = {}
    for row in rows:
        groups.setdefault((row["lock"], row["critical_ns"], row["threads"]), []).append(row)

    with summary_path.open("w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=SUMMARY_FIELDS, lineterminator="\n")
        writer.writeheader()
        for key in sorted(
            groups,
            key=lambda item: (experiment_defaults.lock_sort_key(item[0]), int(item[1]), int(item[2])),
        ):
            group_rows = groups[key]
            first = group_rows[0]
            out: dict[str, str] = {
                "lock": first["lock"],
                "lock_label": first["lock_label"],
                "threads": first["threads"],
                "critical_ns": first["critical_ns"],
                "outside_ns": first["outside_ns"],
                "repeats": str(len(group_rows)),
            }
            for field in SUMMARY_NUMERIC_FIELDS:
                if field == "bench_wall_seconds":
                    values = [float(row[field]) for row in group_rows if row.get(field)]
                else:
                    values = [float(row[field]) for row in group_rows if row.get(field) not in ("", None)]
                out[field] = f"{statistics.mean(values):.6f}" if values else ""
            writer.writerow(out)


def safe_name(value: str) -> str:
    out = "".join(ch.lower() if ch.isalnum() else "_" for ch in value.strip())
    return "_".join(part for part in out.split("_") if part) or "unknown"


def lock_plot_style_key(lock: str) -> str:
    return experiment_defaults.normalize_lock(lock)


def plot_color_map(lock_keys: Iterable[str]) -> dict[str, str]:
    color_keys = tuple(dict.fromkeys(lock_plot_style_key(lock) for lock in lock_keys))
    return {
        lock: FALLBACK_PLOT_COLORS[index % len(FALLBACK_PLOT_COLORS)]
        for index, lock in enumerate(color_keys)
    }


def lock_plot_style(lock: str, color_by_key: dict[str, str]) -> dict[str, str]:
    style_key = lock_plot_style_key(lock)
    return {
        "color": experiment_three.plot_color(style_key, color_by_key[style_key]),
        "linestyle": experiment_three.plot_linestyle(style_key),
        "marker": experiment_three.plot_marker(style_key),
    }


def load_summary_rows(summary_path: Path) -> list[dict[str, str]]:
    if not summary_path.is_file():
        raise RuntimeError(f"{summary_path} does not exist")
    with summary_path.open(newline="", encoding="utf-8") as f:
        return list(csv.DictReader(f))


def plot_fairness_for_critical(
    result_root: Path,
    rows: list[dict[str, str]],
    critical_ns: str,
    color_by_key: dict[str, str],
) -> Path:
    try:
        import matplotlib
    except ImportError as exc:
        raise RuntimeError("matplotlib is required to generate experiment7 plots") from exc

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.ticker import ScalarFormatter

    critical_rows = [row for row in rows if row["critical_ns"] == critical_ns and row.get("fairness_factor")]
    locks = tuple(dict.fromkeys(row["lock"] for row in critical_rows))
    thread_values = sorted({int(row["threads"]) for row in critical_rows})
    fig, ax = plt.subplots(figsize=(9.5, 5.5))
    for lock in locks:
        lock_rows = sorted(
            (row for row in critical_rows if row["lock"] == lock),
            key=lambda row: float(row["threads"]),
        )
        if not lock_rows:
            continue
        xs = [float(row["threads"]) for row in lock_rows]
        ys = [float(row["fairness_factor"]) for row in lock_rows]
        label = lock_rows[0].get("lock_label") or lock
        style = lock_plot_style(lock, color_by_key)
        ax.plot(
            xs,
            ys,
            marker=style["marker"],
            color=style["color"],
            linestyle=style["linestyle"],
            linewidth=1.8,
            markersize=4.5,
            markerfacecolor="white",
            markeredgewidth=1.4,
            label=label,
        )

    ax.set_title(f"Fairness Factor, critical={critical_ns} ns")
    ax.set_xlabel("Threads")
    ax.set_ylabel("Fairness factor (top half ops / total ops)")
    ax.set_ylim(0.45, 1.02)
    experiment_three.add_thread_axis_formatting(ax, thread_values)
    ax.xaxis.set_major_formatter(ScalarFormatter())
    ax.grid(True, axis="y", alpha=0.28)
    ax.grid(True, axis="x", which="major", alpha=0.16)
    if locks:
        ax.legend(loc="best", fontsize=8, frameon=False)
    fig.tight_layout()
    output_path = result_root / f"fairness_factor_vs_threads_c{safe_name(critical_ns)}.png"
    fig.savefig(output_path, dpi=180)
    plt.close(fig)
    return output_path


def generate_plots(result_root: Path) -> tuple[Path, ...]:
    rows = load_summary_rows(result_root / "summary.csv")
    critical_values = sorted(tuple(dict.fromkeys(row["critical_ns"] for row in rows)), key=lambda value: int(value))
    color_by_key = plot_color_map(row["lock"] for row in rows)
    return tuple(plot_fairness_for_critical(result_root, rows, critical_ns, color_by_key) for critical_ns in critical_values)


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


def dry_run(args: argparse.Namespace) -> None:
    print("raw_fields: " + ",".join(RAW_FIELDS))
    print("summary_fields: " + ",".join(SUMMARY_FIELDS))
    ensure_builds(args.lock_keys, dry_run=True)
    for lock in args.lock_keys:
        for threads in args.threads:
            for critical_ns in args.critical_ns:
                cmd, env, needs_sudo = mutexbench_command(lock, threads, critical_ns, args)
                run_cmd = experiment_six.env_command(cmd, env, needs_sudo=needs_sudo, sudo_mode=args.sudo_mode)
                print(f"lock={lock} threads={threads} critical_ns={critical_ns} repeat=1 {shlex_join(run_cmd)}")


def run_experiment(args: argparse.Namespace) -> Path:
    root = args.output_root
    raw_path, summary_path, logs_dir = prepare_output(root, force=args.force, resume=args.resume)
    write_settings(root, args)
    ensure_builds(args.lock_keys, dry_run=False)
    existing = read_existing_keys(raw_path) if args.resume else set()
    raw_exists = raw_path.exists() and args.resume

    with raw_path.open("a" if raw_exists else "w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=RAW_FIELDS, lineterminator="\n")
        if not raw_exists:
            writer.writeheader()
        for lock in args.lock_keys:
            for threads in args.threads:
                for critical_ns in args.critical_ns:
                    for repeat in range(1, args.repeats + 1):
                        key = (lock, threads, critical_ns, repeat)
                        if key in existing:
                            print(f"Skipping complete run: lock={lock} threads={threads} critical_ns={critical_ns} repeat={repeat}")
                            continue
                        print(f"Running lock={lock} threads={threads} critical_ns={critical_ns} repeat={repeat}")
                        row = run_one(lock, threads, critical_ns, repeat, args, logs_dir)
                        writer.writerow({field: row.get(field, "") for field in RAW_FIELDS})
                        f.flush()

    write_summary(raw_path, summary_path)
    return root


def main() -> int:
    args = parse_args()
    if args.plot_only is not None:
        summary_path, plot_paths = regenerate_summary_and_plots(args.plot_only)
        print(f"Summary results: {summary_path}")
        for path in plot_paths:
            print(f"Plot: {path}")
        return 0
    if args.dry_run:
        dry_run(args)
        return 0
    root = run_experiment(args)
    print(f"Raw results: {root / 'raw.csv'}")
    print(f"Summary results: {root / 'summary.csv'}")
    if not args.skip_plots:
        for path in generate_plots(root):
            print(f"Plot: {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
