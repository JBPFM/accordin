from __future__ import annotations

import os
import re
import subprocess
from dataclasses import dataclass


PROFILE_ENV = "ACCORDIN_EXPERIMENT_MACHINE_PROFILE"


@dataclass(frozen=True)
class MachineConfig:
    name: str
    physical_cores: int
    logical_cpus: int
    thread_counts: tuple[int, ...]
    decomposition_threads: tuple[int, ...]
    mcs_accordin_taskset_cpus: str
    leveldb_per_lock_max_threads: int


CURRENT_MACHINE = MachineConfig(
    name="current-20c40t",
    physical_cores=40,
    logical_cpus=40,
    thread_counts=(4, 8, 16, 20, 32, 40, 64, 96),
    decomposition_threads=(40, 64, 96),
    mcs_accordin_taskset_cpus="0,2,4,6,8,10,12,14,16,18,20",
    leveldb_per_lock_max_threads=96,
)

ORIGINAL_MACHINE = MachineConfig(
    name="original-96c",
    physical_cores=96,
    logical_cpus=256,
    thread_counts=(4, 8, 16, 32, 64, 96, 128, 192, 256),
    decomposition_threads=(128, 192, 256),
    mcs_accordin_taskset_cpus="0,2,4,8,10,12,14,16,18,20,22",
    leveldb_per_lock_max_threads=128,
)

PROFILE_ALIASES = {
    "auto": None,
    "current": CURRENT_MACHINE,
    "local": CURRENT_MACHINE,
    "small": CURRENT_MACHINE,
    "20c40t": CURRENT_MACHINE,
    CURRENT_MACHINE.name: CURRENT_MACHINE,
    "original": ORIGINAL_MACHINE,
    "large": ORIGINAL_MACHINE,
    "paper": ORIGINAL_MACHINE,
    "96c": ORIGINAL_MACHINE,
    "96core": ORIGINAL_MACHINE,
    ORIGINAL_MACHINE.name: ORIGINAL_MACHINE,
}


def detect_physical_cores() -> int | None:
    try:
        completed = subprocess.run(
            ["lscpu"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except OSError:
        return None
    if completed.returncode != 0:
        return None

    cores_per_socket = None
    sockets = None
    for line in completed.stdout.splitlines():
        if match := re.match(r"^Core\(s\) per socket:\s+(\d+)$", line):
            cores_per_socket = int(match.group(1))
        elif match := re.match(r"^Socket\(s\):\s+(\d+)$", line):
            sockets = int(match.group(1))
    if cores_per_socket is None or sockets is None:
        return None
    return cores_per_socket * sockets


def select_machine_config() -> MachineConfig:
    requested = os.environ.get(PROFILE_ENV, "auto").strip().lower()
    if requested not in PROFILE_ALIASES:
        supported = ", ".join(sorted(key for key in PROFILE_ALIASES if key != "auto"))
        raise RuntimeError(f"Unsupported {PROFILE_ENV}={requested!r}. Supported profiles: auto, {supported}.")

    configured = PROFILE_ALIASES[requested]
    if configured is not None:
        return configured

    logical_cpus = os.cpu_count() or 0
    physical_cores = detect_physical_cores()
    if logical_cpus <= CURRENT_MACHINE.logical_cpus or physical_cores == CURRENT_MACHINE.physical_cores:
        return CURRENT_MACHINE
    return ORIGINAL_MACHINE


def first_oversubscribed_thread_count(thread_counts: tuple[int, ...], physical_cores: int) -> int | None:
    oversubscribed = [thread for thread in thread_counts if thread > physical_cores]
    if not oversubscribed:
        return None
    return min(oversubscribed)


def limit_to_single_oversubscribed_thread_count(
    thread_counts: tuple[int, ...],
    physical_cores: int,
) -> tuple[int, ...]:
    first_oversubscribed = first_oversubscribed_thread_count(thread_counts, physical_cores)
    if first_oversubscribed is None:
        return thread_counts
    return tuple(
        thread
        for thread in thread_counts
        if thread <= physical_cores or thread == first_oversubscribed
    )


ACTIVE_MACHINE_CONFIG = select_machine_config()
MACHINE_PHYSICAL_CORES = ACTIVE_MACHINE_CONFIG.physical_cores
MACHINE_LOGICAL_CPUS = ACTIVE_MACHINE_CONFIG.logical_cpus
DEFAULT_THREAD_COUNTS = ACTIVE_MACHINE_CONFIG.thread_counts
DECOMPOSITION_THREAD_COUNTS = ACTIVE_MACHINE_CONFIG.decomposition_threads
DEFAULT_MCS_ACCORDIN_TASKSET_CPUS = ACTIVE_MACHINE_CONFIG.mcs_accordin_taskset_cpus
LEVELDB_PER_LOCK_MAX_THREADS = ACTIVE_MACHINE_CONFIG.leveldb_per_lock_max_threads
