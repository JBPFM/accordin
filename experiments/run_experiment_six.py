#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import json
import os
import shutil
import shlex
import statistics
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence

import experiment_defaults
import run_experiment_three as experiment_three


REPO_ROOT = Path(__file__).resolve().parents[1]
MUTEXBENCH_DIR = REPO_ROOT / "bench" / "mutexbench"
MUTEX_BENCH = MUTEXBENCH_DIR / "mutex_bench"
FLEXGUARD_DIR = REPO_ROOT / "bench" / "flexguard"
OTHERLOCKS_DIR = REPO_ROOT / "bench" / "otherlocks"
OTHERLOCKS_BUILD_DIR = OTHERLOCKS_DIR / "build"
DEFAULT_OUTPUT_ROOT = REPO_ROOT / "experiments" / "results" / "experiment6_multilock"
DEFAULT_COMMAND_TIMEOUT_SECONDS = 21600
ACCORDIN_DIRECT_PACKAGE = "mcs_tas_accordin_direct"
ACCORDIN_DIRECT_LOCK_KIND = "mcs_tas_accordin_direct"
ACCORDIN_DIRECT_RELEASE_LIB = REPO_ROOT / "target" / "release" / "libmcs_tas_accordin_direct.so"
ACCORDIN_DIRECT_LIB_ENV = "MCS_TAS_ACCORDIN_DIRECT_LIB"
ACCORDIN_DIRECT_DISABLE_BPF_ENV = "MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF"
ACCORDIN_DIRECT_STATS_ONLY_ENV = "MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY"
ACCORDIN_DIRECT_ENV_PREFIX = "MCS_TAS_ACCORDIN_DIRECT_"
MCS_ACCORDIN_LOCK = experiment_defaults.MCS_ACCORDIN_LOCK
MCS_ACCORDIN_PACKAGE = "mcs_accordin"
MCS_ACCORDIN_RELEASE_LIB = experiment_three.MCS_ACCORDIN_RELEASE_LIB
MCS_ACCORDIN_DIRECT_PACKAGE = experiment_three.MCS_ACCORDIN_DIRECT_PACKAGE
MCS_ACCORDIN_DIRECT_LOCK_KIND = experiment_three.MCS_ACCORDIN_DIRECT_LOCK_KIND
MCS_ACCORDIN_DIRECT_RELEASE_LIB = experiment_three.MCS_ACCORDIN_DIRECT_RELEASE_LIB
MCS_ACCORDIN_DIRECT_LIB_ENV = experiment_three.MCS_ACCORDIN_DIRECT_LIB_ENV
MCS_ACCORDIN_DIRECT_DISABLE_BPF_ENV = experiment_three.MCS_ACCORDIN_DIRECT_DISABLE_BPF_ENV
MCS_ACCORDIN_DIRECT_STATS_ONLY_ENV = experiment_three.MCS_ACCORDIN_DIRECT_STATS_ONLY_ENV
MCS_ACCORDIN_DIRECT_ENV_PREFIX = experiment_three.MCS_ACCORDIN_DIRECT_ENV_PREFIX
EXCLUDED_PLOT_LOCKS = {experiment_defaults.ACCORDIN_TASKSET_LOCK}
FALLBACK_PLOT_COLORS = [
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
]


@dataclass(frozen=True)
class TwoLockCase:
    name: str
    group_a_critical_ns: int
    group_a_outside_ns: int
    group_b_critical_ns: int
    group_b_outside_ns: int


@dataclass(frozen=True)
class FlexguardInterposeBuildSpec:
    make_target: str | None = None
    make_vars: tuple[str, ...] = ()
    clean_first: bool = False


CASES = (
    TwoLockCase("homogeneous", 300, 3000, 300, 3000),
    TwoLockCase("heterogeneous_mild", 3000, 3000, 300, 3000),
    TwoLockCase("heterogeneous_extreme", 3000, 300, 100, 3000),
)

BUILTIN_LOCK_KINDS = {
    "mutex": "mutex",
    experiment_defaults.PTHREAD_SPINLOCK_LOCK: "pthread_spinlock",
    "mcs": "mcs",
    "reciprocating": "reciprocating",
}
FLEXGUARD_INTERPOSE_LOCKS = {"flexguard", "mcstas", "mcs_extension", "mcstp", "malthusian"}
FLEXGUARD_INTERPOSE_ARTIFACT_LOCKS = {
    "mcs_extension": "mcs",
}
FLEXGUARD_TIMESLICE_EXTENSIONS = {
    "mcs_extension": "require",
}
FLEXGUARD_INTERPOSE_BUILD_SPECS = {
    "flexguard": FlexguardInterposeBuildSpec(make_target="build/interpose_flexguard.sh"),
    "mcstas": FlexguardInterposeBuildSpec(make_target="mcstas"),
    "mcs_extension": FlexguardInterposeBuildSpec(make_target="mcs"),
    "mcstp": FlexguardInterposeBuildSpec(
        make_vars=("LOCK_VERSION=MCSTP", "ADD_PADDING=1", "USE_REAL_PTHREAD=1"),
        clean_first=True,
    ),
    "malthusian": FlexguardInterposeBuildSpec(
        make_vars=("LOCK_VERSION=MALTHUSIAN", "ADD_PADDING=1", "USE_REAL_PTHREAD=1"),
        clean_first=True,
    ),
}
LOCAL_LOCK_ALIASES = {
    "pthread": "mutex",
    "stock": "mutex",
}
SUPPORTED_LOCKS = (
    set(BUILTIN_LOCK_KINDS)
    | FLEXGUARD_INTERPOSE_LOCKS
    | set(experiment_defaults.OTHERLOCKS_INTERPOSE_LOCKS)
    | set(experiment_defaults.ACCORDIN_VARIANT_LOCKS)
    | {MCS_ACCORDIN_LOCK}
)

RAW_FIELDS = (
    "case",
    "lock",
    "lock_label",
    "threads",
    "group_a_threads",
    "group_b_threads",
    "group_a_critical_ns",
    "group_a_outside_ns",
    "group_b_critical_ns",
    "group_b_outside_ns",
    "repeat",
    "throughput_ops_per_sec",
    "elapsed_seconds",
    "bench_wall_seconds",
    "total_operations",
    "avg_lock_hold_ns",
    "avg_wait_ns_estimated",
    "avg_lock_handoff_ns_estimated",
    "lock_hold_samples",
    "group_a_total_operations",
    "group_a_throughput_ops_per_sec",
    "group_a_avg_lock_hold_ns",
    "group_a_avg_wait_ns_estimated",
    "group_a_avg_lock_handoff_ns_estimated",
    "group_a_ideal_throughput_ops_per_sec",
    "group_a_normalized_efficiency",
    "group_a_normalized_slowdown",
    "group_b_total_operations",
    "group_b_throughput_ops_per_sec",
    "group_b_avg_lock_hold_ns",
    "group_b_avg_wait_ns_estimated",
    "group_b_avg_lock_handoff_ns_estimated",
    "group_b_ideal_throughput_ops_per_sec",
    "group_b_normalized_efficiency",
    "group_b_normalized_slowdown",
    "fairness_jain",
    "command_log",
)

SUMMARY_FIELDS = tuple(field for field in RAW_FIELDS if field not in {"repeat", "command_log"})
SUMMARY_NUMERIC_FIELDS = tuple(
    field
    for field in SUMMARY_FIELDS
    if field
    not in {
        "case",
        "lock",
        "lock_label",
    }
)


def parse_csv_ints(text: str, name: str) -> tuple[int, ...]:
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
        if value % 2 != 0:
            raise argparse.ArgumentTypeError(f"{name} values must be even for two-lock group splitting: {item}")
        values.append(value)
    if not values:
        raise argparse.ArgumentTypeError(f"{name} must contain at least one value")
    return tuple(dict.fromkeys(values))


def parse_csv_strings(text: str) -> tuple[str, ...]:
    return tuple(item.strip() for item in text.split(",") if item.strip())


def normalize_lock(raw: str) -> str:
    key = raw.strip().lower()
    return LOCAL_LOCK_ALIASES.get(key, experiment_defaults.normalize_lock(key))


def parse_locks(text: str | None, profile: str) -> tuple[str, ...]:
    raw_locks = experiment_defaults.lock_profile_locks(profile) if text is None else parse_csv_strings(text)
    locks: list[str] = []
    for raw in raw_locks:
        lock = normalize_lock(raw)
        if lock not in SUPPORTED_LOCKS:
            supported = sorted(SUPPORTED_LOCKS | set(LOCAL_LOCK_ALIASES))
            raise argparse.ArgumentTypeError(
                f"Unsupported experiment6 lock {raw!r}. Supported locks: {', '.join(supported)}"
            )
        if lock not in locks:
            locks.append(lock)
    if not locks:
        raise argparse.ArgumentTypeError("--locks resolved to an empty lock set")
    return tuple(locks)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Experiment 6: two independent locks with two half-sized worker groups."
    )
    parser.add_argument("--output-root", type=Path, default=DEFAULT_OUTPUT_ROOT)
    parser.add_argument("--plot-only", type=Path, default=None, metavar="RESULT_ROOT", help="Regenerate summary.csv and PNGs from raw.csv.")
    parser.add_argument("--skip-plots", action="store_true", help="Do not generate PNG plots after running benchmarks.")
    parser.add_argument("--force", action="store_true", help="Replace existing raw/summary/settings files")
    parser.add_argument("--resume", action="store_true", help="Skip complete raw rows already present")
    parser.add_argument("--dry-run", action="store_true", help="Print commands and CSV schema without running")
    parser.add_argument(
        "--lock-profile",
        choices=experiment_defaults.lock_profile_names(),
        default=experiment_defaults.DEFAULT_LOCK_PROFILE,
    )
    parser.add_argument("--locks", help="Comma-separated lock list. Default comes from --lock-profile.")
    parser.add_argument("--threads", default="32,64,96,128,192,256")
    parser.add_argument("--duration-ms", type=int, default=5000)
    parser.add_argument("--warmup-duration-ms", type=int, default=1000)
    parser.add_argument("--repeats", type=int, default=4)
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

    if args.duration_ms <= 0:
        parser.error("--duration-ms must be > 0")
    if args.warmup_duration_ms < 0:
        parser.error("--warmup-duration-ms must be >= 0")
    if args.repeats <= 0:
        parser.error("--repeats must be > 0")
    if args.command_timeout_seconds <= 0:
        parser.error("--command-timeout-seconds must be > 0")

    args.thread_counts = parse_csv_ints(args.threads, "--threads")
    try:
        args.lock_keys = parse_locks(args.locks, args.lock_profile)
    except argparse.ArgumentTypeError as exc:
        parser.error(str(exc))
    args.lock_profile_source = "manual" if args.locks is not None else "profile"
    return args


def shlex_join(cmd: Sequence[str]) -> str:
    return shlex.join(str(part) for part in cmd)


def env_command(cmd: Sequence[str], env: dict[str, str | None], *, needs_sudo: bool, sudo_mode: str) -> list[str]:
    env_unsets = [key for key, value in sorted(env.items()) if value is None]
    env_items = [f"{key}={value}" for key, value in sorted(env.items()) if value is not None]
    env_prefix = ["env"]
    for key in env_unsets:
        env_prefix.extend(["-u", key])
    env_prefix.extend(env_items)
    use_sudo = sudo_mode == "all" or (sudo_mode == "auto" and needs_sudo)
    if needs_sudo and sudo_mode == "none" and os.geteuid() != 0:
        raise RuntimeError(f"{cmd[0]} requires root for scheduler/BPF setup; use --sudo-mode auto/all")
    if use_sudo and os.geteuid() != 0:
        if env:
            return ["sudo", "-n", "--", *env_prefix, *map(str, cmd)]
        return ["sudo", "-n", "--", *map(str, cmd)]
    if env:
        return [*env_prefix, *map(str, cmd)]
    return [str(part) for part in cmd]


def lock_label(lock: str) -> str:
    return experiment_defaults.lock_label(lock)


def is_accordin_direct_lock(lock: str) -> bool:
    return experiment_defaults.is_accordin_lock(lock)


def is_mcs_accordin_lock(lock: str) -> bool:
    return experiment_defaults.is_mcs_accordin_lock(lock)


def is_flexguard_interpose_lock(lock: str) -> bool:
    return lock in FLEXGUARD_INTERPOSE_LOCKS


def is_otherlocks_interpose_lock(lock: str) -> bool:
    return experiment_defaults.is_otherlocks_interpose_lock(lock)


def flexguard_interpose_artifact_lock(lock: str) -> str:
    return FLEXGUARD_INTERPOSE_ARTIFACT_LOCKS.get(lock, lock)


def flexguard_interpose_script(lock: str) -> Path:
    return FLEXGUARD_DIR / "build" / f"interpose_{flexguard_interpose_artifact_lock(lock)}.sh"


def flexguard_interpose_library(lock: str) -> Path:
    return FLEXGUARD_DIR / "build" / f"interpose_{flexguard_interpose_artifact_lock(lock)}.so"


def flexguard_interpose_needs_sudo(lock: str) -> bool:
    return lock.startswith("flexguard")


def flexguard_timeslice_extension(lock: str) -> str:
    return FLEXGUARD_TIMESLICE_EXTENSIONS.get(lock, "off")


def otherlocks_interpose_script(lock: str) -> Path:
    return OTHERLOCKS_BUILD_DIR / f"interpose_{lock}.sh"


def otherlocks_interpose_library(lock: str) -> Path:
    return OTHERLOCKS_BUILD_DIR / f"interpose_{lock}.so"


def accordin_env(lock: str) -> dict[str, str | None]:
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
    if experiment_defaults.accordin_disables_admission(lock):
        env["ACCORDIN_DISABLE_ADMISSION"] = "1"
        env[ACCORDIN_DIRECT_STATS_ONLY_ENV] = "1"
    return env


def mcs_accordin_env() -> dict[str, str | None]:
    return experiment_three.mcs_accordin_direct_env()


def mutexbench_command(case: TwoLockCase, lock: str, threads: int, args: argparse.Namespace) -> tuple[list[str], dict[str, str | None], bool]:
    if is_mcs_accordin_lock(lock):
        lock_kind = MCS_ACCORDIN_DIRECT_LOCK_KIND
        env = mcs_accordin_env()
        needs_sudo = True
        cmd_prefix = []
    elif is_flexguard_interpose_lock(lock) or is_otherlocks_interpose_lock(lock):
        lock_kind = "mutex"
        env: dict[str, str | None] = {}
        needs_sudo = flexguard_interpose_needs_sudo(lock) if is_flexguard_interpose_lock(lock) else False
        script = flexguard_interpose_script(lock) if is_flexguard_interpose_lock(lock) else otherlocks_interpose_script(lock)
        cmd_prefix = [str(script)]
        timeslice_extension = flexguard_timeslice_extension(lock) if is_flexguard_interpose_lock(lock) else "off"
    else:
        lock_kind = BUILTIN_LOCK_KINDS.get(lock, ACCORDIN_DIRECT_LOCK_KIND)
        env = accordin_env(lock) if is_accordin_direct_lock(lock) else {}
        needs_sudo = is_accordin_direct_lock(lock)
        cmd_prefix = []
        timeslice_extension = "off"
    cmd = [
        *cmd_prefix,
        str(MUTEX_BENCH),
        "--workload",
        "two-lock",
        "--threads",
        str(threads),
        "--duration-ms",
        str(args.duration_ms),
        "--warmup-duration-ms",
        str(args.warmup_duration_ms),
        "--group-a-critical-ns",
        str(case.group_a_critical_ns),
        "--group-a-outside-ns",
        str(case.group_a_outside_ns),
        "--group-b-critical-ns",
        str(case.group_b_critical_ns),
        "--group-b-outside-ns",
        str(case.group_b_outside_ns),
        "--lock-kind",
        lock_kind,
        "--timeslice-extension",
        timeslice_extension,
    ]
    if experiment_defaults.accordin_uses_taskset(lock):
        cmd = ["taskset", "-c", args.mcs_accordin_taskset_cpus, *cmd]
    return cmd, env, needs_sudo


def run_build_command(cmd: Sequence[str], *, dry_run: bool) -> None:
    if dry_run:
        print(shlex_join(cmd))
    else:
        subprocess.run(list(cmd), check=True)


def ensure_flexguard_interpose(lock: str, *, dry_run: bool) -> None:
    script = flexguard_interpose_script(lock)
    library = flexguard_interpose_library(lock)
    if not dry_run and script.is_file() and os.access(script, os.X_OK) and library.is_file():
        return

    spec = FLEXGUARD_INTERPOSE_BUILD_SPECS[lock]
    if spec.make_target is not None:
        run_build_command(["make", "-C", str(FLEXGUARD_DIR), spec.make_target], dry_run=dry_run)
    else:
        if spec.clean_first:
            run_build_command(["make", "-C", str(FLEXGUARD_DIR), "clean"], dry_run=dry_run)
        build_cmd = ["make", "-C", str(FLEXGUARD_DIR), *spec.make_vars, "interpose.so", "interpose.sh"]
        run_build_command(build_cmd, dry_run=dry_run)
        if dry_run:
            print(shlex_join(["cp", str(FLEXGUARD_DIR / "interpose.so"), str(library)]))
            print(shlex_join(["cp", str(FLEXGUARD_DIR / "interpose.sh"), str(script)]))
        else:
            source_library = FLEXGUARD_DIR / "interpose.so"
            source_script = FLEXGUARD_DIR / "interpose.sh"
            if not source_library.is_file():
                raise RuntimeError(f"FlexGuard interpose library was not produced: {source_library}")
            if not os.access(source_script, os.X_OK):
                raise RuntimeError(f"FlexGuard interpose script was not produced: {source_script}")
            library.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source_library, library)
            shutil.copy2(source_script, script)

    if not dry_run:
        if not os.access(script, os.X_OK):
            raise RuntimeError(f"FlexGuard interpose script was not produced: {script}")
        if not library.is_file():
            raise RuntimeError(f"FlexGuard interpose library was not produced: {library}")


def ensure_otherlocks_interpose(lock: str, *, dry_run: bool) -> None:
    script = otherlocks_interpose_script(lock)
    library = otherlocks_interpose_library(lock)
    if not dry_run and script.is_file() and os.access(script, os.X_OK) and library.is_file():
        return
    run_build_command(["make", "-C", str(OTHERLOCKS_DIR), f"build/interpose_{lock}.sh"], dry_run=dry_run)
    if not dry_run:
        if not os.access(script, os.X_OK):
            raise RuntimeError(f"otherlocks interpose script was not produced: {script}")
        if not library.is_file():
            raise RuntimeError(f"otherlocks interpose library was not produced: {library}")


def ensure_builds(locks: Iterable[str], *, dry_run: bool) -> None:
    run_build_command(["make", "-C", str(MUTEXBENCH_DIR), "mutex_bench"], dry_run=dry_run)

    if any(is_accordin_direct_lock(lock) for lock in locks):
        build_cmd = ["cargo", "build", "-p", ACCORDIN_DIRECT_PACKAGE, "--release"]
        if dry_run:
            print(shlex_join(build_cmd))
        else:
            subprocess.run(build_cmd, cwd=REPO_ROOT, check=True)
            if not ACCORDIN_DIRECT_RELEASE_LIB.is_file():
                raise RuntimeError(f"{ACCORDIN_DIRECT_PACKAGE} library was not produced: {ACCORDIN_DIRECT_RELEASE_LIB}")
    if any(is_mcs_accordin_lock(lock) for lock in locks):
        build_cmd = ["cargo", "build", "-p", MCS_ACCORDIN_DIRECT_PACKAGE, "--release"]
        if dry_run:
            print(shlex_join(build_cmd))
        else:
            subprocess.run(build_cmd, cwd=REPO_ROOT, check=True)
            if not MCS_ACCORDIN_DIRECT_RELEASE_LIB.is_file():
                raise RuntimeError(
                    f"{MCS_ACCORDIN_DIRECT_PACKAGE} library was not produced: {MCS_ACCORDIN_DIRECT_RELEASE_LIB}"
                )
    for lock in sorted(lock for lock in locks if is_flexguard_interpose_lock(lock)):
        ensure_flexguard_interpose(lock, dry_run=dry_run)
    for lock in sorted(lock for lock in locks if is_otherlocks_interpose_lock(lock)):
        ensure_otherlocks_interpose(lock, dry_run=dry_run)


def read_existing_keys(raw_path: Path) -> set[tuple[str, str, int, int]]:
    if not raw_path.is_file():
        return set()
    keys: set[tuple[str, str, int, int]] = set()
    with raw_path.open(newline="", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        for row in reader:
            keys.add((row["case"], row["lock"], int(row["threads"]), int(row["repeat"])))
    return keys


def parse_metrics(text: str) -> dict[str, str]:
    metrics: dict[str, str] = {}
    for line in text.splitlines():
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        metrics[key.strip()] = value.strip()
    return metrics


def run_one(
    case: TwoLockCase,
    lock: str,
    threads: int,
    repeat: int,
    args: argparse.Namespace,
    logs_dir: Path,
) -> dict[str, str]:
    cmd, env, needs_sudo = mutexbench_command(case, lock, threads, args)
    run_cmd = env_command(cmd, env, needs_sudo=needs_sudo, sudo_mode=args.sudo_mode)
    log_path = logs_dir / f"{case.name}_{lock}_t{threads}_r{repeat}.log"

    start_ns = time.time_ns()
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
    end_ns = time.time_ns()
    bench_wall_seconds = (end_ns - start_ns) / 1_000_000_000.0

    log_path.write_text(
        "$ " + shlex_join(run_cmd) + "\n\n" + output,
        encoding="utf-8",
    )
    if status != 0:
        raise RuntimeError(f"benchmark failed for case={case.name} lock={lock} threads={threads} repeat={repeat}; see {log_path}")

    metrics = parse_metrics(output)
    row = {
        "case": case.name,
        "lock": lock,
        "lock_label": lock_label(lock),
        "threads": str(threads),
        "group_a_threads": metrics.get("group_a_threads", ""),
        "group_b_threads": metrics.get("group_b_threads", ""),
        "group_a_critical_ns": str(case.group_a_critical_ns),
        "group_a_outside_ns": str(case.group_a_outside_ns),
        "group_b_critical_ns": str(case.group_b_critical_ns),
        "group_b_outside_ns": str(case.group_b_outside_ns),
        "repeat": str(repeat),
        "bench_wall_seconds": f"{bench_wall_seconds:.6f}",
        "command_log": str(log_path),
    }
    for field in RAW_FIELDS:
        if field not in row:
            row[field] = metrics.get(field, "")
    missing = [
        field
        for field in (
            "throughput_ops_per_sec",
            "group_a_throughput_ops_per_sec",
            "group_b_throughput_ops_per_sec",
            "group_a_avg_lock_handoff_ns_estimated",
            "group_b_avg_lock_handoff_ns_estimated",
            "fairness_jain",
        )
        if not row[field]
    ]
    if missing:
        raise RuntimeError(f"benchmark output missing metrics {missing}; see {log_path}")
    return row


def write_settings(root: Path, args: argparse.Namespace) -> None:
    settings = {
        "experiment": "experiment6_two_lock_mutexbench",
        "description": "Two independent locks with two half-sized worker groups.",
        "output_root": str(root),
        "lock_profile": args.lock_profile,
        "lock_profile_source": args.lock_profile_source,
        "locks": list(args.lock_keys),
        "threads": list(args.thread_counts),
        "duration_ms": args.duration_ms,
        "warmup_duration_ms": args.warmup_duration_ms,
        "repeats": args.repeats,
        "mcs_accordin_taskset_cpus": args.mcs_accordin_taskset_cpus,
        "cases": [case.__dict__ for case in CASES],
    }
    (root / "settings.json").write_text(json.dumps(settings, indent=2) + "\n", encoding="utf-8")


def write_summary(raw_path: Path, summary_path: Path) -> None:
    with raw_path.open(newline="", encoding="utf-8") as f:
        rows = list(csv.DictReader(f))
    groups: dict[tuple[str, str, str], list[dict[str, str]]] = {}
    for row in rows:
        groups.setdefault((row["case"], row["lock"], row["threads"]), []).append(row)

    with summary_path.open("w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=SUMMARY_FIELDS, lineterminator="\n")
        writer.writeheader()
        for key in sorted(groups, key=lambda item: (item[0], item[1], int(item[2]))):
            group_rows = groups[key]
            first = group_rows[0]
            out: dict[str, str] = {
                "case": first["case"],
                "lock": first["lock"],
                "lock_label": lock_label(first["lock"]),
            }
            for field in SUMMARY_NUMERIC_FIELDS:
                values = [float(row[field]) for row in group_rows if row.get(field) not in ("", None)]
                out[field] = f"{statistics.mean(values):.6f}" if values else ""
            writer.writerow(out)


def safe_name(value: str) -> str:
    out = "".join(ch.lower() if ch.isalnum() else "_" for ch in value.strip())
    return "_".join(part for part in out.split("_") if part) or "unknown"


def case_sort_key(name: str) -> tuple[int, str]:
    case_order = {case.name: index for index, case in enumerate(CASES)}
    return (case_order.get(name, len(case_order)), name)


def load_summary_rows(summary_path: Path) -> list[dict[str, str]]:
    if not summary_path.is_file():
        raise RuntimeError(f"{summary_path} does not exist")
    with summary_path.open(newline="", encoding="utf-8") as f:
        return list(csv.DictReader(f))


def plot_rows(rows: Iterable[dict[str, str]]) -> list[dict[str, str]]:
    return [row for row in rows if row["lock"] not in EXCLUDED_PLOT_LOCKS]


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


def plot_metric_for_case(
    result_root: Path,
    rows: list[dict[str, str]],
    case: str,
    metric: str,
    output_name: str,
    ylabel: str,
    color_by_key: dict[str, str],
) -> Path:
    try:
        import matplotlib
    except ImportError as exc:
        raise RuntimeError("matplotlib is required to generate experiment6 plots") from exc

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.ticker import ScalarFormatter

    case_rows = [row for row in rows if row["case"] == case and row.get(metric)]
    locks = tuple(dict.fromkeys(row["lock"] for row in case_rows))
    fig, ax = plt.subplots(figsize=(8.8, 5.2))
    for lock in locks:
        lock_rows = sorted(
            (row for row in case_rows if row["lock"] == lock),
            key=lambda row: float(row["threads"]),
        )
        if not lock_rows:
            continue
        label = lock_rows[0].get("lock_label") or lock
        xs = [float(row["threads"]) for row in lock_rows]
        ys = [float(row[metric]) for row in lock_rows]
        if metric.endswith("throughput_ops_per_sec"):
            ys = [value / 1_000_000.0 for value in ys]
        style = lock_plot_style(lock, color_by_key)
        ax.plot(xs, ys, marker="o", linewidth=1.8, markersize=4.5, label=label)
        ax.lines[-1].set_color(style["color"])
        ax.lines[-1].set_linestyle(style["linestyle"])
        ax.lines[-1].set_marker(style["marker"])

    ax.set_title(case.replace("_", " ").title())
    ax.set_xlabel("Threads")
    ax.set_ylabel(ylabel)
    ax.grid(True, which="major", linestyle="--", linewidth=0.5, alpha=0.45)
    ax.xaxis.set_major_formatter(ScalarFormatter())
    if metric == "fairness_jain":
        ax.set_ylim(0.0, 1.05)
    if locks:
        ax.legend(loc="best", fontsize=8)
    fig.tight_layout()
    output_path = result_root / output_name
    fig.savefig(output_path, dpi=180)
    plt.close(fig)
    return output_path


def generate_plots(result_root: Path) -> tuple[Path, ...]:
    rows = plot_rows(load_summary_rows(result_root / "summary.csv"))
    cases = sorted(tuple(dict.fromkeys(row["case"] for row in rows)), key=case_sort_key)
    color_by_key = plot_color_map(row["lock"] for row in rows)
    plot_paths: list[Path] = []
    for path in result_root.glob("ops_vs_threads_*.png"):
        path.unlink()
    for case in cases:
        suffix = safe_name(case)
        plot_paths.append(
            plot_metric_for_case(
                result_root,
                rows,
                case,
                "group_a_throughput_ops_per_sec",
                f"group_a_ops_vs_threads_{suffix}.png",
                "Group A throughput (M ops/s)",
                color_by_key,
            )
        )
        plot_paths.append(
            plot_metric_for_case(
                result_root,
                rows,
                case,
                "group_b_throughput_ops_per_sec",
                f"group_b_ops_vs_threads_{suffix}.png",
                "Group B throughput (M ops/s)",
                color_by_key,
            )
        )
        plot_paths.append(
            plot_metric_for_case(
                result_root,
                rows,
                case,
                "fairness_jain",
                f"fairness_vs_threads_{suffix}.png",
                "Jain fairness index",
                color_by_key,
            )
        )
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


def dry_run(args: argparse.Namespace) -> None:
    print("raw_fields: " + ",".join(RAW_FIELDS))
    print("summary_fields: " + ",".join(SUMMARY_FIELDS))
    ensure_builds(args.lock_keys, dry_run=True)
    for case in CASES:
        for lock in args.lock_keys:
            for threads in args.thread_counts:
                cmd, env, needs_sudo = mutexbench_command(case, lock, threads, args)
                run_cmd = env_command(cmd, env, needs_sudo=needs_sudo, sudo_mode=args.sudo_mode)
                print(f"case={case.name} lock={lock} threads={threads} repeat=1 {shlex_join(run_cmd)}")


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
        for case in CASES:
            for lock in args.lock_keys:
                for threads in args.thread_counts:
                    for repeat in range(1, args.repeats + 1):
                        key = (case.name, lock, threads, repeat)
                        if key in existing:
                            print(f"Skipping complete run: case={case.name} lock={lock} threads={threads} repeat={repeat}")
                            continue
                        print(f"Running case={case.name} lock={lock} threads={threads} repeat={repeat}")
                        row = run_one(case, lock, threads, repeat, args, logs_dir)
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
