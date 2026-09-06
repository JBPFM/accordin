#!/usr/bin/env python3
"""Run stock LevelDB db_bench under the fullhook lock arms.

Each arm differs only in the environment wrapped around the same db_bench
binary. The readrandom benchmark reuses one filled database for every measured
run; the fillrandom benchmark writes a fresh database on each run instead.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import re
import shutil
import signal
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

SCRIPT_PATH = Path(__file__).resolve()
REPO_ROOT = SCRIPT_PATH.parent.parent

SCHED_EXT_STATE = Path("/sys/kernel/sched_ext/state")
SCHED_EXT_OPS = Path("/sys/kernel/sched_ext/root/ops")
PROC_STAT = Path("/proc/stat")
PROC_LOADAVG = Path("/proc/loadavg")

SETTLE_TIMEOUT_SECONDS = 60.0
SETTLE_POLL_SECONDS = 0.2
STDERR_TAIL_LINES = 20
STDOUT_TAIL_LINES = 40
DMESG_TAIL_LINES = 50

READ_BENCHMARK = "readrandom"
WRITE_BENCHMARK = "fillrandom"
BENCHMARKS = (READ_BENCHMARK, WRITE_BENCHMARK)
FILL_BENCHMARK = "fillseq"
FILL_THREADS = 1

DEFAULT_FLEXGUARD_LIB = "bench/flexguard/build/interpose_flexguard.so"

# db_bench prints e.g.
# readrandom   :       3.021 micros/op; 36.7 MB/s (1572864 of 1572864 found)
RESULT_PATTERN = re.compile(
    r"^(?P<name>\w+)\s+:\s+(?P<micros>\d+(?:\.\d+)?)\s+micros/op;"
    r"(?:\s+(?P<mbps>\d+(?:\.\d+)?)\s+MB/s)?"
    r"(?:\s+\((?P<found>\d+)\s+of\s+(?P<total>\d+)\s+found\))?",
    re.MULTILINE,
)


@dataclass(frozen=True)
class Arm:
    """One measured configuration: a preload library plus its environment."""

    name: str
    library: str | None = None
    env: dict[str, str] = field(default_factory=dict)
    sudo: bool = False
    uses_bpf: bool = False


ARMS: tuple[Arm, ...] = (
    Arm(name="pthread"),
    Arm(
        name="fullhook_mcs_tas",
        library="libmcs_tas_accordin_fullhook.so",
        sudo=True,
        uses_bpf=True,
    ),
    Arm(
        name="fullhook_mcs",
        library="libmcs_accordin_fullhook.so",
        sudo=True,
        uses_bpf=True,
    ),
    Arm(
        name="fullhook_mcs_tas_noadm",
        library="libmcs_tas_accordin_fullhook.so",
        env={"ACCORDIN_DISABLE_ADMISSION": "1"},
        sudo=True,
        uses_bpf=True,
    ),
    Arm(
        name="bpf_off",
        library="libmcs_tas_accordin_fullhook.so",
        env={"MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF": "1"},
    ),
    # The flexguard interposer loads its own BPF program on sched_switch, which
    # requires root, but it attaches no sched_ext scheduler, so the sched_ext
    # settle and dmesg probes stay off for this arm.
    Arm(name="flexguard", sudo=True),
)


@dataclass(frozen=True)
class Run:
    """One entry of the run plan."""

    arm: Arm
    threads: int
    repeat: int


ARMS_BY_NAME = {arm.name: arm for arm in ARMS}
NON_DEFAULT_ARMS = frozenset({"bpf_off", "fullhook_mcs_tas_noadm"})
DEFAULT_ARMS = tuple(arm.name for arm in ARMS if arm.name not in NON_DEFAULT_ARMS)


def split_csv(text: str) -> list[str]:
    """Split a comma separated option value, preserving order without repeats."""
    return list(dict.fromkeys(part.strip() for part in text.split(",") if part.strip()))


def resolve_path(value: str) -> Path:
    path = Path(value).expanduser()
    return path if path.is_absolute() else (REPO_ROOT / path)


def read_text(path: Path) -> str:
    try:
        return path.read_text().strip()
    except OSError:
        return ""


def sched_ext_state() -> str:
    return read_text(SCHED_EXT_STATE) or "absent"


def sched_ext_ops() -> str:
    return read_text(SCHED_EXT_OPS)


def loadavg1() -> float | None:
    text = read_text(PROC_LOADAVG)
    if not text:
        return None
    try:
        return float(text.split()[0])
    except (IndexError, ValueError):
        return None


def proc_stat_sample() -> tuple[int | None, int | None]:
    """Return the cumulative context switch count and the runnable task count."""
    ctxt: int | None = None
    procs_running: int | None = None
    text = read_text(PROC_STAT)
    for line in text.splitlines():
        if line.startswith("ctxt "):
            ctxt = int(line.split()[1])
        elif line.startswith("procs_running "):
            procs_running = int(line.split()[1])
    return ctxt, procs_running


def wait_sched_ext_disabled() -> tuple[bool, str, float]:
    """Poll until no sched_ext scheduler is attached."""
    started = time.monotonic()
    while True:
        state = sched_ext_state()
        if state in ("disabled", "absent"):
            return True, state, time.monotonic() - started
        if time.monotonic() - started >= SETTLE_TIMEOUT_SECONDS:
            return False, state, time.monotonic() - started
        time.sleep(SETTLE_POLL_SECONDS)


def dmesg_sched_ext_tail() -> str:
    try:
        result = subprocess.run(
            ["sudo", "-n", "dmesg", "-T"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return f"dmesg unavailable: {exc}"
    lines = [line for line in result.stdout.splitlines() if "sched_ext" in line or "sched-ext" in line]
    return "\n".join(lines[-DMESG_TAIL_LINES:])


def tail(text: str, lines: int) -> str:
    return "\n".join(text.splitlines()[-lines:])


def db_bench_args(
    *,
    db_bench: Path,
    db_dir: Path,
    benchmark: str,
    threads: int,
    num: int,
    use_existing_db: bool,
    reads: int | None = None,
) -> list[str]:
    args = [
        str(db_bench),
        f"--benchmarks={benchmark}",
        f"--threads={threads}",
        f"--num={num}",
        f"--db={db_dir}",
        f"--use_existing_db={1 if use_existing_db else 0}",
    ]
    if reads is not None:
        args.append(f"--reads={reads}")
    return args


def ops_per_thread(args: argparse.Namespace, threads: int) -> int:
    """The per-thread operation count db_bench receives for one run."""
    return args.total_ops // threads


def db_dir_is_writable(db_dir: Path) -> bool:
    """True when this user can create and unlink files inside the database."""
    return os.access(db_dir, os.R_OK | os.W_OK | os.X_OK)


def reset_db_dir(db_dir: Path) -> None:
    """Drop the database so the next run starts from an empty store.

    Elevated arms create their files as root, so a plain removal can fail with
    EACCES; the privileged removal is the fallback for exactly that case.
    """
    if db_dir.exists():
        shutil.rmtree(db_dir, ignore_errors=True)
    if db_dir.exists():
        subprocess.run(
            ["sudo", "-n", "rm", "-rf", "--", str(db_dir)],
            capture_output=True,
            text=True,
            check=False,
        )
    if db_dir.exists():
        raise RuntimeError(f"could not remove database directory {db_dir}")
    db_dir.mkdir(parents=True, exist_ok=True)


def run_tag(run: Run) -> str:
    return f"{run.arm.name} threads={run.threads} repeat={run.repeat}"


def arm_command(run: Run, args: argparse.Namespace) -> list[str]:
    """Wrap db_bench so the arm's environment survives the privilege change.

    sudo drops LD_PRELOAD from its own environment, so the assignments are
    handed to `env` inside the elevated command instead.
    """
    library = args.libraries.get(run.arm.name)
    writes = args.benchmark == WRITE_BENCHMARK
    assignments: list[str] = []
    if library is not None:
        assignments.append(f"LD_PRELOAD={library}")
    assignments.extend(f"{key}={value}" for key, value in sorted(run.arm.env.items()))

    command: list[str] = []
    if run.arm.sudo:
        command.extend(["sudo", "-n"])
    if assignments:
        command.append("env")
        command.extend(assignments)
    command.extend(
        db_bench_args(
            db_bench=args.db_bench,
            db_dir=args.db_dir,
            benchmark=args.benchmark,
            threads=run.threads,
            # db_bench interprets --num as the per-thread write count, so the
            # write benchmark splits the requested total across the threads.
            num=ops_per_thread(args, run.threads) if writes else args.num,
            use_existing_db=not writes,
            reads=None if writes else ops_per_thread(args, run.threads),
        )
    )
    return command


def kill_process_group(process: subprocess.Popen, use_sudo: bool) -> None:
    """Kill the whole group so preloaded children cannot outlive the timeout."""
    try:
        pgid = os.getpgid(process.pid)
    except OSError:
        pgid = process.pid
    try:
        os.killpg(pgid, signal.SIGKILL)
    except OSError:
        if not use_sudo:
            return
    if use_sudo:
        subprocess.run(
            ["sudo", "-n", "kill", "-9", "--", f"-{pgid}"],
            capture_output=True,
            text=True,
            check=False,
        )


def run_command(command: list[str], timeout_seconds: float) -> tuple[int, str, str, bool, float]:
    started = time.monotonic()
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=timeout_seconds)
    except subprocess.TimeoutExpired:
        timed_out = True
        kill_process_group(process, use_sudo=command[:1] == ["sudo"])
        try:
            stdout, stderr = process.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            stdout, stderr = "", ""
    wall_seconds = time.monotonic() - started
    return process.returncode, stdout or "", stderr or "", timed_out, wall_seconds


def parse_result(output: str, benchmark: str) -> dict[str, str]:
    """Prefer the measured benchmark's line, falling back to the first one."""
    matches = RESULT_PATTERN.findall(output)
    named = [match for match in matches if match[0] == benchmark]
    groups = (named or matches or [("", "", "", "", "")])[0]
    return {
        "micros_per_op": groups[1],
        "mb_per_s": groups[2],
        "found": groups[3],
        "total": groups[4],
    }


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                digest.update(chunk)
    except OSError:
        return ""
    return digest.hexdigest()


def git_head() -> str:
    try:
        result = subprocess.run(
            ["git", "-C", str(REPO_ROOT), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    return result.stdout.strip() if result.returncode == 0 else ""


def fill_command(args: argparse.Namespace) -> list[str] | None:
    """The command that creates the database, or None when no fill is needed."""
    if args.benchmark == WRITE_BENCHMARK:
        return None
    if (args.db_dir / "CURRENT").exists() and db_dir_is_writable(args.db_dir):
        return None
    return db_bench_args(
        db_bench=args.db_bench,
        db_dir=args.db_dir,
        benchmark=FILL_BENCHMARK,
        threads=FILL_THREADS,
        num=args.num,
        use_existing_db=False,
    )


def fill_database(args: argparse.Namespace, command: list[str]) -> None:
    reset_db_dir(args.db_dir)
    returncode, stdout, stderr, timed_out, wall_seconds = run_command(
        command, args.timeout_seconds
    )
    if timed_out or returncode != 0:
        sys.stderr.write(tail(stdout, STDOUT_TAIL_LINES) + "\n")
        sys.stderr.write(tail(stderr, STDERR_TAIL_LINES) + "\n")
        raise RuntimeError(
            f"fill failed: exit={returncode} timed_out={timed_out} after {wall_seconds:.1f}s"
        )
    subprocess.run(["sync"], check=False)
    print(f"[fill] done in {wall_seconds:.1f}s", flush=True)


def write_metadata(args: argparse.Namespace, arms: tuple[str, ...]) -> None:
    metadata = {
        "kernel": os.uname().release,
        "nproc": os.cpu_count(),
        "git_head": git_head(),
        "db_bench": str(args.db_bench),
        "db_bench_sha256": sha256_of(args.db_bench),
        "lib_dir": str(args.lib_dir),
        "flexguard_lib": str(args.flexguard_lib),
        "db_dir": str(args.db_dir),
        "benchmark": args.benchmark,
        "arms": list(arms),
        "threads": list(args.threads),
        "repeats": args.repeats,
        "num": args.num,
        "total_ops": args.total_ops,
        "timeout_seconds": args.timeout_seconds,
        "argv": sys.argv,
        "started_at": datetime.now().astimezone().isoformat(timespec="seconds"),
    }
    (args.out / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")


def run_one(run: Run, args: argparse.Namespace) -> tuple[dict[str, object], dict[str, object]]:
    command = arm_command(run, args)
    tag = run_tag(run)
    errors: list[str] = []
    if args.benchmark == WRITE_BENCHMARK:
        reset_db_dir(args.db_dir)
    state_before = sched_ext_state()
    if run.arm.uses_bpf:
        settled, state_before, settle_seconds = wait_sched_ext_disabled()
        if not settled:
            errors.append(
                f"sched_ext still {state_before} after {settle_seconds:.1f}s before run"
            )
            print(f"[{tag}] WARNING {errors[-1]}", flush=True)

    ctxt_before, procs_before = proc_stat_sample()
    load_before = loadavg1()
    print(f"[{tag}] " + " ".join(command), flush=True)
    returncode, stdout, stderr, timed_out, wall_seconds = run_command(
        command, args.timeout_seconds
    )
    ctxt_after, procs_after = proc_stat_sample()
    state_after = sched_ext_state()
    ops_after = sched_ext_ops()

    parsed = parse_result(stdout + "\n" + stderr, args.benchmark)
    failed = timed_out or returncode != 0
    dmesg_tail = dmesg_sched_ext_tail() if run.arm.uses_bpf and failed else ""

    if run.arm.uses_bpf:
        settled, settle_state, settle_seconds = wait_sched_ext_disabled()
        if not settled:
            errors.append(
                f"sched_ext still {settle_state} after {settle_seconds:.1f}s after run"
            )
            print(f"[{tag}] WARNING {errors[-1]}", flush=True)

    row: dict[str, object] = {
        "benchmark": args.benchmark,
        "arm": run.arm.name,
        "threads": run.threads,
        "repeat": run.repeat,
        "wall_seconds": round(wall_seconds, 3),
        "micros_per_op": parsed["micros_per_op"],
        "mb_per_s": parsed["mb_per_s"],
        "found": parsed["found"],
        "total": parsed["total"],
        "exit_status": returncode,
        "timed_out": 1 if timed_out else 0,
        "ctxt_delta": (ctxt_after - ctxt_before) if (ctxt_before is not None and ctxt_after is not None) else "",
        "procs_running_before": procs_before if procs_before is not None else "",
        "procs_running_after": procs_after if procs_after is not None else "",
        "loadavg1_before": load_before if load_before is not None else "",
        "sched_ext_state_before": state_before,
        "sched_ext_state_after": state_after,
        "sched_ext_ops_after": ops_after,
        "error": "; ".join(errors),
    }
    record = dict(row)
    record.update(
        {
            "command": command,
            "ops_per_thread": ops_per_thread(args, run.threads),
            "epoch": time.time(),
            "stdout_tail": tail(stdout, STDOUT_TAIL_LINES),
            "stderr_tail": tail(stderr, STDERR_TAIL_LINES),
            "dmesg_sched_ext_tail": dmesg_tail,
        }
    )
    status = "TIMEOUT" if timed_out else f"exit={returncode}"
    print(
        f"[{tag}] {status} wall={wall_seconds:.1f}s micros/op={parsed['micros_per_op'] or 'NA'}",
        flush=True,
    )
    return row, record


def summarize(rows: list[dict[str, object]], benchmark: str) -> list[dict[str, object]]:
    groups: dict[tuple[str, int], list[dict[str, object]]] = {}
    for row in rows:
        groups.setdefault((str(row["arm"]), int(row["threads"])), []).append(row)

    arm_order = {arm.name: index for index, arm in enumerate(ARMS)}
    summary: list[dict[str, object]] = []
    for (arm_name, threads) in sorted(groups, key=lambda key: (arm_order.get(key[0], len(ARMS)), key[1])):
        group = groups[(arm_name, threads)]
        values = [float(row["micros_per_op"]) for row in group if row["micros_per_op"] != ""]
        timeouts = sum(1 for row in group if row["timed_out"] == 1)
        failures = sum(1 for row in group if row["exit_status"] != 0)
        summary.append(
            {
                "benchmark": benchmark,
                "arm": arm_name,
                "threads": threads,
                "n": len(values),
                "runs": len(group),
                "median_micros_per_op": round(statistics.median(values), 3) if values else "",
                "min_micros_per_op": round(min(values), 3) if values else "",
                "max_micros_per_op": round(max(values), 3) if values else "",
                "timeouts": timeouts,
                "failures": failures,
            }
        )
    return summary


def print_summary(summary: list[dict[str, object]], benchmark: str) -> None:
    width = max((len(str(row["arm"])) for row in summary), default=8)
    print(f"\n{benchmark} micros/op (lower is better)")
    print(
        f"{'arm':<{width}}  {'threads':>7}  {'n':>3}  {'median':>10}  "
        f"{'min':>10}  {'max':>10}  {'timeouts':>8}  {'failures':>8}"
    )
    for row in summary:
        print(
            f"{row['arm']:<{width}}  {row['threads']:>7}  {row['n']:>3}  "
            f"{row['median_micros_per_op']:>10}  {row['min_micros_per_op']:>10}  "
            f"{row['max_micros_per_op']:>10}  {row['timeouts']:>8}  {row['failures']:>8}"
        )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run stock LevelDB db_bench under the fullhook lock arms.",
    )
    parser.add_argument("--db-bench", default="target/leveldb-stock/build/db_bench")
    parser.add_argument("--benchmark", choices=BENCHMARKS, default=READ_BENCHMARK)
    parser.add_argument("--arms", default=",".join(DEFAULT_ARMS))
    parser.add_argument("--threads", default="48,96,192")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--num", type=int, default=500000)
    # --total-reads is the former name of this option, kept for older invocations.
    parser.add_argument(
        "--total-ops", "--total-reads", dest="total_ops", type=int, default=1572864
    )
    parser.add_argument("--timeout-seconds", type=float, default=900.0)
    parser.add_argument("--db-dir", default="/tmp/accordin-readrandom-db")
    parser.add_argument("--out", default=None)
    parser.add_argument("--lib-dir", default="target/release")
    parser.add_argument("--flexguard-lib", default=DEFAULT_FLEXGUARD_LIB)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)

    names = split_csv(args.arms)
    unknown = [name for name in names if name not in ARMS_BY_NAME]
    if unknown:
        parser.error(
            f"Unknown arm {unknown[0]!r}. Supported arms: {', '.join(ARMS_BY_NAME)}"
        )
    if not names:
        parser.error("--arms must list at least one arm")
    args.arms = tuple(names)

    threads: list[int] = []
    for value in split_csv(args.threads):
        if not value.lstrip("+").isdigit() or int(value) <= 0:
            parser.error(f"--threads expects positive integers, got {value!r}")
        threads.append(int(value))
    if not threads:
        parser.error("--threads must list at least one value")
    args.threads = tuple(threads)

    if args.repeats <= 0:
        parser.error("--repeats must be positive")
    if args.out is None:
        args.out = (
            f"experiments/results/fullhook_{args.benchmark}"
            f"_{datetime.now():%Y%m%d_%H%M%S}"
        )
    args.db_bench = resolve_path(args.db_bench)
    args.db_dir = resolve_path(args.db_dir)
    args.out = resolve_path(args.out)
    args.lib_dir = resolve_path(args.lib_dir)
    args.flexguard_lib = resolve_path(args.flexguard_lib)
    # Preload paths resolved once: arms name a file inside --lib-dir, and the
    # flexguard interposer comes from its own option.
    args.libraries = {
        arm.name: args.lib_dir / arm.library for arm in ARMS if arm.library
    }
    args.libraries["flexguard"] = args.flexguard_lib
    return args


def build_plan(args: argparse.Namespace) -> tuple[Run, ...]:
    """Every measured run, in execution order."""
    return tuple(
        Run(arm=ARMS_BY_NAME[name], threads=threads, repeat=repeat)
        for repeat in range(1, args.repeats + 1)
        for threads in args.threads
        for name in args.arms
    )


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    plan = build_plan(args)

    if not args.dry_run:
        if not args.db_bench.is_file():
            raise SystemExit(f"db_bench not found: {args.db_bench}")
        missing = sorted(
            {
                str(library)
                for run in plan
                for library in (args.libraries.get(run.arm.name),)
                if library is not None and not library.is_file()
            }
        )
        if missing:
            raise SystemExit("preload library not found: " + ", ".join(missing))
        for threads in args.threads:
            if ops_per_thread(args, threads) <= 0:
                raise SystemExit(
                    f"--total-ops {args.total_ops} yields no operations at {threads} threads"
                )

    command = fill_command(args)
    if args.benchmark == WRITE_BENCHMARK:
        print(
            f"[fill] skipped: every {args.benchmark} run rebuilds {args.db_dir}",
            flush=True,
        )
    elif command is None:
        print(f"[fill] reusing existing database at {args.db_dir}", flush=True)
    else:
        print("[fill] " + " ".join(command), flush=True)
        if args.dry_run:
            print("[fill] sync")
        else:
            fill_database(args, command)

    if args.dry_run:
        for run in plan:
            print(f"[{run_tag(run)}] " + " ".join(arm_command(run, args)), flush=True)
        return 0

    args.out.mkdir(parents=True, exist_ok=True)
    write_metadata(args, args.arms)
    rows: list[dict[str, object]] = []

    with (args.out / "raw.csv").open("w", newline="") as raw_handle:
        writer: csv.DictWriter | None = None
        with (args.out / "runs.jsonl").open("w") as jsonl_handle:
            for run in plan:
                row, record = run_one(run, args)
                rows.append(row)
                if writer is None:
                    writer = csv.DictWriter(raw_handle, fieldnames=list(row))
                    writer.writeheader()
                writer.writerow(row)
                raw_handle.flush()
                jsonl_handle.write(json.dumps(record) + "\n")
                jsonl_handle.flush()

    summary = summarize(rows, args.benchmark)
    if summary:
        with (args.out / "summary.csv").open("w", newline="") as handle:
            summary_writer = csv.DictWriter(handle, fieldnames=list(summary[0]))
            summary_writer.writeheader()
            summary_writer.writerows(summary)
        print_summary(summary, args.benchmark)
    print(f"\nresults: {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
