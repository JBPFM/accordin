#!/usr/bin/env python3
from __future__ import annotations

import csv
import sys
from pathlib import Path
from typing import Iterable


FAILURE_FIELDS = (
    "experiment",
    "workload",
    "benchmark",
    "lock",
    "threads",
    "repeat",
    "stage",
    "status",
    "returncode",
    "command_log",
    "message",
)
TIMEOUT_RETURNCODES = {-9, 124, 137}


def relative_path(root: Path, path: Path | str | None) -> str:
    if path is None:
        return ""
    candidate = Path(path)
    try:
        return str(candidate.relative_to(root))
    except ValueError:
        return str(candidate)


def status_from_returncode(returncode: int | None) -> str:
    if returncode in TIMEOUT_RETURNCODES:
        return "timeout"
    return "failed"


def append_failure(
    failures: list[dict[str, str]],
    *,
    result_root: Path,
    experiment: str,
    workload: str = "",
    benchmark: str = "",
    lock: str = "",
    threads: int | str = "",
    repeat: int | str = "",
    stage: str,
    status: str,
    returncode: int | str = "",
    command_log: Path | str | None = None,
    message: str,
) -> dict[str, str]:
    row = {
        "experiment": experiment,
        "workload": workload,
        "benchmark": benchmark,
        "lock": lock,
        "threads": str(threads),
        "repeat": str(repeat),
        "stage": stage,
        "status": status,
        "returncode": str(returncode),
        "command_log": relative_path(result_root, command_log),
        "message": message,
    }
    failures.append(row)
    print(
        "Recorded failed test: "
        f"experiment={experiment} workload={workload or '-'} benchmark={benchmark or '-'} "
        f"lock={lock or '-'} threads={threads or '-'} repeat={repeat or '-'} "
        f"stage={stage} status={status} log={row['command_log'] or '-'}",
        file=sys.stderr,
        flush=True,
    )
    return row


def append_command_failure(
    failures: list[dict[str, str]],
    *,
    result_root: Path,
    experiment: str,
    workload: str = "",
    benchmark: str = "",
    lock: str = "",
    threads: int | str = "",
    repeat: int | str = "",
    stage: str = "run",
    exc: object,
) -> dict[str, str]:
    returncode = getattr(exc, "returncode", "")
    log_path = getattr(exc, "log_path", None)
    return append_failure(
        failures,
        result_root=result_root,
        experiment=experiment,
        workload=workload,
        benchmark=benchmark,
        lock=lock,
        threads=threads,
        repeat=repeat,
        stage=stage,
        status=status_from_returncode(returncode if isinstance(returncode, int) else None),
        returncode=returncode,
        command_log=log_path,
        message=str(exc),
    )


def write_failures_csv(result_root: Path, failures: list[dict[str, str]]) -> Path | None:
    path = result_root / "failed_runs.csv"
    if not failures:
        return None
    with path.open("w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=FAILURE_FIELDS)
        writer.writeheader()
        writer.writerows(failures)
    return path


def print_failure_summary(failures: Iterable[dict[str, str]], path: Path | None = None) -> None:
    rows = list(failures)
    if not rows:
        print("Failed tests: none")
        return
    print(f"Failed tests: {len(rows)}")
    if path is not None:
        print(f"Failures CSV: {path}")
    for row in rows:
        print(
            "  "
            f"{row['experiment']}/{row['workload'] or '-'}"
            f"/{row['benchmark'] or '-'}"
            f"/{row['lock'] or '-'}"
            f"/t{row['threads'] or '-'}"
            f"/r{row['repeat'] or '-'}"
            f" {row['stage']} {row['status']} rc={row['returncode'] or '-'} "
            f"log={row['command_log'] or '-'}"
        )
