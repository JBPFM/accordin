"""Resolution of the LiTL algorithm that puts a lock in front of a benchmarked process.

Every lock the benchmarks compare lives in the LiTL checkout. A LiTL launcher is
a shell wrapper that appends the algorithm's shared object to LD_PRELOAD and
execs the command it is given, so the process below it sees the algorithm on
every pthread_mutex_* call and the benchmark itself only ever needs a plain
pthread mutex. Preloading the same shared object directly selects the same
algorithm.

This module is the single place that knows which LiTL algorithm implements which
lock name and how a built checkout is laid out.
"""

from __future__ import annotations

import hashlib
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_LITL_DIR = REPO_ROOT / "third_party" / "litl"

# Each compared lock is a LiTL "direct" algorithm: they share src/directlock.c
# and src/directcond.c, so the compared configurations differ in the lock
# algorithm and in nothing else. Accordin reaches a process through the same
# front end, but keeps its own environment variables and its scheduler still
# needs root. The two mcs*_accordin_direct names are the microbenchmark's own
# spellings of the Accordin adapters.
ALGORITHMS = {
    "mcs": "mbmcs_original",
    "mcs-tas": "mbmcstas_original",
    "gcr": "gcr_original",
    "flexguard": "flexguard_original",
    # MCS holding an rseq time-slice extension across the critical section.
    "mcs-tse": "mbmcstse_original",
    "accordin": "mcstasaccordin_original",
    "mcs_accordin_direct": "mcsaccordin_original",
    "mcs_tas_accordin_direct": "mcstasaccordin_original",
}

# The six locks the application experiments compare, in reporting order.
EXPERIMENT_LOCKS: tuple[str, ...] = (
    "mcs", "mcs-tas", "gcr", "flexguard", "mcs-tse", "accordin")


def sha256(path) -> str:
    with Path(path).open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def litl_algorithm(lock: str) -> str:
    try:
        return ALGORITHMS[lock]
    except KeyError as exc:
        supported = ", ".join(sorted(ALGORITHMS))
        raise ValueError(
            f"No LiTL algorithm is mapped for lock {lock}. Mapped locks: {supported}."
        ) from exc


def litl_launcher(lock: str, directory: Path = DEFAULT_LITL_DIR) -> Path:
    return directory / f"lib{litl_algorithm(lock)}.sh"


def litl_library(lock: str, directory: Path) -> Path:
    return directory / "lib" / f"lib{litl_algorithm(lock)}.so"


def manifest_locks(litl_dir: Path) -> dict[str, str]:
    """The library path of every compared lock inside a built LiTL checkout."""
    return {lock: str(litl_library(lock, litl_dir)) for lock in EXPERIMENT_LOCKS}
