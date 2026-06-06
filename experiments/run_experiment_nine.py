#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import os
import re
import shlex
import statistics
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

import experiment_defaults


REPO_ROOT = Path(__file__).resolve().parents[1]
MUTEXBENCH_DIR = REPO_ROOT / "bench" / "mutexbench"
MULTILOCKBENCH_BINARY = MUTEXBENCH_DIR / "multilockbench"
FLEXGUARD_DIR = REPO_ROOT / "bench" / "flexguard"
FLEXGUARD_INTERPOSE_SCRIPT = FLEXGUARD_DIR / "build" / "interpose_flexguard.sh"
FLEXGUARD_INTERPOSE_LIBRARY = FLEXGUARD_DIR / "build" / "interpose_flexguard.so"
OTHERLOCKS_DIR = REPO_ROOT / "bench" / "otherlocks"
OTHERLOCKS_BUILD_DIR = OTHERLOCKS_DIR / "build"
MCS_TAS_ACCORDIN_DIRECT_PACKAGE = "mcs_tas_accordin_direct"
MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB = (
    REPO_ROOT / "target" / "release" / "libmcs_tas_accordin_direct.so"
)
MCS_TAS_ACCORDIN_DIRECT_LIB_ENV = "MCS_TAS_ACCORDIN_DIRECT_LIB"
MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF_ENV = "MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF"
MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY_ENV = "MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY"
MCS_ACCORDIN_LOCK = experiment_defaults.MCS_ACCORDIN_LOCK
MCS_ACCORDIN_DIRECT_PACKAGE = "mcs_accordin_direct"
MCS_ACCORDIN_DIRECT_RELEASE_LIB = REPO_ROOT / "target" / "release" / "libmcs_accordin_direct.so"
MCS_ACCORDIN_DIRECT_LIB_ENV = "MCS_ACCORDIN_DIRECT_LIB"
MCS_ACCORDIN_DIRECT_DISABLE_BPF_ENV = "MCS_ACCORDIN_DIRECT_DISABLE_BPF"
MCS_ACCORDIN_DIRECT_STATS_ONLY_ENV = "MCS_ACCORDIN_DIRECT_STATS_ONLY"

MCS_TAS_LOCK = "mcstas"
MCS_EXTENSION_LOCK = "mcs_extension"
MCSTP_LOCK = "mcstp"
FLEXGUARD_LOCK = "flexguard"
ACCORDIN_LOCK = experiment_defaults.ACCORDIN_BASE_LOCK
DEFAULT_LOCK_PROFILE = experiment_defaults.DEFAULT_LOCK_PROFILE
DEFAULT_LOCKS = experiment_defaults.DEFAULT_LOCKS
EXPERIMENT9_LOCK_ALIASES = {
    "mcs_tas_accordin_direct": ACCORDIN_LOCK,
    "mcs-tas-accordin-direct": ACCORDIN_LOCK,
    "accordin_direct": ACCORDIN_LOCK,
    "accordin-direct": ACCORDIN_LOCK,
}
LOCK_KIND_BY_LOCK = {
    "mutex": "mutex",
    experiment_defaults.PTHREAD_SPINLOCK_LOCK: "pthread_spinlock",
    "mcs": "mcs",
    MCS_EXTENSION_LOCK: "mutex",
    "reciprocating": "reciprocating",
    MCS_TAS_LOCK: "mutex",
    MCSTP_LOCK: "mutex",
    FLEXGUARD_LOCK: "mutex",
    "cna": "mutex",
    "gcr": "mutex",
    MCS_ACCORDIN_LOCK: "mcs_accordin_direct",
    ACCORDIN_LOCK: "mcs_tas_accordin_direct",
}
FLEXGUARD_INTERPOSE_ARTIFACT_LOCKS = {
    FLEXGUARD_LOCK: "flexguard",
    MCS_TAS_LOCK: "mcstas",
    MCS_EXTENSION_LOCK: "mcs",
    MCSTP_LOCK: "mcstp",
}
FLEXGUARD_INTERPOSE_BUILD_TARGETS = {
    FLEXGUARD_LOCK: "build/interpose_flexguard.sh",
    MCS_TAS_LOCK: "mcstas",
    MCS_EXTENSION_LOCK: "mcs",
    MCSTP_LOCK: "mcstp",
}
FLEXGUARD_TIMESLICE_EXTENSIONS = {
    MCS_EXTENSION_LOCK: "require",
}

DEFAULT_DURATION_MS = 9_000
DEFAULT_WARMUP_DURATION_MS = 1_000
DEFAULT_LOCK_COUNT = 64
DEFAULT_ZIPF_ALPHA = 1.2
DEFAULT_CRITICAL_NS = 300
DEFAULT_OUTSIDE_NS = 3_000
DEFAULT_SEED = 1
DEFAULT_TIMING_SAMPLE_STRIDE = 8
DEFAULT_REPEATS = experiment_defaults.DEFAULT_REPEATS
DEFAULT_THREADS = experiment_defaults.DEFAULT_THREADS
DEFAULT_COMMAND_TIMEOUT_SECONDS = 21_600

RAW_FIELDS = (
    "lock",
    "lock_label",
    "threads",
    "duration_ms",
    "warmup_duration_ms",
    "lock_count",
    "zipf_alpha",
    "critical_ns",
    "outside_ns",
    "seed",
    "repeat",
    "throughput_ops_per_sec",
    "elapsed_seconds",
    "total_operations",
    "avg_lock_hold_ns",
    "avg_wait_ns_estimated",
    "lock_hold_samples",
    "hotspot_lock",
    "hotspot_lock_operations",
    "hotspot_lock_operation_pct",
    "per_thread_operations",
    "per_lock_operations",
    "wall_seconds",
    "command_log",
)
SUMMARY_FIELDS = tuple(
    field
    for field in RAW_FIELDS
    if field not in {"repeat", "command_log", "per_thread_operations", "per_lock_operations"}
) + ("runs",)
SUMMARY_NUMERIC_FIELDS = {
    "throughput_ops_per_sec",
    "elapsed_seconds",
    "total_operations",
    "avg_lock_hold_ns",
    "avg_wait_ns_estimated",
    "lock_hold_samples",
    "hotspot_lock_operations",
    "hotspot_lock_operation_pct",
    "wall_seconds",
}


@dataclass(frozen=True)
class RunArgs:
    output_root: Path
    lock_keys: tuple[str, ...]
    threads: tuple[int, ...]
    duration_ms: int
    warmup_duration_ms: int
    lock_count: int
    zipf_alpha: float
    critical_ns: int
    outside_ns: int
    seed: int
    repeats: int
    command_timeout_seconds: int
    timing_sample_stride: int = DEFAULT_TIMING_SAMPLE_STRIDE
    sudo_mode: str = "auto"
    build_missing: bool = False
    skip_plots: bool = False
    dry_run: bool = False
    force: bool = False
    resume: bool = False
    lock_profile: str = DEFAULT_LOCK_PROFILE
    lock_profile_source: str = "profile"
    plot_only: Path | None = None


@dataclass(frozen=True)
class CommandResult:
    output: str
    log_path: Path
    wall_seconds: float


class CommandError(RuntimeError):
    def __init__(self, message: str, returncode: int, log_path: Path, output: str) -> None:
        super().__init__(message)
        self.returncode = returncode
        self.log_path = log_path
        self.output = output


class CommandLogger:
    def __init__(self, result_root: Path) -> None:
        self.result_root = result_root
        self.log_dir = result_root / "logs"
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.manifest_path = result_root / "commands.json"
        self.records: list[dict[str, object]] = self._load_records()

    def _load_records(self) -> list[dict[str, object]]:
        if not self.manifest_path.is_file():
            return []
        with self.manifest_path.open("r", encoding="utf-8") as f:
            records = json.load(f)
        if not isinstance(records, list) or not all(isinstance(record, dict) for record in records):
            raise RuntimeError(f"commands manifest must be a JSON list: {self.manifest_path}")
        return list(records)

    def _write_records(self) -> None:
        with self.manifest_path.open("w", encoding="utf-8") as f:
            json.dump(self.records, f, indent=2)
            f.write("\n")

    def resolve_log_path(self, log_name: str) -> Path:
        candidate = self.log_dir / log_name
        if not candidate.exists():
            return candidate
        for index in range(1, 10_000):
            indexed = self.log_dir / f"{candidate.stem}_{index}{candidate.suffix}"
            if not indexed.exists():
                return indexed
        raise RuntimeError(f"could not find unused log path for {log_name}")

    def run(
        self,
        cmd: Sequence[str],
        *,
        log_name: str,
        cwd: Path = REPO_ROOT,
        timeout_seconds: int,
    ) -> CommandResult:
        log_path = self.resolve_log_path(log_name)
        started_at = dt.datetime.now(dt.timezone.utc)
        start = time.monotonic()
        record: dict[str, object] = {
            "command": [str(part) for part in cmd],
            "command_text": shlex_join(cmd),
            "cwd": str(cwd),
            "log_path": str(log_path),
            "started_at": started_at.isoformat(),
        }
        output = ""
        returncode = 0
        try:
            completed = subprocess.run(
                [str(part) for part in cmd],
                cwd=str(cwd),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=timeout_seconds if timeout_seconds > 0 else None,
                check=False,
            )
            output = completed.stdout or ""
            returncode = completed.returncode
        except subprocess.TimeoutExpired as exc:
            output = (exc.stdout or "") if isinstance(exc.stdout, str) else ""
            output += f"\nCommand timed out after {exc.timeout} seconds\n"
            returncode = 124
        wall_seconds = time.monotonic() - start
        ended_at = dt.datetime.now(dt.timezone.utc)
        record.update(
            {
                "ended_at": ended_at.isoformat(),
                "returncode": returncode,
                "wall_seconds": wall_seconds,
            }
        )
        log_path.write_text(
            f"$ {shlex_join(cmd)}\n"
            f"cwd: {cwd}\n"
            f"started_at: {started_at.isoformat()}\n"
            f"ended_at: {ended_at.isoformat()}\n"
            f"returncode: {returncode}\n"
            f"wall_seconds: {wall_seconds:.6f}\n\n"
            f"{output}",
            encoding="utf-8",
        )
        self.records.append(record)
        self._write_records()
        if returncode != 0:
            raise CommandError(f"command failed with return code {returncode}: {log_path}", returncode, log_path, output)
        return CommandResult(output=output, log_path=log_path, wall_seconds=wall_seconds)


def shlex_join(cmd: Sequence[str]) -> str:
    return shlex.join(str(part) for part in cmd)


def default_output_root() -> Path:
    timestamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    return REPO_ROOT / "experiments" / "results" / f"experiment9_multilockbench_{timestamp}"


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


def non_negative_float(value: str) -> float:
    parsed = float(value)
    if parsed < 0.0:
        raise argparse.ArgumentTypeError("value must be >= 0")
    return parsed


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
        if value not in values:
            values.append(value)
    if not values:
        raise argparse.ArgumentTypeError(f"{name} must contain at least one value")
    return tuple(values)


def parse_csv_strings(text: str) -> tuple[str, ...]:
    return tuple(item.strip() for item in text.split(",") if item.strip())


def normalize_lock(raw: str) -> str:
    normalized = raw.strip().lower()
    lock = EXPERIMENT9_LOCK_ALIASES.get(normalized, normalized)
    if lock not in LOCK_KIND_BY_LOCK:
        lock = experiment_defaults.normalize_lock(lock)
        lock = EXPERIMENT9_LOCK_ALIASES.get(lock, lock)
    if lock not in LOCK_KIND_BY_LOCK:
        supported = ", ".join(sorted(set(LOCK_KIND_BY_LOCK) | set(EXPERIMENT9_LOCK_ALIASES)))
        raise argparse.ArgumentTypeError(
            f"Unsupported experiment9 lock {raw!r}. Supported locks: {supported}"
        )
    return lock


def parse_locks(text: str | None, profile: str) -> tuple[str, ...]:
    try:
        raw_locks = experiment_defaults.lock_profile_locks(profile) if text is None else parse_csv_strings(text)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(str(exc)) from exc
    locks: list[str] = []
    for raw in raw_locks:
        lock = normalize_lock(raw)
        if lock not in locks:
            locks.append(lock)
    if not locks:
        raise argparse.ArgumentTypeError("At least one lock must be selected")
    return tuple(locks)


def lock_label(lock: str) -> str:
    return experiment_defaults.lock_label(lock)


def lock_sort_key(lock: str) -> tuple[int, str]:
    return experiment_defaults.lock_sort_key(lock)


def otherlocks_interpose_script(lock: str) -> Path:
    return OTHERLOCKS_BUILD_DIR / f"interpose_{lock}.sh"


def otherlocks_interpose_library(lock: str) -> Path:
    return OTHERLOCKS_BUILD_DIR / f"interpose_{lock}.so"


def is_flexguard_interpose_lock(lock: str) -> bool:
    return lock in FLEXGUARD_INTERPOSE_ARTIFACT_LOCKS


def flexguard_interpose_artifact_lock(lock: str) -> str:
    return FLEXGUARD_INTERPOSE_ARTIFACT_LOCKS[lock]


def flexguard_interpose_script(lock: str) -> Path:
    return FLEXGUARD_DIR / "build" / f"interpose_{flexguard_interpose_artifact_lock(lock)}.sh"


def flexguard_interpose_library(lock: str) -> Path:
    return FLEXGUARD_DIR / "build" / f"interpose_{flexguard_interpose_artifact_lock(lock)}.so"


def flexguard_interpose_needs_sudo(lock: str) -> bool:
    return lock == FLEXGUARD_LOCK


def timeslice_extension_for_lock(lock: str) -> str:
    return FLEXGUARD_TIMESLICE_EXTENSIONS.get(lock, "off")


def direct_env() -> dict[str, str | None]:
    env: dict[str, str | None] = {
        "ACCORDIN_CPU_MASK_K": None,
        "ACCORDIN_DISABLE_ADMISSION": None,
        "K": None,
        "MCS_TAS_ACCORDIN_DISABLE_BPF": None,
        MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF_ENV: None,
        MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY_ENV: None,
        MCS_TAS_ACCORDIN_DIRECT_LIB_ENV: str(MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB),
    }
    for key, value in os.environ.items():
        if key.startswith("MCS_TAS_ACCORDIN_DIRECT_"):
            env[key] = value
    env[MCS_TAS_ACCORDIN_DIRECT_LIB_ENV] = str(MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB)
    return env


def mcs_accordin_direct_env() -> dict[str, str | None]:
    env: dict[str, str | None] = {
        "ACCORDIN_CPU_MASK_K": None,
        "ACCORDIN_DISABLE_ADMISSION": None,
        "K": None,
        "MCS_ACCORDIN_DISABLE_BPF": None,
        "MCS_ACCORDIN_STATS_ONLY": None,
        MCS_ACCORDIN_DIRECT_DISABLE_BPF_ENV: None,
        MCS_ACCORDIN_DIRECT_STATS_ONLY_ENV: None,
        MCS_TAS_ACCORDIN_DIRECT_LIB_ENV: None,
        MCS_ACCORDIN_DIRECT_LIB_ENV: str(MCS_ACCORDIN_DIRECT_RELEASE_LIB),
    }
    for key, value in os.environ.items():
        if key.startswith("MCS_ACCORDIN_DIRECT_"):
            env[key] = value
    env[MCS_ACCORDIN_DIRECT_LIB_ENV] = str(MCS_ACCORDIN_DIRECT_RELEASE_LIB)
    return env


def env_command(
    cmd: Sequence[str],
    env: dict[str, str | None],
    *,
    needs_sudo: bool,
    sudo_mode: str,
) -> list[str]:
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


def build_command(lock: str, threads: int, args: RunArgs) -> list[str]:
    return [
        str(MULTILOCKBENCH_BINARY),
        "--threads",
        str(threads),
        "--locks",
        str(args.lock_count),
        "--zipf-alpha",
        f"{args.zipf_alpha:g}",
        "--duration-ms",
        str(args.duration_ms),
        "--warmup-duration-ms",
        str(args.warmup_duration_ms),
        "--critical-ns",
        str(args.critical_ns),
        "--outside-ns",
        str(args.outside_ns),
        "--timing-sample-stride",
        str(args.timing_sample_stride),
        "--seed",
        str(args.seed),
        "--timeslice-extension",
        timeslice_extension_for_lock(lock),
        "--lock-kind",
        LOCK_KIND_BY_LOCK[lock],
    ]


def effective_command(lock: str, base_cmd: list[str], args: RunArgs) -> list[str]:
    if is_flexguard_interpose_lock(lock):
        return env_command(
            [str(flexguard_interpose_script(lock)), *base_cmd],
            {},
            needs_sudo=flexguard_interpose_needs_sudo(lock),
            sudo_mode=args.sudo_mode,
        )
    if experiment_defaults.is_otherlocks_interpose_lock(lock):
        return env_command(
            [str(otherlocks_interpose_script(lock)), *base_cmd],
            {},
            needs_sudo=False,
            sudo_mode=args.sudo_mode,
        )
    if lock == MCS_ACCORDIN_LOCK:
        return env_command(base_cmd, mcs_accordin_direct_env(), needs_sudo=True, sudo_mode=args.sudo_mode)
    if lock == ACCORDIN_LOCK:
        return env_command(base_cmd, direct_env(), needs_sudo=True, sudo_mode=args.sudo_mode)
    return env_command(base_cmd, {}, needs_sudo=False, sudo_mode=args.sudo_mode)


def parse_benchmark_output(output: str) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in output.splitlines():
        if ": " not in line:
            continue
        key, value = line.split(": ", 1)
        values[key.strip()] = value.strip()
    required = {
        "throughput_ops_per_sec",
        "elapsed_seconds",
        "total_operations",
        "avg_lock_hold_ns",
        "avg_wait_ns_estimated",
        "lock_hold_samples",
        "hotspot_lock",
        "hotspot_lock_operations",
        "hotspot_lock_operation_pct",
        "per_thread_operations",
        "per_lock_operations",
    }
    missing = sorted(required - set(values))
    if missing:
        raise ValueError(f"multilockbench output missing fields: {', '.join(missing)}")

    return {
        "throughput_ops_per_sec": values["throughput_ops_per_sec"],
        "elapsed_seconds": values["elapsed_seconds"],
        "total_operations": values["total_operations"],
        "avg_lock_hold_ns": values["avg_lock_hold_ns"],
        "avg_wait_ns_estimated": values["avg_wait_ns_estimated"],
        "lock_hold_samples": values["lock_hold_samples"],
        "hotspot_lock": values["hotspot_lock"],
        "hotspot_lock_operations": values["hotspot_lock_operations"],
        "hotspot_lock_operation_pct": values["hotspot_lock_operation_pct"],
        "per_thread_operations": values["per_thread_operations"].replace(",", ";"),
        "per_lock_operations": values["per_lock_operations"].replace(",", ";"),
    }


def format_float(value: float) -> str:
    return f"{value:.6f}"


def write_settings(root: Path, args: RunArgs) -> None:
    settings = {
        "experiment": "experiment9_multilockbench",
        "description": "Zipfian hotspot multilockbench runner; only thread count is swept by default.",
        "output_root": str(root),
        "machine_profile": experiment_defaults.ACTIVE_MACHINE_CONFIG.name,
        "machine_config_physical_cores": experiment_defaults.MACHINE_CORE_COUNT,
        "lock_profile": args.lock_profile,
        "lock_profile_source": args.lock_profile_source,
        "locks": [{"key": lock, "label": lock_label(lock)} for lock in args.lock_keys],
        "threads": list(args.threads),
        "sweep_dimensions": ["threads"],
        "duration_ms": args.duration_ms,
        "warmup_duration_ms": args.warmup_duration_ms,
        "lock_count": args.lock_count,
        "zipf_alpha": args.zipf_alpha,
        "critical_ns": args.critical_ns,
        "outside_ns": args.outside_ns,
        "seed": args.seed,
        "timing_sample_stride": args.timing_sample_stride,
        "repeats": args.repeats,
        "build_missing": args.build_missing,
        "sudo_mode": args.sudo_mode,
        "command_timeout_seconds": args.command_timeout_seconds,
        "multilockbench_binary": str(MULTILOCKBENCH_BINARY),
        "flexguard_interpose_script": str(FLEXGUARD_INTERPOSE_SCRIPT),
        "mcs_accordin_direct_library": str(MCS_ACCORDIN_DIRECT_RELEASE_LIB),
        "mcs_tas_accordin_direct_library": str(MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB),
    }
    (root / "settings.json").write_text(json.dumps(settings, indent=2) + "\n", encoding="utf-8")


def summary_key(row: dict[str, str]) -> tuple[str, str, str, str, str, str, str, str, str]:
    return (
        row["lock"],
        row["threads"],
        row["duration_ms"],
        row["warmup_duration_ms"],
        row["lock_count"],
        row["zipf_alpha"],
        row["critical_ns"],
        row["outside_ns"],
        row["seed"],
    )


def write_summary(raw_path: Path, summary_path: Path) -> None:
    if not raw_path.is_file():
        return
    with raw_path.open(newline="", encoding="utf-8") as f:
        rows = list(csv.DictReader(f))
    groups: dict[tuple[str, str, str, str, str, str, str, str, str], list[dict[str, str]]] = {}
    for row in rows:
        groups.setdefault(summary_key(row), []).append(row)

    with summary_path.open("w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=SUMMARY_FIELDS, lineterminator="\n")
        writer.writeheader()
        for key in sorted(groups, key=lambda item: (lock_sort_key(item[0]), int(item[1]))):
            group_rows = groups[key]
            first = group_rows[0]
            out: dict[str, str] = {
                "lock": first["lock"],
                "lock_label": first["lock_label"],
                "threads": first["threads"],
                "duration_ms": first["duration_ms"],
                "warmup_duration_ms": first["warmup_duration_ms"],
                "lock_count": first["lock_count"],
                "zipf_alpha": first["zipf_alpha"],
                "critical_ns": first["critical_ns"],
                "outside_ns": first["outside_ns"],
                "seed": first["seed"],
                "hotspot_lock": first["hotspot_lock"],
                "runs": str(len(group_rows)),
            }
            for field in SUMMARY_NUMERIC_FIELDS:
                values = [float(row[field]) for row in group_rows if row.get(field)]
                out[field] = format_float(statistics.mean(values)) if values else ""
            writer.writerow({field: out.get(field, "") for field in SUMMARY_FIELDS})


def load_existing_keys(raw_path: Path) -> set[tuple[str, int, int]]:
    if not raw_path.is_file():
        return set()
    with raw_path.open(newline="", encoding="utf-8") as f:
        rows = csv.DictReader(f)
        return {(row["lock"], int(row["threads"]), int(row["repeat"])) for row in rows}


def prepare_output_root(root: Path, *, force: bool, resume: bool) -> None:
    root.mkdir(parents=True, exist_ok=True)
    if resume:
        return
    managed_files = (root / "raw.csv", root / "summary.csv", root / "settings.json", root / "commands.json")
    existing = [path for path in managed_files if path.exists()]
    if existing and not force:
        raise RuntimeError(f"output root already contains result files; use --force or --resume: {root}")
    if force:
        for path in existing:
            path.unlink()


def build_multilockbench(logger: CommandLogger | None) -> None:
    cmd = ["make", "-C", str(MUTEXBENCH_DIR), "multilockbench"]
    if logger is None:
        print(shlex_join(cmd))
        return
    logger.run(cmd, log_name="build_multilockbench.log", cwd=REPO_ROOT, timeout_seconds=0)


def build_flexguard_interpose(lock: str, logger: CommandLogger | None) -> None:
    cmd = ["make", "-C", str(FLEXGUARD_DIR), FLEXGUARD_INTERPOSE_BUILD_TARGETS[lock]]
    if logger is None:
        print(shlex_join(cmd))
        return
    logger.run(cmd, log_name=f"build_flexguard_{safe_name(lock)}.log", cwd=REPO_ROOT, timeout_seconds=0)


def build_otherlocks_interpose(lock: str, logger: CommandLogger | None) -> None:
    cmd = ["make", "-C", str(OTHERLOCKS_DIR), f"build/interpose_{lock}.sh"]
    if logger is None:
        print(shlex_join(cmd))
        return
    logger.run(cmd, log_name=f"build_otherlocks_{safe_name(lock)}.log", cwd=REPO_ROOT, timeout_seconds=0)


def ensure_inputs(locks: tuple[str, ...], *, build_missing: bool, logger: CommandLogger | None) -> None:
    if logger is None:
        if not MULTILOCKBENCH_BINARY.is_file() or not os.access(MULTILOCKBENCH_BINARY, os.X_OK):
            build_multilockbench(logger)
        for lock in sorted(lock for lock in locks if is_flexguard_interpose_lock(lock)):
            script = flexguard_interpose_script(lock)
            library = flexguard_interpose_library(lock)
            if build_missing and (
                not script.is_file()
                or not os.access(script, os.X_OK)
                or not library.is_file()
            ):
                build_flexguard_interpose(lock, logger)
        for lock in sorted(lock for lock in locks if experiment_defaults.is_otherlocks_interpose_lock(lock)):
            if build_missing and (
                not otherlocks_interpose_script(lock).is_file()
                or not os.access(otherlocks_interpose_script(lock), os.X_OK)
                or not otherlocks_interpose_library(lock).is_file()
            ):
                build_otherlocks_interpose(lock, logger)
        if build_missing and ACCORDIN_LOCK in locks and not MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB.is_file():
            print(shlex_join(["cargo", "build", "-p", MCS_TAS_ACCORDIN_DIRECT_PACKAGE, "--release"]))
        if build_missing and MCS_ACCORDIN_LOCK in locks and not MCS_ACCORDIN_DIRECT_RELEASE_LIB.is_file():
            print(shlex_join(["cargo", "build", "-p", MCS_ACCORDIN_DIRECT_PACKAGE, "--release"]))
        return

    if not MULTILOCKBENCH_BINARY.is_file() or not os.access(MULTILOCKBENCH_BINARY, os.X_OK):
        build_multilockbench(logger)
    if not MULTILOCKBENCH_BINARY.is_file() or not os.access(MULTILOCKBENCH_BINARY, os.X_OK):
        raise RuntimeError(f"multilockbench is missing or not executable: {MULTILOCKBENCH_BINARY}")

    missing: list[str] = []
    for lock in sorted(lock for lock in locks if is_flexguard_interpose_lock(lock)):
        script = flexguard_interpose_script(lock)
        library = flexguard_interpose_library(lock)
        if not script.is_file() or not os.access(script, os.X_OK) or not library.is_file():
            if build_missing:
                build_flexguard_interpose(lock, logger)
            if not script.is_file() or not os.access(script, os.X_OK) or not library.is_file():
                missing.append(f"{lock} interpose artifacts are missing: {script}, {library}")
    for lock in sorted(lock for lock in locks if experiment_defaults.is_otherlocks_interpose_lock(lock)):
        script = otherlocks_interpose_script(lock)
        library = otherlocks_interpose_library(lock)
        if not script.is_file() or not os.access(script, os.X_OK) or not library.is_file():
            if build_missing:
                build_otherlocks_interpose(lock, logger)
            if not script.is_file() or not os.access(script, os.X_OK) or not library.is_file():
                missing.append(f"{lock} interpose artifacts are missing: {script}, {library}")
    if ACCORDIN_LOCK in locks and not MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB.is_file():
        if build_missing:
            build_cmd = ["cargo", "build", "-p", MCS_TAS_ACCORDIN_DIRECT_PACKAGE, "--release"]
            if logger is None:
                print(shlex_join(build_cmd))
            else:
                logger.run(
                    build_cmd,
                    log_name=f"build_{MCS_TAS_ACCORDIN_DIRECT_PACKAGE}.log",
                    cwd=REPO_ROOT,
                    timeout_seconds=0,
                )
        if not MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB.is_file():
            missing.append(f"mcs_tas_accordin_direct library is missing: {MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB}")
    if MCS_ACCORDIN_LOCK in locks and not MCS_ACCORDIN_DIRECT_RELEASE_LIB.is_file():
        if build_missing:
            build_cmd = ["cargo", "build", "-p", MCS_ACCORDIN_DIRECT_PACKAGE, "--release"]
            if logger is None:
                print(shlex_join(build_cmd))
            else:
                logger.run(
                    build_cmd,
                    log_name=f"build_{MCS_ACCORDIN_DIRECT_PACKAGE}.log",
                    cwd=REPO_ROOT,
                    timeout_seconds=0,
                )
        if not MCS_ACCORDIN_DIRECT_RELEASE_LIB.is_file():
            missing.append(f"mcs_accordin_direct library is missing: {MCS_ACCORDIN_DIRECT_RELEASE_LIB}")
    if missing:
        raise RuntimeError("Required inputs are missing: " + "; ".join(missing))


def row_for_run(
    lock: str,
    threads: int,
    repeat: int,
    args: RunArgs,
    metrics: dict[str, str],
    wall_seconds: float,
    log_path: Path,
) -> dict[str, str]:
    row = {
        "lock": lock,
        "lock_label": lock_label(lock),
        "threads": str(threads),
        "duration_ms": str(args.duration_ms),
        "warmup_duration_ms": str(args.warmup_duration_ms),
        "lock_count": str(args.lock_count),
        "zipf_alpha": format_float(args.zipf_alpha),
        "critical_ns": str(args.critical_ns),
        "outside_ns": str(args.outside_ns),
        "seed": str(args.seed),
        "repeat": str(repeat),
        "wall_seconds": format_float(wall_seconds),
        "command_log": str(log_path.relative_to(args.output_root)),
    }
    for field in RAW_FIELDS:
        row.setdefault(field, metrics.get(field, ""))
    return row


def run_experiment(args: RunArgs) -> Path:
    root = args.output_root
    prepare_output_root(root, force=args.force, resume=args.resume)
    logger = CommandLogger(root)
    ensure_inputs(args.lock_keys, build_missing=args.build_missing, logger=logger)
    write_settings(root, args)

    raw_path = root / "raw.csv"
    summary_path = root / "summary.csv"
    existing = load_existing_keys(raw_path) if args.resume else set()
    write_header = not raw_path.is_file() or not args.resume
    mode = "a" if args.resume and raw_path.is_file() else "w"
    with raw_path.open(mode, newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=RAW_FIELDS, lineterminator="\n")
        if write_header:
            writer.writeheader()
        for lock in args.lock_keys:
            for threads in args.threads:
                for repeat in range(1, args.repeats + 1):
                    key = (lock, threads, repeat)
                    if key in existing:
                        print(f"Skipping complete run: lock={lock} threads={threads} repeat={repeat}")
                        continue
                    base_cmd = build_command(lock, threads, args)
                    run_cmd = effective_command(lock, base_cmd, args)
                    log_name = f"multilockbench_{safe_name(lock)}_{threads:03d}_r{repeat}.log"
                    try:
                        result = logger.run(run_cmd, log_name=log_name, cwd=REPO_ROOT, timeout_seconds=args.command_timeout_seconds)
                        metrics = parse_benchmark_output(result.output)
                    except CommandError as exc:
                        print(f"Failed lock={lock} threads={threads} repeat={repeat}: {exc.log_path}")
                        continue
                    except ValueError as exc:
                        log_path = logger.log_dir / log_name
                        print(f"Failed to parse lock={lock} threads={threads} repeat={repeat}: {exc}; log={log_path}")
                        continue
                    row = row_for_run(lock, threads, repeat, args, metrics, result.wall_seconds, result.log_path)
                    writer.writerow({field: row.get(field, "") for field in RAW_FIELDS})
                    f.flush()
    write_summary(raw_path, summary_path)
    return root


def safe_name(value: str) -> str:
    return "_".join(part for part in re.sub(r"[^A-Za-z0-9]+", "_", value).strip("_").lower().split("_") if part) or "unknown"


def load_summary_rows(summary_path: Path) -> list[dict[str, str]]:
    if not summary_path.is_file():
        raise RuntimeError(f"{summary_path} does not exist")
    with summary_path.open(newline="", encoding="utf-8") as f:
        return list(csv.DictReader(f))


def generate_plots(result_root: Path) -> tuple[Path, ...]:
    try:
        import matplotlib
    except ImportError as exc:
        raise RuntimeError("matplotlib is required to generate experiment9 plots") from exc

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    rows = load_summary_rows(result_root / "summary.csv")
    if not rows:
        return ()
    plot_dir = result_root / "plots"
    plot_dir.mkdir(parents=True, exist_ok=True)
    locks = tuple(dict.fromkeys(row["lock"] for row in rows))
    fig, ax = plt.subplots(figsize=(8.8, 5.2))
    for lock in locks:
        lock_rows = sorted([row for row in rows if row["lock"] == lock], key=lambda row: int(row["threads"]))
        xs = [int(row["threads"]) for row in lock_rows]
        ys = [float(row["throughput_ops_per_sec"]) / 1_000_000.0 for row in lock_rows if row["throughput_ops_per_sec"]]
        if len(xs) != len(ys):
            continue
        ax.plot(xs, ys, marker="o", label=lock_rows[0]["lock_label"] or lock)
    ax.set_xlabel("Threads")
    ax.set_ylabel("Throughput (M ops/s)")
    ax.set_title("Experiment 9 Multilockbench Throughput")
    ax.grid(True, alpha=0.35)
    ax.legend()
    output = plot_dir / "throughput_vs_threads.png"
    fig.tight_layout()
    fig.savefig(output, dpi=180)
    plt.close(fig)
    return (output,)


def dry_run(args: RunArgs) -> None:
    print(f"Output root: {args.output_root}")
    ensure_inputs(args.lock_keys, build_missing=args.build_missing, logger=None)
    for lock in args.lock_keys:
        for threads in args.threads:
            for repeat in range(1, args.repeats + 1):
                base_cmd = build_command(lock, threads, args)
                print(f"lock={lock} threads={threads} repeat={repeat}: {shlex_join(effective_command(lock, base_cmd, args))}")


def parse_args(argv: Sequence[str] | None = None) -> RunArgs:
    parser = argparse.ArgumentParser(
        description="Run Experiment 9: Zipfian multilockbench thread-count sweep.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Examples:
  python3 experiments/run_experiment_nine.py --threads 16,32,64 --repeats 1
  python3 experiments/run_experiment_nine.py --plot-only experiments/results/experiment9_multilockbench_manual
""",
    )
    parser.add_argument("--output-root", type=Path, default=None)
    parser.add_argument("--plot-only", type=Path, default=None, metavar="RESULT_ROOT", help="Regenerate summary.csv and PNGs from raw.csv.")
    parser.add_argument("--skip-plots", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--force", action="store_true")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--build-missing", action="store_true")
    parser.add_argument(
        "--lock-profile",
        choices=experiment_defaults.lock_profile_names(),
        default=DEFAULT_LOCK_PROFILE,
        help="Named lock set used when --locks is omitted. Default comes from experiment_defaults.py.",
    )
    parser.add_argument("--locks", help="Comma-separated lock list. Default comes from --lock-profile.")
    parser.add_argument("--threads", default=",".join(str(thread) for thread in DEFAULT_THREADS))
    parser.add_argument("--duration-ms", type=positive_int, default=DEFAULT_DURATION_MS)
    parser.add_argument("--warmup-duration-ms", type=non_negative_int, default=DEFAULT_WARMUP_DURATION_MS)
    parser.add_argument("--lock-count", type=positive_int, default=DEFAULT_LOCK_COUNT)
    parser.add_argument("--zipf-alpha", type=non_negative_float, default=DEFAULT_ZIPF_ALPHA)
    parser.add_argument("--critical-ns", type=non_negative_int, default=DEFAULT_CRITICAL_NS)
    parser.add_argument("--outside-ns", type=non_negative_int, default=DEFAULT_OUTSIDE_NS)
    parser.add_argument("--seed", type=non_negative_int, default=DEFAULT_SEED)
    parser.add_argument("--timing-sample-stride", type=positive_int, default=DEFAULT_TIMING_SAMPLE_STRIDE)
    parser.add_argument("--repeats", type=positive_int, default=DEFAULT_REPEATS)
    parser.add_argument("--sudo-mode", choices=("all", "auto", "none"), default="auto")
    parser.add_argument("--command-timeout-seconds", type=positive_int, default=DEFAULT_COMMAND_TIMEOUT_SECONDS)
    parsed = parser.parse_args(argv)

    try:
        lock_keys = parse_locks(parsed.locks, parsed.lock_profile)
        threads = parse_csv_positive_ints(parsed.threads, "--threads")
    except argparse.ArgumentTypeError as exc:
        parser.error(str(exc))
    return RunArgs(
        output_root=parsed.output_root or default_output_root(),
        lock_keys=lock_keys,
        threads=threads,
        duration_ms=parsed.duration_ms,
        warmup_duration_ms=parsed.warmup_duration_ms,
        lock_count=parsed.lock_count,
        zipf_alpha=parsed.zipf_alpha,
        critical_ns=parsed.critical_ns,
        outside_ns=parsed.outside_ns,
        seed=parsed.seed,
        timing_sample_stride=parsed.timing_sample_stride,
        repeats=parsed.repeats,
        command_timeout_seconds=parsed.command_timeout_seconds,
        sudo_mode=parsed.sudo_mode,
        build_missing=parsed.build_missing,
        skip_plots=parsed.skip_plots,
        dry_run=parsed.dry_run,
        force=parsed.force,
        resume=parsed.resume,
        lock_profile=parsed.lock_profile,
        lock_profile_source="manual" if parsed.locks is not None else "profile",
        plot_only=parsed.plot_only,
    )


def main(argv: Sequence[str] | None = None) -> int:
    cli_args = parse_args(argv)
    if cli_args.plot_only is not None:
        write_summary(cli_args.plot_only / "raw.csv", cli_args.plot_only / "summary.csv")
        if not cli_args.skip_plots:
            for path in generate_plots(cli_args.plot_only):
                print(f"Plot: {path}")
        print(f"Summary results: {cli_args.plot_only / 'summary.csv'}")
        return 0

    if cli_args.dry_run:
        dry_run(cli_args)
        return 0

    root = run_experiment(cli_args)
    print(f"Raw results: {root / 'raw.csv'}")
    print(f"Summary results: {root / 'summary.csv'}")
    if not cli_args.skip_plots:
        for path in generate_plots(root):
            print(f"Plot: {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
