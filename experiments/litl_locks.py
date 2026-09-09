"""Resolution of the LiTL algorithm that puts a lock in front of a benchmarked process.

Every lock the benchmarks compare lives in the LiTL checkout. A LiTL launcher is
a shell wrapper that appends the algorithm's shared object to LD_PRELOAD and
execs the command it is given, so the process below it sees the algorithm on
every pthread_mutex_* call and the benchmark itself only ever needs a plain
pthread mutex. Preloading the same shared object directly selects the same
algorithm.

This module is the single place that knows which LiTL algorithm implements which
lock name, where a checkout is, and how to build one.
"""

from __future__ import annotations

import os
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_LITL_DIR = REPO_ROOT / "third_party" / "litl"
LITL_DIR_ENV = "LITL_DIR"

# Baseline locks of the application experiments. Each one is a LiTL "direct"
# algorithm: they share src/directlock.c and src/directcond.c, so the compared
# configurations differ in the lock algorithm and in nothing else.
BASELINE_ALGORITHMS = {
    "mcs": "mbmcs_original",
    "mcs-tas": "mbmcstas_original",
    "gcr": "gcr_original",
    "flexguard": "flexguard_original",
    # MCS holding an rseq time-slice extension across the critical section.
    "mcs-tse": "mbmcstse_original",
}

# Accordin reaches a process through the same front end, but keeps its own
# environment variables and its scheduler still needs root.
ACCORDIN_ALGORITHMS = {
    "accordin": "mcstasaccordin_original",
    "mcs_accordin_direct": "mcsaccordin_original",
    "mcs_tas_accordin_direct": "mcstasaccordin_original",
}

ALGORITHMS = {**BASELINE_ALGORITHMS, **ACCORDIN_ALGORITHMS}

# The six locks the application experiments compare, in reporting order.
EXPERIMENT_LOCKS: tuple[str, ...] = tuple(BASELINE_ALGORITHMS) + ("accordin",)


def litl_dir() -> Path:
    override = os.environ.get(LITL_DIR_ENV)
    if override:
        return Path(override).expanduser().resolve()
    return DEFAULT_LITL_DIR


def litl_algorithm(lock: str) -> str:
    try:
        return ALGORITHMS[lock]
    except KeyError as exc:
        supported = ", ".join(sorted(ALGORITHMS))
        raise ValueError(
            f"No LiTL algorithm is mapped for lock {lock}. Mapped locks: {supported}."
        ) from exc


def litl_launcher(lock: str, directory: Path | None = None) -> Path:
    return (directory or litl_dir()) / f"lib{litl_algorithm(lock)}.sh"


def litl_library(lock: str, directory: Path | None = None) -> Path:
    return (directory or litl_dir()) / "lib" / f"lib{litl_algorithm(lock)}.so"


def build_algorithms(locks) -> list[str]:
    """The LiTL algorithm list that builds every one of `locks`, without repeats."""
    return sorted({litl_algorithm(lock) for lock in locks})
