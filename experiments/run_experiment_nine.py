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
from typing import Iterable, Sequence

import experiment_defaults


REPO_ROOT = Path(__file__).resolve().parents[1]
FLEXGUARD_DIR = REPO_ROOT / "bench" / "flexguard"
FLEXGUARD_BUILD_DIR = FLEXGUARD_DIR / "build"
MAKE_ALL_SCRIPT = FLEXGUARD_DIR / "scripts" / "make_all.sh"
DIRECT_LOCK = "mcs_tas_accordin_direct"
DIRECT_BUCKETS_BINARY = FLEXGUARD_BUILD_DIR / "direct_buckets_bench"
MCS_TAS_ACCORDIN_DIRECT_PACKAGE = "mcs_tas_accordin_direct"
MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB = REPO_ROOT / "target" / "release" / "libmcs_tas_accordin_direct.so"
MCS_TAS_ACCORDIN_DIRECT_LIB_ENV = "MCS_TAS_ACCORDIN_DIRECT_LIB"
MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF_ENV = "MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF"
MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY_ENV = "MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY"
DIRECT_ALIASES = {
    DIRECT_LOCK,
    "mcs-tas-accordin-direct",
    "accordin_direct",
    "accordin-direct",
}

DEFAULT_DURATION_MS = 10_000
DEFAULT_BUCKETS = 100
DEFAULT_MAX_VALUE = 100_000
DEFAULT_OFFSET_CHANGES = 40
DEFAULT_NON_CRITICAL_CYCLES = 0
DEFAULT_REPEATS = experiment_defaults.DEFAULT_REPEATS
DEFAULT_THREADS = experiment_defaults.DEFAULT_THREADS
DEFAULT_COMMAND_TIMEOUT_SECONDS = 21_600

LOCK_PROFILES = {
    "minimal": ("flexguard", "mcs", DIRECT_LOCK),
    "full": ("mutex", "mcs", "mcstas", "reciprocating", "flexguard", DIRECT_LOCK),
}
DEFAULT_LOCK_PROFILE = "minimal"

RAW_FIELDS = (
    "lock",
    "lock_label",
    "threads",
    "duration_ms",
    "buckets",
    "max_value",
    "offset_changes",
    "non_critical_cycles",
    "pin_threads",
    "repeat",
    "throughput_cs_per_sec",
    "critical_path_throughput_cs_per_sec",
    "mean_thread_throughput_cs_per_sec",
    "min_thread_throughput_cs_per_sec",
    "max_thread_throughput_cs_per_sec",
    "thread_iterations_total",
    "pauses",
    "elapsed_seconds",
    "wall_seconds",
    "command_log",
)
SUMMARY_FIELDS = tuple(field for field in RAW_FIELDS if field not in {"repeat", "command_log"}) + ("runs",)
SUMMARY_NUMERIC_FIELDS = tuple(
    field
    for field in SUMMARY_FIELDS
    if field
    not in {
        "lock",
        "lock_label",
        "pin_threads",
        "runs",
    }
)

THROUGHPUT_RE = re.compile(r"^#Throughput:\s*(?P<value>[\d.]+)\s*CS/s")
LOCAL_RE = re.compile(
    r"^#Local result for Thread\s+(?P<thread>\d+):\s*"
    r"(?P<throughput>[\d.]+)\s*CS/s\s*\((?P<iterations>\d+)\s+iterations\)"
)
PAUSES_RE = re.compile(r"^Pauses:\s*(?P<value>\d+)")
DIRECT_RESULT_RE = re.compile(r"^RESULT\s+(?P<items>.+)$")


@dataclass(frozen=True)
class RunArgs:
    output_root: Path
    lock_keys: tuple[str, ...]
    threads: tuple[int, ...]
    duration_ms: int
    buckets: int
    max_value: int
    offset_changes: int
    non_critical_cycles: int
    pin_threads: bool
    repeats: int
    command_timeout_seconds: int
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
    return REPO_ROOT / "experiments" / "results" / f"experiment9_buckets_{timestamp}"


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


def lock_profile_names() -> tuple[str, ...]:
    return tuple(LOCK_PROFILES)


def lock_profile_locks(profile: str) -> tuple[str, ...]:
    try:
        return LOCK_PROFILES[profile]
    except KeyError as exc:
        supported = ", ".join(lock_profile_names())
        raise ValueError(f"Unsupported lock profile: {profile}. Supported profiles: {supported}") from exc


def normalize_lock(raw: str) -> str:
    normalized = raw.strip().lower()
    if normalized in DIRECT_ALIASES:
        return DIRECT_LOCK
    return experiment_defaults.normalize_lock(normalized)


def parse_locks(text: str | None, profile: str) -> tuple[str, ...]:
    raw_locks = lock_profile_locks(profile) if text is None else parse_csv_strings(text)
    locks: list[str] = []
    supported = set(lock for values in LOCK_PROFILES.values() for lock in values) | set(experiment_defaults.LOCK_LABELS)
    supported |= {DIRECT_LOCK, *DIRECT_ALIASES}
    for raw in raw_locks:
        lock = normalize_lock(raw)
        if lock not in supported:
            raise argparse.ArgumentTypeError(
                f"Unsupported experiment9 lock {raw!r}. Supported locks: {', '.join(sorted(supported))}"
            )
        if lock not in locks:
            locks.append(lock)
    if not locks:
        raise argparse.ArgumentTypeError("At least one lock must be selected")
    return tuple(locks)


def lock_label(lock: str) -> str:
    if lock == DIRECT_LOCK:
        return "MCS-TAS Accordin direct"
    return experiment_defaults.lock_label(lock)


def ordinary_bucket_binary(lock: str) -> Path:
    return FLEXGUARD_BUILD_DIR / f"buckets_{lock}"


def is_direct_lock(lock: str) -> bool:
    return lock == DIRECT_LOCK


def runnable_threads_for_lock(lock: str, threads: tuple[int, ...]) -> tuple[int, ...]:
    if is_direct_lock(lock):
        return threads
    return experiment_defaults.runnable_threads_for_lock(lock, threads)


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
    if is_direct_lock(lock):
        return [
            str(DIRECT_BUCKETS_BINARY),
            "--lib",
            str(MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB),
            "--label",
            lock,
            "--threads",
            str(threads),
            "--duration-ms",
            str(args.duration_ms),
            "--buckets",
            str(args.buckets),
            "--max-value",
            str(args.max_value),
            "--offset-changes",
            str(args.offset_changes),
        ]
    cmd = [
        str(ordinary_bucket_binary(lock)),
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
    ]
    if args.pin_threads:
        cmd.extend(["--pin-threads", "1"])
    return cmd


def effective_command(lock: str, base_cmd: list[str], args: RunArgs) -> list[str]:
    if is_direct_lock(lock):
        return env_command(base_cmd, direct_env(), needs_sudo=True, sudo_mode=args.sudo_mode)
    needs_sudo = lock.startswith("flexguard")
    return env_command(base_cmd, {}, needs_sudo=needs_sudo, sudo_mode=args.sudo_mode)


def parse_direct_result(line: str) -> dict[str, str]:
    match = DIRECT_RESULT_RE.match(line)
    if match is None:
        raise ValueError("missing direct RESULT line")
    pairs: dict[str, str] = {}
    for item in match.group("items").split():
        if "=" not in item:
            continue
        key, value = item.split("=", 1)
        pairs[key] = value
    required = {
        "total_ops",
        "elapsed_seconds",
        "wall_throughput_ops_per_sec",
        "critical_path_cs_per_sec",
    }
    missing = sorted(required - set(pairs))
    if missing:
        raise ValueError(f"direct RESULT line missing fields: {', '.join(missing)}")
    return {
        "throughput_cs_per_sec": format_float(float(pairs["wall_throughput_ops_per_sec"])),
        "critical_path_throughput_cs_per_sec": format_float(float(pairs["critical_path_cs_per_sec"])),
        "mean_thread_throughput_cs_per_sec": "",
        "min_thread_throughput_cs_per_sec": "",
        "max_thread_throughput_cs_per_sec": "",
        "thread_iterations_total": str(int(pairs["total_ops"])),
        "pauses": "",
        "elapsed_seconds": format_float(float(pairs["elapsed_seconds"])),
    }


def parse_benchmark_output(lock: str, output: str) -> dict[str, str]:
    if is_direct_lock(lock):
        for line in output.splitlines():
            if DIRECT_RESULT_RE.match(line):
                return parse_direct_result(line)
        raise ValueError("direct benchmark output did not contain RESULT line")

    throughput: float | None = None
    local_values: list[float] = []
    total_iterations = 0
    pauses = 0
    for line in output.splitlines():
        if match := THROUGHPUT_RE.match(line):
            throughput = float(match.group("value"))
            continue
        if match := LOCAL_RE.match(line):
            local_values.append(float(match.group("throughput")))
            total_iterations += int(match.group("iterations"))
            continue
        if match := PAUSES_RE.match(line):
            pauses += int(match.group("value"))
    if throughput is None:
        raise ValueError("ordinary buckets output did not contain #Throughput")

    return {
        "throughput_cs_per_sec": format_float(throughput),
        "critical_path_throughput_cs_per_sec": "",
        "mean_thread_throughput_cs_per_sec": format_float(statistics.mean(local_values)) if local_values else "",
        "min_thread_throughput_cs_per_sec": format_float(min(local_values)) if local_values else "",
        "max_thread_throughput_cs_per_sec": format_float(max(local_values)) if local_values else "",
        "thread_iterations_total": str(total_iterations) if local_values else "",
        "pauses": str(pauses) if pauses else "",
        "elapsed_seconds": "",
    }


def format_float(value: float) -> str:
    return f"{value:.6f}"


def write_settings(root: Path, args: RunArgs) -> None:
    settings = {
        "experiment": "experiment9_buckets",
        "description": "FlexGuard buckets hash-table benchmark runner.",
        "output_root": str(root),
        "machine_profile": experiment_defaults.ACTIVE_MACHINE_CONFIG.name,
        "machine_config_physical_cores": experiment_defaults.MACHINE_CORE_COUNT,
        "lock_profile": args.lock_profile,
        "lock_profile_source": args.lock_profile_source,
        "locks": [{"key": lock, "label": lock_label(lock)} for lock in args.lock_keys],
        "threads": list(args.threads),
        "runnable_threads_by_lock": {
            lock: list(runnable_threads_for_lock(lock, args.threads))
            for lock in args.lock_keys
        },
        "duration_ms": args.duration_ms,
        "buckets": args.buckets,
        "max_value": args.max_value,
        "offset_changes": args.offset_changes,
        "non_critical_cycles": args.non_critical_cycles,
        "pin_threads": args.pin_threads,
        "repeats": args.repeats,
        "build_missing": args.build_missing,
        "sudo_mode": args.sudo_mode,
        "command_timeout_seconds": args.command_timeout_seconds,
        "flexguard_dir": str(FLEXGUARD_DIR),
        "direct_buckets_binary": str(DIRECT_BUCKETS_BINARY),
        "mcs_tas_accordin_direct_library": str(MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB),
    }
    (root / "settings.json").write_text(json.dumps(settings, indent=2) + "\n", encoding="utf-8")


def summary_key(row: dict[str, str]) -> tuple[str, str, str, str, str, str, str, str]:
    return (
        row["lock"],
        row["threads"],
        row["duration_ms"],
        row["buckets"],
        row["max_value"],
        row["offset_changes"],
        row["non_critical_cycles"],
        row["pin_threads"],
    )


def write_summary(raw_path: Path, summary_path: Path) -> None:
    if not raw_path.is_file():
        return
    with raw_path.open(newline="", encoding="utf-8") as f:
        rows = list(csv.DictReader(f))
    groups: dict[tuple[str, str, str, str, str, str, str, str], list[dict[str, str]]] = {}
    for row in rows:
        groups.setdefault(summary_key(row), []).append(row)

    with summary_path.open("w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=SUMMARY_FIELDS, lineterminator="\n")
        writer.writeheader()
        for key in sorted(groups, key=lambda item: (experiment_defaults.lock_sort_key(item[0]), int(item[1]))):
            group_rows = groups[key]
            first = group_rows[0]
            out: dict[str, str] = {
                "lock": first["lock"],
                "lock_label": first["lock_label"],
                "threads": first["threads"],
                "duration_ms": first["duration_ms"],
                "buckets": first["buckets"],
                "max_value": first["max_value"],
                "offset_changes": first["offset_changes"],
                "non_critical_cycles": first["non_critical_cycles"],
                "pin_threads": first["pin_threads"],
                "runs": str(len(group_rows)),
            }
            for field in SUMMARY_NUMERIC_FIELDS:
                if field in out:
                    continue
                values = [float(row[field]) for row in group_rows if row.get(field)]
                out[field] = format_float(statistics.mean(values)) if values else ""
            writer.writerow(out)


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


def missing_inputs(locks: Iterable[str]) -> list[str]:
    missing: list[str] = []
    for lock in locks:
        if is_direct_lock(lock):
            if not DIRECT_BUCKETS_BINARY.is_file() or not os.access(DIRECT_BUCKETS_BINARY, os.X_OK):
                missing.append(f"direct buckets helper is missing or not executable: {DIRECT_BUCKETS_BINARY}")
            if not MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB.is_file():
                missing.append(f"mcs_tas_accordin_direct library is missing: {MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB}")
            continue
        binary = ordinary_bucket_binary(lock)
        if not binary.is_file() or not os.access(binary, os.X_OK):
            missing.append(f"{lock} buckets executable is missing or not executable: {binary}")
    return missing


def ensure_inputs(locks: tuple[str, ...], *, build_missing: bool, logger: CommandLogger | None) -> None:
    missing = missing_inputs(locks)
    if not missing:
        return
    if not build_missing:
        raise RuntimeError("Required inputs are missing: " + "; ".join(missing))

    ordinary_missing = [lock for lock in locks if not is_direct_lock(lock) and not ordinary_bucket_binary(lock).is_file()]
    direct_lib_missing = any(is_direct_lock(lock) for lock in locks) and not MCS_TAS_ACCORDIN_DIRECT_RELEASE_LIB.is_file()

    if ordinary_missing:
        if logger is None:
            print(shlex_join(["bash", str(MAKE_ALL_SCRIPT)]))
        else:
            logger.run(
                ["bash", str(MAKE_ALL_SCRIPT)],
                log_name="build_flexguard_buckets.log",
                cwd=FLEXGUARD_DIR,
                timeout_seconds=0,
            )
    if direct_lib_missing:
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

    missing_after_build = missing_inputs(locks)
    if missing_after_build:
        raise RuntimeError("Required inputs are still missing: " + "; ".join(missing_after_build))


def row_for_run(lock: str, threads: int, repeat: int, args: RunArgs, metrics: dict[str, str], wall_seconds: float, log_path: Path) -> dict[str, str]:
    row = {
        "lock": lock,
        "lock_label": lock_label(lock),
        "threads": str(threads),
        "duration_ms": str(args.duration_ms),
        "buckets": str(args.buckets),
        "max_value": str(args.max_value),
        "offset_changes": str(args.offset_changes),
        "non_critical_cycles": str(args.non_critical_cycles),
        "pin_threads": "1" if args.pin_threads else "0",
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
            for threads in runnable_threads_for_lock(lock, args.threads):
                for repeat in range(1, args.repeats + 1):
                    key = (lock, threads, repeat)
                    if key in existing:
                        print(f"Skipping complete run: lock={lock} threads={threads} repeat={repeat}")
                        continue
                    base_cmd = build_command(lock, threads, args)
                    run_cmd = effective_command(lock, base_cmd, args)
                    log_name = f"buckets_{safe_name(lock)}_{threads:03d}_r{repeat}.log"
                    try:
                        result = logger.run(run_cmd, log_name=log_name, cwd=REPO_ROOT, timeout_seconds=args.command_timeout_seconds)
                        metrics = parse_benchmark_output(lock, result.output)
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
        ys = [float(row["throughput_cs_per_sec"]) / 1_000_000.0 for row in lock_rows if row["throughput_cs_per_sec"]]
        if len(xs) != len(ys):
            continue
        ax.plot(xs, ys, marker="o", label=lock_rows[0]["lock_label"] or lock)
    ax.set_xlabel("Threads")
    ax.set_ylabel("Throughput (M CS/s)")
    ax.set_title("Experiment 9 Buckets Throughput")
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
        for threads in runnable_threads_for_lock(lock, args.threads):
            for repeat in range(1, args.repeats + 1):
                base_cmd = build_command(lock, threads, args)
                print(f"lock={lock} threads={threads} repeat={repeat}: {shlex_join(effective_command(lock, base_cmd, args))}")


def parse_args(argv: Sequence[str] | None = None) -> RunArgs:
    parser = argparse.ArgumentParser(
        description="Run Experiment 9: FlexGuard buckets hash-table benchmark.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Examples:
  python3 experiments/run_experiment_nine.py --locks flexguard,mcs,mcs_tas_accordin_direct --threads 64 --repeats 1
  python3 experiments/run_experiment_nine.py --plot-only experiments/results/experiment9_buckets_manual
""",
    )
    parser.add_argument("--output-root", type=Path, default=None)
    parser.add_argument("--plot-only", type=Path, default=None, metavar="RESULT_ROOT", help="Regenerate summary.csv and PNGs from raw.csv.")
    parser.add_argument("--skip-plots", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--force", action="store_true")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--build-missing", action="store_true")
    parser.add_argument("--lock-profile", choices=lock_profile_names(), default=DEFAULT_LOCK_PROFILE)
    parser.add_argument("--locks", help="Comma-separated lock list. Default comes from --lock-profile.")
    parser.add_argument("--threads", default=",".join(str(thread) for thread in DEFAULT_THREADS))
    parser.add_argument("--duration-ms", type=positive_int, default=DEFAULT_DURATION_MS)
    parser.add_argument("--buckets", type=positive_int, default=DEFAULT_BUCKETS)
    parser.add_argument("--max-value", type=positive_int, default=DEFAULT_MAX_VALUE)
    parser.add_argument("--offset-changes", type=positive_int, default=DEFAULT_OFFSET_CHANGES)
    parser.add_argument("--non-critical-cycles", type=non_negative_int, default=DEFAULT_NON_CRITICAL_CYCLES)
    parser.add_argument("--pin-threads", action="store_true")
    parser.add_argument("--repeats", type=positive_int, default=DEFAULT_REPEATS)
    parser.add_argument("--sudo-mode", choices=("all", "auto", "none"), default="auto")
    parser.add_argument("--command-timeout-seconds", type=positive_int, default=DEFAULT_COMMAND_TIMEOUT_SECONDS)
    parsed = parser.parse_args(argv)

    if parsed.max_value <= parsed.buckets:
        parser.error("--max-value must be greater than --buckets")
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
        buckets=parsed.buckets,
        max_value=parsed.max_value,
        offset_changes=parsed.offset_changes,
        non_critical_cycles=parsed.non_critical_cycles,
        pin_threads=parsed.pin_threads,
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
